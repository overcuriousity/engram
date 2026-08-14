//! One pair, one call.
//!
//! The sweep used to make up to `max_judgements` of these in a single job, so a
//! consolidation run blocked every capture behind it for as long as twenty model
//! calls took — the second-worst blocker in the system after synthesis. The
//! sweep now decides *which* pairs are worth asking about, which costs nothing,
//! and arms one unit per pair. The queue paces them and interleaves them with
//! everything else.

use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::artifacts::ArtifactStatus;
use crate::store::pairs::{ArtifactPair, PairState};

pub async fn run(core: &Core, pair_id: &str) -> Result<()> {
    let id: i64 = pair_id.parse().map_err(|_| Error::NotFound)?;
    let p = core.store.get_pair(id).await?;
    if p.state != PairState::Pending {
        // Settled by an operator, or by a later sweep, while this waited.
        return Ok(());
    }

    // Reported, not swallowed. This read used to treat any failure as "one side
    // was deleted while the unit waited" — it recorded an attempt and returned
    // `Ok`, so a store that was briefly unwell closed the unit and counted the
    // pair as having had its turn. There is no deletion to handle: `a_id` and
    // `b_id` are `ON DELETE CASCADE` and every pool sets `foreign_keys`, so a
    // pair naming an artifact that is gone is a state the schema does not allow.
    let (a, b) = (
        core.store.get_artifact(&p.a_id).await?,
        core.store.get_artifact(&p.b_id).await?,
    );

    // Re-checked here and not only when the unit was armed. A pair can wait out
    // a backoff, and in that time a member can be superseded by a later sweep or
    // deprecated by an operator — spending the scarcest thing here to post a
    // contradiction about an artifact no longer in results.
    if a.status != ArtifactStatus::Active
        || b.status != ArtifactStatus::Active
        || a.superseded_by.is_some()
        || b.superseded_by.is_some()
    {
        core.store
            .set_pair_state(id, PairState::Dismissed, None)
            .await?;
        return Ok(());
    }

    // Counted before the call and regardless of how it goes, so a pair the model
    // keeps failing on drops behind the rest of the queue rather than absorbing
    // the budget again on the next sweep.
    core.store.record_judge_attempt(id).await?;

    let permit = core.gate.background().await;
    let reply = match core
        .completer
        .complete(
            crate::infer::prompt::DEDUPE_SYSTEM,
            &crate::infer::prompt::dedupe_prompt(
                &[
                    (a.title.as_deref().unwrap_or("untitled"), a.text.as_str()),
                    (b.title.as_deref().unwrap_or("untitled"), b.text.as_str()),
                ],
                &crate::infer::facts::differing_values(&[&a.text, &b.text]),
                p.judge_attempts,
            ),
        )
        .await
    {
        Ok(r) => {
            permit.succeeded();
            r
        }
        Err(e) => {
            permit.failed(&e);
            return Err(e);
        }
    };

    apply(core, &p, &a, &b, &reply).await
}

/// Record what the judge said. Split out because it is the half that has
/// nothing to do with the endpoint, and the half worth reading on its own.
async fn apply(
    core: &Core,
    p: &ArtifactPair,
    a: &crate::store::artifacts::Chunk,
    b: &crate::store::artifacts::Chunk,
    reply: &str,
) -> Result<()> {
    use crate::infer::prompt::Relation;
    let verdict = match crate::infer::prompt::parse_dedupe(reply) {
        Ok(v) => v,
        Err(e) => {
            core.store.record_unreadable_judgement(p.id).await?;
            tracing::warn!(
                pair = p.id,
                attempt = p.judge_attempts,
                error = %e,
                "dedupe reply unreadable; pair stays pending"
            );
            return Err(e);
        }
    };

    match verdict.relation {
        Relation::Distinct => {
            core.store
                .set_pair_state(p.id, PairState::NoConflict, verdict.detail.as_deref())
                .await?;
        }
        Relation::Conflict => {
            core.store
                .set_pair_state(p.id, PairState::Contradiction, verdict.detail.as_deref())
                .await?;
            tracing::info!(pair = p.id, a = %a.id, b = %b.id, "artifacts disagree");
        }
        Relation::Replaced => {
            // Trust the named direction only when it agrees with the sweep's own
            // newest-wins bias (see `keeper`): a call naming the *newer* artifact
            // obsolete is exactly the failure mode worth guarding against, since
            // it would otherwise hide the side more likely to be current.
            let obsolete_id = verdict.supersedes.and_then(|side| {
                let (named, other) = match side {
                    'a' => (a, b),
                    _ => (b, a),
                };
                (named.created_at <= other.created_at).then(|| named.id.clone())
            });
            match obsolete_id {
                Some(obsolete_id) => {
                    core.store
                        .set_pair_superseded(p.id, &obsolete_id, verdict.detail.as_deref())
                        .await?;
                    tracing::info!(
                        pair = p.id,
                        obsolete = %obsolete_id,
                        "dedupe proposed a supersede, pending operator confirmation"
                    );
                }
                None => {
                    core.store
                        .set_pair_state(p.id, PairState::Contradiction, verdict.detail.as_deref())
                        .await?;
                }
            }
        }
        // The write path lands in the next commit. Until then a duplicate is
        // recorded rather than applied, which is what `autonomous = false` will
        // keep doing afterwards.
        Relation::Duplicate => {
            let detail = verdict
                .detail
                .clone()
                .unwrap_or_else(|| "would merge".into());
            core.store
                .set_pair_state(p.id, PairState::Contradiction, Some(&detail))
                .await?;
        }
    }
    Ok(())
}
