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

use chrono::{Datelike, NaiveDate, TimeZone, Weekday};
use chrono_tz::Tz;
use std::sync::LazyLock;

pub const DEFAULT_HOUR: u32 = 9;

pub fn zone(name: Option<&str>) -> Tz {
    name.and_then(|n| n.parse::<Tz>().ok()).unwrap_or(Tz::UTC)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub at: i64,
    /// The date as it stood in the text, without any time that followed it.
    pub span: String,
}

/// Month names and their usual abbreviations, lowercase, ten languages. A
/// word of three letters or more that is a prefix of one of these names is
/// that month: `sept`, `septembre`, `september` and `сент` all reach it.
const MONTHS: [&[&str]; 12] = [
    &["january", "januar", "janvier", "enero", "janeiro", "gennaio", "januari", "stycznia", "styczeń", "ocak", "января", "январь"],
    &["february", "februar", "février", "fevrier", "febrero", "fevereiro", "febbraio", "februari", "lutego", "luty", "şubat", "февраля", "февраль"],
    &["march", "märz", "maerz", "mars", "marzo", "março", "marco", "maart", "marca", "marzec", "mart", "марта", "март"],
    &["april", "avril", "abril", "aprile", "kwietnia", "kwiecień", "nisan", "апреля", "апрель"],
    &["may", "mai", "mayo", "maio", "maggio", "mei", "maja", "maj", "mayıs", "мая", "май"],
    &["june", "juni", "juin", "junio", "junho", "giugno", "czerwca", "czerwiec", "haziran", "июня", "июнь"],
    &["july", "juli", "juillet", "julio", "julho", "luglio", "lipca", "lipiec", "temmuz", "июля", "июль"],
    &["august", "août", "aout", "agosto", "augustus", "sierpnia", "sierpień", "ağustos", "августа", "август"],
    &["september", "septembre", "septiembre", "setembro", "settembre", "września", "wrzesień", "eylül", "сентября", "сентябрь"],
    &["october", "oktober", "octobre", "octubre", "outubro", "ottobre", "października", "październik", "ekim", "октября", "октябрь"],
    &["november", "novembre", "noviembre", "novembro", "listopada", "listopad", "kasım", "ноября", "ноябрь"],
    &["december", "dezember", "décembre", "decembre", "diciembre", "dezembro", "dicembre", "grudnia", "grudzień", "aralık", "декабря", "декабрь"],
];

fn month_of(word: &str) -> Option<u32> {
    let w = word.to_lowercase();
    if w.chars().count() < 3 {
        return None;
    }
    MONTHS
        .iter()
        .position(|names| names.iter().any(|n| n.starts_with(&w)))
        .map(|i| i as u32 + 1)
}

/// No trailing `\b`: `2026-09-14T14:30` has none between the day and the `T`.
/// The caller checks that no digit follows instead.
static ISO: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b(\d{4})-(\d{2})-(\d{2})").unwrap());
/// `12.9.`, `12.09.2026`, `12/09/2026`. The leading group is the character
/// before the date, which must not be a word character, a dot or a `v`:
/// `1.21.4` and `v12.9` are versions. Group 1 is the date text.
static NUMERIC: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?:^|[^\w.])((\d{1,2})([./])(\d{1,2})(?:([./])(\d{4}))?(\.)?)").unwrap()
});
/// `12 September`, `12. Sept`, `12 de septiembre` — and, apart, `Sept 12`.
/// Two patterns rather than one alternation: the leftmost match of an
/// alternation is `meeting 12` in `meeting 12 September`, and a rejected
/// match is consumed. Group 1 is the date text, 2 the day, 3 the name.
static NAMED_DAY_FIRST: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b((\d{1,2})\.?\s+(?:de\s+|di\s+)?(\p{L}{3,}))\b").unwrap()
});
static NAMED_MONTH_FIRST: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b((\p{L}{3,})\.?\s+(\d{1,2}))\b").unwrap()
});
static YEAR_AFTER: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^\.?,?\s+(\d{4})\b").unwrap());
/// A time directly after a date: `T14:30`, ` um 10 Uhr`, ` at 3pm`, `, 14:00`.
/// Accepted only with a marker — a joining word, a colon or a suffix — so
/// `01.03.2027 5 apples` does not read the five as an hour.
static TIME_AFTER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)^(?:T|\s*(?:(um|at|à|a las|alle|om|o|saat|в|,)\s*))?(\d{1,2})(?::(\d{2}))?\s*(am|pm|uhr|h)?\b").unwrap()
});

fn stamp(tz: Tz, date: NaiveDate, hour: u32, minute: u32) -> Option<i64> {
    let dt = date.and_hms_opt(hour, minute, 0)?;
    tz.from_local_datetime(&dt)
        .single()
        .or_else(|| tz.from_local_datetime(&dt).earliest())
        .map(|d| d.timestamp())
}

/// A year-less date is the next occurrence on or after the capture date.
fn roll_forward(tz: Tz, captured_at: i64, day: u32, month: u32) -> Option<NaiveDate> {
    let today = tz.timestamp_opt(captured_at, 0).single()?.date_naive();
    match NaiveDate::from_ymd_opt(today.year(), month, day) {
        Some(d) if d >= today => Some(d),
        _ => NaiveDate::from_ymd_opt(today.year() + 1, month, day),
    }
}

/// `(hour, minute)` read from the text right after a date, or the default.
fn time_after(rest: &str) -> (u32, u32) {
    let Some(c) = TIME_AFTER.captures(rest) else { return (DEFAULT_HOUR, 0) };
    let marked = c.get(1).is_some() || c.get(3).is_some() || c.get(4).is_some() || rest.starts_with('T');
    if !marked {
        return (DEFAULT_HOUR, 0);
    }
    let mut hour: u32 = c[2].parse().unwrap_or(DEFAULT_HOUR);
    let suffix = c.get(4).map(|m| m.as_str().to_lowercase());
    if suffix.as_deref() == Some("pm") && hour < 12 {
        hour += 12;
    }
    if suffix.as_deref() == Some("am") && hour == 12 {
        hour = 0;
    }
    if hour > 23 {
        return (DEFAULT_HOUR, 0);
    }
    (hour, c.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(0))
}

pub fn absolute_dates(text: &str, captured_at: i64, tz: Tz, month_first: bool) -> Vec<Found> {
    // (start, end, date, span) — ranges so a later pattern never re-reads a
    // date an earlier one took.
    let mut out: Vec<(usize, usize, Found)> = Vec::new();
    let taken = |out: &Vec<(usize, usize, Found)>, s: usize, e: usize| out.iter().any(|(a, b, _)| s < *b && e > *a);

    for c in ISO.captures_iter(text) {
        let m = c.get(0).unwrap();
        if text[m.end()..].chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
            continue;
        }
        let (y, mo, d) = (c[1].parse().unwrap_or(0), c[2].parse().unwrap_or(0), c[3].parse().unwrap_or(0));
        let Some(date) = NaiveDate::from_ymd_opt(y, mo, d) else { continue };
        let (h, mi) = time_after(&text[m.end()..]);
        if let Some(at) = stamp(tz, date, h, mi) {
            out.push((m.start(), m.end(), Found { at, span: m.as_str().to_string() }));
        }
    }
    for c in NUMERIC.captures_iter(text) {
        let m = c.get(1).unwrap();
        if taken(&out, m.start(), m.end()) {
            continue;
        }
        let year = c.get(6).and_then(|y| y.as_str().parse::<i32>().ok());
        let sep = &c[3];
        if let Some(sep2) = c.get(5) && sep2.as_str() != sep {
            continue;
        }
        // A trailing dot or a year is what makes `12.9` a date and not a number.
        if year.is_none() && c.get(7).is_none() {
            continue;
        }
        let a: u32 = c[2].parse().unwrap_or(0);
        let b: u32 = c[4].parse().unwrap_or(0);
        let (day, month) = if sep == "/" && month_first { (b, a) } else { (a, b) };
        let date = match year {
            Some(y) => NaiveDate::from_ymd_opt(y, month, day),
            None => roll_forward(tz, captured_at, day, month),
        };
        let Some(date) = date else { continue };
        let (h, mi) = time_after(&text[m.end()..]);
        if let Some(at) = stamp(tz, date, h, mi) {
            out.push((m.start(), m.end(), Found { at, span: m.as_str().to_string() }));
        }
    }
    let named = NAMED_DAY_FIRST
        .captures_iter(text)
        .map(|c| (c.get(1).unwrap(), c.get(2).unwrap(), c.get(3).unwrap()))
        .chain(NAMED_MONTH_FIRST.captures_iter(text).map(|c| (c.get(1).unwrap(), c.get(3).unwrap(), c.get(2).unwrap())))
        .collect::<Vec<_>>();
    for (m, day, name) in named {
        if taken(&out, m.start(), m.end()) {
            continue;
        }
        let (day, name) = (day.as_str(), name.as_str());
        let Some(month) = month_of(name) else { continue };
        let day: u32 = day.parse().unwrap_or(0);
        let mut end = m.end();
        let year = YEAR_AFTER.captures(&text[end..]).map(|y| {
            end += y.get(0).unwrap().end();
            y[1].parse::<i32>().unwrap_or(0)
        });
        let date = match year {
            Some(y) => NaiveDate::from_ymd_opt(y, month, day),
            None => roll_forward(tz, captured_at, day, month),
        };
        let Some(date) = date else { continue };
        let (h, mi) = time_after(&text[end..]);
        if let Some(at) = stamp(tz, date, h, mi) {
            out.push((m.start(), end, Found { at, span: m.as_str().to_string() }));
        }
    }
    out.sort_by_key(|(s, _, _)| *s);
    out.into_iter().map(|(_, _, f)| f).collect()
}

/// (word, days ahead) for the bare relative words; weekdays are handled apart.
const RELATIVE: &[(&str, i64)] = &[
    ("tomorrow", 1), ("morgen", 1), ("demain", 1), ("mañana", 1), ("amanhã", 1), ("domani", 1),
    ("jutro", 1), ("yarın", 1), ("завтра", 1),
    ("day after tomorrow", 2), ("übermorgen", 2), ("après-demain", 2), ("pasado mañana", 2),
    ("depois de amanhã", 2), ("dopodomani", 2), ("overmorgen", 2), ("pojutrze", 2),
    ("öbür gün", 2), ("послезавтра", 2),
];

const WEEKDAYS: &[(&str, Weekday)] = &[
    ("monday", Weekday::Mon), ("montag", Weekday::Mon), ("lundi", Weekday::Mon), ("lunes", Weekday::Mon),
    ("segunda", Weekday::Mon), ("lunedì", Weekday::Mon), ("maandag", Weekday::Mon), ("poniedziałek", Weekday::Mon),
    ("pazartesi", Weekday::Mon), ("понедельник", Weekday::Mon),
    ("tuesday", Weekday::Tue), ("dienstag", Weekday::Tue), ("mardi", Weekday::Tue), ("martes", Weekday::Tue),
    ("terça", Weekday::Tue), ("martedì", Weekday::Tue), ("dinsdag", Weekday::Tue), ("wtorek", Weekday::Tue),
    ("salı", Weekday::Tue), ("вторник", Weekday::Tue),
    ("wednesday", Weekday::Wed), ("mittwoch", Weekday::Wed), ("mercredi", Weekday::Wed), ("miércoles", Weekday::Wed),
    ("quarta", Weekday::Wed), ("mercoledì", Weekday::Wed), ("woensdag", Weekday::Wed), ("środa", Weekday::Wed),
    ("çarşamba", Weekday::Wed), ("среда", Weekday::Wed),
    ("thursday", Weekday::Thu), ("donnerstag", Weekday::Thu), ("jeudi", Weekday::Thu), ("jueves", Weekday::Thu),
    ("quinta", Weekday::Thu), ("giovedì", Weekday::Thu), ("donderdag", Weekday::Thu), ("czwartek", Weekday::Thu),
    ("perşembe", Weekday::Thu), ("четверг", Weekday::Thu),
    ("friday", Weekday::Fri), ("freitag", Weekday::Fri), ("vendredi", Weekday::Fri), ("viernes", Weekday::Fri),
    ("sexta", Weekday::Fri), ("venerdì", Weekday::Fri), ("vrijdag", Weekday::Fri), ("piątek", Weekday::Fri),
    ("cuma", Weekday::Fri), ("пятница", Weekday::Fri),
    ("saturday", Weekday::Sat), ("samstag", Weekday::Sat), ("samedi", Weekday::Sat), ("sábado", Weekday::Sat),
    ("sabato", Weekday::Sat), ("zaterdag", Weekday::Sat), ("sobota", Weekday::Sat), ("cumartesi", Weekday::Sat),
    ("суббота", Weekday::Sat),
    ("sunday", Weekday::Sun), ("sonntag", Weekday::Sun), ("dimanche", Weekday::Sun), ("domingo", Weekday::Sun),
    ("domenica", Weekday::Sun), ("zondag", Weekday::Sun), ("niedziela", Weekday::Sun), ("pazar", Weekday::Sun),
    ("воскресенье", Weekday::Sun),
];

/// `in 3 days`, `in 2 Wochen`, `dans 3 jours`, `за 3 дня`, `tra 2 settimane`, …
static IN_N: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b(\d{1,3})\s+(days?|tag(?:e|en)?|jours?|d[ií]as?|giorn[oi]|dag(?:en)?|dni|gün|дня|дней|weeks?|wochen?|semaines?|semanas?|settimane?|weken|tygodni(?:e)?|hafta|недел[иья])\b").unwrap()
});
static AT_TIME: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b(?:at|um|à|a las|alle|om|o|saat|в)\s+(\d{1,2})(?::(\d{2}))?\s*(am|pm|uhr|h)?\b").unwrap()
});

/// The nearest future date the relative words name, or none. A weekday, with
/// or without "next", is the next such day strictly after today.
pub fn relative_date(text: &str, captured_at: i64, tz: Tz) -> Option<Found> {
    let lower = text.to_lowercase();
    let today = tz.timestamp_opt(captured_at, 0).single()?.date_naive();
    let (h, mi) = match AT_TIME.find(&lower) {
        Some(m) => time_after(&lower[m.start()..]),
        None => (DEFAULT_HOUR, 0),
    };
    let mut best: Option<(NaiveDate, String)> = None;
    let mut consider = |d: NaiveDate, span: &str| {
        if d > today && best.as_ref().is_none_or(|(b, _)| d < *b) {
            best = Some((d, span.to_string()));
        }
    };
    // Longest words first, so "day after tomorrow" beats "tomorrow".
    let mut rel: Vec<&(&str, i64)> = RELATIVE.iter().collect();
    rel.sort_by_key(|(w, _)| std::cmp::Reverse(w.len()));
    for (word, days) in rel {
        if contains_cue(&lower, word, false) {
            consider(today + chrono::Duration::days(*days), word);
            break;
        }
    }
    for (word, wd) in WEEKDAYS {
        if contains_cue(&lower, word, false) {
            let mut d = today + chrono::Duration::days(1);
            while d.weekday() != *wd {
                d += chrono::Duration::days(1);
            }
            consider(d, word);
        }
    }
    if let Some(c) = IN_N.captures(&lower) {
        let n: i64 = c[1].parse().ok()?;
        let unit = c[2].to_lowercase();
        let weeks = ["w", "sem", "sett", "tyg", "haf", "нед"].iter().any(|p| unit.starts_with(p));
        consider(today + chrono::Duration::days(if weeks { n * 7 } else { n }), &c[0]);
    }
    let (date, span) = best?;
    Some(Found { at: stamp(tz, date, h, mi)?, span })
}

/// The RRULE subset: FREQ, INTERVAL, BYDAY (weekday codes), BYMONTHDAY, and
/// UNTIL or COUNT. Spelled as iCalendar so a feed can carry it later; parsed
/// here and nowhere else.
#[derive(Debug)]
struct Rule {
    freq: Freq,
    interval: u32,
    by_day: Vec<Weekday>,
    by_month_day: Option<u32>,
    until: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

fn parse_rule(rule: &str) -> Result<Rule, String> {
    let mut r = Rule { freq: Freq::Daily, interval: 1, by_day: vec![], by_month_day: None, until: None };
    let mut has_freq = false;
    for part in rule.split(';').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=').ok_or_else(|| format!("not key=value: {part}"))?;
        match k {
            "FREQ" => {
                r.freq = match v {
                    "DAILY" => Freq::Daily,
                    "WEEKLY" => Freq::Weekly,
                    "MONTHLY" => Freq::Monthly,
                    "YEARLY" => Freq::Yearly,
                    other => return Err(format!("FREQ={other} is outside the subset")),
                };
                has_freq = true;
            }
            "INTERVAL" => {
                r.interval = v.parse::<u32>().ok().filter(|n| *n >= 1).ok_or("INTERVAL must be a positive integer")?
            }
            "BYDAY" => {
                for d in v.split(',') {
                    r.by_day.push(match d {
                        "MO" => Weekday::Mon,
                        "TU" => Weekday::Tue,
                        "WE" => Weekday::Wed,
                        "TH" => Weekday::Thu,
                        "FR" => Weekday::Fri,
                        "SA" => Weekday::Sat,
                        "SU" => Weekday::Sun,
                        other => return Err(format!("BYDAY={other}: weekday codes only, no ordinals")),
                    });
                }
            }
            "BYMONTHDAY" => {
                r.by_month_day =
                    Some(v.parse::<u32>().ok().filter(|n| (1..=31).contains(n)).ok_or("BYMONTHDAY out of range")?)
            }
            "UNTIL" => {
                let dt = chrono::NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%SZ")
                    .or_else(|_| {
                        NaiveDate::parse_from_str(v, "%Y%m%d").map(|d| d.and_hms_opt(23, 59, 59).unwrap())
                    })
                    .map_err(|_| format!("UNTIL={v} is not a date"))?;
                r.until = Some(dt.and_utc().timestamp());
            }
            "COUNT" => {
                v.parse::<u32>().map_err(|_| "COUNT must be an integer")?;
            }
            other => return Err(format!("{other} is outside the subset")),
        }
    }
    if !has_freq {
        return Err("FREQ is required".into());
    }
    Ok(r)
}

pub fn validate_rule(rule: &str) -> Result<(), String> {
    parse_rule(rule).map(|_| ())
}

/// The next occurrence strictly after `at`, keeping `at`'s wall-clock time in
/// `tz`. None when the rule is exhausted or invalid. COUNT is the caller's to
/// enforce by counting rows.
pub fn next_after(rule: &str, at: i64, tz: Tz) -> Option<i64> {
    let r = parse_rule(rule).ok()?;
    let start = tz.timestamp_opt(at, 0).single()?;
    let time = start.time();
    let origin = start.date_naive();
    let mut date = origin;
    // Day by day, bounded: the subset never needs more than four years of
    // days per interval step to find the next occurrence (a yearly 29 Feb).
    for _ in 0..(366 * 4 * r.interval as usize + 1) {
        date += chrono::Duration::days(1);
        let hit = match r.freq {
            Freq::Daily => (date - origin).num_days() % r.interval as i64 == 0,
            Freq::Weekly => {
                let own = [origin.weekday()];
                let days: &[Weekday] = if r.by_day.is_empty() { &own } else { &r.by_day };
                let weeks = (date - origin).num_days().div_euclid(7);
                days.contains(&date.weekday()) && weeks % r.interval as i64 == 0
            }
            Freq::Monthly => {
                let dom = r.by_month_day.unwrap_or(origin.day());
                let months = (date.year() - origin.year()) * 12 + (date.month() as i32 - origin.month() as i32);
                date.day() == dom && months % r.interval as i32 == 0
            }
            Freq::Yearly => {
                date.month() == origin.month()
                    && date.day() == origin.day()
                    && (date.year() - origin.year()) % r.interval as i32 == 0
            }
        };
        if !hit {
            continue;
        }
        let dt = date.and_time(time);
        let Some(next) = tz.from_local_datetime(&dt).single().or_else(|| tz.from_local_datetime(&dt).earliest()) else {
            continue;
        };
        let ts = next.timestamp();
        if let Some(until) = r.until
            && ts > until
        {
            return None;
        }
        return Some(ts);
    }
    None
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

    use chrono::TimeZone;
    fn berlin() -> chrono_tz::Tz {
        chrono_tz::Tz::Europe__Berlin
    }
    /// 2026-08-30 12:00 Berlin, a Sunday.
    fn captured() -> i64 {
        berlin().with_ymd_and_hms(2026, 8, 30, 12, 0, 0).unwrap().timestamp()
    }
    fn local(at: i64) -> String {
        berlin().timestamp_opt(at, 0).unwrap().format("%Y-%m-%d %H:%M").to_string()
    }

    #[test]
    fn iso_dates_with_and_without_a_time() {
        let f = absolute_dates("Deadline 2026-09-12, call 2026-09-14T14:30", captured(), berlin(), false);
        assert_eq!(f.len(), 2);
        assert_eq!(local(f[0].at), "2026-09-12 09:00");
        assert_eq!(f[0].span, "2026-09-12");
        assert_eq!(local(f[1].at), "2026-09-14 14:30");
    }

    #[test]
    fn day_first_numerics_and_a_yearless_date_rolls_forward() {
        let f = absolute_dates("Zahnarzt 12.9. um 10 Uhr, Rechnung 01.03.2027", captured(), berlin(), false);
        assert_eq!(local(f[0].at), "2026-09-12 10:00");
        assert_eq!(f[0].span, "12.9.");
        assert_eq!(local(f[1].at), "2027-03-01 09:00");
        let g = absolute_dates("Party am 12.7.", captured(), berlin(), false);
        assert_eq!(local(g[0].at), "2027-07-12 09:00");
    }

    #[test]
    fn a_slash_date_reads_month_first_only_when_told_to() {
        let dmy = absolute_dates("due 9/12/2026", captured(), berlin(), false);
        assert_eq!(local(dmy[0].at), "2026-12-09 09:00");
        let mdy = absolute_dates("due 9/12/2026", captured(), berlin(), true);
        assert_eq!(local(mdy[0].at), "2026-09-12 09:00");
    }

    #[test]
    fn month_names_in_several_languages() {
        for (text, want) in [
            ("meeting Sept 12", "2026-09-12 09:00"),
            ("meeting 12 September 2026 at 3pm", "2026-09-12 15:00"),
            ("réunion le 12 septembre", "2026-09-12 09:00"),
            ("Termin am 3. Okt", "2026-10-03 09:00"),
            ("spotkanie 5 października", "2026-10-05 09:00"),
            ("встреча 7 ноября", "2026-11-07 09:00"),
        ] {
            let f = absolute_dates(text, captured(), berlin(), false);
            assert_eq!(f.first().map(|x| local(x.at)).as_deref(), Some(want), "{text}");
        }
    }

    #[test]
    fn bare_numbers_versions_and_list_markers_are_not_dates() {
        for text in ["qdrant 1.21.4", "step 1. then 2.", "12.9 is the score", "port 8080", "v12.9"] {
            assert!(absolute_dates(text, captured(), berlin(), false).is_empty(), "{text}");
        }
    }

    #[test]
    fn an_impossible_date_is_skipped() {
        assert!(absolute_dates("on 31.02.2027", captured(), berlin(), false).is_empty());
    }

    #[test]
    fn relative_words_in_several_languages() {
        for (text, want) in [
            ("remind me tomorrow", "2026-08-31 09:00"),
            ("erinnere mich übermorgen", "2026-09-01 09:00"),
            ("next monday", "2026-08-31 09:00"),
            ("nächsten Freitag", "2026-09-04 09:00"),
            ("in 3 days", "2026-09-02 09:00"),
            ("in 2 Wochen", "2026-09-13 09:00"),
            ("dans 3 jours", "2026-09-02 09:00"),
            ("за 3 дня", "2026-09-02 09:00"),
            ("tomorrow at 14:00", "2026-08-31 14:00"),
        ] {
            let f = relative_date(text, captured(), berlin());
            assert_eq!(f.map(|x| local(x.at)).as_deref(), Some(want), "{text}");
        }
        assert!(relative_date("nothing here", captured(), berlin()).is_none());
    }

    #[test]
    fn an_unknown_zone_falls_back_to_utc() {
        assert_eq!(zone(Some("Mars/Olympus")), chrono_tz::Tz::UTC);
        assert_eq!(zone(None), chrono_tz::Tz::UTC);
        assert_eq!(zone(Some("Europe/Berlin")), berlin());
    }

    #[test]
    fn the_subset_is_accepted_and_the_rest_refused() {
        for ok in [
            "FREQ=DAILY",
            "FREQ=WEEKLY;BYDAY=MO,WE",
            "FREQ=MONTHLY;BYMONTHDAY=1",
            "FREQ=YEARLY",
            "FREQ=WEEKLY;INTERVAL=2;UNTIL=20271231T000000Z",
            "FREQ=DAILY;COUNT=5",
        ] {
            assert!(validate_rule(ok).is_ok(), "{ok}");
        }
        for bad in ["FREQ=HOURLY", "FREQ=WEEKLY;BYDAY=2MO", "BYDAY=MO", "FREQ=WEEKLY;BYSETPOS=1", ""] {
            assert!(validate_rule(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn weekly_keeps_the_wall_clock_across_dst() {
        // Monday 2026-10-19 09:00 CEST → Monday 2026-10-26 09:00 CET (DST ends 25 Oct).
        let at = berlin().with_ymd_and_hms(2026, 10, 19, 9, 0, 0).unwrap().timestamp();
        let next = next_after("FREQ=WEEKLY;BYDAY=MO", at, berlin()).unwrap();
        assert_eq!(local(next), "2026-10-26 09:00");
        assert_eq!(next - at, 7 * 86_400 + 3_600, "one week and the hour DST gave back");
    }

    #[test]
    fn byday_picks_the_next_listed_day() {
        let mon = berlin().with_ymd_and_hms(2026, 8, 31, 9, 0, 0).unwrap().timestamp();
        assert_eq!(local(next_after("FREQ=WEEKLY;BYDAY=MO,WE", mon, berlin()).unwrap()), "2026-09-02 09:00");
    }

    #[test]
    fn monthly_on_the_31st_skips_short_months() {
        let at = berlin().with_ymd_and_hms(2026, 8, 31, 9, 0, 0).unwrap().timestamp();
        assert_eq!(local(next_after("FREQ=MONTHLY;BYMONTHDAY=31", at, berlin()).unwrap()), "2026-10-31 09:00");
    }

    #[test]
    fn until_ends_the_rule() {
        let at = berlin().with_ymd_and_hms(2026, 8, 31, 9, 0, 0).unwrap().timestamp();
        assert!(next_after("FREQ=DAILY;UNTIL=20260831T235959Z", at, berlin()).is_none());
        assert!(next_after("FREQ=DAILY;UNTIL=20260901T235959Z", at, berlin()).is_some());
    }

    #[test]
    fn interval_and_yearly() {
        let at = berlin().with_ymd_and_hms(2026, 8, 31, 9, 0, 0).unwrap().timestamp();
        assert_eq!(local(next_after("FREQ=DAILY;INTERVAL=3", at, berlin()).unwrap()), "2026-09-03 09:00");
        assert_eq!(local(next_after("FREQ=YEARLY", at, berlin()).unwrap()), "2027-08-31 09:00");
    }
}
