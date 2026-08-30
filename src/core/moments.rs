//! Time, read out of a note. Pure functions: the cue table, the prototype
//! classifier over vectors the embed stage already paid for, the date rules,
//! and the recurrence subset. No store, no model, no clock of its own.

use crate::core::gaps::cosine;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Intent {
    Remind,
    Journal,
}

impl Intent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Intent::Remind => "remind",
            Intent::Journal => "journal",
        }
    }
}

/// How far into a note a cue counts. Past this a note is about something,
/// and "remind me" in paragraph nine is a quotation.
const OPENING_CHARS: usize = 200;

/// Lowercase, whole-word. A `journal` cue must sit at the head of the text.
pub const CUES: &[(Intent, &str)] = &[
    (Intent::Remind, "remind me"),
    (Intent::Remind, "erinnere mich"),
    (Intent::Remind, "erinner mich"),
    (Intent::Remind, "rappelle-moi"),
    (Intent::Remind, "rappelle moi"),
    (Intent::Remind, "recuérdame"),
    (Intent::Remind, "recuerdame"),
    (Intent::Remind, "lembre-me"),
    (Intent::Remind, "lembra-me"),
    (Intent::Remind, "ricordami"),
    (Intent::Remind, "herinner me"),
    (Intent::Remind, "przypomnij mi"),
    (Intent::Remind, "hatırlat"),
    (Intent::Remind, "напомни"),
    (Intent::Journal, "today i"),
    (Intent::Journal, "dear diary"),
    (Intent::Journal, "heute"),
    (Intent::Journal, "liebes tagebuch"),
    (Intent::Journal, "aujourd'hui"),
    (Intent::Journal, "hoy"),
    (Intent::Journal, "hoje"),
    (Intent::Journal, "oggi"),
    (Intent::Journal, "vandaag"),
    (Intent::Journal, "dzisiaj"),
    (Intent::Journal, "dziś"),
    (Intent::Journal, "bugün"),
    (Intent::Journal, "сегодня"),
];

/// Sentence-shaped on purpose: the embedder places "remind me to X" near other
/// requests for future action, and a bare cue word near a dictionary entry.
pub const PROTOTYPES: &[(Intent, &str)] = &[
    (Intent::Remind, "remind me to send the invoice on friday"),
    (Intent::Remind, "remind me next week to call the bank"),
    (Intent::Remind, "erinnere mich morgen an den zahnarzttermin"),
    (Intent::Remind, "erinnere mich nächste woche an die steuererklärung"),
    (Intent::Remind, "rappelle-moi d'appeler la banque lundi"),
    (Intent::Remind, "recuérdame pagar el alquiler el día uno"),
    (Intent::Remind, "lembre-me de renovar o passaporte em setembro"),
    (Intent::Remind, "ricordami di comprare i biglietti domani"),
    (Intent::Remind, "herinner me eraan om de huur te betalen"),
    (Intent::Remind, "przypomnij mi jutro o spotkaniu z lekarzem"),
    (Intent::Remind, "yarın bana faturayı ödemeyi hatırlat"),
    (Intent::Remind, "напомни мне завтра позвонить маме"),
    (Intent::Journal, "today i finally got the migration working"),
    (Intent::Journal, "long day, nothing got done, but the walk helped"),
    (Intent::Journal, "heute war ein langer tag und ich bin müde"),
    (Intent::Journal, "heute morgen endlich den fehler gefunden"),
    (Intent::Journal, "aujourd'hui j'ai enfin terminé le rapport"),
    (Intent::Journal, "hoy fue un día tranquilo, leí mucho"),
    (Intent::Journal, "hoje acordei cedo e fui correr"),
    (Intent::Journal, "oggi è stata una giornata pesante"),
    (Intent::Journal, "vandaag eindelijk de tuin gedaan"),
    (Intent::Journal, "dzisiaj byłem u dentysty, poszło dobrze"),
    (Intent::Journal, "bugün çok yorucu bir gündü"),
    (Intent::Journal, "сегодня наконец закончил проект"),
];

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// Whole-word containment on already-lowercased text; `at_start` demands the
/// cue open the text (after whitespace).
fn contains_cue(hay: &str, cue: &str, at_start: bool) -> bool {
    let hay = if at_start { hay.trim_start() } else { hay };
    let mut from = 0;
    while let Some(i) = hay[from..].find(cue) {
        let start = from + i;
        let end = start + cue.len();
        let before_ok = start == 0 || !hay[..start].chars().next_back().is_some_and(is_word_char);
        let after_ok = end == hay.len() || !hay[end..].chars().next().is_some_and(is_word_char);
        if before_ok && after_ok {
            return !at_start || start == 0;
        }
        if at_start {
            return false;
        }
        from = end;
    }
    false
}

pub fn cue(text: &str) -> Option<Intent> {
    let opening = text.chars().take(OPENING_CHARS).collect::<String>().to_lowercase();
    CUES.iter()
        .find(|(intent, c)| contains_cue(&opening, c, *intent == Intent::Journal))
        .map(|(intent, _)| *intent)
}

/// Maximum over an intent's prototypes: a note matches one phrasing, not the
/// average of ten languages.
pub fn classify(vec: &[f32], protos: &[(Intent, Vec<f32>)], line: f32) -> Option<(Intent, f32)> {
    let mut best: Option<(Intent, f32)> = None;
    for (intent, p) in protos {
        let s = cosine(vec, p);
        if s >= line && best.is_none_or(|(_, b)| s > b) {
            best = Some((*intent, s));
        }
    }
    best
}

pub const INTENT_LINE_FLOOR: f32 = 0.70;
pub const INTENT_LINE_CEILING: f32 = 0.92;
const MIN_CALIBRATION: usize = 30;
const UNRELATED_PERCENTILE: f64 = 0.99;

/// Where "an ordinary note against a prototype" stops: the 99th percentile of
/// the sampled scores, rounded up to a hundredth, clamped. Below thirty
/// samples the configured `floor` stands. `gaps::link_threshold`, applied to a
/// different question.
pub fn intent_line(protos: &[(Intent, Vec<f32>)], sample: &[Vec<f32>], floor: f32) -> f32 {
    if sample.len() < MIN_CALIBRATION || protos.is_empty() {
        return floor;
    }
    let mut scores: Vec<f32> = sample
        .iter()
        .map(|v| protos.iter().map(|(_, p)| cosine(v, p)).fold(f32::MIN, f32::max))
        .collect();
    scores.sort_by(f32::total_cmp);
    let at = ((scores.len() - 1) as f64 * UNRELATED_PERCENTILE).round() as usize;
    let measured = (scores[at] * 100.0).ceil() / 100.0;
    measured.clamp(INTENT_LINE_FLOOR, INTENT_LINE_CEILING)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remind_cue_fires_anywhere_in_the_opening() {
        assert_eq!(cue("Please remind me to send the invoice"), Some(Intent::Remind));
        assert_eq!(cue("Erinnere mich morgen an den Zahnarzt"), Some(Intent::Remind));
        assert_eq!(cue("Напомни мне позвонить"), Some(Intent::Remind));
    }

    #[test]
    fn a_journal_cue_counts_only_at_the_head_of_a_note() {
        assert_eq!(cue("Heute war ein langer Tag."), Some(Intent::Journal));
        assert_eq!(cue("Today I finally fixed the build"), Some(Intent::Journal));
        assert_eq!(cue("Das Meeting ist heute um drei"), None, "mid-sentence heute is a word");
    }

    #[test]
    fn a_cue_is_a_whole_word_and_not_a_substring() {
        assert_eq!(cue("The reminder service uses cron"), None);
        assert_eq!(cue("hoyos en la pared"), None, "hoy inside hoyos");
    }

    #[test]
    fn a_cue_past_the_opening_is_prose() {
        let late = format!("{} remind me to call", "x ".repeat(120));
        assert_eq!(cue(&late), None);
    }

    fn unit(i: usize, dim: usize) -> Vec<f32> {
        let mut v = vec![0.0; dim];
        v[i] = 1.0;
        v
    }

    #[test]
    fn the_best_intent_above_the_line_fires_by_maximum_not_mean() {
        let protos = vec![
            (Intent::Remind, unit(0, 4)),
            (Intent::Remind, unit(1, 4)),
            (Intent::Remind, unit(2, 4)),
            (Intent::Journal, unit(3, 4)),
        ];
        let note = vec![0.95, 0.05, 0.0, 0.0];
        let (intent, score) = classify(&note, &protos, 0.8).expect("fires");
        assert_eq!(intent, Intent::Remind);
        assert!(score > 0.9);
    }

    #[test]
    fn below_the_line_nothing_fires() {
        let protos = vec![(Intent::Remind, unit(0, 4))];
        assert_eq!(classify(&[0.5, 0.5, 0.5, 0.5], &protos, 0.8), None);
    }

    #[test]
    fn the_line_is_measured_from_the_sample_and_clamped() {
        let protos = vec![(Intent::Remind, unit(0, 4))];
        let sample: Vec<Vec<f32>> = (0..30).map(|_| unit(1, 4)).collect();
        assert_eq!(intent_line(&protos, &sample, 0.80), INTENT_LINE_FLOOR);
        let close: Vec<Vec<f32>> = (0..30).map(|_| vec![0.95, 0.312, 0.0, 0.0]).collect();
        assert_eq!(intent_line(&protos, &close, 0.80), INTENT_LINE_CEILING);
    }

    #[test]
    fn a_small_sample_leaves_the_configured_line_standing() {
        let protos = vec![(Intent::Remind, unit(0, 4))];
        let sample: Vec<Vec<f32>> = (0..10).map(|_| unit(1, 4)).collect();
        assert_eq!(intent_line(&protos, &sample, 0.83), 0.83);
    }

    #[test]
    fn every_language_has_a_cue_and_prototypes_for_both_intents() {
        for intent in [Intent::Remind, Intent::Journal] {
            assert!(CUES.iter().filter(|(i, _)| *i == intent).count() >= 10);
            assert!(PROTOTYPES.iter().filter(|(i, _)| *i == intent).count() >= 10);
        }
        for (_, p) in PROTOTYPES {
            assert!(p.split_whitespace().count() >= 3, "a prototype is a sentence, not a word: {p}");
        }
    }
}
