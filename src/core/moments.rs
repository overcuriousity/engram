//! Time, as the base keeps it: the intent vocabulary and its refusals, the
//! zone table, and the recurrence subset. Pure functions — no store, no
//! model, no clock of its own.
//!
//! Reading time out of prose retired in the 2026-09 capture reshape: the
//! judged synthesis call is the reader now (`jobs::judgement`), and the cue
//! tables, the prototype classifier and the date rules went with it. What
//! stays is what the judgement and the band still stand on.

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

/// Example phrasings per intent and language, ten languages. Once the
/// classifier's prototypes; now the example table the capture box teaches
/// from — see `examples_for`, which reads the first row of each pair.
///
/// The language tag is carried in the row rather than in a parallel array: a
/// tag is not recoverable from a sentence, and at eighty rows two lists kept
/// in step by counting is a defect waiting for its first careless insert.
pub const PROTOTYPES: &[(Intent, &str, &str)] = &[
    // ── en ──
    (
        Intent::Remind,
        "en",
        "remind me to send the invoice on friday",
    ),
    (Intent::Remind, "en", "remind me next week to call the bank"),
    (
        Intent::Remind,
        "en",
        "don't forget to book the train tickets tomorrow",
    ),
    (
        Intent::Remind,
        "en",
        "i need to renew my passport before the end of the month",
    ),
    (
        Intent::Journal,
        "en",
        "today i finally got the migration working",
    ),
    (
        Intent::Journal,
        "en",
        "long day, nothing got done, but the walk helped",
    ),
    (
        Intent::Journal,
        "en",
        "slept badly again, the whole afternoon was a write-off",
    ),
    (
        Intent::Journal,
        "en",
        "quiet evening, cooked properly for once and felt better for it",
    ),
    // ── de ──
    (
        Intent::Remind,
        "de",
        "erinnere mich morgen an den zahnarzttermin",
    ),
    (
        Intent::Remind,
        "de",
        "erinnere mich nächste woche an die steuererklärung",
    ),
    (
        Intent::Remind,
        "de",
        "nicht vergessen: am freitag die rechnung abschicken",
    ),
    (
        Intent::Remind,
        "de",
        "ich muss bis ende des monats den pass verlängern",
    ),
    (
        Intent::Remind,
        "de",
        "erinnerung termin foto ausweis mittwoch 0900 zimmer a323",
    ),
    (
        Intent::Journal,
        "de",
        "heute war ein langer tag und ich bin müde",
    ),
    (
        Intent::Journal,
        "de",
        "heute morgen endlich den fehler gefunden",
    ),
    (
        Intent::Journal,
        "de",
        "wieder schlecht geschlafen, der ganze nachmittag war für die katz",
    ),
    (
        Intent::Journal,
        "de",
        "ruhiger abend, endlich mal richtig gekocht und es ging mir besser",
    ),
    // ── fr ──
    (
        Intent::Remind,
        "fr",
        "rappelle-moi d'appeler la banque lundi",
    ),
    (
        Intent::Remind,
        "fr",
        "rappelle-moi la semaine prochaine de renouveler l'assurance",
    ),
    (
        Intent::Remind,
        "fr",
        "ne pas oublier d'envoyer la facture vendredi",
    ),
    (
        Intent::Remind,
        "fr",
        "je dois refaire mon passeport avant la fin du mois",
    ),
    (
        Intent::Journal,
        "fr",
        "aujourd'hui j'ai enfin terminé le rapport",
    ),
    (
        Intent::Journal,
        "fr",
        "longue journée, rien d'avancé, mais la promenade m'a fait du bien",
    ),
    (
        Intent::Journal,
        "fr",
        "encore mal dormi, tout l'après-midi est passé à côté",
    ),
    (
        Intent::Journal,
        "fr",
        "soirée tranquille, j'ai enfin cuisiné correctement",
    ),
    // ── es ──
    (
        Intent::Remind,
        "es",
        "recuérdame pagar el alquiler el día uno",
    ),
    (
        Intent::Remind,
        "es",
        "recuérdame la semana que viene llamar al banco",
    ),
    (
        Intent::Remind,
        "es",
        "no olvidar enviar la factura el viernes",
    ),
    (
        Intent::Remind,
        "es",
        "tengo que renovar el pasaporte antes de fin de mes",
    ),
    (Intent::Journal, "es", "hoy fue un día tranquilo, leí mucho"),
    (
        Intent::Journal,
        "es",
        "día largo, no avancé nada, pero el paseo ayudó",
    ),
    (
        Intent::Journal,
        "es",
        "otra vez dormí mal, se me fue la tarde entera",
    ),
    (
        Intent::Journal,
        "es",
        "por fin cociné bien esta noche y me sentó bien",
    ),
    // ── pt ──
    (
        Intent::Remind,
        "pt",
        "lembre-me de renovar o passaporte em setembro",
    ),
    (
        Intent::Remind,
        "pt",
        "lembre-me na próxima semana de ligar para o banco",
    ),
    (
        Intent::Remind,
        "pt",
        "não esquecer de enviar a fatura na sexta",
    ),
    (
        Intent::Remind,
        "pt",
        "preciso pagar o aluguel até o dia primeiro",
    ),
    (Intent::Journal, "pt", "hoje acordei cedo e fui correr"),
    (
        Intent::Journal,
        "pt",
        "dia longo, não rendi nada, mas a caminhada ajudou",
    ),
    (
        Intent::Journal,
        "pt",
        "dormi mal de novo, perdi a tarde inteira",
    ),
    (
        Intent::Journal,
        "pt",
        "noite tranquila, cozinhei direito pela primeira vez em semanas",
    ),
    // ── it ──
    (
        Intent::Remind,
        "it",
        "ricordami di comprare i biglietti domani",
    ),
    (
        Intent::Remind,
        "it",
        "ricordami la settimana prossima di chiamare la banca",
    ),
    (
        Intent::Remind,
        "it",
        "non dimenticare di mandare la fattura venerdì",
    ),
    (
        Intent::Remind,
        "it",
        "devo rinnovare il passaporto entro fine mese",
    ),
    (Intent::Journal, "it", "oggi è stata una giornata pesante"),
    (
        Intent::Journal,
        "it",
        "giornata lunga, non ho concluso niente, ma la passeggiata è servita",
    ),
    (
        Intent::Journal,
        "it",
        "ho dormito male di nuovo, pomeriggio buttato",
    ),
    (
        Intent::Journal,
        "it",
        "serata tranquilla, finalmente ho cucinato come si deve",
    ),
    // ── nl ──
    (
        Intent::Remind,
        "nl",
        "herinner me eraan om de huur te betalen",
    ),
    (
        Intent::Remind,
        "nl",
        "herinner me volgende week aan het gesprek met de bank",
    ),
    (
        Intent::Remind,
        "nl",
        "niet vergeten om vrijdag de factuur te versturen",
    ),
    (
        Intent::Remind,
        "nl",
        "ik moet mijn paspoort verlengen voor het eind van de maand",
    ),
    (Intent::Journal, "nl", "vandaag eindelijk de tuin gedaan"),
    (
        Intent::Journal,
        "nl",
        "lange dag, niets afgekregen, maar die wandeling hielp",
    ),
    (
        Intent::Journal,
        "nl",
        "weer slecht geslapen, de hele middag was verloren",
    ),
    (
        Intent::Journal,
        "nl",
        "rustige avond, eindelijk weer eens goed gekookt",
    ),
    // ── pl ──
    (
        Intent::Remind,
        "pl",
        "przypomnij mi jutro o spotkaniu z lekarzem",
    ),
    (
        Intent::Remind,
        "pl",
        "przypomnij mi w przyszłym tygodniu zadzwonić do banku",
    ),
    (
        Intent::Remind,
        "pl",
        "nie zapomnieć wysłać faktury w piątek",
    ),
    (
        Intent::Remind,
        "pl",
        "muszę wyrobić paszport do końca miesiąca",
    ),
    (
        Intent::Journal,
        "pl",
        "dzisiaj byłem u dentysty, poszło dobrze",
    ),
    (
        Intent::Journal,
        "pl",
        "długi dzień, nic nie zrobiłem, ale spacer pomógł",
    ),
    (
        Intent::Journal,
        "pl",
        "znowu źle spałem, całe popołudnie zmarnowane",
    ),
    (
        Intent::Journal,
        "pl",
        "spokojny wieczór, w końcu porządnie ugotowałem",
    ),
    // ── tr ──
    (Intent::Remind, "tr", "yarın bana faturayı ödemeyi hatırlat"),
    (
        Intent::Remind,
        "tr",
        "gelecek hafta bankayı aramayı hatırlat",
    ),
    (Intent::Remind, "tr", "cuma günü faturayı göndermeyi unutma"),
    (
        Intent::Remind,
        "tr",
        "ay sonuna kadar pasaportu yenilemem lazım",
    ),
    (Intent::Journal, "tr", "bugün çok yorucu bir gündü"),
    (
        Intent::Journal,
        "tr",
        "uzun bir gündü, hiçbir şey ilerlemedi ama yürüyüş iyi geldi",
    ),
    (
        Intent::Journal,
        "tr",
        "yine kötü uyudum, bütün öğleden sonra boşa gitti",
    ),
    (
        Intent::Journal,
        "tr",
        "sakin bir akşam, uzun zamandır ilk kez doğru düzgün yemek yaptım",
    ),
    // ── ru ──
    (Intent::Remind, "ru", "напомни мне завтра позвонить маме"),
    (
        Intent::Remind,
        "ru",
        "напомни мне на следующей неделе про страховку",
    ),
    (Intent::Remind, "ru", "не забыть отправить счёт в пятницу"),
    (
        Intent::Remind,
        "ru",
        "нужно продлить паспорт до конца месяца",
    ),
    (Intent::Journal, "ru", "сегодня наконец закончил проект"),
    (
        Intent::Journal,
        "ru",
        "длинный день, ничего не сделал, но прогулка помогла",
    ),
    (
        Intent::Journal,
        "ru",
        "опять плохо спал, весь день насмарку",
    ),
    (
        Intent::Journal,
        "ru",
        "тихий вечер, наконец нормально приготовил ужин",
    ),
];

/// One reminder and one journal example, in the reader's language where the
/// example table has it and in English where it does not.
///
/// `accept_language` is the raw header. Only the primary subtag of the first
/// entry is read: `de-DE,de;q=0.9,en;q=0.8` is a reader who wants German, and
/// weighing the rest to discover that is arithmetic for nothing.
pub fn examples_for(accept_language: &str) -> (&'static str, &'static str) {
    // The same reading `infer::lang` gives the header, and deliberately the
    // same one: the examples this table teaches and the language the
    // synthesizer is instructed in are one decision, and two parsers over one
    // header is two places for a `de-DE` to stop being German.
    let want = crate::infer::lang::primary_subtag(accept_language);
    let pick = |intent: Intent| -> &'static str {
        let of = |lang: &str| {
            PROTOTYPES
                .iter()
                .find(|(i, l, _)| *i == intent && *l == lang)
                .map(|(_, _, p)| *p)
        };
        of(&want).or_else(|| of("en")).unwrap_or("")
    };
    (pick(Intent::Remind), pick(Intent::Journal))
}

/// Whether the operator has already said this note is not that.
///
/// `metadata.intent_refused` is a list of intent names, and it has to outlive
/// a re-read: the judged synthesis derives the intent again every time a
/// window is re-read, so without a record of the refusal a re-synthesis
/// quietly files the note again over somebody who had said no. The journal
/// side has worked this way for a while under `entry_refused`; that key is
/// still read here, so a base written before this keeps its refusals.
pub fn intent_refused(meta: &serde_json::Value, intent: Intent) -> bool {
    if intent == Intent::Journal && meta["entry_refused"].as_bool().unwrap_or(false) {
        return true;
    }
    meta["intent_refused"]
        .as_array()
        .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(intent.as_str())))
}

/// Record a refusal. Idempotent.
pub fn refuse_intent(meta: &mut serde_json::Value, intent: Intent) {
    if intent_refused(meta, intent) {
        return;
    }
    let mut all: Vec<serde_json::Value> = meta["intent_refused"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    all.push(serde_json::Value::String(intent.as_str().to_string()));
    meta["intent_refused"] = serde_json::Value::Array(all);
}

/// Withdraw one, the legacy key included — turning a filing back on by hand is
/// the operator saying the note may be read again.
pub fn allow_intent(meta: &mut serde_json::Value, intent: Intent) {
    if let Some(a) = meta["intent_refused"].as_array() {
        let kept: Vec<serde_json::Value> = a
            .iter()
            .filter(|v| v.as_str() != Some(intent.as_str()))
            .cloned()
            .collect();
        if kept.is_empty() {
            if let Some(m) = meta.as_object_mut() {
                m.remove("intent_refused");
            }
        } else {
            meta["intent_refused"] = serde_json::Value::Array(kept);
        }
    }
    if intent == Intent::Journal
        && let Some(m) = meta.as_object_mut()
    {
        m.remove("entry_refused");
    }
}

use chrono::{Datelike, NaiveDate, TimeZone, Weekday};
use chrono_tz::Tz;
use std::sync::LazyLock;

pub const DEFAULT_HOUR: u32 = 9;

pub fn zone(name: Option<&str>) -> Tz {
    name.and_then(|n| n.parse::<Tz>().ok()).unwrap_or(Tz::UTC)
}

/// The zone the server itself is in, named the way the zone table names it, or
/// `UTC` where the platform cannot say. Resolved once: it is read on the path
/// of every capture that arrives without a zone, and asking the host on each
/// one is a syscall and a file read for an answer that does not change.
static SERVER_ZONE: LazyLock<String> = LazyLock::new(|| {
    match iana_time_zone::get_timezone()
        .ok()
        .filter(|n| n.parse::<Tz>().is_ok())
    {
        Some(n) => n,
        None => {
            tracing::info!(
                "the platform cannot name its zone; dates from doors that send none are read in UTC"
            );
            Tz::UTC.name().to_string()
        }
    }
});

/// The zone a door that sent none is read in: what `time.default_tz` names, or
/// — where it is empty, which is the shipped default — the server's own.
pub fn default_zone_name(configured: &str) -> String {
    let configured = configured.trim();
    if configured.is_empty() {
        return SERVER_ZONE.clone();
    }
    configured.to_string()
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
    count: Option<u32>,
}

/// The largest INTERVAL the subset accepts. `next_after` walks day by day and
/// its bound scales with the interval, so an unbounded one is an unbounded
/// loop inside a request. Every hundredth year is past anything a reminder
/// means, and a rule beyond it is a typo or an attack, not an intention.
pub const MAX_INTERVAL: u32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

fn parse_rule(rule: &str) -> Result<Rule, String> {
    let mut r = Rule {
        freq: Freq::Daily,
        interval: 1,
        by_day: vec![],
        by_month_day: None,
        until: None,
        count: None,
    };
    let mut has_freq = false;
    for part in rule.split(';').filter(|p| !p.is_empty()) {
        let (k, v) = part
            .split_once('=')
            .ok_or_else(|| format!("not key=value: {part}"))?;
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
                r.interval = v
                    .parse::<u32>()
                    .ok()
                    .filter(|n| (1..=MAX_INTERVAL).contains(n))
                    .ok_or_else(|| format!("INTERVAL must be between 1 and {MAX_INTERVAL}"))?
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
                        other => {
                            return Err(format!("BYDAY={other}: weekday codes only, no ordinals"));
                        }
                    });
                }
            }
            "BYMONTHDAY" => {
                r.by_month_day = Some(
                    v.parse::<u32>()
                        .ok()
                        .filter(|n| (1..=31).contains(n))
                        .ok_or("BYMONTHDAY out of range")?,
                )
            }
            "UNTIL" => {
                let dt = chrono::NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%SZ")
                    .or_else(|_| {
                        NaiveDate::parse_from_str(v, "%Y%m%d")
                            .map(|d| d.and_hms_opt(23, 59, 59).unwrap())
                    })
                    .map_err(|_| format!("UNTIL={v} is not a date"))?;
                r.until = Some(dt.and_utc().timestamp());
            }
            "COUNT" => {
                r.count = Some(
                    v.parse::<u32>()
                        .ok()
                        .filter(|n| *n >= 1)
                        .ok_or("COUNT must be a positive integer")?,
                );
            }
            other => return Err(format!("{other} is outside the subset")),
        }
    }
    if !has_freq {
        return Err("FREQ is required".into());
    }
    // BYDAY under YEARLY expands to every such weekday of the year, which is
    // a rule nobody means and which the ordinals the subset refuses are what
    // would narrow. Every other pairing `next_after` honours; this one is
    // refused here rather than silently ignored there.
    if !r.by_day.is_empty() && r.freq == Freq::Yearly {
        return Err("BYDAY under FREQ=YEARLY is outside the subset".into());
    }
    // And its mirror. `next_after`'s Daily and Weekly arms never read
    // `by_month_day`, so the constraint was accepted and then dropped:
    // `FREQ=DAILY;BYMONTHDAY=15` validated, was stored by the API door, and
    // fired every day of the month with the whole push ladder behind each one —
    // a rule that says "the fifteenth" behaving as "always". RFC 5545 forbids
    // the WEEKLY pairing outright, and under DAILY the day-of-month is a limit
    // this subset does not implement. Refused for the reason directly above:
    // silently ignoring a constraint somebody wrote is the failure.
    if r.by_month_day.is_some() && matches!(r.freq, Freq::Daily | Freq::Weekly) {
        return Err("BYMONTHDAY under FREQ=DAILY or FREQ=WEEKLY is outside the subset".into());
    }
    Ok(r)
}

pub fn validate_rule(rule: &str) -> Result<(), String> {
    parse_rule(rule).map(|_| ())
}

/// How many occurrences the rule allows in all, `None` for open-ended. A
/// bounded recurrence is bounded by its rows: see `Store::occurrences_of_rule`
/// and `complete_moment`, which is where this is enforced.
pub fn rule_count(rule: &str) -> Option<u32> {
    parse_rule(rule).ok()?.count
}

/// A local wall-clock time as an instant, choosing for the operator on the
/// two days a year when the zone cannot. An ambiguous fall-back hour takes
/// its earlier reading; a time inside a spring-forward gap rolls forward to
/// the first instant the zone has again, in quarter-hour steps — chrono's
/// mapping for a gap is `None` with nothing to call `earliest()` on, and
/// treating that as "no answer" silently dropped a date the operator named.
pub(crate) fn resolve_local(dt: chrono::NaiveDateTime, tz: Tz) -> Option<i64> {
    if let Some(d) = tz.from_local_datetime(&dt).earliest() {
        return Some(d.timestamp());
    }
    // Gaps are 30 or 60 minutes almost everywhere (Lord Howe's is 30); three
    // hours of quarter-hour steps covers the historical 2 h ones too.
    (1..=12)
        .find_map(|q| {
            tz.from_local_datetime(&(dt + chrono::Duration::minutes(15 * q)))
                .earliest()
        })
        .map(|d| d.timestamp())
}

/// The next occurrence strictly after `at`, keeping `at`'s wall-clock time in
/// `tz`. None when the rule is exhausted or invalid. COUNT is enforced by
/// `complete_moment`, which counts the occurrences already on the artifact.
pub fn next_after(rule: &str, at: i64, tz: Tz) -> Option<i64> {
    next_after_anchored(rule, at, tz, None)
}

/// The same, with the wall-clock time read off `anchor` rather than off `at`.
///
/// Which matters exactly twice a year. `resolve_local` rolls a time that falls
/// inside a spring-forward gap to the first instant the zone has again, which
/// is the right answer for *that* occurrence — a daily 02:30 in Europe/Berlin
/// has to happen somewhere on the Sunday the zone has no 02:30. But the next
/// occurrence then took its wall-clock from that stored instant and the
/// reminder read 03:00 from that day on, for good: an RRULE with no DTSTART
/// has nothing to re-anchor to, so the roll became the new time.
///
/// The anchor is the series' own starting wall-clock — see
/// `Store::series_anchor`, which prefers a time somebody moved the reminder to
/// over the one it was first read at, because a person moving a recurring row
/// is restating what time it happens. `None` keeps the old reading, which is
/// what a one-shot and every pre-series row have.
pub fn next_after_anchored(rule: &str, at: i64, tz: Tz, anchor: Option<i64>) -> Option<i64> {
    let r = parse_rule(rule).ok()?;
    let start = tz.timestamp_opt(at, 0).single()?;
    let time = anchor
        .and_then(|a| tz.timestamp_opt(a, 0).single())
        .map(|a| a.time())
        .unwrap_or_else(|| start.time());
    let origin = start.date_naive();
    let mut date = origin;
    // Day by day, bounded: the subset never needs more than four years of
    // days per interval step to find the next occurrence (a yearly 29 Feb).
    for _ in 0..(366 * 4 * r.interval as usize + 1) {
        date += chrono::Duration::days(1);
        let hit = match r.freq {
            // BYDAY narrows a daily rule rather than expanding it (RFC 5545):
            // "every weekday at 9" is FREQ=DAILY;BYDAY=MO,TU,WE,TH,FR, and
            // read without the filter it fired on Saturday too.
            Freq::Daily => {
                (date - origin).num_days() % r.interval as i64 == 0
                    && (r.by_day.is_empty() || r.by_day.contains(&date.weekday()))
            }
            Freq::Weekly => {
                let own = [origin.weekday()];
                let days: &[Weekday] = if r.by_day.is_empty() { &own } else { &r.by_day };
                // RFC 5545 counts calendar weeks (WKST defaults to Monday),
                // not 7-day blocks from the origin instant: INTERVAL=2 with
                // BYDAY=MO from a Wednesday origin must skip the Monday five
                // days later — it is already the next week.
                let monday = |d: chrono::NaiveDate| {
                    d - chrono::Duration::days(d.weekday().num_days_from_monday() as i64)
                };
                let weeks = (monday(date) - monday(origin)).num_days() / 7;
                days.contains(&date.weekday()) && weeks % r.interval as i64 == 0
            }
            Freq::Monthly => {
                let months = (date.year() - origin.year()) * 12
                    + (date.month() as i32 - origin.month() as i32);
                // BYDAY with no ordinal names every such weekday of the month
                // — "every Monday" as a monthly rule — and with BYMONTHDAY
                // beside it the two are an intersection. Only when neither is
                // given does the origin's day-of-month stand in.
                let day = match (r.by_month_day, r.by_day.is_empty()) {
                    (Some(dom), _) => date.day() == dom,
                    (None, false) => true,
                    (None, true) => date.day() == origin.day(),
                };
                day && (r.by_day.is_empty() || r.by_day.contains(&date.weekday()))
                    && months % r.interval as i32 == 0
            }
            // BYMONTHDAY moves a yearly rule off the origin's day within the
            // origin's month; BYDAY is refused for YEARLY in `parse_rule`.
            Freq::Yearly => {
                date.month() == origin.month()
                    && date.day() == r.by_month_day.unwrap_or(origin.day())
                    && (date.year() - origin.year()) % r.interval as i32 == 0
            }
        };
        if !hit {
            continue;
        }
        let dt = date.and_time(time);
        let Some(ts) = resolve_local(dt, tz) else {
            continue;
        };
        if let Some(until) = r.until
            && ts > until
        {
            return None;
        }
        return Some(ts);
    }
    None
}

impl crate::core::Core {
    /// Done, and — for a recurring moment — the next occurrence as a new row
    /// carrying the same rule and source. The done row stays: the history of
    /// a recurring reminder is its rows.
    /// Answers whether the completion actually happened, so a door can tell
    /// "done" from "there was nothing there to finish" — a button pressed
    /// twice, or one on a page open since before a re-read replaced the row.
    /// The band reported "Done · undone" for both, and offered an undo for a
    /// moment nothing had changed.
    /// The wall-clock a recurrence is anchored to, or `None` where there is
    /// no series to read one from — a row written before `series_id` existed.
    /// A store error here is not the completion's to fail on: the worst it
    /// costs is the reading `next_after` did before the anchor existed.
    async fn series_anchor_of(&self, m: &crate::store::moments::Moment) -> Option<i64> {
        let series = m.series_id.as_deref()?;
        self.store.series_anchor(series).await.unwrap_or_else(|e| {
            tracing::warn!(series, error = %e, "no anchor for this recurrence");
            None
        })
    }

    pub async fn complete_moment(&self, id: &str) -> crate::error::Result<bool> {
        let now = self.clock.now();
        if let Some(m) = self.store.moment(id).await?
            // The claim, and the gate: everything below arms rows and retires
            // corpora, and none of it may run twice for one completion.
            && self.store.mark_done(id, now).await?
        {
            // A bounded recurrence is bounded by its rows: the done row stays,
            // so the occurrences that have existed are the ones on the
            // artifact carrying this rule — including the one just completed.
            // At COUNT, nothing further is armed.
            if let (Some(rule), Some(at)) = (m.rule.as_deref(), m.at)
                && !self
                    .store
                    .rule_is_exhausted(&m.artifact_id, rule, m.series_id.as_deref())
                    .await?
                // Anchored, so a daylight-saving gap cannot move the time of
                // day permanently. `uncomplete_moment` anchors the same way,
                // or it would look for the successor at a different instant
                // than the one this armed.
                && let Some(next) = next_after_anchored(
                    rule,
                    at,
                    zone(Some(&m.tz)),
                    self.series_anchor_of(&m).await,
                )
                // Not `has_moment_at`, which is the re-read's question and
                // answers yes for an instant that is only some row's
                // `moved_from`. Moving this very reminder off next week's
                // occurrence is exactly what an operator does with a recurring
                // row, and it made completion decline to arm the successor it
                // had just stepped over — the recurrence ended there, with
                // `has_moved_moment` refusing every re-read that might have
                // put it back. What arming needs to know is whether a row is
                // parked on the instant, and nothing else.
                && !self
                    .store
                    .moment_parked_at(&m.artifact_id, crate::store::moments::Kind::Due, Some(next))
                    .await?
            {
                self.store
                    .insert_moment(&crate::store::moments::NewMoment {
                        artifact_id: m.artifact_id,
                        kind: crate::store::moments::Kind::Due,
                        at: Some(next),
                        tz: m.tz,
                        rule: m.rule.clone(),
                        // Not the parent's reading. Nothing read this
                        // occurrence out of the prose and nobody set it, and a
                        // row wearing `cue` here is one a re-read takes for
                        // its own and deletes — after which the re-read finds
                        // the original instant, sees it done, and arms
                        // nothing. See `Source::Armed`.
                        source: crate::store::moments::Source::Armed,
                        span: m.span,
                        // The successor continues this recurrence rather than
                        // starting one: same series, so `COUNT` keeps counting
                        // from where the completed occurrence left it however
                        // many supersessions the rows have ridden since.
                        series_id: m.series_id.clone(),
                    })
                    .await?;
            }
            self.store.rearm_remind().await?;
            // Last, and only after the recurrence above has armed the next
            // occurrence: "no open reminder remains" is a question about the
            // state this call leaves behind, not the one it found.
            if let Some(cid) = self.store.corpus_of_moment(id).await?
                && !self.store.has_open_reminder_for_corpus(&cid).await?
                && self.store.corpus_was_read_as_reminder(&cid).await?
            {
                self.store.retire_corpus(&cid, now).await?;
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// The exact inverse of `complete_moment`, and the only way to undo one.
    ///
    /// Undoing has to put back everything completing changed, or the undo the
    /// band offers a second after "Done" leaves the base in a state neither
    /// press asked for: the successor occurrence a recurrence armed would stay
    /// on the band beside the row that came back — two open rows for one rule,
    /// and one occurrence of a `COUNT=n` burned — and the note the completion
    /// retired would stay out of `recent_captures` and below the search cliff
    /// with nothing left to clear it.
    ///
    /// The successor is deleted rather than marked: it never happened.
    pub async fn uncomplete_moment(&self, id: &str) -> crate::error::Result<()> {
        let Some(m) = self.store.moment(id).await? else {
            return Ok(());
        };
        // Before the row is reopened: while it is still done, it is the row
        // whose completion armed the successor, and `next_after` from its own
        // `at` is the instant that successor was given.
        if let (Some(rule), Some(at)) = (m.rule.as_deref(), m.at)
            && let Some(next) =
                next_after_anchored(rule, at, zone(Some(&m.tz)), self.series_anchor_of(&m).await)
        {
            self.store
                .delete_armed_occurrence(&m.artifact_id, rule, next)
                .await?;
        }
        self.store.undo_done(id).await?;
        // The row comes back, so the note comes back with it. Unconditional: a
        // corpus that was never retired is already NULL here.
        if let Some(cid) = self.store.corpus_of_moment(id).await? {
            self.store.unretire_corpus(&cid).await?;
        }
        self.store.rearm_remind().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_counted_recurrence_stops_at_its_count() {
        use crate::store::moments::{Kind, NewMoment, Source};
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new("Water the plants", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        let rule = "FREQ=DAILY;COUNT=2";
        let first = core
            .store
            .insert_moment(&NewMoment {
                artifact_id: aid.clone(),
                kind: Kind::Due,
                at: Some(
                    berlin()
                        .with_ymd_and_hms(2026, 8, 31, 9, 0, 0)
                        .unwrap()
                        .timestamp(),
                ),
                tz: "Europe/Berlin".into(),
                rule: Some(rule.into()),
                source: Source::Cue,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();

        core.complete_moment(&first).await.unwrap();
        assert_eq!(
            core.store.occurrences_of_rule(&aid, rule).await.unwrap(),
            2,
            "the second occurrence is armed"
        );
        let second = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(second.len(), 1);
        core.complete_moment(&second[0].moment.id).await.unwrap();
        assert_eq!(
            core.store.occurrences_of_rule(&aid, rule).await.unwrap(),
            2,
            "two of two, and no third"
        );
        assert!(core.store.open_due(0, i64::MAX).await.unwrap().is_empty());
    }

    /// The recurrence that died on its first completion.
    ///
    /// A daily reminder, moved by hand off the occurrence it was about to reach
    /// — which is the ordinary thing to do with a recurring row that lands
    /// badly one week. Completing the moved row computes the next occurrence,
    /// and that instant is exactly what the row was moved *away from*, which
    /// `has_moment_at` answers "yes" to by design: it is the re-read's
    /// question, and a re-read must not put a row back on a corrected-away-from
    /// date. Used as the gate for arming, it declined to arm the successor —
    /// and `has_moved_moment` then refused every re-read that might have put
    /// one back, so the recurrence was over, silently and for good.
    #[tokio::test]
    async fn a_recurrence_moved_off_its_next_occurrence_still_arms_one() {
        use crate::store::moments::{Kind, NewMoment, Source};
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new("Take the tablets", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        let rule = "FREQ=DAILY";
        let wed = berlin()
            .with_ymd_and_hms(2026, 9, 2, 9, 0, 0)
            .unwrap()
            .timestamp();
        let id = core
            .store
            .insert_moment(&NewMoment {
                artifact_id: aid.clone(),
                kind: Kind::Due,
                at: Some(wed),
                tz: "Europe/Berlin".into(),
                rule: Some(rule.into()),
                source: Source::Cue,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        // Moved a day earlier by hand. `next_after` from Tuesday is Wednesday,
        // which is now this row's `moved_from`.
        let tue = wed - 86_400;
        core.store
            .move_moment(&id, tue, "Europe/Berlin")
            .await
            .unwrap();

        assert!(core.complete_moment(&id).await.unwrap());
        let open = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(open.len(), 1, "the next occurrence is armed: {open:?}");
        assert_eq!(open[0].moment.at, Some(wed));
        assert_eq!(open[0].moment.rule.as_deref(), Some(rule));
    }

    /// And the guard the one above must not undo: an instant the artifact
    /// really does carry is still not armed a second time.
    #[tokio::test]
    async fn a_recurrence_does_not_arm_an_occurrence_that_is_already_there() {
        use crate::store::moments::{Kind, NewMoment, Source};
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new("Water the plants", "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        let rule = "FREQ=DAILY";
        let day = |d| {
            berlin()
                .with_ymd_and_hms(2026, 9, d, 9, 0, 0)
                .unwrap()
                .timestamp()
        };
        let row = |at: i64| NewMoment {
            artifact_id: aid.clone(),
            kind: Kind::Due,
            at: Some(at),
            tz: "Europe/Berlin".into(),
            rule: Some(rule.into()),
            source: Source::Cue,
            span: None,
            series_id: None,
        };
        let first = core.store.insert_moment(&row(day(2))).await.unwrap();
        // Somebody already set tomorrow's by hand.
        core.store.insert_moment(&row(day(3))).await.unwrap();
        assert!(core.complete_moment(&first).await.unwrap());
        let open = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(open.len(), 1, "no second row on the same instant: {open:?}");
        assert_eq!(open[0].moment.at, Some(day(3)));
    }

    #[test]
    fn examples_come_from_the_prototypes_in_the_readers_language() {
        let (remind, journal) = examples_for("de-DE,de;q=0.9,en;q=0.8");
        assert!(remind.starts_with("erinnere mich"), "{remind}");
        assert!(journal.starts_with("heute"), "{journal}");

        let (remind, journal) = examples_for("");
        assert!(
            remind.starts_with("remind me"),
            "English is the fallback: {remind}"
        );
        assert!(journal.starts_with("today i"), "{journal}");

        let (remind, _) = examples_for("xx-YY");
        assert!(
            remind.starts_with("remind me"),
            "an unknown language falls back too"
        );
    }

    /// The table is read by index and by language, so its shape is a contract
    /// rather than a convention: every language carries both intents, no
    /// sentence is written twice, and nothing is left in a language
    /// `examples_for` cannot reach.
    #[test]
    fn every_language_carries_both_intents_and_no_sentence_twice() {
        let langs: std::collections::BTreeSet<&str> =
            PROTOTYPES.iter().map(|(_, l, _)| *l).collect();
        assert!(
            langs.contains("en"),
            "English is the fallback and must exist"
        );
        for l in &langs {
            for intent in [Intent::Remind, Intent::Journal] {
                let n = PROTOTYPES
                    .iter()
                    .filter(|(i, x, _)| i == &intent && x == l)
                    .count();
                assert!(n >= 2, "{l} has {n} {} prototypes", intent.as_str());
            }
            let (r, j) = examples_for(l);
            assert!(!r.is_empty() && !j.is_empty(), "{l} resolves to examples");
        }
        let mut seen = std::collections::BTreeSet::new();
        for (_, _, p) in PROTOTYPES {
            assert!(seen.insert(*p), "duplicate prototype: {p}");
        }
    }

    #[test]
    fn a_refusal_is_recorded_per_intent_and_reads_the_key_it_replaced() {
        let mut meta = serde_json::json!({});
        assert!(!intent_refused(&meta, Intent::Remind));
        refuse_intent(&mut meta, Intent::Remind);
        refuse_intent(&mut meta, Intent::Remind);
        assert_eq!(
            meta["intent_refused"],
            serde_json::json!(["remind"]),
            "idempotent"
        );
        assert!(intent_refused(&meta, Intent::Remind));
        assert!(
            !intent_refused(&meta, Intent::Journal),
            "one refusal is not the other"
        );

        refuse_intent(&mut meta, Intent::Journal);
        allow_intent(&mut meta, Intent::Remind);
        assert!(!intent_refused(&meta, Intent::Remind));
        assert!(
            intent_refused(&meta, Intent::Journal),
            "and the other one stands"
        );
        allow_intent(&mut meta, Intent::Journal);
        assert!(
            meta.get("intent_refused").is_none(),
            "the last one takes the key with it"
        );

        // A base written before this keeps its refusals, and withdrawing one
        // clears the old key too — otherwise the undo would appear to work
        // and the judgement would go on refusing.
        let mut old = serde_json::json!({"entry_refused": true});
        assert!(intent_refused(&old, Intent::Journal));
        assert!(!intent_refused(&old, Intent::Remind));
        allow_intent(&mut old, Intent::Journal);
        assert!(!intent_refused(&old, Intent::Journal));
    }

    fn berlin() -> chrono_tz::Tz {
        chrono_tz::Tz::Europe__Berlin
    }
    fn local(at: i64) -> String {
        berlin()
            .timestamp_opt(at, 0)
            .unwrap()
            .format("%Y-%m-%d %H:%M")
            .to_string()
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
        for bad in [
            "FREQ=HOURLY",
            "FREQ=WEEKLY;BYDAY=2MO",
            "BYDAY=MO",
            "FREQ=WEEKLY;BYSETPOS=1",
            "",
            // `next_after` walks day by day and its bound scales with the
            // interval: unbounded here is an unbounded loop inside a request.
            "FREQ=DAILY;INTERVAL=0",
            "FREQ=DAILY;INTERVAL=4000000000",
            // `next_after`'s Daily and Weekly arms never read `by_month_day`,
            // so accepting these was accepting a constraint and then dropping
            // it: "the fifteenth" stored, and a push every day of the month.
            // RFC 5545 forbids the weekly pairing outright.
            "FREQ=DAILY;BYMONTHDAY=15",
            "FREQ=WEEKLY;BYMONTHDAY=15",
        ] {
            assert!(validate_rule(bad).is_err(), "{bad}");
        }
        assert_eq!(
            rule_count("FREQ=DAILY;COUNT=5"),
            Some(5),
            "COUNT is kept, not discarded"
        );
        assert_eq!(rule_count("FREQ=DAILY"), None);
    }

    #[test]
    fn weekly_keeps_the_wall_clock_across_dst() {
        // Monday 2026-10-19 09:00 CEST → Monday 2026-10-26 09:00 CET (DST ends 25 Oct).
        let at = berlin()
            .with_ymd_and_hms(2026, 10, 19, 9, 0, 0)
            .unwrap()
            .timestamp();
        let next = next_after("FREQ=WEEKLY;BYDAY=MO", at, berlin()).unwrap();
        assert_eq!(local(next), "2026-10-26 09:00");
        assert_eq!(
            next - at,
            7 * 86_400 + 3_600,
            "one week and the hour DST gave back"
        );
    }

    /// A daily 02:30 in Berlin meets the Sunday the zone has no 02:30. That
    /// occurrence is rolled to 03:00, which is right — and then every later
    /// one read its wall-clock off the rolled instant and the reminder was a
    /// 03:00 reminder from that day on, permanently, with no DTSTART to
    /// re-anchor to.
    #[test]
    fn a_spring_forward_does_not_move_a_daily_reminder_for_good() {
        let anchor = berlin()
            .with_ymd_and_hms(2027, 3, 26, 2, 30, 0)
            .unwrap()
            .timestamp();
        let gap_day = next_after("FREQ=DAILY", anchor, berlin()).unwrap();
        assert_eq!(
            local(gap_day),
            "2027-03-27 02:30",
            "the day before the change"
        );
        let rolled = next_after("FREQ=DAILY", gap_day, berlin()).unwrap();
        assert_eq!(
            local(rolled),
            "2027-03-28 03:00",
            "the Sunday has no 02:30, so the occurrence rolls out of the gap"
        );

        assert_eq!(
            local(next_after("FREQ=DAILY", rolled, berlin()).unwrap()),
            "2027-03-29 03:00",
            "unanchored, the roll becomes the new time"
        );
        assert_eq!(
            local(next_after_anchored("FREQ=DAILY", rolled, berlin(), Some(anchor)).unwrap()),
            "2027-03-29 02:30",
            "anchored, the reminder goes back to the time it was set for"
        );
    }

    #[test]
    fn byday_picks_the_next_listed_day() {
        let mon = berlin()
            .with_ymd_and_hms(2026, 8, 31, 9, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(
            local(next_after("FREQ=WEEKLY;BYDAY=MO,WE", mon, berlin()).unwrap()),
            "2026-09-02 09:00"
        );
    }

    #[test]
    fn monthly_on_the_31st_skips_short_months() {
        let at = berlin()
            .with_ymd_and_hms(2026, 8, 31, 9, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(
            local(next_after("FREQ=MONTHLY;BYMONTHDAY=31", at, berlin()).unwrap()),
            "2026-10-31 09:00"
        );
    }

    #[test]
    fn until_ends_the_rule() {
        let at = berlin()
            .with_ymd_and_hms(2026, 8, 31, 9, 0, 0)
            .unwrap()
            .timestamp();
        assert!(next_after("FREQ=DAILY;UNTIL=20260831T235959Z", at, berlin()).is_none());
        assert!(next_after("FREQ=DAILY;UNTIL=20260901T235959Z", at, berlin()).is_some());
    }

    #[test]
    fn interval_and_yearly() {
        let at = berlin()
            .with_ymd_and_hms(2026, 8, 31, 9, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(
            local(next_after("FREQ=DAILY;INTERVAL=3", at, berlin()).unwrap()),
            "2026-09-03 09:00"
        );
        assert_eq!(
            local(next_after("FREQ=YEARLY", at, berlin()).unwrap()),
            "2027-08-31 09:00"
        );
    }

    #[test]
    fn a_weekly_interval_counts_calendar_weeks_not_day_blocks() {
        // RFC 5545: INTERVAL=2 with BYDAY=MO from a Wednesday origin skips
        // the Monday five days later — it is already the next week — and
        // fires the one after. Counted in 7-day blocks from the origin it
        // fired at +5 days.
        let wed = resolve_local(
            chrono::NaiveDate::from_ymd_opt(2026, 9, 2)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
            berlin(),
        )
        .unwrap();
        let next = next_after("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO", wed, berlin()).unwrap();
        assert_eq!(local(next), "2026-09-14 09:00");
    }

    #[test]
    fn byday_narrows_a_daily_rule_instead_of_being_ignored() {
        // "every weekday at 9" is the natural answer to a note that says so,
        // and the parser took the BYDAY while `next_after` read past it: the
        // Friday occurrence armed Saturday.
        let fri = berlin()
            .with_ymd_and_hms(2026, 9, 4, 9, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(
            local(next_after("FREQ=DAILY;BYDAY=MO,TU,WE,TH,FR", fri, berlin()).unwrap()),
            "2026-09-07 09:00",
            "the weekend is skipped, not fired through"
        );
    }

    #[test]
    fn byday_under_monthly_names_every_such_weekday_of_the_month() {
        // No BYMONTHDAY beside it: the origin's day-of-month must not stand
        // in, or the rule fires only when the two happen to coincide.
        let mon = berlin()
            .with_ymd_and_hms(2026, 9, 7, 9, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(
            local(next_after("FREQ=MONTHLY;BYDAY=MO", mon, berlin()).unwrap()),
            "2026-09-14 09:00"
        );
    }

    #[test]
    fn monthly_with_both_parts_is_their_intersection() {
        // BYMONTHDAY=13 and BYDAY=SU: the next 13th that is a Sunday.
        let at = berlin()
            .with_ymd_and_hms(2026, 9, 13, 9, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(
            local(next_after("FREQ=MONTHLY;BYMONTHDAY=13;BYDAY=SU", at, berlin()).unwrap()),
            "2026-12-13 09:00"
        );
    }

    #[test]
    fn bymonthday_moves_a_yearly_rule_off_the_origin_day() {
        let at = berlin()
            .with_ymd_and_hms(2026, 8, 31, 9, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(
            local(next_after("FREQ=YEARLY;BYMONTHDAY=15", at, berlin()).unwrap()),
            "2027-08-15 09:00"
        );
    }

    #[test]
    fn byday_under_yearly_is_refused_rather_than_ignored() {
        assert!(validate_rule("FREQ=YEARLY;BYDAY=MO").is_err());
        assert!(validate_rule("FREQ=DAILY;BYDAY=MO").is_ok());
        assert!(validate_rule("FREQ=MONTHLY;BYDAY=MO").is_ok());
    }

    #[test]
    fn a_time_in_the_spring_forward_gap_rolls_to_the_gap_close() {
        // 02:30 on 2027-03-28 does not exist in Berlin. chrono maps it to
        // `None` — there is no `earliest()` to take — and the date was
        // silently dropped where the operator had named one.
        let dt = chrono::NaiveDate::from_ymd_opt(2027, 3, 28)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap();
        let ts = resolve_local(dt, berlin()).unwrap();
        assert_eq!(local(ts), "2027-03-28 03:00");
    }
}
