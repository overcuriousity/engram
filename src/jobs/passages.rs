//! Capture without synthesis: a window becomes verbatim passages sized to the
//! embedder, each under the heading the document gave it.

use crate::core::Core;
use crate::error::Result;
use crate::infer::budget::TokenCounter;
use crate::infer::split::{Window, split_into_segments};
use crate::store::artifacts::{CorpusSpan, NewArtifact, Provenance};
use crate::store::corpora::CorpusStatus;

/// A document's name, derived locally: longer than this is a paragraph.
pub const TITLE_MAX: usize = 80;

/// One verbatim slice of a window: the retrieval unit at `off` and `earned`.
#[derive(Debug, Clone, PartialEq)]
pub struct Passage {
    /// The heading this text sits under, as the document wrote it — the
    /// carried heading of a continuation, or the most recent heading inside
    /// the window above it. Never inferred.
    pub title: Option<String>,
    /// The slice itself. A carried heading is *not* in here: it is not one of
    /// the lines the span names, and the embedding input would carry it twice.
    pub text: String,
    pub start_line: i64,
    pub end_line: i64,
}

fn is_heading(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') && t.trim_start_matches('#').starts_with(' ')
}

/// "## Recovering deleted entries" → "Recovering deleted entries".
///
/// A link left in the heading is reduced to its words, and whitespace runs to
/// one space. The HTML extractor already keeps in-page anchors out of a
/// heading; this is the belt for every other way a heading reaches here — a
/// pasted markdown document, another extractor — so that a title is never
/// `[](#NAME)NAME [top](#top_of_page)` whatever wrote it.
pub fn heading_title(line: &str) -> String {
    let bare = line.trim().trim_start_matches('#');
    let mut out = String::with_capacity(bare.len());
    let mut rest = bare;
    // `[text](target)` → `text`. Anything that is not exactly that shape is
    // left as written: a bracket in prose is not a link.
    while let Some(first) = rest.find('[') {
        let Some(close_rel) = rest[first..].find("](") else {
            break;
        };
        let close = first + close_rel;
        // The link opens at the LAST bracket before `](`, not the first one on
        // the line. Anchored on the first, a plain bracket standing in prose
        // ahead of a real link swallowed everything between the two:
        // `Arrays [0] and [the docs](…)` came out `Arrays 0] and [the docs`.
        let open = rest[..close].rfind('[').unwrap_or(first);
        // A markdown image is a link with a `!` in front. The alt text is the
        // words; the `!` is syntax, and `![diagram](a.png) Overview` has no
        // business becoming `!diagram Overview`.
        let text_end = match rest[..open].ends_with('!') {
            true => open - 1,
            false => open,
        };
        // Parens nest. A bare `find(')')` stopped at the inner one of
        // `[Loop device](https://en.wikipedia.org/wiki/Loop_device_(computing))`
        // and left the outer `)` standing in the title as `See Loop device)`,
        // and Wikipedia headings are exactly what this path is handed.
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
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first heading among the next `n` lines, with all `n` consumed.
///
/// Consuming all of them is the whole point. The carried lines belong to the
/// window above rather than to this passage's body, so the iterator must be
/// left standing just past them — and `find` alone stops at the heading, which
/// leaves every carried line after it to be read as body text. `flush` emits a
/// carry of 0 or 1 today, and at 1 the two spellings agree; at 2 they would
/// not, and every passage span below here would quietly shift by a line.
fn carried_heading<'a>(lines: &mut impl Iterator<Item = &'a str>, n: usize) -> Option<String> {
    let carried: Vec<&str> = lines.take(n).collect();
    carried
        .into_iter()
        .find(|l| is_heading(l))
        .map(heading_title)
}

/// Split one window into passages sized to the embedder.
///
/// The same splitter that made the window, called again with the retrieval
/// budget, over the window's *body* — its text minus the heading the outer
/// split carried in (`carry_lines`), which belongs to the document further up
/// and occupies none of this window's lines. Passage spans therefore partition
/// `start_line..=end_line` and never cross the window: promotion later
/// supersedes a whole number of them.
///
/// Titles: a continuation passage takes the heading the inner split carried
/// into it (and that heading leaves its `text`); a passage that contains a
/// heading is titled by the first one it contains; a passage with neither
/// takes the last heading seen above it, the window's own carried heading
/// included. `is_heading` knows Markdown `#` headings only, so plain text
/// yields untitled passages — nothing is inferred to fill the gap.
pub fn split_passages(
    window: &Window,
    counter: &TokenCounter,
    chunk_tokens: usize,
) -> Vec<Passage> {
    let carry = window.carry_lines.max(0) as usize;
    let mut lines = window.text.lines();
    let outer_heading: Option<String> = (carry > 0)
        .then(|| carried_heading(&mut lines, carry))
        .flatten();
    let body: String = lines.collect::<Vec<_>>().join("\n");

    let inner = split_into_segments(&body, counter, chunk_tokens.max(1));
    let mut out = Vec::with_capacity(inner.len());
    let mut last_heading: Option<String> = outer_heading;
    for p in inner {
        let pc = p.carry_lines.max(0) as usize;
        let mut plines = p.text.lines();
        let carried: Option<String> = (pc > 0).then(|| carried_heading(&mut plines, pc)).flatten();
        let own: Vec<&str> = plines.collect();
        let inside: Option<String> = own.iter().find(|l| is_heading(l)).map(|l| heading_title(l));
        let title = carried
            .clone()
            .or_else(|| inside.clone())
            .or_else(|| last_heading.clone());
        // What the next passage inherits if the splitter carries nothing: the
        // last heading seen in document order.
        if let Some(h) = own.iter().rev().find(|l| is_heading(l)) {
            last_heading = Some(heading_title(h));
        } else if carried.is_some() {
            last_heading = carried.clone();
        }
        // Body line k is document line `window.start_line + k - 1`: the body
        // starts where the window's own lines start. Clamped, because a cut
        // long line can put more text lines in the body than the document has.
        let start =
            (window.start_line + p.start_line - 1).clamp(window.start_line, window.end_line);
        let end = (window.start_line + p.end_line - 1).clamp(start, window.end_line);
        out.push(Passage {
            title,
            text: own.join("\n"),
            start_line: start,
            end_line: end,
        });
    }
    out
}

/// The whole of capture at `off` and `earned`: split into windows, write them
/// `verbatim`, split each window into passages, write those, name the document
/// without a model, and finish the corpus the way a synthesized one finishes.
/// No inference call anywhere on this path.
///
/// Idempotent per segment: a process that dies between two segments' inserts
/// re-runs this, and a segment that already owns rows is left alone.
pub async fn capture_verbatim(core: &Core, corpus_id: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    let windows = split_into_segments(
        &src.raw_text,
        &core.counter,
        super::synthesize::segment_budget(core),
    );
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
            carry_lines: w.carry_lines,
        })
        .collect();
    core.store.upsert_segments(corpus_id, &rows).await?;
    core.store.mark_segments_verbatim(corpus_id).await?;

    // Under the document lock like every other local rearrangement of a
    // corpus's rows, held through `finish` the way `settle` holds it: a
    // promotion's write must not interleave with the renumbering.
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
                corpus_span: Some(CorpusSpan {
                    start_line: p.start_line,
                    end_line: p.end_line,
                }),
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
    // Renumbers, measures coverage (green: the passages partition the
    // document), arms the embed and moves the corpus on. The same function a
    // synthesized corpus reaches through its last window's settle.
    super::synthesize::finish(core, corpus_id).await
}

/// A corpus title with no model: the first heading, else the first non-empty
/// line, cut to `TITLE_MAX` characters. `None` for whitespace.
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
    Some(line.chars().take(TITLE_MAX).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::budget::TokenCounter;
    use crate::infer::split::Window;

    fn window(text: &str, start_line: i64, carry_lines: i64) -> Window {
        let own = text.lines().count() as i64 - carry_lines;
        Window {
            text: text.to_string(),
            start_line,
            end_line: start_line + own - 1,
            carry_lines,
        }
    }

    #[test]
    fn a_window_that_fits_is_one_passage_whose_span_is_the_window() {
        let w = window("para one\n\npara two", 10, 0);
        let p = split_passages(&w, &TokenCounter, 1000);
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
        let w = window(&text, 1, 0);
        let p = split_passages(&w, &TokenCounter, 40);
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
    fn the_carried_heading_becomes_the_title_and_leaves_the_text() {
        // A continuation window: the splitter carried "## Recovery" in from
        // the previous window as line 1 of its text.
        let w = window("## Recovery\nstep three\nstep four", 40, 1);
        let p = split_passages(&w, &TokenCounter, 1000);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].title.as_deref(), Some("Recovery"));
        assert_eq!(p[0].text, "step three\nstep four");
        assert_eq!((p[0].start_line, p[0].end_line), (40, 41));
    }

    #[test]
    fn a_heading_inside_the_window_titles_the_passage_that_holds_it_and_is_carried_on() {
        let body = format!(
            "intro line\n## Mounting\n{}\n\n{}",
            "mount words ".repeat(12),
            "more mount words ".repeat(12)
        );
        let w = window(&body, 1, 0);
        let p = split_passages(&w, &TokenCounter, 40);
        assert!(p.len() >= 3, "{}", p.len());
        // The heading opens its own passage, which holds the heading line
        // verbatim and is titled by it; the intro line before it stands alone
        // and untitled.
        let k = p
            .iter()
            .position(|x| x.text.contains("## Mounting"))
            .expect("a passage holds the heading");
        assert_eq!(p[k].title.as_deref(), Some("Mounting"));
        assert_eq!(p[0].title, None, "{p:?}");
        // The continuation takes the carried heading as title, not as text.
        assert_eq!(p[k + 1].title.as_deref(), Some("Mounting"));
        assert!(
            !p[k + 1].text.contains("## Mounting"),
            "{:?}",
            p[k + 1].text
        );
        // And every heading line appears exactly once across the passages.
        let total = p.iter().filter(|x| x.text.contains("## Mounting")).count();
        assert_eq!(total, 1);
    }

    #[test]
    fn a_continuation_window_with_no_inner_heading_keeps_the_outer_one_throughout() {
        let body = format!(
            "## Outer\n{}\n\n{}",
            "first part words ".repeat(12),
            "second part words ".repeat(12)
        );
        let w = window(&body, 20, 1);
        let p = split_passages(&w, &TokenCounter, 40);
        assert!(p.len() >= 2);
        assert!(
            p.iter().all(|x| x.title.as_deref() == Some("Outer")),
            "{p:?}"
        );
        assert!(p.iter().all(|x| !x.text.contains("## Outer")));
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
        assert_eq!(derive_title(&long).unwrap().chars().count(), TITLE_MAX);
        assert_eq!(derive_title("   \n\t\n"), None);
        assert_eq!(heading_title("###   Deep heading  "), "Deep heading");
        // A link is its words, an empty one is nothing, and runs of spaces are
        // one — the man7 heading shape, arriving from any extractor at all.
        assert_eq!(
            heading_title("## [](#NAME)NAME         [top](#top_of_page)"),
            "NAME top"
        );
        assert_eq!(
            heading_title("## See [the docs](https://x.test/a)"),
            "See the docs"
        );
        assert_eq!(heading_title("## Arrays [0] and [1]"), "Arrays [0] and [1]");
        // A bracket in prose standing *before* a real link on the same line.
        // Anchored on the first `[`, the stripper read from it all the way to
        // the link's `](` and ate the words in between: this came out
        // `Arrays 0] and [the docs`.
        assert_eq!(
            heading_title("## Arrays [0] and [the docs](https://x.test/a)"),
            "Arrays [0] and the docs"
        );
        // An image is a link with a `!` in front. The alt text is words; the
        // `!` is syntax, and `!diagram Overview` is neither.
        assert_eq!(
            heading_title("## ![diagram](a.png) Overview"),
            "diagram Overview"
        );
        // A parenthesised URL — the Wikipedia disambiguation shape, which is
        // most of what this path is handed. Stopping at the first `)` left the
        // outer one behind as `See Loop device)`.
        assert_eq!(
            heading_title(
                "## See [Loop device](https://en.wikipedia.org/wiki/Loop_device_(computing))"
            ),
            "See Loop device"
        );
    }

    #[tokio::test]
    async fn capture_at_off_writes_passages_marks_segments_verbatim_and_finishes() {
        let mut core = crate::core::test_support::test_core().await;
        core.synthesis = crate::config::SynthesisMode::Off;
        core.synthesizer = None;
        let text = format!(
            "# Manual\n\n## Install\n{}\n\n## Recover\n{}",
            "install words ".repeat(40),
            "recover words ".repeat(40)
        );
        let out = core.ingest(&text, "web", None).await.unwrap();
        // The Synthesize job is what capture queued; run it.
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
        // Green by construction: every line is claimed.
        assert_eq!(s.coverage.map(|c| (c * 100.0).round() as i64), Some(100));
        assert!(
            core.store
                .live_job(crate::store::jobs::Stage::Embed, &out.id)
                .await
                .unwrap()
        );
        // No model unit was armed.
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
        let mut core = crate::core::test_support::test_core().await;
        core.synthesis = crate::config::SynthesisMode::Off;
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
        let mut core = crate::core::test_support::test_core().await;
        core.synthesis = crate::config::SynthesisMode::Off;
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

    /// The note is written at capture and the passages arrive later, each
    /// numbered from 0 within its own window. Renumbering has to put the note
    /// first and push the document down by one — with no change to this
    /// writer, which is the whole reason the note carries no `segment_idx`.
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
