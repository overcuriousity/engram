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
