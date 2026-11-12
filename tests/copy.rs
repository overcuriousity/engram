#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::fs;
use std::path::Path;

const MAX_WORDS: usize = 12;

const INLINE: &[&str] = &[
    "a", "abbr", "b", "em", "i", "kbd", "mark", "q", "s", "small", "span", "strong", "sub", "sup",
    "time", "u", "var", "bdi",
];

const SKIPPED: &[&str] = &["pre", "code", "script", "style", "svg", "textarea"];

fn templates() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/web/templates");
    let mut out: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "html"))
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (name, fs::read_to_string(&p).unwrap())
        })
        .collect();
    out.sort();
    out
}

fn cut_between(src: &str, open: &str, close: &str, with: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(i) = rest.find(open) {
        out.push_str(&rest[..i]);
        out.push_str(with);
        match rest[i + open.len()..].find(close) {
            Some(j) => rest = &rest[i + open.len() + j + close.len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn tag_name(tag: &str) -> String {
    tag.trim_start_matches(['<', '/'])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Text runs a reader sees as one piece: inline elements join, everything else splits.
fn text_runs(src: &str) -> Vec<String> {
    let mut s = cut_between(src, "{#", "#}", "\n");
    s = cut_between(&s, "{%", "%}", "\u{1}");
    s = cut_between(&s, "{{", "}}", " X ");
    for tag in SKIPPED {
        s = cut_between(&s, &format!("<{tag}"), &format!("</{tag}>"), "\u{1}");
    }
    let mut runs = vec![String::new()];
    let mut rest = s.as_str();
    while let Some(i) = rest.find('<') {
        runs.last_mut().unwrap().push_str(&rest[..i]);
        let Some(j) = rest[i..].find('>') else { break };
        let tag = &rest[i..i + j + 1];
        if !INLINE.contains(&tag_name(tag).as_str()) {
            runs.push(String::new());
        }
        rest = &rest[i + j + 1..];
    }
    runs.last_mut().unwrap().push_str(rest);
    runs.iter()
        .flat_map(|r| r.split('\u{1}'))
        .map(|r| r.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|r| r.chars().any(char::is_alphabetic))
        .collect()
}

fn words(run: &str) -> usize {
    run.split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .count()
}

#[test]
fn no_template_paragraph_is_longer_than_a_gloss() {
    let mut offenders = Vec::new();
    for (name, src) in templates() {
        for run in text_runs(&src) {
            if words(&run) > MAX_WORDS || run.contains('!') {
                offenders.push(format!("{name}: {run}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "copy longer than a gloss:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn every_ui_error_is_one_sentence() {
    let src = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/web/ui_error.rs"))
        .unwrap();
    let body = &src[src.find("fn page(").unwrap()..];
    let body = &body[..body.find("\n    }\n").unwrap()];
    let mut offenders = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find('"') {
        let Some(j) = rest[i + 1..].find('"') else {
            break;
        };
        let lit = &rest[i + 1..i + 1 + j];
        rest = &rest[i + j + 2..];
        if !lit.contains(' ') {
            continue;
        }
        let stops = lit.trim_end_matches('.').matches(". ").count();
        if stops > 0 || words(lit) > 20 || lit.contains('!') {
            offenders.push(lit.to_string());
        }
    }
    assert!(
        offenders.is_empty(),
        "errors longer than a sentence:\n{}",
        offenders.join("\n")
    );
}
