//! Capture without synthesis: a window becomes verbatim passages sized to the embedder, under the
//! document's headings.

use crate::core::Core;
use crate::error::Result;
use crate::infer::budget::TokenCounter;
use crate::infer::split::{Window, split_into_segments};
use crate::store::artifacts::{CorpusSpan, NewArtifact, Provenance};
use crate::store::corpora::CorpusStatus;
use crate::store::segments::SegmentState;

/// A document's name, derived locally: longer than this is a paragraph.
pub const TITLE_MAX: usize = 80;

/// A near-empty embedding sits close to every query, so an orphan ranks first where the base has
/// least to offer.
pub const MIN_PASSAGE_CHARS: usize = 40;

#[derive(Debug, Clone, PartialEq)]
pub struct Passage {
    /// As the document wrote it; never inferred.
    pub title: Option<String>,
    pub text: String,
    pub start_line: i64,
    pub end_line: i64,
}

fn is_heading(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') && t.trim_start_matches('#').starts_with(' ')
}

fn is_orphan(text: &str) -> bool {
    text.chars().filter(|c| !c.is_whitespace()).count() < MIN_PASSAGE_CHARS
}

fn opens_with_heading(text: &str) -> bool {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .is_some_and(is_heading)
}

/// The preceding passage, else the following one; never across a heading, which is a boundary the
/// document drew.
fn orphan_home(texts: &[&str], i: usize) -> Option<usize> {
    let back = i > 0 && !texts[i].lines().any(is_heading);
    let forward = i + 1 < texts.len() && !opens_with_heading(texts[i + 1]);
    match (back, forward) {
        (true, _) => Some(i - 1),
        (false, true) => Some(i + 1),
        (false, false) => None,
    }
}

/// One at a time: each merge changes the neighbours of the next.
pub(crate) fn next_orphan_merge(texts: &[&str]) -> Option<(usize, usize)> {
    (0..texts.len())
        .filter(|&i| is_orphan(texts[i]))
        .find_map(|i| orphan_home(texts, i).map(|h| (i, h)))
}

/// The slices are contiguous, so joining neighbours drops no text and keeps the line ranges
/// partitioning the window.
fn merge_orphans(mut inner: Vec<Window>) -> Vec<Window> {
    while let Some((i, h)) = {
        let texts: Vec<&str> = inner.iter().map(|w| w.text.as_str()).collect();
        next_orphan_merge(&texts)
    } {
        let (lo, hi) = (i.min(h), i.max(h));
        let tail = inner.remove(hi);
        let head = &mut inner[lo];
        head.text.push_str(&tail.text);
        head.end_line = head.end_line.max(tail.end_line);
    }
    inner
}

/// "## Recovering deleted entries" → "Recovering deleted entries"; links reduced to their words,
/// whitespace runs to one space.
pub fn heading_title(line: &str) -> String {
    let bare = line.trim().trim_start_matches('#');
    // To a fixed point: an image inside a link (`## [![Logo](logo.png)](/)`) needs two passes.
    // Bounded, as every changing pass shortens the line.
    let mut title = bare.to_string();
    for _ in 0..4 {
        let next = strip_links(&title);
        if next == title {
            break;
        }
        title = next;
    }
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_links(bare: &str) -> String {
    let mut out = String::with_capacity(bare.len());
    let mut rest = bare;
    // Anything not exactly `[text](target)` is left as written: a bracket in prose is not a link.
    while let Some(first) = rest.find('[') {
        let Some(close_rel) = rest[first..].find("](") else {
            break;
        };
        let close = first + close_rel;
        // The LAST `[` before `](`, or a prose bracket ahead of a real link swallows the words
        // between.
        let open = rest[..close].rfind('[').unwrap_or(first);
        // A markdown image's `!` is syntax, not a word.
        let text_end = match rest[..open].ends_with('!') {
            true => open - 1,
            false => open,
        };
        // Parens nest: Wikipedia URLs end in `_(computing))`.
        let mut depth = 0usize;
        let target = rest[close + 2..].char_indices().find_map(|(i, c)| match c {
            '(' => {
                depth += 1;
                None
            }
            ')' if depth == 0 => Some(close + 2 + i),
            ')' => {
                depth -= 1;
                None
            }
            _ => None,
        });
        let Some(target) = target else {
            break;
        };
        out.push_str(&rest[..text_end]);
        out.push_str(&rest[open + 1..close]);
        rest = &rest[target + 1..];
    }
    out.push_str(rest);
    out
}

/// Spans partition the window and never cross it: promotion supersedes whole passages. A passage is
/// titled by its first heading, else the last one above it; plain text stays untitled. Orphans are
/// folded in before titling (`MIN_PASSAGE_CHARS`).
pub fn split_passages(
    window: &Window,
    counter: &TokenCounter,
    chunk_tokens: usize,
) -> Vec<Passage> {
    let inner = merge_orphans(split_into_segments(
        &window.text,
        counter,
        chunk_tokens.max(1),
    ));
    let mut out = Vec::with_capacity(inner.len());
    let mut last_heading: Option<String> = None;
    for p in inner {
        let own: Vec<&str> = p.text.lines().collect();
        let inside: Option<String> = own.iter().find(|l| is_heading(l)).map(|l| heading_title(l));
        let title = inside.or_else(|| last_heading.clone());
        if let Some(h) = own.iter().rev().find(|l| is_heading(l)) {
            last_heading = Some(heading_title(h));
        }
        // Clamped: a hard-cut long line can put more lines in the window's text than the document
        // has.
        let start =
            (window.start_line + p.start_line - 1).clamp(window.start_line, window.end_line);
        let end = (window.start_line + p.end_line - 1).clamp(start, window.end_line);
        out.push(Passage {
            title,
            text: p.text,
            start_line: start,
            end_line: end,
        });
    }
    out
}

/// No inference call: the one call a small capture gets is armed here. Idempotent per segment: a
/// segment that already owns rows is left alone.
pub async fn capture_verbatim(core: &Core, corpus_id: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    let lang = crate::infer::lang::of_corpus(&src.metadata);
    // Measured against the prompt this corpus will be synthesized with; with no room left there is
    // no size to cut to.
    let Some(budget) = super::synthesize::segment_budget(core, lang) else {
        let reason = super::synthesize::no_budget_reason(core, lang);
        tracing::error!(corpus_id, %reason, "cannot plan windows");
        core.store
            .set_corpus_status(corpus_id, CorpusStatus::Failed)
            .await?;
        return Ok(());
    };
    let windows = split_into_segments(&src.raw_text, &core.counter, budget);
    if windows.is_empty() {
        tracing::warn!(corpus_id, "source has no usable text");
        core.store
            .set_corpus_status(corpus_id, CorpusStatus::Failed)
            .await?;
        return Ok(());
    }

    let rows: Vec<crate::store::segments::NewSegment<'_>> = windows
        .iter()
        .map(|w| crate::store::segments::NewSegment {
            start_line: w.start_line,
            end_line: w.end_line,
            text: w.text.as_str(),
        })
        .collect();
    core.store.upsert_segments(corpus_id, &rows).await?;
    core.store.mark_segments_verbatim(corpus_id).await?;

    // Under the document lock through `finish`: a promotion's write must not interleave with the
    // renumbering.
    let _corpus = core.corpus_lock(corpus_id).await;
    for (idx, w) in windows.iter().enumerate() {
        let idx = idx as i64;
        if !core
            .store
            .artifact_ids_for_segment(corpus_id, idx)
            .await?
            .is_empty()
        {
            continue;
        }
        let new: Vec<NewArtifact> = split_passages(w, &core.counter, core.chunk_tokens)
            .into_iter()
            .enumerate()
            .map(|(i, p)| NewArtifact {
                ordinal: i as i64,
                text: p.text,
                // Found, not claimed: a passage is these lines verbatim.
                corpus_span: Some(CorpusSpan::located(p.start_line, p.end_line)),
                title: p.title,
                category: None,
                tags: vec![],
                segment_idx: Some(idx),
                caveats: vec![],
            })
            .collect();
        core.store
            .insert_artifacts_with_provenance(corpus_id, &new, Provenance::Passage)
            .await?;
    }

    if src.title_hint.is_none()
        && let Some(t) = derive_title(&src.raw_text)
    {
        core.store.set_title_hint(corpus_id, &t).await?;
    }

    tracing::info!(corpus_id, windows = windows.len(), "captured verbatim");
    super::synthesize::finish(core, corpus_id).await?;

    // One stored window means the capture fits one call, so arm it now on the promotion path
    // (`keep_artifacts`). Counted off stored rows: `upsert_segments` keeps an old split, and arming
    // on a fresh count would promote half a document.
    let stored_windows = core.store.segment_progress(corpus_id).await?.1;
    if core.synthesizer.is_some()
        && stored_windows == 1
        && core.store.segment_state(corpus_id, 0).await? == Some(SegmentState::Verbatim)
        && !core.store.segment_no_promote(corpus_id, 0).await?
    {
        core.store.reset_segment(corpus_id, 0, true).await?;
        core.store
            .rearm_idle_seq(
                crate::store::jobs::Stage::SegmentWindow,
                "segment",
                &crate::jobs::window::unit_target(corpus_id, 0),
                0,
            )
            .await?;
        tracing::info!(corpus_id, "small capture: synthesis armed at capture");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrphanMerge {
    pub orphan_id: String,
    pub orphan_text: String,
    pub into_id: String,
}

/// Repairs a base captured before `split_passages` folded orphans. Only live passages within one
/// window and one unbroken run; a superseded or reaped row is a boundary. Idempotent.
pub async fn merge_orphan_passages(
    core: &Core,
    corpus_id: &str,
    dry_run: bool,
) -> Result<Vec<OrphanMerge>> {
    use crate::store::artifacts::Chunk;
    let _corpus = core.corpus_lock(corpus_id).await;
    let mut rows: Vec<Chunk> = core
        .store
        .artifacts_for_corpus(corpus_id)
        .await?
        .into_iter()
        .filter(|c| c.provenance == Provenance::Passage && c.segment_idx.is_some())
        .collect();
    rows.sort_by_key(|c| (c.segment_idx, c.ordinal));
    let live = |c: &Chunk| c.in_results() && c.reaped_at.is_none() && c.corpus_span.is_some();

    let mut done = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let mut j = i;
        while j < rows.len() && live(&rows[j]) && rows[j].segment_idx == rows[i].segment_idx {
            j += 1;
        }
        let mut run: Vec<Chunk> = rows[i..j].to_vec();
        i = j.max(i + 1);
        while let Some((o, h)) = {
            let texts: Vec<&str> = run.iter().map(|c| c.text.as_str()).collect();
            next_orphan_merge(&texts)
        } {
            let orphan = run.remove(o);
            let home = &mut run[if h > o { h - 1 } else { h }];
            home.text = if h > o {
                format!("{}{}", orphan.text, home.text)
            } else {
                format!("{}{}", home.text, orphan.text)
            };
            let (Some(a), Some(b)) = (orphan.corpus_span.as_ref(), home.corpus_span.as_ref())
            else {
                return Err(crate::error::Error::Internal(
                    "a passage without a span".into(),
                ));
            };
            let span =
                CorpusSpan::located(a.start_line.min(b.start_line), a.end_line.max(b.end_line));
            if !dry_run {
                core.store
                    .absorb_passage(&home.id, &home.text, &span)
                    .await?;
                core.store
                    .enqueue(crate::store::jobs::Stage::Embed, "artifact", &home.id)
                    .await?;
                core.delete_artifact(&orphan.id).await?;
            }
            home.corpus_span = Some(span);
            done.push(OrphanMerge {
                orphan_id: orphan.id,
                orphan_text: orphan.text,
                into_id: home.id.clone(),
            });
        }
    }
    if !dry_run && !done.is_empty() {
        core.store.renumber_artifacts(corpus_id).await?;
    }
    Ok(done)
}

/// The first heading, else the first non-empty line, cut at a word to `TITLE_MAX`; it is displayed
/// verbatim. `None` for whitespace.
pub fn derive_title(raw_text: &str) -> Option<String> {
    let line = raw_text
        .lines()
        .find(|l| is_heading(l))
        .map(heading_title)
        .or_else(|| {
            raw_text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_string)
        })?;
    if line.is_empty() {
        return None;
    }
    Some(crate::web::markdown::truncate_at_word(&line, TITLE_MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::budget::TokenCounter;
    use crate::infer::split::Window;

    fn window(text: &str, start_line: i64) -> Window {
        let own = text.lines().count() as i64;
        Window {
            text: text.to_string(),
            start_line,
            end_line: start_line + own - 1,
        }
    }

    #[test]
    fn a_window_that_fits_is_one_passage_whose_span_is_the_window() {
        let w = window("para one\n\npara two", 10);
        let p = split_passages(&w, &TokenCounter::default(), 1000);
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].start_line, p[0].end_line), (10, 12));
        assert_eq!(p[0].text, "para one\n\npara two");
        assert_eq!(p[0].title, None);
    }

    #[test]
    fn passages_partition_the_window_and_never_cross_it() {
        // ~30 estimator tokens per paragraph, budget 40: one paragraph each.
        let paras: Vec<String> = (0..4)
            .map(|i| format!("paragraph {i} ").repeat(8))
            .collect();
        let text = paras.join("\n\n");
        let w = window(&text, 1);
        let p = split_passages(&w, &TokenCounter::default(), 40);
        assert!(p.len() > 1, "{}", p.len());
        assert_eq!(p[0].start_line, 1);
        assert_eq!(p.last().unwrap().end_line, w.end_line);
        for pair in p.windows(2) {
            assert_eq!(pair[0].end_line + 1, pair[1].start_line, "spans must abut");
        }
        assert!(
            p.iter()
                .all(|x| x.start_line >= w.start_line && x.end_line <= w.end_line)
        );
    }

    #[test]
    fn a_window_opening_with_a_heading_titles_its_passage_by_it() {
        let w = window("## Recovery\nstep three\nstep four", 40);
        let p = split_passages(&w, &TokenCounter::default(), 1000);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].title.as_deref(), Some("Recovery"));
        assert_eq!((p[0].start_line, p[0].end_line), (40, 42));
    }

    #[test]
    fn a_heading_inside_the_window_titles_the_passage_that_holds_it_and_is_carried_on() {
        let body = format!(
            "intro line\n## Mounting\n{}\n\n{}",
            "mount words ".repeat(12),
            "more mount words ".repeat(12)
        );
        let w = window(&body, 1);
        let p = split_passages(&w, &TokenCounter::default(), 40);
        assert!(p.len() >= 2, "{}", p.len());
        let k = p
            .iter()
            .position(|x| x.text.contains("## Mounting"))
            .expect("a passage holds the heading");
        assert_eq!(p[k].title.as_deref(), Some("Mounting"));
        assert_eq!(p[0].title, None, "{p:?}");
        assert_eq!(p[k + 1].title.as_deref(), Some("Mounting"));
        assert!(
            !p[k + 1].text.contains("## Mounting"),
            "{:?}",
            p[k + 1].text
        );
        let total = p.iter().filter(|x| x.text.contains("## Mounting")).count();
        assert_eq!(total, 1);
    }

    #[test]
    fn a_windows_heading_titles_every_passage_under_it() {
        let body = format!(
            "## Outer\n{}\n\n{}",
            "first part words ".repeat(12),
            "second part words ".repeat(12)
        );
        let w = window(&body, 20);
        let p = split_passages(&w, &TokenCounter::default(), 40);
        assert!(p.len() >= 2);
        assert!(
            p.iter().all(|x| x.title.as_deref() == Some("Outer")),
            "{p:?}"
        );
    }

    #[test]
    fn a_budget_cut_tail_is_folded_into_the_passage_it_was_cut_from() {
        let text = format!(
            "opening paragraph\n\n{}is needed.",
            "some words here and ".repeat(23)
        );
        let w = window(&text, 7);
        let counter = TokenCounter::default();
        let raw = split_into_segments(&w.text, &counter, 32);
        assert_eq!(raw.last().unwrap().text, "is needed.", "{raw:?}");

        let p = split_passages(&w, &counter, 32);
        assert!(
            p.iter().all(|x| !is_orphan(&x.text)),
            "{:?}",
            p.iter().map(|x| &x.text).collect::<Vec<_>>()
        );
        assert!(p.last().unwrap().text.ends_with("and is needed."), "{p:?}");
        let joined: String = p.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(joined, text);
        assert_eq!(p[0].start_line, w.start_line);
        assert_eq!(p.last().unwrap().end_line, w.end_line);
        for pair in p.windows(2) {
            assert!(pair[1].start_line <= pair[0].end_line + 1, "gap: {p:?}");
            assert!(pair[1].start_line >= pair[0].start_line);
        }
    }

    #[test]
    fn a_window_shorter_than_the_minimum_is_still_one_passage() {
        let w = window("short note", 3);
        let p = split_passages(&w, &TokenCounter::default(), 1000);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].text, "short note");
        assert_eq!((p[0].start_line, p[0].end_line), (3, 3));
    }

    #[test]
    fn an_orphan_goes_back_then_forward_and_never_across_a_heading() {
        let long = "a passage long enough to stand on its own, easily.";
        assert_eq!(next_orphan_merge(&[long, "is needed."]), Some((1, 0)));
        assert_eq!(next_orphan_merge(&["is needed.", long]), Some((0, 1)));
        assert_eq!(
            next_orphan_merge(&[long, "## Recover\n", long]),
            Some((1, 2))
        );
        assert_eq!(
            next_orphan_merge(&["intro line\n", "## Mounting\nbody"]),
            None
        );
        assert_eq!(next_orphan_merge(&[long, "ok.\n## Next"]), None);
        assert_eq!(next_orphan_merge(&["short note"]), None);
    }

    #[test]
    fn derive_title_prefers_a_heading_then_the_first_line_and_truncates() {
        assert_eq!(
            derive_title("\n\n# Big title\nbody").as_deref(),
            Some("Big title")
        );
        assert_eq!(
            derive_title("plain first line\nsecond").as_deref(),
            Some("plain first line")
        );
        let long = "x".repeat(200);
        let cut = derive_title(&long).unwrap();
        assert_eq!(cut.chars().count(), TITLE_MAX + 1);
        assert!(cut.ends_with('…'), "{cut}");
        let prose = "Remind me to renew the domain before 14 November 2026. \
                     The registrar does not auto-renew because the card expired.";
        let cut = derive_title(prose).unwrap();
        assert_eq!(
            cut,
            "Remind me to renew the domain before 14 November 2026. The registrar does not…"
        );
        assert_eq!(derive_title("   \n\t\n"), None);
        assert_eq!(heading_title("###   Deep heading  "), "Deep heading");
        assert_eq!(
            heading_title("## [](#NAME)NAME         [top](#top_of_page)"),
            "NAME top"
        );
        assert_eq!(
            heading_title("## See [the docs](https://x.test/a)"),
            "See the docs"
        );
        assert_eq!(heading_title("## Arrays [0] and [1]"), "Arrays [0] and [1]");
        assert_eq!(
            heading_title("## Arrays [0] and [the docs](https://x.test/a)"),
            "Arrays [0] and the docs"
        );
        assert_eq!(
            heading_title("## ![diagram](a.png) Overview"),
            "diagram Overview"
        );
        assert_eq!(
            heading_title(
                "## See [Loop device](https://en.wikipedia.org/wiki/Loop_device_(computing))"
            ),
            "See Loop device"
        );
        assert_eq!(heading_title("## [![Logo](logo.png)](/)"), "Logo");
        assert_eq!(
            heading_title("## [![Logo](logo.png)](/) Reference"),
            "Logo Reference"
        );
        assert_eq!(
            heading_title("## See [![icon](i.png)](https://x.test/a) for more"),
            "See icon for more"
        );
    }

    #[tokio::test]
    async fn a_small_capture_with_no_synthesizer_stays_verbatim() {
        let core = crate::core::test_support::test_core_without_synthesizer().await;
        let out = core
            .ingest("The boiler pressure should sit at 1.5 bar.", "web", None)
            .await
            .unwrap();
        assert!(crate::jobs::run_one(&core).await.unwrap());

        let segs = core.store.segments_for_corpus(&out.id).await.unwrap();
        assert_eq!(segs.len(), 1, "small enough to be one window");
        assert_eq!(
            segs[0].state,
            crate::store::segments::SegmentState::Verbatim
        );
        assert!(
            !core
                .store
                .live_job(
                    crate::store::jobs::Stage::SegmentWindow,
                    &crate::jobs::window::unit_target(&out.id, 0)
                )
                .await
                .unwrap(),
            "nothing armed: there is no call to make"
        );
        let rows = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(!rows.is_empty());
        assert!(
            rows.iter()
                .all(|c| c.provenance == crate::store::artifacts::Provenance::Passage)
        );
    }

    #[tokio::test]
    async fn capture_writes_passages_marks_segments_verbatim_and_finishes() {
        let core = crate::core::test_support::test_core().await;
        let text = format!(
            "# Manual\n\n## Install\n{}\n\n## Recover\n{}",
            "install words ".repeat(600),
            "recover words ".repeat(600)
        );
        let out = core.ingest(&text, "web", None).await.unwrap();
        // Multi-window on purpose: a one-call capture would arm its synthesis at capture.
        assert!(crate::jobs::run_one(&core).await.unwrap());

        let s = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(
            s.status,
            crate::store::corpora::CorpusStatus::Embedding,
            "{:?}",
            s.status
        );
        assert_eq!(s.title_hint.as_deref(), Some("Manual"));
        let segs = core.store.segments_for_corpus(&out.id).await.unwrap();
        assert!(!segs.is_empty());
        assert!(
            segs.iter()
                .all(|w| w.state == crate::store::segments::SegmentState::Verbatim)
        );
        let rows = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(!rows.is_empty());
        assert!(
            rows.iter()
                .all(|c| c.provenance == crate::store::artifacts::Provenance::Passage)
        );
        assert!(rows.iter().all(|c| c.corpus_span.is_some()));
        assert_eq!(s.coverage.map(|c| (c * 100.0).round() as i64), Some(100));
        assert!(
            core.store
                .live_job(crate::store::jobs::Stage::Embed, &out.id)
                .await
                .unwrap()
        );
        for w in &segs {
            assert!(
                !core
                    .store
                    .live_job(
                        crate::store::jobs::Stage::SegmentWindow,
                        &crate::jobs::window::unit_target(&out.id, w.idx)
                    )
                    .await
                    .unwrap()
            );
        }
        assert!(
            !core
                .store
                .has_job(crate::store::jobs::Stage::Title, &out.id)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn capture_at_off_is_idempotent_per_segment() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("one line\n\nanother line", "web", None)
            .await
            .unwrap();
        capture_verbatim(&core, &out.id).await.unwrap();
        let n = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .len();
        capture_verbatim(&core, &out.id).await.unwrap();
        assert_eq!(
            core.store
                .artifacts_for_corpus(&out.id)
                .await
                .unwrap()
                .len(),
            n
        );
    }

    #[tokio::test]
    async fn a_passage_never_gets_a_relate_unit() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("some verbatim text", "web", None)
            .await
            .unwrap();
        capture_verbatim(&core, &out.id).await.unwrap();
        let id = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0]
            .id
            .clone();
        crate::jobs::embed::run(&core, &id).await.unwrap();
        assert!(
            !core
                .store
                .live_job(crate::store::jobs::Stage::Relate, &id)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn stored_orphans_are_merged_into_their_neighbour_and_removed() {
        use crate::store::artifacts::EmbedState;
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("first line\nsecond line\nthird line", "web", None)
            .await
            .unwrap();
        core.store
            .upsert_segments(
                &out.id,
                &[crate::store::segments::NewSegment {
                    start_line: 1,
                    end_line: 3,
                    text: "first line\nsecond line\nthird line",
                }],
            )
            .await
            .unwrap();
        let long_a = "a passage long enough to stand on its own, easily.\n";
        let long_b = "another passage that is long enough to be one too.\n";
        let rows = [
            (long_a, 1, 1),
            (".\n\nwerden\n", 2, 2),
            (long_b, 3, 3),
            ("is needed.", 3, 3),
        ];
        let new: Vec<NewArtifact> = rows
            .iter()
            .enumerate()
            .map(|(i, (t, a, b))| NewArtifact {
                ordinal: i as i64,
                text: t.to_string(),
                corpus_span: Some(CorpusSpan::located(*a, *b)),
                title: None,
                category: None,
                tags: vec![],
                segment_idx: Some(0),
                caveats: vec![],
            })
            .collect();
        core.store
            .insert_artifacts_with_provenance(&out.id, &new, Provenance::Passage)
            .await
            .unwrap();
        let before = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        let id = |t: &str| before.iter().find(|c| c.text == t).unwrap().id.clone();
        for c in &before {
            crate::jobs::embed::run(&core, &c.id).await.unwrap();
        }

        let planned = merge_orphan_passages(&core, &out.id, true).await.unwrap();
        assert_eq!(
            planned
                .iter()
                .map(|m| (m.orphan_text.as_str(), m.into_id.clone()))
                .collect::<Vec<_>>(),
            vec![(".\n\nwerden\n", id(long_a)), ("is needed.", id(long_b))]
        );
        assert_eq!(
            core.store
                .artifacts_for_corpus(&out.id)
                .await
                .unwrap()
                .len(),
            4,
            "a dry run writes nothing"
        );

        let done = merge_orphan_passages(&core, &out.id, false).await.unwrap();
        assert_eq!(done, planned);
        let after = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].text, format!("{long_a}.\n\nwerden\n"));
        assert_eq!(after[0].corpus_span.as_ref().unwrap().end_line, 2);
        assert_eq!(after[1].text, format!("{long_b}is needed."));
        assert_eq!((after[0].ordinal, after[1].ordinal), (0, 1));
        for c in &after {
            assert_eq!(c.embed_state, EmbedState::Pending);
            assert!(
                core.store
                    .live_job(crate::store::jobs::Stage::Embed, &c.id)
                    .await
                    .unwrap()
            );
        }
        for m in &done {
            assert!(core.store.get_artifact(&m.orphan_id).await.is_err());
        }
        let points = core.vectors.all_artifact_ids().await.unwrap();
        assert!(
            done.iter().all(|m| !points.contains(&m.orphan_id)),
            "{points:?}"
        );

        assert!(
            merge_orphan_passages(&core, &out.id, false)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_note_sorts_ahead_of_the_document_it_annotates() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest_capture(
                crate::core::ingest::Capture::new(
                    "# Heading\n\nThe body of the uploaded document.",
                    "upload",
                )
                .with_note(Some("printout from the hallway scanner".into())),
            )
            .await
            .unwrap();

        capture_verbatim(&core, &out.id).await.unwrap();

        let all = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(all.len() >= 2, "the note and at least one passage");
        assert_eq!(all[0].text, "printout from the hallway scanner");
        assert_eq!(all[0].ordinal, 0);
        assert_eq!(all[0].corpus_span, None);
        assert_eq!(all[1].ordinal, 1, "ordinals stay continuous");
        assert!(
            all[1].corpus_span.is_some(),
            "a passage still anchors to its lines"
        );
    }
}
