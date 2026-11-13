//! The language a model is instructed in, the ten of `core::moments::PROTOTYPES` and English.
//! JSON shape, field names, `category` values and INPUT markers stay untranslated: the parser compares them byte for byte.

/// `Default` is English, the fallback for unknown tags, empty headers and values from newer builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    En,
    De,
    Es,
    Fr,
    It,
    Nl,
    Pl,
    Pt,
    Ru,
    Tr,
}

impl Lang {
    /// English first: the settings page order.
    pub const ALL: [Lang; 10] = [
        Lang::En,
        Lang::De,
        Lang::Es,
        Lang::Fr,
        Lang::It,
        Lang::Nl,
        Lang::Pl,
        Lang::Pt,
        Lang::Ru,
        Lang::Tr,
    ];

    /// What is stored and what a header carries.
    pub fn tag(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::De => "de",
            Lang::Es => "es",
            Lang::Fr => "fr",
            Lang::It => "it",
            Lang::Nl => "nl",
            Lang::Pl => "pl",
            Lang::Pt => "pt",
            Lang::Ru => "ru",
            Lang::Tr => "tr",
        }
    }

    /// The language's name in itself, for the one control that offers the choice.
    pub fn endonym(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::De => "Deutsch",
            Lang::Es => "Español",
            Lang::Fr => "Français",
            Lang::It => "Italiano",
            Lang::Nl => "Nederlands",
            Lang::Pl => "Polski",
            Lang::Pt => "Português",
            Lang::Ru => "Русский",
            Lang::Tr => "Türkçe",
        }
    }

    /// `None` rather than English: a form must refuse it while a stored column falls back.
    pub fn parse(tag: &str) -> Option<Lang> {
        let t = tag.trim().to_ascii_lowercase();
        Lang::ALL.into_iter().find(|l| l.tag() == t)
    }

    /// Only the primary subtag of the first entry is read; anything outside the ten is English.
    pub fn from_accept_language(header: &str) -> Lang {
        Lang::parse(&primary_subtag(header)).unwrap_or_default()
    }
}

/// Shared with `core::moments::examples_for` so both read a `de-DE` the same way.
pub fn primary_subtag(header: &str) -> String {
    header
        .split(',')
        .next()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .split('-')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Absent is English: older corpora, doors without a language, and tags from newer builds.
pub fn of_corpus(metadata: &serde_json::Value) -> Lang {
    metadata["lang"]
        .as_str()
        .and_then(Lang::parse)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_resolves_to_its_first_entrys_primary_subtag() {
        assert_eq!(
            Lang::from_accept_language("de-DE,de;q=0.9,en;q=0.8"),
            Lang::De
        );
        assert_eq!(Lang::from_accept_language("pt-BR"), Lang::Pt);
        assert_eq!(Lang::from_accept_language("EN-GB"), Lang::En);
        assert_eq!(Lang::from_accept_language("ja"), Lang::En);
        assert_eq!(Lang::from_accept_language(""), Lang::En);
        assert_eq!(Lang::default(), Lang::En);
    }

    #[test]
    fn a_tag_round_trips_and_a_stranger_is_none() {
        for l in Lang::ALL {
            assert_eq!(Lang::parse(l.tag()), Some(l), "{}", l.tag());
            assert!(!l.endonym().is_empty());
        }
        assert_eq!(Lang::parse("ja"), None);
        assert_eq!(Lang::parse(""), None);
    }

    #[test]
    fn a_corpus_with_no_stamp_is_read_in_english() {
        assert_eq!(of_corpus(&serde_json::json!({})), Lang::En);
        assert_eq!(of_corpus(&serde_json::json!({"lang": "de"})), Lang::De);
        assert_eq!(of_corpus(&serde_json::json!({"lang": "ja"})), Lang::En);
        assert_eq!(of_corpus(&serde_json::json!({"lang": 7})), Lang::En);
    }

    #[test]
    fn the_ten_are_the_ten_the_example_table_carries() {
        let taught: std::collections::BTreeSet<&str> = crate::core::moments::PROTOTYPES
            .iter()
            .map(|(_, l, _)| *l)
            .collect();
        let instructed: std::collections::BTreeSet<&str> =
            Lang::ALL.iter().map(|l| l.tag()).collect();
        assert_eq!(taught, instructed);
    }
}
