//! The fixed icon vocabulary: one meaning per icon, one icon per meaning.

macro_rules! icons {
    ($($variant:ident => $file:literal),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Icon { $($variant),* }

        impl Icon {
            pub const ALL: &'static [Icon] = &[$(Icon::$variant),*];

            pub fn file(self) -> &'static str {
                match self { $(Icon::$variant => $file),* }
            }

            pub fn by_file(name: &str) -> Option<Icon> {
                match name { $($file => Some(Icon::$variant),)* _ => None }
            }

            fn raw(self) -> &'static str {
                match self { $(Icon::$variant => include_str!(concat!("../../assets/icons/", $file, ".svg"))),* }
            }
        }
    };
}

icons! {
    Copy => "copy", Check => "check", FilterTo => "filter", Exclude => "filter-x", Pin => "pin",
    Archive => "archive", Delete => "trash-2", Restore => "undo-2", Edit => "pencil", Share => "share-2",
    Download => "download", Search => "search", Settings => "settings", Close => "x", New => "plus",
    History => "history", Link => "link", Tag => "tag", Show => "eye", Hide => "eye-off",
    ThemeLight => "sun", ThemeDark => "moon", ThemeSystem => "monitor", TextSize => "a-large-small",
    Info => "info", Success => "circle-check", Warning => "triangle-alert", Danger => "circle-x",
    More => "ellipsis", ChevronRight => "chevron-right", ChevronDown => "chevron-down", Notes => "notebook",
    Folder => "folder", Account => "user", SignOut => "log-out", Panel => "panel-left", Read => "book-open",
    Key => "key-round", List => "list", Dictate => "mic", Due => "alarm-clock", Attach => "paperclip",
}

impl Icon {
    /// Inline SVG at 14 (rows), 16 (toolbars) or 20 (tab bar) pixels.
    pub fn svg(self, size: u8) -> String {
        let class = match size {
            0..=14 => "oc-icon oc-icon--row",
            15..=17 => "oc-icon",
            _ => "oc-icon oc-icon--tab",
        };
        self.raw().trim_end().replacen(
            "class=\"oc-icon\"",
            &format!("class=\"{class}\" data-icon=\"{}\"", self.file()),
            1,
        )
    }
}

/// An icon from the vocabulary by its Lucide name; templates call this.
pub fn icon(name: &str, size: u8) -> askama::filters::Safe<String> {
    askama::filters::Safe(Icon::by_file(name).map(|i| i.svg(size)).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_is_an_inline_svg_that_hides_itself_from_readers() {
        for i in Icon::ALL {
            let svg = i.svg(16);
            assert!(svg.starts_with("<svg"), "{}", i.file());
            assert!(svg.contains("aria-hidden=\"true\""), "{}", i.file());
            assert!(svg.contains("currentColor"), "{}", i.file());
        }
    }

    #[test]
    fn a_name_outside_the_vocabulary_draws_nothing() {
        assert!(icon("sparkles", 16).0.is_empty());
    }
}
