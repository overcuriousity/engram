//! Notices delivered out of band into `#oc-notices`.

use crate::web::icons::Icon;

fn escape(s: &str) -> String {
    askama::filters::escape(s, askama::filters::Html)
        .map(|e| e.to_string())
        .unwrap_or_default()
}

fn notice(kind: &str, icon: Icon, text: &str, action: &str, autohide: bool) -> String {
    let hide = if autohide {
        format!(" data-oc-autohide=\"{}\"", crate::core::UNDO_MS)
    } else {
        String::new()
    };
    format!(
        "<div hx-swap-oob=\"afterbegin:#oc-notices\"><div class=\"oc-notice oc-notice--{kind}\" role=\"status\"{hide}>{}<span class=\"oc-notice__text\">{}</span>{action}</div></div>",
        icon.svg(16),
        escape(text)
    )
}

/// "Deleted 'X' · Undo", gone after six seconds unless hovered.
pub fn undo(text: &str, restore_url: &str) -> String {
    let button = format!(
        "<button type=\"button\" class=\"oc-btn\" hx-post=\"{}\" hx-swap=\"none\">Undo</button>",
        escape(restore_url)
    );
    notice(
        "success",
        Icon::Success,
        &format!("{text} ·"),
        &button,
        true,
    )
}

/// A danger notice in place, not out of band.
pub fn inline_danger(text: &str) -> String {
    format!(
        "<div class=\"oc-notice oc-notice--danger\" role=\"alert\">{}<span class=\"oc-notice__text\">{}</span></div>",
        Icon::Danger.svg(16),
        escape(text)
    )
}

pub fn success(text: &str) -> String {
    notice("success", Icon::Success, text, "", true)
}

pub fn danger(text: &str) -> String {
    notice("danger", Icon::Danger, text, "", false)
}

/// How a deleted item is named: its own title, or the first words of its own text.
pub fn name_of(title: Option<&str>, text: &str) -> String {
    if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
        return t.to_string();
    }
    let words: Vec<&str> = text.split_whitespace().take(6).collect();
    let more = text.split_whitespace().count() > words.len();
    format!("{}{}", words.join(" "), if more { "…" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nameless_artifact_is_named_by_its_own_first_words() {
        assert_eq!(
            name_of(None, "Mount the loop image with losetup then run fsck"),
            "Mount the loop image with losetup…"
        );
        assert_eq!(name_of(Some("  "), "short text"), "short text");
        assert_eq!(name_of(Some("Volumes"), "ignored"), "Volumes");
    }

    #[test]
    fn the_undo_notice_escapes_the_name_and_hides_itself() {
        let n = undo("Deleted '<b>'", "/ui/artifacts/1/restore");
        assert!(n.contains("Deleted &#39;&#60;b&#62;&#39; ·"), "{n}");
        assert!(n.contains("data-oc-autohide=\"6000\""), "{n}");
        assert!(n.contains("hx-post=\"/ui/artifacts/1/restore\""), "{n}");
    }
}
