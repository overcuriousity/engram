//! The language a model is instructed in.
//!
//! Ten languages and English behind them. The set is not new here: it is the
//! one `core::moments::PROTOTYPES` already teaches the capture box in, and
//! keeping the two in step matters more than either list being ideal — a base
//! that shows a German a German example of a reminder and then writes his
//! artifacts in English is one program disagreeing with itself.
//!
//! What this is *for* is the system prompt. Telling an English instruction to
//! answer in German is a request a 9B model grants for a paragraph and then
//! quietly stops granting; asking it in German is not a request at all. So the
//! instruction is translated and the hint is gone.
//!
//! What is deliberately **not** translated, in any of the ten: the JSON shape,
//! the field names, the `category` values, and the `----- INPUT -----` markers
//! `prompt::user_prompt` writes around the text. Those are the contract between
//! the prompt and the parser that reads its reply, they are compared byte for
//! byte, and a translated key is a parse failure that looks exactly like a bad
//! model. The prose around them is what changes.

/// One of the ten, or English.
///
/// `Default` is English, and every road that does not end at one of the ten
/// ends there: an unknown tag, an empty header, a stored value from a build
/// that knew a language this one does not.
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
    /// Every language, English first — the order the settings page lists them
    /// in, and the order a test walks them in.
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

    /// The ISO 639-1 tag, which is what is stored and what a header carries.
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

    /// The language's name in itself, for the one control that offers the
    /// choice. A list that says "German" to somebody who is looking for
    /// "Deutsch" is a list they have to translate before they can read it.
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

    /// A stored or submitted tag, or `None` for anything else.
    ///
    /// `None` rather than English, because the callers mean different things
    /// by an unrecognised value: a settings form must refuse it, and a stored
    /// column must fall back. Only one of those is this function's business.
    pub fn parse(tag: &str) -> Option<Lang> {
        let t = tag.trim().to_ascii_lowercase();
        Lang::ALL.into_iter().find(|l| l.tag() == t)
    }

    /// The language of an `Accept-Language` header.
    ///
    /// Only the primary subtag of the first entry is read: `de-DE,de;q=0.9,
    /// en;q=0.8` is a reader who wants German, and weighing the rest to
    /// discover that is arithmetic for nothing. Anything outside the ten is
    /// English, which is the fallback everywhere else too.
    pub fn from_accept_language(header: &str) -> Lang {
        Lang::parse(&primary_subtag(header)).unwrap_or_default()
    }
}

/// The primary subtag of an `Accept-Language` header's first entry, lowercased.
///
/// Split out because `core::moments::examples_for` reads the same header for
/// the same reason, and two parsers over one header is two places for a
/// `de-DE` to stop being German.
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

/// The language a corpus was captured in, off the metadata a door stamped.
///
/// Absent is English, and absent covers three real cases: every corpus stored
/// before this key existed, every door that knows no language — the API, MCP,
/// a fetch — where the account setting did not answer either, and a stored tag
/// from a build that knew a language this one does not.
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
        // Outside the ten, and nothing at all: both English.
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
        // A tag this build does not know, and a tag of the wrong type: both
        // are the fallback, never a panic.
        assert_eq!(of_corpus(&serde_json::json!({"lang": "ja"})), Lang::En);
        assert_eq!(of_corpus(&serde_json::json!({"lang": 7})), Lang::En);
    }

    /// The ten are the ten the capture box already teaches in. They drift
    /// apart the moment one list is edited alone, and what that looks like is
    /// a German shown a German example and handed English artifacts.
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
