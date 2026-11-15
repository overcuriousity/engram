//! What each question is shown. Subjects are never cut; context goes first, and with no room for the subjects there is no state.

use crate::core::Core;
use crate::core::judging::Asked;
use crate::infer::budget::TokenCounter;
use crate::infer::decide::{Question, budget, swapped_questions};

pub struct Note<'a> {
    pub title: Option<&'a str>,
    /// The capture date, when it matters.
    pub day: Option<String>,
    pub text: &'a str,
}

fn head(label: &str, n: &Note) -> String {
    let day = n
        .day
        .as_ref()
        .map(|d| format!(" (captured {d})"))
        .unwrap_or_default();
    format!("{label}: {}{day}", n.title.unwrap_or("untitled"))
}

/// `<label>: <title> (captured …)\n<text>`.
fn block(label: &str, n: &Note) -> String {
    format!("{}\n{}", head(label, n), n.text)
}

/// `<title>\n<text>`, for a context item under a heading.
fn plain(prefix: &str, n: &Note) -> String {
    format!("{prefix}{}\n{}", n.title.unwrap_or("untitled"), n.text)
}

/// The fullest `build(k)`, k from `max` down to 0, that fits.
fn fullest(
    counter: &TokenCounter,
    tokens: usize,
    max: usize,
    build: impl Fn(usize) -> String,
) -> Option<String> {
    (0..=max)
        .rev()
        .map(build)
        .find(|s| counter.count(s) <= tokens)
}

fn join(parts: Vec<String>) -> String {
    parts.join("\n\n")
}

pub fn pair(
    a: &Note,
    b: &Note,
    sources_a: &[Note],
    sources_b: &[Note],
    counter: &TokenCounter,
    tokens: usize,
) -> Option<String> {
    // Oldest first across both members; undated sources count as oldest.
    let mut order: Vec<&Note> = sources_a.iter().chain(sources_b).collect();
    order.sort_by(|x, y| x.day.cmp(&y.day));
    fullest(counter, tokens, order.len(), |kept| {
        let live = &order[order.len() - kept..];
        let mut parts = vec![block("A", a), block("B", b)];
        for (is_a, name) in [(true, "A"), (false, "B")] {
            let mine: Vec<String> = [sources_a, sources_b][usize::from(!is_a)]
                .iter()
                .filter(|n| live.iter().any(|l| std::ptr::eq(*l, *n)))
                .map(|n| plain("- ", n))
                .collect();
            if !mine.is_empty() {
                // A blank line between sources, as between every block: a source's text has line
                // breaks of its own, and a single one would not show where the next begins.
                parts.push(format!(
                    "Earlier versions merged into {name}:\n{}",
                    mine.join("\n\n")
                ));
            }
        }
        join(parts)
    })
}

pub fn link(
    a: &Note,
    b: &Note,
    cues: &[String],
    counter: &TokenCounter,
    tokens: usize,
) -> Option<String> {
    fullest(counter, tokens, cues.len(), |kept| {
        let mut parts = vec![block("A", a), block("B", b)];
        if kept > 0 {
            parts.push(format!(
                "Found together by these searches: {}",
                cues[..kept].join("; ")
            ));
        }
        join(parts)
    })
}

pub fn reap(
    retired: &Note,
    successor: Option<&Note>,
    neighbours: &[Note],
    counter: &TokenCounter,
    tokens: usize,
) -> Option<String> {
    fullest(counter, tokens, neighbours.len(), |kept| {
        let mut parts = vec![block("Retired note", retired)];
        parts.extend(successor.map(|s| block("Its replacement", s)));
        if kept > 0 {
            let live: Vec<String> = neighbours[..kept]
                .iter()
                .enumerate()
                .map(|(i, n)| plain(&format!("[{}] ", i + 1), n))
                .collect();
            parts.push(format!("Live notes:\n{}", live.join("\n\n")));
        }
        join(parts)
    })
}

pub fn one(note: &Note, counter: &TokenCounter, tokens: usize) -> Option<String> {
    let s = plain("", note);
    (counter.count(&s) <= tokens).then_some(s)
}

pub fn window(text: &str, counter: &TokenCounter, tokens: usize) -> Option<String> {
    (counter.count(text) <= tokens).then(|| text.to_string())
}

fn answerable_of(questions: &[String], excerpts: &[Note]) -> String {
    let mut parts = vec![format!("Questions:\n{}", questions.join("\n"))];
    parts.extend(
        excerpts
            .iter()
            .enumerate()
            .map(|(i, n)| plain(&format!("Excerpt {}: ", i + 1), n)),
    );
    join(parts)
}

pub fn answerable(
    questions: &[String],
    excerpts: &[Note],
    counter: &TokenCounter,
    tokens: usize,
) -> Option<String> {
    (1..=excerpts.len()).rev().find_map(|kept| {
        let s = answerable_of(questions, &excerpts[..kept]);
        (counter.count(&s) <= tokens).then_some(s)
    })
}

/// Every excerpt or no state: a gate that closes must not judge a prefix.
pub fn answerable_whole(
    questions: &[String],
    excerpts: &[Note],
    counter: &TokenCounter,
    tokens: usize,
) -> Option<String> {
    let s = answerable_of(questions, excerpts);
    (!excerpts.is_empty() && counter.count(&s) <= tokens).then_some(s)
}

pub fn rewrite(
    originals: &[Note],
    rewrite: &Note,
    counter: &TokenCounter,
    tokens: usize,
) -> Option<String> {
    let mut parts: Vec<String> = originals
        .iter()
        .enumerate()
        .map(|(i, n)| plain(&format!("Original {}: ", i + 1), n))
        .collect();
    parts.push(plain("Rewrite: ", rewrite));
    let s = join(parts);
    (counter.count(&s) <= tokens).then_some(s)
}

/// The state budget for these questions on this decider, under the keys they are sent with: their
/// own, or `q0`, `q1`, … with a twin per choice in a swapped read (`decide_swapped`). A mixed list
/// may go either way, so it gets the smaller budget and its state fits both.
pub fn room(core: &Core, asked: &[&Asked]) -> usize {
    let own: Vec<Question> = asked.iter().map(|a| a.question.clone()).collect();
    let window = core.decider.context_tokens();
    let as_they_are = budget(&core.counter, window, &own);
    if asked.iter().any(|a| a.swapped) {
        as_they_are.min(budget(&core.counter, window, &swapped_questions(&own)))
    } else {
        as_they_are
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::judging::catalogue::{pair_relation, reap_worth};

    fn note<'a>(title: Option<&'a str>, day: Option<&str>, text: &'a str) -> Note<'a> {
        Note {
            title,
            day: day.map(String::from),
            text,
        }
    }
    fn c() -> TokenCounter {
        TokenCounter::default()
    }

    #[test]
    fn a_whole_answerable_state_has_every_excerpt_or_is_none() {
        let qs = vec!["q".to_string()];
        let long = "word ".repeat(200);
        let ex = [note(Some("a"), None, "short"), note(Some("b"), None, &long)];
        let one = c().count(&answerable(&qs, &ex[..1], &c(), 1000).unwrap());
        assert!(answerable(&qs, &ex, &c(), one).is_some(), "a prefix fits");
        assert!(answerable_whole(&qs, &ex, &c(), one).is_none());
        let full = answerable_whole(&qs, &ex, &c(), 10_000).unwrap();
        assert_eq!(full, answerable(&qs, &ex, &c(), 10_000).unwrap());
        assert!(answerable_whole(&qs, &[], &c(), 10_000).is_none());
    }

    #[test]
    fn a_pair_is_laid_out_with_titles_and_capture_dates() {
        let a = note(Some("Ports"), Some("2026-09-14"), "Use 8080.");
        let b = note(Some("Ports v2"), Some("2026-10-02"), "Use 9090.");
        assert_eq!(
            pair(&a, &b, &[], &[], &c(), 1000).unwrap(),
            "A: Ports (captured 2026-09-14)\nUse 8080.\n\nB: Ports v2 (captured 2026-10-02)\nUse 9090."
        );
    }

    #[test]
    fn merge_sources_are_dropped_oldest_first_and_members_never() {
        let a = note(Some("A"), None, "alpha alpha");
        let b = note(Some("B"), None, "beta beta");
        let s = [
            note(Some("s1"), Some("2026-01-01"), "one one one one"),
            note(Some("s2"), Some("2026-02-01"), "two two two two"),
            note(Some("s3"), Some("2026-03-01"), "three three three"),
        ];
        let base = c().count(&pair(&a, &b, &[], &[], &c(), 1000).unwrap());
        let full = pair(&a, &b, &s, &[], &c(), 1000).unwrap();
        assert!(full.contains("s1") && full.contains("s3"));
        let one_src = pair(&a, &b, &s[2..], &[], &c(), 1000).unwrap();
        let tight = c().count(&one_src);
        let got = pair(&a, &b, &s, &[], &c(), tight).unwrap();
        assert!(got.contains("s3") && !got.contains("s2") && !got.contains("s1"));
        assert!(got.contains("alpha alpha") && got.contains("beta beta"));
        let bare = pair(&a, &b, &s, &[], &c(), base).unwrap();
        assert!(!bare.contains("Earlier"));
    }

    #[test]
    fn members_that_do_not_fit_alone_give_no_state() {
        let long = "word ".repeat(200);
        let a = note(Some("A"), None, &long);
        let b = note(Some("B"), None, "x");
        assert!(pair(&a, &b, &[], &[], &c(), 10).is_none());
    }

    #[test]
    fn an_untitled_note_is_called_untitled() {
        let n = note(None, None, "text");
        assert_eq!(one(&n, &c(), 100).unwrap(), "untitled\ntext");
        assert!(
            pair(&n, &n, &[], &[], &c(), 100)
                .unwrap()
                .starts_with("A: untitled\ntext")
        );
    }

    #[test]
    fn a_link_names_the_searches_that_found_both_and_drops_them_first() {
        let a = note(Some("A"), None, "a");
        let b = note(Some("B"), None, "b");
        let cues = vec!["q1".to_string(), "q2".to_string()];
        let full = link(&a, &b, &cues, &c(), 1000).unwrap();
        assert_eq!(
            full,
            "A: A\na\n\nB: B\nb\n\nFound together by these searches: q1; q2"
        );
        let bare = link(&a, &b, &[], &c(), 1000).unwrap();
        let got = link(&a, &b, &cues, &c(), c().count(&bare)).unwrap();
        assert_eq!(got, bare);
        let one_cue = link(&a, &b, &cues[..1], &c(), 1000).unwrap();
        assert_eq!(
            link(&a, &b, &cues, &c(), c().count(&one_cue)).unwrap(),
            one_cue
        );
    }

    #[test]
    fn a_retired_note_is_shown_with_its_replacement_and_numbered_live_notes() {
        let r = note(Some("Old"), None, "old text");
        let s = note(Some("New"), None, "new text");
        let n = [note(Some("N1"), None, "n1"), note(Some("N2"), None, "n2")];
        assert_eq!(
            reap(&r, Some(&s), &n, &c(), 1000).unwrap(),
            "Retired note: Old\nold text\n\nIts replacement: New\nnew text\n\nLive notes:\n[1] N1\nn1\n\n[2] N2\nn2"
        );
    }

    #[test]
    fn neighbours_go_before_the_replacement_does() {
        let r = note(Some("Old"), None, "old text");
        let s = note(Some("New"), None, "new text");
        let n = [note(Some("N1"), None, "n1"), note(Some("N2"), None, "n2")];
        let without = reap(&r, Some(&s), &[], &c(), 1000).unwrap();
        let got = reap(&r, Some(&s), &n, &c(), c().count(&without)).unwrap();
        assert_eq!(got, without);
        assert!(got.contains("Its replacement"));
    }

    #[test]
    fn a_rewrite_follows_its_originals() {
        let o = [note(Some("O1"), None, "x"), note(Some("O2"), None, "y")];
        let w = note(Some("W"), None, "z");
        let s = rewrite(&o, &w, &c(), 1000).unwrap();
        assert_eq!(s, "Original 1: O1\nx\n\nOriginal 2: O2\ny\n\nRewrite: W\nz");
    }

    #[test]
    fn excerpts_are_dropped_from_the_end_and_one_must_remain() {
        let qs = vec!["why?".to_string()];
        let e = [
            note(Some("E1"), None, "first"),
            note(Some("E2"), None, "second"),
        ];
        let one_e = answerable(&qs, &e[..1], &c(), 1000).unwrap();
        assert_eq!(one_e, "Questions:\nwhy?\n\nExcerpt 1: E1\nfirst");
        assert_eq!(answerable(&qs, &e, &c(), c().count(&one_e)).unwrap(), one_e);
        assert!(answerable(&qs, &e, &c(), 1).is_none());
        assert!(answerable(&qs, &[], &c(), 1000).is_none());
    }

    #[tokio::test]
    async fn room_counts_a_swapped_questions_twin() {
        let core = crate::core::test_support::test_core().await;
        let (p, r) = (pair_relation(), reap_worth());
        assert!(room(&core, &[&p]) < room(&core, &[&r]));
    }

    #[tokio::test]
    async fn room_counts_a_swapped_question_once_with_its_twin() {
        let core = crate::core::test_support::test_core().await;
        let ctx = core.decider.context_tokens();
        let (p, r) = (pair_relation(), reap_worth());
        let only = budget(
            &core.counter,
            ctx,
            &swapped_questions(std::slice::from_ref(&p.question)),
        );
        assert_eq!(room(&core, &[&p]), only);
        // With a plain question beside it, as `ask_with` sends the two: `q0`, `q0r`, `q1`.
        let wire = swapped_questions(&[p.question.clone(), r.question.clone()]);
        assert_eq!(room(&core, &[&p, &r]), budget(&core.counter, ctx, &wire));
    }

    #[tokio::test]
    async fn a_state_sized_by_room_fits_under_the_keys_the_request_is_sent_with() {
        use crate::core::judging::catalogue::{category, link_direction, link_kind, pinned};
        use crate::infer::decide::check;
        let core = crate::core::test_support::test_core().await;
        let ctx = core.decider.context_tokens();
        let long = "word ".repeat(ctx);
        let sized = |asked: &[&Asked]| -> String {
            core.counter
                .cut(&long, room(&core, asked), true)
                .unwrap_or_default()
        };
        let own = |asked: &[&Asked]| -> Vec<Question> {
            asked.iter().map(|a| a.question.clone()).collect()
        };
        let (c, p, k, d) = (category(), pinned(), link_kind(), link_direction());
        // A reading goes as it is, under the questions' own keys.
        let state = sized(&[&c, &p]);
        assert!(core.counter.count(&state) + 8 > room(&core, &[&c, &p]));
        check(&core.counter, ctx, &state, &own(&[&c, &p])).unwrap();
        // A link is a swapped read: `q0`, `q0r`, `q1`, `q1r`.
        let wire = swapped_questions(&own(&[&k, &d]));
        check(&core.counter, ctx, &sized(&[&k, &d]), &wire).unwrap();
        // Led by a swapped question, a plain choice is renumbered and twinned with the rest.
        let wire = swapped_questions(&own(&[&k, &c]));
        check(&core.counter, ctx, &sized(&[&k, &c]), &wire).unwrap();
        // Led by a plain one, the same two go as they are.
        check(&core.counter, ctx, &sized(&[&c, &k]), &own(&[&c, &k])).unwrap();
    }

    #[test]
    fn a_rewrite_fits_with_every_original_or_gives_no_state() {
        let o = [note(Some("O1"), None, "x"), note(Some("O2"), None, "y")];
        let w = note(Some("W"), None, "z");
        let whole = c().count(&rewrite(&o, &w, &c(), 1000).unwrap());
        assert!(rewrite(&o, &w, &c(), whole).is_some());
        assert!(
            rewrite(&o, &w, &c(), whole - 1).is_none(),
            "no original is dropped to make room"
        );
        let alone = c().count(&rewrite(&[], &w, &c(), 1000).unwrap());
        assert!(rewrite(&o, &w, &c(), alone).is_none());
    }

    #[test]
    fn one_note_fits_whole_or_gives_no_state() {
        let n = note(Some("Ports"), None, "Use 8080.");
        let whole = c().count("Ports\nUse 8080.");
        assert_eq!(one(&n, &c(), whole).unwrap(), "Ports\nUse 8080.");
        assert!(one(&n, &c(), whole - 1).is_none(), "a note is never cut");
    }

    #[test]
    fn a_window_fits_whole_or_gives_no_state() {
        let text = "remind me to call Anna on Friday";
        let whole = c().count(text);
        assert_eq!(window(text, &c(), whole).unwrap(), text);
        assert!(window(text, &c(), whole - 1).is_none(), "a window is never cut");
    }

    #[test]
    fn the_oldest_source_across_both_members_goes_first() {
        let a = note(Some("A"), None, "alpha");
        let b = note(Some("B"), None, "beta");
        let sa = [
            note(Some("a1"), Some("2026-01-01"), "x"),
            note(Some("a2"), Some("2026-04-01"), "x"),
        ];
        let sb = [
            note(Some("b1"), Some("2026-02-01"), "x"),
            note(Some("b2"), Some("2026-03-01"), "x"),
        ];
        let keep3 = pair(&a, &b, &sa[1..], &sb, &c(), 1000).unwrap();
        let got = pair(&a, &b, &sa, &sb, &c(), c().count(&keep3)).unwrap();
        assert_eq!(got, keep3);
        assert!(
            !got.contains("a1") && got.contains("b1") && got.contains("a2") && got.contains("b2")
        );
    }
}
