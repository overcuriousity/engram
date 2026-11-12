#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn files(dir: &str, ext: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![root().join(dir)];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == ext) {
                let rel = p.strip_prefix(root()).unwrap().display().to_string();
                out.push((rel, fs::read_to_string(&p).unwrap()));
            }
        }
    }
    out.sort();
    out
}

/// The Rust source up to its `#[cfg(test)]` tests module.
fn non_test(src: &str) -> &str {
    let mut from = 0;
    while let Some(i) = src[from..].find("#[cfg(test)]\n") {
        let at = from + i;
        let next = src[at..].lines().nth(1).unwrap_or("");
        if next.contains("mod tests") {
            return &src[..at];
        }
        from = at + 1;
    }
    src
}

/// Askama comments blanked out, newlines kept so line numbers still point at the source.
fn without_template_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(i) = rest.find("{#") {
        out.push_str(&rest[..i]);
        let end = rest[i..].find("#}").map_or(rest.len(), |j| i + j + 2);
        out.extend(rest[i..end].chars().filter(|c| *c == '\n'));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn inline_handler(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while let Some(j) = line[i..].find(" on") {
        let start = i + j + 3;
        let name_len = line[start..]
            .bytes()
            .take_while(|b| b.is_ascii_lowercase())
            .count();
        if name_len > 0
            && bytes.get(start + name_len) == Some(&b'=')
            && bytes.get(start + name_len + 1) == Some(&b'"')
        {
            return true;
        }
        i = start;
    }
    false
}

fn markup_offences(line: &str) -> Vec<&'static str> {
    let mut found = Vec::new();
    if line.contains("style=\"") || line.contains("style='") || line.contains("style=\\\"") {
        found.push("style attribute");
    }
    if inline_handler(line) {
        found.push("on* handler");
    }
    if line.contains("<script>") || line.contains("<script type=\"text/javascript\">") {
        found.push("inline script");
    }
    if line.contains("hx-on") {
        found.push("hx-on");
    }
    if line.contains("hx-confirm") {
        found.push("hx-confirm");
    }
    if line.contains("hx-trigger=\"")
        && line[line.find("hx-trigger=\"").unwrap_or(0)..]
            .split('"')
            .nth(1)
            .is_some_and(|t| t.contains('['))
    {
        found.push("hx-trigger filter");
    }
    if line.contains("javascript:") {
        found.push("javascript: URL");
    }
    if line.contains("hx-vals='js:") || line.contains("hx-vals=\"js:") || line.contains("hx-vars") {
        found.push("hx-vals js:");
    }
    found
}

#[test]
fn no_template_or_handler_emits_inline_code() {
    let mut offenders = Vec::new();
    for (name, src) in files("src/web/templates", "html") {
        for (n, line) in without_template_comments(&src).lines().enumerate() {
            for what in markup_offences(line) {
                offenders.push(format!("{name}:{}: {what}", n + 1));
            }
        }
    }
    for (name, src) in files("src/web", "rs") {
        for (n, line) in non_test(&src).lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for what in markup_offences(line) {
                offenders.push(format!("{name}:{}: {what}", n + 1));
            }
        }
    }
    let mut scripts = files("assets/js", "js");
    scripts.push((
        "assets/app.js".into(),
        fs::read_to_string(root().join("assets/app.js")).unwrap(),
    ));
    for (name, src) in scripts {
        for (n, line) in src.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            for call in ["confirm(", "alert(", "prompt("] {
                let mut rest = code;
                while let Some(i) = rest.find(call) {
                    let before = rest[..i].chars().last();
                    let member = rest[..i].ends_with("installEvent.");
                    let word = before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
                    if !word || (before == Some('.') && !member) {
                        offenders.push(format!("{name}:{}: {call}", n + 1));
                    }
                    rest = &rest[i + call.len()..];
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "inline code:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_static_scan_reads_the_files_it_claims_to() {
    assert!(Path::new(&root().join("src/web/templates/layout.html")).exists());
    assert!(!files("src/web/templates", "html").is_empty());
}

#[test]
fn no_template_polls() {
    let mut offenders = Vec::new();
    for (name, src) in files("src/web/templates", "html") {
        for (n, line) in without_template_comments(&src).lines().enumerate() {
            let mut rest = line;
            while let Some(i) = rest.find("every ") {
                let after = &rest[i + 6..];
                let digits = after
                    .chars()
                    .take_while(|c| {
                        c.is_ascii_digit()
                            || *c == '{'
                            || *c == ' '
                            || c.is_alphanumeric()
                            || *c == '}'
                    })
                    .collect::<String>();
                if after.starts_with(|c: char| c.is_ascii_digit() || c == '{')
                    && digits.contains('s')
                {
                    offenders.push(format!("{name}:{}: {line}", n + 1));
                    break;
                }
                rest = after;
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "polling triggers:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_css_sets_a_px_root_font_size_or_text_below_12px() {
    let mut offenders = Vec::new();
    for (name, src) in files("assets/css", "css") {
        for (n, line) in src.lines().enumerate() {
            let t = line.trim();
            if (t.starts_with("html") || t.starts_with(":root"))
                && t.contains("font-size")
                && t.contains("px")
            {
                offenders.push(format!("{name}:{}: px root font size", n + 1));
            }
            let mut rest = line;
            while let Some(i) = rest.find("font-size:") {
                let v = rest[i + 10..].trim_start();
                let num: String = v
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                let unit = &v[num.len()..];
                if let Ok(x) = num.parse::<f64>() {
                    let px = if unit.starts_with("rem") {
                        Some(x * 14.0)
                    } else if unit.starts_with("px") {
                        Some(x)
                    } else {
                        None
                    };
                    if px.is_some_and(|p| p > 0.0 && p < 12.0) {
                        offenders.push(format!("{name}:{}: {}", n + 1, line.trim()));
                    }
                }
                rest = &rest[i + 10..];
            }
        }
    }
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
}

#[test]
fn the_extension_panel_has_no_inline_code_or_literal_colours() {
    let mut offenders = Vec::new();
    for ext in ["html", "js", "css"] {
        for (name, src) in files("extension/shared", ext) {
            for (n, line) in src.lines().enumerate() {
                if ext == "html" {
                    for what in markup_offences(line) {
                        offenders.push(format!("{name}:{}: {what}", n + 1));
                    }
                }
                let code = if ext == "js" {
                    line.split("//").next().unwrap_or("")
                } else {
                    line
                };
                let hex = code.char_indices().any(|(i, c)| {
                    c == '#' && {
                        let h: String = code[i + 1..]
                            .chars()
                            .take_while(|c| c.is_ascii_hexdigit())
                            .collect();
                        matches!(h.len(), 3 | 4 | 6 | 8)
                            && !code[i + 1 + h.len()..]
                                .starts_with(|c: char| c.is_alphanumeric() || c == '-')
                    }
                });
                if ext != "html"
                    && (hex
                        || code.contains("rgb(")
                        || code.contains("rgba(")
                        || code.contains("hsl("))
                {
                    offenders.push(format!("{name}:{}: literal colour: {}", n + 1, line.trim()));
                }
                if ext == "css" && code.contains("@import") {
                    offenders.push(format!("{name}:{}: @import", n + 1));
                }
                if ext == "js"
                    && (code.contains("confirm(")
                        || code.contains("alert(")
                        || code.contains("prompt("))
                {
                    offenders.push(format!("{name}:{}: native dialog", n + 1));
                }
            }
        }
    }
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
}

#[test]
fn unsafe_is_allowed_only_in_the_contained_ffi_modules() {
    let allowed = ["src/infer/whisper.rs", "src/vector/sqlite.rs"];
    let mut found = Vec::new();
    for (name, src) in files("src", "rs") {
        if src.contains("allow(unsafe_code)") {
            found.push(name);
        }
    }
    found.sort();
    assert_eq!(
        found, allowed,
        "allow(unsafe_code) outside the contained FFI"
    );
    let cargo = fs::read_to_string(root().join("Cargo.toml")).unwrap();
    assert!(cargo.contains("unsafe_code = \"deny\""));
}

#[test]
fn no_literal_colour_outside_the_token_file() {
    let mut offenders = Vec::new();
    for (name, src) in files("assets/css", "css") {
        if name.ends_with("00-tokens.css") {
            continue;
        }
        for (n, line) in src.lines().enumerate() {
            let code = line.split("/*").next().unwrap_or("");
            let hex = code.char_indices().any(|(i, c)| {
                c == '#' && {
                    let h: String = code[i + 1..]
                        .chars()
                        .take_while(|c| c.is_ascii_hexdigit())
                        .collect();
                    matches!(h.len(), 3 | 4 | 6 | 8)
                        && !code[i + 1 + h.len()..]
                            .starts_with(|c: char| c.is_alphanumeric() || c == '-')
                }
            });
            if hex
                || ["rgb(", "rgba(", "hsl(", "hsla(", "oklch("]
                    .iter()
                    .any(|f| code.contains(f))
            {
                offenders.push(format!("{name}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
}

/// Files whose runtime queries are not yet converted to the checked macros; shrinks to empty.
const NOT_YET_CHECKED: &[&str] = &[
    "src/backup/restore.rs",
    "src/backup/sqlite.rs",
    "src/core/ingest.rs",
    "src/core/search.rs",
    "src/eval/anchor.rs",
    "src/eval/lived.rs",
    "src/jobs/associate.rs",
    "src/jobs/consolidate.rs",
    "src/jobs/mod.rs",
    "src/jobs/observe.rs",
    "src/jobs/pursuit.rs",
    "src/jobs/synthesize.rs",
    "src/store/actions.rs",
    "src/store/artifacts.rs",
    "src/store/asks.rs",
    "src/store/attachments.rs",
    "src/store/auth.rs",
    "src/store/context.rs",
    "src/store/control.rs",
    "src/store/corpora.rs",
    "src/store/eval_runs.rs",
    "src/store/feedback.rs",
    "src/store/gaps.rs",
    "src/store/generations.rs",
    "src/store/grants.rs",
    "src/store/insights.rs",
    "src/store/integrations.rs",
    "src/store/jobs.rs",
    "src/store/lineage.rs",
    "src/store/links.rs",
    "src/store/mod.rs",
    "src/store/moments.rs",
    "src/store/observations.rs",
    "src/store/pairs.rs",
    "src/store/pursuits.rs",
    "src/store/rehearsals.rs",
    "src/store/segments.rs",
    "src/store/sleep_runs.rs",
    "src/store/sweeps.rs",
    "src/store/versions.rs",
    "src/vector/sqlite.rs",
    "src/web/api.rs",
    "src/web/insights.rs",
    "src/web/ui.rs",
];

fn runtime_query_lines(src: &str) -> Vec<usize> {
    let calls = [
        "sqlx::query(",
        "sqlx::query_as(",
        "sqlx::query_scalar(",
        "query_as::<",
        "query_scalar::<",
        "sqlx::query::<",
    ];
    src.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim_start().starts_with("//") && calls.iter().any(|c| l.contains(c)))
        .map(|(n, _)| n)
        .collect()
}

#[test]
fn every_runtime_query_says_why() {
    let mut offenders = Vec::new();
    let mut sources = files("src", "rs");
    sources.extend(files("tests", "rs"));
    for (name, src) in sources.iter().filter(|(n, _)| n != "tests/csp_static.rs") {
        let lines: Vec<&str> = src.lines().collect();
        let found = runtime_query_lines(src);
        if NOT_YET_CHECKED.contains(&name.as_str()) {
            if found.is_empty() {
                offenders.push(format!("{name}: converted; drop it from NOT_YET_CHECKED"));
            }
            continue;
        }
        for n in found {
            let why = lines[n.saturating_sub(3)..n]
                .iter()
                .any(|l| l.trim_start().starts_with("// runtime:"));
            if !why {
                offenders.push(format!(
                    "{name}:{}: runtime query without `// runtime:`",
                    n + 1
                ));
            }
        }
    }
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
}
