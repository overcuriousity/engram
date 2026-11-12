use std::collections::BTreeMap;

const TOKENS: &str = include_str!("../assets/css/00-tokens.css");
const BACKGROUNDS: [&str; 3] = ["bg-base", "bg-surface", "bg-elevated"];
const DARK_MEDIA: &str = ":root:not([data-theme=\"light\"])";
const DARK_ATTR: &str = ":root[data-theme=\"dark\"]";

type Theme = BTreeMap<String, String>;

fn strip_comments(css: &str) -> String {
    let mut out = String::new();
    let mut rest = css;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        rest = rest[i..].find("*/").map_or("", |j| &rest[i + j + 2..]);
    }
    out + rest
}

/// Every innermost `selector { … }` block, in order.
fn blocks(css: &str) -> Vec<(String, String)> {
    let css = strip_comments(css);
    let mut out = Vec::new();
    let mut stack: Vec<(String, usize, bool)> = Vec::new();
    let mut boundary = 0;
    for (i, c) in css.char_indices() {
        match c {
            '{' => {
                if let Some(parent) = stack.last_mut() {
                    parent.2 = true;
                }
                stack.push((css[boundary..i].trim().to_string(), i + 1, false));
                boundary = i + 1;
            }
            '}' => {
                if let Some((sel, body, nested)) = stack.pop()
                    && !nested
                {
                    out.push((sel, css[body..i].to_string()));
                }
                boundary = i + 1;
            }
            ';' => boundary = i + 1,
            _ => {}
        }
    }
    out
}

fn hex(v: &str) -> String {
    let h = v.trim_start_matches('#').to_ascii_lowercase();
    if h.len() == 3 {
        format!("#{}", h.chars().flat_map(|c| [c, c]).collect::<String>())
    } else {
        format!("#{h}")
    }
}

fn declarations(selector: &str) -> Theme {
    blocks(TOKENS)
        .into_iter()
        .filter(|(s, _)| s == selector)
        .flat_map(|(_, body)| {
            body.split(';')
                .filter_map(|d| {
                    let (k, v) = d.split_once(':')?;
                    let (k, v) = (k.trim(), v.trim());
                    (k.starts_with("--") && v.starts_with('#')).then(|| (k.to_owned(), hex(v)))
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn themes() -> (Theme, Theme) {
    let light = declarations(":root");
    let mut dark = light.clone();
    dark.extend(declarations(DARK_ATTR));
    (light, dark)
}

fn luminance(hex: &str) -> f64 {
    let h = hex.trim_start_matches('#');
    let channel = |i: usize| {
        let c = f64::from(u8::from_str_radix(&h[i..i + 2], 16).unwrap()) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(0) + 0.7152 * channel(2) + 0.0722 * channel(4)
}

fn contrast(a: &str, b: &str) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn check(theme: &Theme, name: &str, fg: &str, bg: &str, floor: f64, failures: &mut Vec<String>) {
    let a = &theme[&format!("--color-{fg}")];
    let b = &theme[&format!("--color-{bg}")];
    let ratio = contrast(a, b);
    if ratio < floor {
        failures.push(format!("{name}: {fg} on {bg} is {ratio:.2}, needs {floor}"));
    }
}

#[test]
fn every_documented_pair_meets_its_floor_in_both_themes() {
    let (light, dark) = themes();
    let mut failures = Vec::new();
    for (name, theme) in [("light", &light), ("dark", &dark)] {
        for bg in BACKGROUNDS {
            check(theme, name, "fg-primary", bg, 7.0, &mut failures);
            for fg in ["fg-secondary", "fg-muted", "accent", "danger", "warning", "success"] {
                check(theme, name, fg, bg, 4.5, &mut failures);
            }
            check(theme, name, "border-control", bg, 3.0, &mut failures);
        }
        check(theme, name, "accent-fg", "accent", 4.5, &mut failures);
        for bg in ["bg-hover", "bg-active"] {
            check(theme, name, "fg-secondary", bg, 4.5, &mut failures);
        }
        check(theme, name, "due", "bg-base", 4.5, &mut failures);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn no_background_or_body_text_is_pure_black_or_white() {
    let (light, dark) = themes();
    for theme in [&light, &dark] {
        for token in ["--color-bg-base", "--color-bg-surface", "--color-fg-primary"] {
            assert!(
                !["#000000", "#ffffff"].contains(&theme[token].as_str()),
                "{token} is {}",
                theme[token]
            );
        }
    }
}

#[test]
fn the_dark_block_is_identical_under_media_query_and_data_theme() {
    let media = declarations(DARK_MEDIA);
    assert!(!media.is_empty());
    assert_eq!(media, declarations(DARK_ATTR));
}
