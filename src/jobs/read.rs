//! `Stage::Read`: every note is read once by the decision model after it is embedded — its kind,
//! whether it records a decision, whether it will go out of date, whether it stands alone.
//! Readings are stored at a fixed line; they are not decisions: no band, no Unsure row, no waiting.

use crate::core::Core;
use crate::core::judging::band::pick_and_top;
use crate::core::judging::catalogue::{category, pinned, self_contained, time_bound};
use crate::core::judging::{Asked, state};
use crate::error::{Error, Result};
use crate::store::artifacts::Chunk;
use crate::store::jobs::{MAX_ATTEMPTS, Stage};
use crate::store::readings::{ReadMark, Visit};

/// `pinned` is stored and has no effect: the only pin that ranks is a tag a person sets.
pub const PINNED_AT: f64 = 0.90;
pub const TIME_BOUND_AT: f64 = 0.5;
pub const SELF_CONTAINED_AT: f64 = 0.5;

/// A note the model would not read is left an hour, twice as long after each further visit, and
/// a day at most.
const RETRY_FIRST_SECS: i64 = 3_600;
const RETRY_MAX_SECS: i64 = 86_400;

/// Rounds of write-then-look in `sync_payload`; an edit lands in one, and each further one needs
/// yet another edit inside a single payload write.
const PAYLOAD_ROUNDS: usize = 4;

/// The four questions the catalogue still holds (time_bound may have been dropped by the replay).
pub fn reading_questions() -> Vec<Asked> {
    vec![category(), pinned(), time_bound(), self_contained()]
}

/// The questions and their versions as a mark keeps them.
pub fn asked_key(asked: &[Asked]) -> String {
    let versions: Vec<(&str, u32)> = asked.iter().map(|a| (a.id, a.version)).collect();
    crate::store::readings::asked_key(&versions)
}

/// The tokens a note has to fit in beside these questions, as a mark keeps them.
pub fn room(core: &Core, asked: &[Asked]) -> i64 {
    let refs: Vec<&Asked> = asked.iter().collect();
    i64::try_from(state::room(core, &refs)).unwrap_or(i64::MAX)
}

/// What a reading is of: the title and the text. A re-embed changes neither.
fn text_hash(c: &Chunk) -> String {
    crate::store::corpora::content_hash(format!(
        "{}\u{0}{}",
        c.title.as_deref().unwrap_or_default(),
        c.text
    ))
}

fn retry_wait(tries: i64) -> i64 {
    let doublings = (tries.clamp(1, 16) - 1) as u32;
    RETRY_FIRST_SECS
        .saturating_mul(1i64 << doublings)
        .min(RETRY_MAX_SECS)
}

/// Whether the reader has anything to do here: not when the note's readings are of the text it
/// has now, and not when the last visit could not read that text and nothing about that changed.
async fn due(core: &Core, me: &Chunk) -> Result<bool> {
    let asked = reading_questions();
    let key = asked_key(&asked);
    let Some(mark) = core.store.read_mark(&me.id).await? else {
        // Readings from before visits were marked stand while the text is no newer than they are.
        return Ok(!core.store.readings_current(&me.id, &key).await?);
    };
    if mark.text_hash != text_hash(me) || mark.asked != key {
        return Ok(true);
    }
    Ok(match mark.visit {
        Visit::Read => false,
        Visit::NoRoom => mark.room != room(core, &asked),
        Visit::Rejected => mark.retry_at <= crate::store::now(),
    })
}

/// After an embed. A reading is of the text, not the vector, so a note whose text was already
/// read (or could not be, see `due`) is left alone: a re-index costs no call.
pub async fn arm(core: &Core, artifact_id: &str) -> Result<()> {
    let me = match core.store.get_artifact(artifact_id).await {
        Ok(me) => Some(me),
        // Gone since: the unit finds that out and closes.
        Err(Error::NotFound) => None,
        Err(e) => return Err(e),
    };
    if let Some(me) = &me
        && !due(core, me).await?
    {
        return Ok(());
    }
    queue(core, artifact_id).await
}

/// Idle-only, as `relate::arm`: a queued read keeps its attempts. For a caller that has already
/// chosen the note, as the quiet scan has.
pub async fn queue(core: &Core, artifact_id: &str) -> Result<()> {
    core.store
        .rearm_idle_seq(Stage::Read, "artifact", artifact_id, 0)
        .await
}

pub async fn run(core: &Core, artifact_id: &str) -> Result<()> {
    let me = core.store.get_artifact(artifact_id).await?;
    if !me.in_results() {
        return Ok(());
    }
    let asked = reading_questions();
    let refs: Vec<&Asked> = asked.iter().collect();
    let room = state::room(core, &refs);
    let mut mark = ReadMark {
        text_hash: text_hash(&me),
        asked: asked_key(&asked),
        room: i64::try_from(room).unwrap_or(i64::MAX),
        visit: Visit::Read,
        tries: 0,
        retry_at: 0,
    };
    let note = state::Note {
        title: me.title.as_deref(),
        day: None,
        text: &me.text,
    };
    // A reading describes and does not act, so here, and only here, a cut text is read: its opening.
    let whole = state::one(&note, &core.counter, usize::MAX).unwrap_or_default();
    let Some(shown) = core.counter.cut(&whole, room, true) else {
        tracing::info!(artifact_id, "no room to read this note");
        // Marked, so the scan does not come back for it while nothing has changed.
        mark.visit = Visit::NoRoom;
        return core.store.put_read_mark(artifact_id, &mark).await;
    };
    let questions: Vec<_> = asked.iter().map(|a| a.question.clone()).collect();
    let permit = core.gate.background_light().await;
    let got = core.decider.decide(&shown, &questions).await;
    permit.finished();
    let answers = match got {
        Ok(got) => got.answers,
        Err(e) => return unread(core, artifact_id, mark, e).await,
    };
    let rows = asked
        .iter()
        .map(|a| {
            answers
                .get(&a.question.key)
                .map(|answer| (a, answer))
                .ok_or_else(|| Error::InferenceRejected {
                    role: "decide",
                    detail: format!("the answer has none for \"{}\"", a.id),
                })
        })
        .collect::<Result<Vec<_>>>();
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => return unread(core, artifact_id, mark, e).await,
    };
    core.store
        .put_readings(artifact_id, &rows, core.decider.model())
        .await?;
    core.store.put_read_mark(artifact_id, &mark).await?;

    // `category` is asked first. Its top pick is taken whatever its probability: a kind describes.
    let (pick, _) = pick_and_top(&rows[0].0.question, rows[0].1);
    let kind = crate::infer::prompt::normalize_category(&pick);
    core.store.fill_category(artifact_id, &kind).await?;
    // Whether or not it landed: a retry after a failed payload write finds the category filled.
    sync_payload(core, artifact_id).await
}

/// A read the model did not give. A failure the queue retries is left to it up to the ceiling.
/// There, or at once for a refusal or for a note already refused, the unit closes and the note
/// waits out `retry_wait`: nothing stays queued, and the scan and the next embed pass it by.
async fn unread(core: &Core, artifact_id: &str, mut mark: ReadMark, e: Error) -> Result<()> {
    let before = core
        .store
        .read_mark(artifact_id)
        .await?
        .filter(|m| {
            m.visit == Visit::Rejected && m.text_hash == mark.text_hash && m.asked == mark.asked
        })
        .map_or(0, |m| m.tries);
    let spent = core
        .store
        .live_job_attempts(Stage::Read, artifact_id)
        .await?
        .is_some_and(|a| a >= MAX_ATTEMPTS);
    if e.retryable() && before == 0 && !spent {
        return Err(e);
    }
    mark.visit = Visit::Rejected;
    mark.tries = before + 1;
    mark.retry_at = crate::store::now() + retry_wait(mark.tries);
    tracing::warn!(
        artifact_id,
        error = %e,
        tries = mark.tries,
        "could not read this note; leaving it for now"
    );
    core.store.put_read_mark(artifact_id, &mark).await
}

/// Writes the vector payload from the row as stored, then looks at the row again: an edit that
/// landed meanwhile is written too, so the last write is never older than the row. That holds
/// between writers that look again; one that does not can still leave its older value last.
pub async fn sync_payload(core: &Core, artifact_id: &str) -> Result<()> {
    let mut chunk = core.store.get_artifact(artifact_id).await?;
    for _ in 0..PAYLOAD_ROUNDS {
        // A pending embed writes the whole payload anyway.
        if chunk.embed_state != crate::store::artifacts::EmbedState::Embedded {
            return Ok(());
        }
        let written = crate::vector::VectorPayload::edited(&chunk);
        core.vectors.set_payload(&written).await?;
        chunk = core.store.get_artifact(artifact_id).await?;
        if crate::vector::VectorPayload::edited(&chunk) == written {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::judging::catalogue::pinned;
    use crate::core::test_support::{seed_artifact, test_core, test_core_without_synthesizer};
    use crate::infer::decide::{Answer, Answers, Kind, Question};
    use crate::infer::fake::RuleDecider;
    use crate::jobs::test_support::drain;
    use crate::store::artifacts::{Chunk, NewArtifact, NewMerged};
    use crate::store::jobs::Stage;
    use std::collections::HashMap;
    use std::sync::Arc;

    const KEYS: [&str; 4] = ["category", "pinned", "time_bound", "self_contained"];

    /// `category` picks `kind` at `top`; `pinned` says `pin`; anything else is asked a flat answer,
    /// so no other stage acts on it.
    fn reader(kind: &'static str, top: f64, pin: f64) -> Arc<RuleDecider> {
        Arc::new(RuleDecider::new(move |_, qs: &[Question]| {
            let mut answers = HashMap::new();
            for q in qs {
                let a = match (&q.kind, q.key.as_str()) {
                    (Kind::Choice { options }, "category") => {
                        let rest = (1.0 - top) / (options.len() - 1) as f64;
                        Answer::Choice {
                            pick: kind.into(),
                            probabilities: options
                                .iter()
                                .map(|(n, _)| if n == kind { top } else { rest })
                                .collect(),
                            confidence: top,
                        }
                    }
                    (_, "pinned") => Answer::Noul { yes: pin },
                    (Kind::Noul, _) => Answer::Noul { yes: 0.5 },
                    (Kind::Choice { options }, _) => Answer::Choice {
                        pick: options[0].0.clone(),
                        probabilities: vec![1.0 / options.len() as f64; options.len()],
                        confidence: 0.0,
                    },
                    (Kind::Score { levels }, _) => Answer::Score {
                        score: 0.0,
                        probabilities: vec![1.0 / levels.len() as f64; levels.len()],
                        confidence: 0.0,
                    },
                };
                answers.insert(q.key.clone(), a);
            }
            Ok(Answers {
                answers,
                model: "rule-decider".into(),
                input_tokens: 0,
            })
        }))
    }

    fn reads(d: &RuleDecider) -> Vec<(String, Vec<Question>)> {
        d.requests()
            .into_iter()
            .filter(|(_, qs)| qs.iter().any(|q| q.key == "category"))
            .collect()
    }

    async fn category_by(core: &Core, id: &str) -> Option<String> {
        sqlx::query_scalar!("SELECT category_by FROM artifacts WHERE id = ?", id)
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }

    async fn capture(core: &Core, text: &str) -> Vec<Chunk> {
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new(text, "ui"))
            .await
            .unwrap();
        drain(core).await;
        core.store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .filter(Chunk::in_results)
            .collect()
    }

    async fn note(core: &Core, text: &str) -> String {
        let src = core.store.insert_corpus(text, "web", None).await.unwrap();
        core.store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: text.into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()
            .remove(0)
            .id
    }

    #[tokio::test]
    async fn a_new_note_is_read_once_after_it_is_embedded() {
        let mut core = test_core().await;
        let dec = reader("procedure", 0.8, 0.1);
        core.decider = dec.clone();
        let live = capture(&core, "To rotate the key, run vault operator rotate.").await;
        assert!(!live.is_empty());
        let asked = reads(&dec);
        for (_, qs) in &asked {
            let keys: Vec<&str> = qs.iter().map(|q| q.key.as_str()).collect();
            assert_eq!(keys, KEYS, "one request with the reading questions");
        }
        let mut read = 0;
        for c in &live {
            let rows = core.store.readings_of(&c.id).await.unwrap();
            assert_eq!(rows.len(), 4, "{}", c.id);
            assert!(rows.iter().all(|r| r.model == "rule-decider"));
            read += 1;
        }
        assert_eq!(asked.len(), read, "each note is read once");
        drain(&core).await;
        assert_eq!(
            reads(&dec).len(),
            read,
            "a drained queue reads nothing again"
        );
    }

    #[tokio::test]
    async fn a_passage_is_read_too_and_gets_a_category() {
        let mut core = test_core_without_synthesizer().await;
        core.decider = reader("reference", 0.7, 0.1);
        let live = capture(&core, "Opening hours: Mon to Fri, 9 to 17.").await;
        assert!(!live.is_empty());
        for c in &live {
            assert_eq!(c.provenance, crate::store::artifacts::Provenance::Passage);
            assert_eq!(c.category.as_deref(), Some("reference"), "{}", c.id);
            assert_eq!(category_by(&core, &c.id).await.as_deref(), Some("reading"));
        }
    }

    #[tokio::test]
    async fn the_top_category_is_taken_whatever_its_probability() {
        let mut core = test_core_without_synthesizer().await;
        core.decider = reader("procedure", 0.21, 0.1);
        let live = capture(&core, "Something that is hard to place.").await;
        assert!(!live.is_empty());
        let ids: Vec<String> = live.iter().map(|c| c.id.clone()).collect();
        let payloads = core.vectors.payloads_of(&ids).await.unwrap();
        for c in &live {
            assert_eq!(c.category.as_deref(), Some("procedure"));
            assert_eq!(category_by(&core, &c.id).await.as_deref(), Some("reading"));
            assert_eq!(
                payloads[&c.id].category.as_deref(),
                Some("procedure"),
                "the vector payload follows"
            );
        }
    }

    #[tokio::test]
    async fn a_reading_never_overwrites_a_category_a_person_set() {
        let mut core = test_core().await;
        let id = seed_artifact(&core, "Keys").await;
        core.store
            .update_artifact_category_by(&id, Some("procedure"), "person")
            .await
            .unwrap();
        core.decider = reader("concept", 0.99, 0.1);
        run(&core, &id).await.unwrap();
        let c = core.store.get_artifact(&id).await.unwrap();
        assert_eq!(c.category.as_deref(), Some("procedure"));
        assert_eq!(category_by(&core, &id).await.as_deref(), Some("person"));
        assert_eq!(core.store.readings_of(&id).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn a_category_a_person_cleared_stays_cleared() {
        let mut core = test_core().await;
        let id = seed_artifact(&core, "Keys").await;
        core.store
            .update_artifact_category_by(&id, None, "person")
            .await
            .unwrap();
        core.decider = reader("concept", 0.99, 0.1);
        run(&core, &id).await.unwrap();
        assert_eq!(core.store.get_artifact(&id).await.unwrap().category, None);
        assert_eq!(category_by(&core, &id).await.as_deref(), Some("person"));
    }

    #[tokio::test]
    async fn a_category_a_writer_gave_is_left() {
        let mut core = test_core().await;
        let roots = crate::jobs::consolidate::tests::seed(
            &core,
            &[
                ("first wording", [1.0, 0.0]),
                ("second wording", [0.99, 0.05]),
            ],
        )
        .await;
        let m = core
            .store
            .insert_merged_artifact(
                &NewMerged {
                    title: Some("merged".into()),
                    text: "both wordings".into(),
                    category: Some("reference".into()),
                    ..Default::default()
                },
                &roots,
            )
            .await
            .unwrap();
        core.decider = reader("concept", 0.99, 0.1);
        run(&core, &m.id).await.unwrap();
        let c = core.store.get_artifact(&m.id).await.unwrap();
        assert_eq!(c.category.as_deref(), Some("reference"));
        assert_eq!(category_by(&core, &m.id).await, None);
        assert_eq!(core.store.readings_of(&m.id).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn pinned_is_stored_and_changes_no_tag_and_no_ranking() {
        let mut core = test_core().await;
        core.decider = reader("reference", 0.8, 0.01);
        capture(&core, "The proxy listens on port 8080.").await;
        capture(&core, "The proxy restarts every night at two.").await;
        let q = crate::core::search::SearchQuery {
            q: "proxy".into(),
            limit: 10,
            tags: vec![],
            category: None,
            mark: false,
            rerank: false,
            explain: false,
            include_deprecated: false,
            include_superseded: false,
        };
        let seen = |hits: Vec<crate::core::search::SearchResult>| {
            hits.into_iter()
                .map(|h| (h.artifact_id, h.tags, h.category))
                .collect::<Vec<_>>()
        };
        let door = crate::store::feedback::Door::Cli;
        let before = seen(core.search(&q, door).await.unwrap());
        assert!(!before.is_empty());

        core.decider = reader("reference", 0.8, 0.99);
        for (id, _, _) in &before {
            run(&core, id).await.unwrap();
            assert_eq!(
                core.store.reading_yes(id, &pinned()).await.unwrap(),
                Some(0.99)
            );
        }
        let after = seen(core.search(&q, door).await.unwrap());
        assert_eq!(after, before, "a pinned reading moved a result or a tag");
    }

    #[tokio::test]
    async fn a_note_too_long_for_one_read_is_read_from_its_opening() {
        let mut core = test_core().await;
        let dec = reader("reference", 0.8, 0.1);
        dec.set_context_tokens(1024);
        core.decider = dec.clone();
        let text: String = (0..2000).map(|i| format!("word{i} ")).collect();
        let id = note(&core, text.trim()).await;
        run(&core, &id).await.unwrap();
        let asked = reads(&dec);
        assert_eq!(asked.len(), 1);
        let state = &asked[0].0;
        assert!(state.starts_with("untitled\nword0 word1"), "{state:.40}");
        assert!(state.len() < text.len());
        assert!(text.starts_with(state.trim_start_matches("untitled\n")));
        assert_eq!(core.store.readings_of(&id).await.unwrap().len(), 4);
        assert_eq!(
            core.store
                .get_artifact(&id)
                .await
                .unwrap()
                .category
                .as_deref(),
            Some("reference")
        );
    }

    #[tokio::test]
    async fn a_retired_note_is_not_read() {
        let mut core = test_core().await;
        let dec = reader("reference", 0.8, 0.1);
        core.decider = dec.clone();
        let id = seed_artifact(&core, "Old").await;
        core.deprecate(&id).await.unwrap();
        run(&core, &id).await.unwrap();
        assert_eq!(dec.calls(), 0);
        assert!(core.store.readings_of(&id).await.unwrap().is_empty());
        assert_eq!(core.store.get_artifact(&id).await.unwrap().category, None);
    }

    #[tokio::test]
    async fn a_two_word_capture_is_read_and_nothing_is_flagged() {
        let mut core = test_core().await;
        let dec = reader("other", 0.4, 0.1);
        core.decider = dec.clone();
        let live = capture(&core, "Buy milk").await;
        assert!(!live.is_empty());
        for c in &live {
            assert!(c.category.is_some(), "{}", c.id);
            assert!(c.flags.is_empty(), "{:?}", c.flags);
            assert_eq!(core.store.readings_of(&c.id).await.unwrap().len(), 4);
        }
    }

    #[tokio::test]
    async fn the_read_unit_takes_the_light_turn_and_is_not_held_with_the_generators() {
        use std::time::Duration;
        assert!(!Stage::GENERATORS.contains(&Stage::Read));
        assert!(!Stage::Read.calls_a_generator());
        assert_eq!(Stage::Read.class(), 1);
        assert!(!Stage::Read.reads_a_capture());
        assert_eq!(Stage::ALL[24], Stage::Read);
        assert_eq!(Stage::parse("read"), Some(Stage::Read));

        let mut core = test_core().await;
        let dec = reader("reference", 0.8, 0.1);
        core.decider = dec.clone();
        core.gate = Arc::new(crate::infer::gate::InferenceGate::new(Duration::from_secs(
            600,
        )));
        core.gate.call_finished();
        let (a, b) = (
            seed_artifact(&core, "A").await,
            seed_artifact(&core, "B").await,
        );
        tokio::time::timeout(Duration::from_secs(10), run(&core, &a))
            .await
            .expect("a reading waited out the cooldown")
            .unwrap();
        assert_eq!(dec.calls(), 1);

        let lease = core.gate.interactive();
        let c = core.clone();
        let waiting = tokio::spawn(async move { run(&c, &b).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiting.is_finished(), "a reading ran past the lease");
        drop(lease);
        tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .expect("the lease's end did not let it through")
            .unwrap()
            .unwrap();
        assert_eq!(dec.calls(), 2);

        core.store
            .enqueue(Stage::Read, "artifact", &a)
            .await
            .unwrap();
        let (_, job) = core
            .store
            .control
            .claim_job_holding(&Stage::GENERATORS)
            .await
            .unwrap()
            .expect("a read unit is held with the generators");
        assert_eq!(job.stage, Stage::Read);
    }

    async fn queued(core: &Core, id: &str) -> bool {
        core.store.live_job(Stage::Read, id).await.unwrap()
    }

    #[tokio::test]
    async fn a_re_embed_of_unchanged_text_arms_no_read_and_changed_text_does() {
        let mut core = test_core_without_synthesizer().await;
        let dec = reader("reference", 0.8, 0.1);
        core.decider = dec.clone();
        let live = capture(&core, "The proxy listens on port 8080.").await;
        assert!(!live.is_empty());
        let read = reads(&dec).len();
        assert!(read > 0);

        for c in &live {
            core.store
                .reset_embed_state(c.corpus_id.as_deref().unwrap())
                .await
                .unwrap();
        }
        for c in &live {
            crate::jobs::embed::run(&core, &c.id).await.unwrap();
            assert!(
                !queued(&core, &c.id).await,
                "a re-embed armed a reading of text already read"
            );
        }
        drain(&core).await;
        assert_eq!(reads(&dec).len(), read);

        let id = &live[0].id;
        core.store
            .update_artifact_text(id, "The proxy listens on port 9090.")
            .await
            .unwrap();
        crate::jobs::embed::run(&core, id).await.unwrap();
        assert!(queued(&core, id).await, "changed text is read again");
        drain(&core).await;
        assert_eq!(reads(&dec).len(), read + 1);
        crate::jobs::embed::run(&core, id).await.unwrap();
        assert!(!queued(&core, id).await);
    }

    #[tokio::test]
    async fn readings_from_before_visits_were_marked_stand_until_the_text_changes() {
        let core = test_core().await;
        let id = note(&core, "a note read long ago").await;
        let asked = reading_questions();
        let yes = Answer::Noul { yes: 0.5 };
        let rows: Vec<(&Asked, &Answer)> = asked.iter().map(|a| (a, &yes)).collect();
        core.store.put_readings(&id, &rows, "m").await.unwrap();
        sqlx::query!("UPDATE artifact_readings SET at = 1")
            .execute(&core.store.pool)
            .await
            .unwrap();
        arm(&core, &id).await.unwrap();
        assert!(!queued(&core, &id).await);
        core.store
            .update_artifact_text(&id, "the note, rewritten")
            .await
            .unwrap();
        arm(&core, &id).await.unwrap();
        assert!(queued(&core, &id).await);
    }

    #[tokio::test]
    async fn a_note_there_is_no_room_for_is_marked_and_passed_by_until_something_changes() {
        let mut core = test_core().await;
        let dec = reader("reference", 0.8, 0.1);
        dec.set_context_tokens(8);
        core.decider = dec.clone();
        let (a, b) = (note(&core, "one note").await, note(&core, "another").await);
        for id in [&a, &b] {
            run(&core, id).await.unwrap();
            let mark = core.store.read_mark(id).await.unwrap().unwrap();
            assert_eq!(mark.visit, Visit::NoRoom);
            arm(&core, id).await.unwrap();
            assert!(!queued(&core, id).await, "armed again with nothing changed");
        }
        assert_eq!(dec.calls(), 0);

        core.store
            .update_artifact_text(&a, "one note, rewritten")
            .await
            .unwrap();
        arm(&core, &a).await.unwrap();
        assert!(queued(&core, &a).await, "new text is another chance");

        dec.set_context_tokens(8192);
        arm(&core, &b).await.unwrap();
        assert!(queued(&core, &b).await, "more room is another chance");
    }

    async fn end_the_wait(core: &Core) {
        sqlx::query!("UPDATE artifact_reads SET retry_at = 0")
            .execute(&core.store.pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_note_the_model_refuses_closes_its_unit_and_waits_longer_after_every_visit() {
        let mut core = test_core().await;
        let dec = Arc::new(crate::infer::fake::ScriptedDecider::rejecting(
            "HTTP 400: no",
        ));
        core.decider = dec.clone();
        let id = note(&core, "a note").await;
        queue(&core, &id).await.unwrap();
        drain(&core).await;
        assert_eq!(dec.calls(), 1, "the queue asked a refusal again");
        assert!(!queued(&core, &id).await, "a refused read stayed queued");
        let first = core.store.read_mark(&id).await.unwrap().unwrap();
        assert_eq!((first.visit, first.tries), (Visit::Rejected, 1));
        let wait = first.retry_at - crate::store::now();
        assert!(
            (RETRY_FIRST_SECS - 5..=RETRY_FIRST_SECS).contains(&wait),
            "{wait}"
        );
        assert!(core.store.readings_of(&id).await.unwrap().is_empty());
        arm(&core, &id).await.unwrap();
        assert!(!queued(&core, &id).await, "armed inside its wait");

        end_the_wait(&core).await;
        arm(&core, &id).await.unwrap();
        assert!(queued(&core, &id).await);
        drain(&core).await;
        assert_eq!(dec.calls(), 2);
        let second = core.store.read_mark(&id).await.unwrap().unwrap();
        assert_eq!(second.tries, 2);
        assert!(second.retry_at - crate::store::now() > RETRY_FIRST_SECS);
        assert!(!queued(&core, &id).await);
    }

    #[tokio::test]
    async fn a_read_that_keeps_failing_is_closed_at_the_ceiling_and_tried_once_a_visit_after() {
        let mut core = test_core().await;
        // An empty script fails every call in the way the queue retries.
        let dec = Arc::new(crate::infer::fake::ScriptedDecider::new(Vec::new()));
        core.decider = dec.clone();
        let id = note(&core, "a note").await;
        queue(&core, &id).await.unwrap();
        for attempt in 1..=MAX_ATTEMPTS {
            assert!(
                queued(&core, &id).await,
                "closed before the ceiling, at attempt {attempt}"
            );
            assert_eq!(core.store.read_mark(&id).await.unwrap(), None);
            sqlx::query!("UPDATE jobs SET run_after = 0")
                .execute(&core.store.control.pool)
                .await
                .unwrap();
            assert!(crate::jobs::run_one(&core).await.unwrap());
        }
        assert!(
            !queued(&core, &id).await,
            "a unit at the ceiling stayed queued"
        );
        assert_eq!(dec.calls() as i64, MAX_ATTEMPTS);
        let mark = core.store.read_mark(&id).await.unwrap().unwrap();
        assert_eq!((mark.visit, mark.tries), (Visit::Rejected, 1));
        assert!(mark.retry_at > crate::store::now());

        end_the_wait(&core).await;
        arm(&core, &id).await.unwrap();
        assert!(queued(&core, &id).await);
        assert!(crate::jobs::run_one(&core).await.unwrap());
        assert!(
            !queued(&core, &id).await,
            "a note that failed before got the queue's retries again"
        );
        assert_eq!(dec.calls() as i64, MAX_ATTEMPTS + 1);
        assert_eq!(core.store.read_mark(&id).await.unwrap().unwrap().tries, 2);
    }

    #[test]
    fn the_wait_doubles_from_an_hour_to_a_day() {
        assert_eq!(retry_wait(1), 3_600);
        assert_eq!(retry_wait(2), 7_200);
        assert_eq!(retry_wait(5), 57_600);
        assert_eq!(retry_wait(6), 86_400);
        assert_eq!(retry_wait(1_000), 86_400);
    }

    /// A person's edit, row and payload, lands just before the first payload write passed
    /// through, which was already on its way with the row as it was.
    struct EditedMidWrite {
        inner: Arc<dyn crate::vector::VectorStore>,
        store: crate::store::Store,
        edited: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl crate::vector::VectorStore for EditedMidWrite {
        async fn ensure_collection(&self, dim: usize) -> Result<()> {
            self.inner.ensure_collection(dim).await
        }
        async fn upsert(&self, points: Vec<crate::vector::VectorPoint>) -> Result<()> {
            self.inner.upsert(points).await
        }
        async fn set_payload(&self, payload: &crate::vector::VectorPayload) -> Result<()> {
            if !self.edited.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let id = &payload.artifact_id;
                self.store
                    .update_artifact_category_by(id, Some("procedure"), "person")
                    .await?;
                let fresh = self.store.get_artifact(id).await?;
                self.inner
                    .set_payload(&crate::vector::VectorPayload::edited(&fresh))
                    .await?;
            }
            self.inner.set_payload(payload).await
        }
        async fn set_lifecycle(
            &self,
            artifact_id: &str,
            status: crate::store::artifacts::ArtifactStatus,
            superseded_by: Option<&str>,
        ) -> Result<()> {
            self.inner
                .set_lifecycle(artifact_id, status, superseded_by)
                .await
        }
        async fn set_last_verified_at(
            &self,
            artifact_id: &str,
            at: i64,
            reset_hits: bool,
        ) -> Result<()> {
            self.inner
                .set_last_verified_at(artifact_id, at, reset_hits)
                .await
        }
        async fn stale_candidates(
            &self,
            older_than: i64,
            max_hits: i64,
            limit: usize,
        ) -> Result<Vec<crate::vector::SearchHit>> {
            self.inner
                .stale_candidates(older_than, max_hits, limit)
                .await
        }
        async fn search(
            &self,
            vector: &[f32],
            sparse: &crate::vector::sparse::SparseVector,
            limit: usize,
            filter: &crate::vector::SearchFilter,
        ) -> Result<Vec<crate::vector::SearchHit>> {
            self.inner.search(vector, sparse, limit, filter).await
        }
        async fn touch(&self, targets: &[crate::vector::Touch], seen_at: i64) -> Result<()> {
            self.inner.touch(targets, seen_at).await
        }
        async fn apply_lifecycle(&self, rows: &[crate::vector::LifecycleRow]) -> Result<()> {
            self.inner.apply_lifecycle(rows).await
        }
        async fn lifecycle_of(
            &self,
            artifact_ids: &[String],
        ) -> Result<HashMap<String, crate::vector::StoredLifecycle>> {
            self.inner.lifecycle_of(artifact_ids).await
        }
        async fn all_artifact_ids(&self) -> Result<Vec<String>> {
            self.inner.all_artifact_ids().await
        }
        async fn payloads_of(
            &self,
            artifact_ids: &[String],
        ) -> Result<HashMap<String, crate::vector::VectorPayload>> {
            self.inner.payloads_of(artifact_ids).await
        }
        async fn resurface(
            &self,
            limit: usize,
            older_than: i64,
            unseen_since: i64,
        ) -> Result<Vec<crate::vector::SearchHit>> {
            self.inner.resurface(limit, older_than, unseen_since).await
        }
        async fn facets(&self, limit: usize) -> Result<crate::vector::Facets> {
            self.inner.facets(limit).await
        }
        async fn neighbours(
            &self,
            artifact_id: &str,
            limit: usize,
        ) -> Result<Vec<crate::vector::SearchHit>> {
            self.inner.neighbours(artifact_id, limit).await
        }
        async fn set_context_vectors(
            &self,
            artifact_id: &str,
            vectors: Vec<Vec<f32>>,
        ) -> Result<()> {
            self.inner.set_context_vectors(artifact_id, vectors).await
        }
        async fn context_query(
            &self,
            vector: &[f32],
            limit: usize,
            filter: &crate::vector::SearchFilter,
        ) -> Result<Vec<crate::vector::SearchHit>> {
            self.inner.context_query(vector, limit, filter).await
        }
        async fn delete_artifacts(&self, artifact_ids: &[String]) -> Result<()> {
            self.inner.delete_artifacts(artifact_ids).await
        }
        async fn delete_by_corpus(&self, corpus_id: &str) -> Result<()> {
            self.inner.delete_by_corpus(corpus_id).await
        }
        async fn sample(&self, limit: usize) -> Result<Vec<(String, Vec<f32>)>> {
            self.inner.sample(limit).await
        }
        async fn dense_of(&self, artifact_id: &str) -> Result<Option<Vec<f32>>> {
            self.inner.dense_of(artifact_id).await
        }
        async fn count(&self) -> Result<u64> {
            self.inner.count().await
        }
    }

    #[tokio::test]
    async fn an_edit_landing_while_the_payload_is_being_written_is_not_left_overwritten() {
        let mut core = test_core().await;
        core.decider = reader("concept", 0.9, 0.1);
        let id = seed_artifact(&core, "Keys").await;
        assert!(core.store.mark_embedded(&id, "m", 0).await.unwrap());
        core.vectors = Arc::new(EditedMidWrite {
            inner: core.vectors.clone(),
            store: core.store.clone(),
            edited: std::sync::atomic::AtomicBool::new(false),
        });
        run(&core, &id).await.unwrap();
        let row = core.store.get_artifact(&id).await.unwrap();
        assert_eq!(row.category.as_deref(), Some("procedure"));
        assert_eq!(category_by(&core, &id).await.as_deref(), Some("person"));
        let payloads = core
            .vectors
            .payloads_of(std::slice::from_ref(&id))
            .await
            .unwrap();
        assert_eq!(
            payloads[&id].category.as_deref(),
            Some("procedure"),
            "the payload kept the reading's category over the row's"
        );
    }

    #[tokio::test]
    async fn a_reading_whose_payload_write_failed_writes_it_on_the_retry() {
        let mut core = test_core().await;
        core.decider = reader("concept", 0.9, 0.1);
        let id = seed_artifact(&core, "Keys").await;
        assert!(core.store.mark_embedded(&id, "m", 0).await.unwrap());
        // As after a run whose category landed and whose payload write did not.
        assert!(core.store.fill_category(&id, "concept").await.unwrap());
        run(&core, &id).await.unwrap();
        let payloads = core
            .vectors
            .payloads_of(std::slice::from_ref(&id))
            .await
            .unwrap();
        assert_eq!(payloads[&id].category.as_deref(), Some("concept"));
    }

    #[test]
    fn the_reading_questions_are_the_four() {
        let ids: Vec<&str> = reading_questions().iter().map(|a| a.id).collect();
        assert_eq!(ids, KEYS);
    }
}
