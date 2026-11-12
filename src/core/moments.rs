//! Time as the base keeps it: intent vocabulary and refusals, the zone table, the recurrence
//! subset. Pure functions: no store, no model, no clock of its own.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Intent {
    Remind,
}

impl Intent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Intent::Remind => "remind",
        }
    }
}

/// Example phrasings per intent and language; `examples_for` reads each language's first row.
pub const PROTOTYPES: &[(Intent, &str, &str)] = &[
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
];

/// The language `examples_for` answered in (`en` where it fell back), so markup can say the
/// language changed.
pub fn examples_lang_for(accept_language: &str) -> &'static str {
    let want = crate::infer::lang::primary_subtag(accept_language);
    PROTOTYPES
        .iter()
        .find(|(i, l, _)| *i == Intent::Remind && *l == want)
        .map(|(_, l, _)| *l)
        .unwrap_or("en")
}

pub fn examples_for(accept_language: &str) -> &'static str {
    // Same reading as `infer::lang`: the examples and the synthesizer's language are one decision.
    let want = crate::infer::lang::primary_subtag(accept_language);
    let of = |lang: &str| {
        PROTOTYPES
            .iter()
            .find(|(i, l, _)| *i == Intent::Remind && *l == lang)
            .map(|(_, _, p)| *p)
    };
    of(&want).or_else(|| of("en")).unwrap_or("")
}

/// `metadata.intent_refused` must outlive a re-read, which derives the intent again each time.
pub fn intent_refused(meta: &serde_json::Value, intent: Intent) -> bool {
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

/// Turning a filing back on by hand says the note may be read again.
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
}

use chrono::{Datelike, NaiveDate, TimeZone, Weekday};
use chrono_tz::Tz;
use std::sync::LazyLock;

pub const DEFAULT_HOUR: u32 = 9;

pub fn zone(name: Option<&str>) -> Tz {
    name.and_then(|n| n.parse::<Tz>().ok()).unwrap_or(Tz::UTC)
}

/// Resolved once: it is read for every capture that arrives without a zone.
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

/// `time.default_tz`, or the server's own zone where that is empty (the default).
pub fn default_zone_name(configured: &str) -> String {
    let configured = configured.trim();
    if configured.is_empty() {
        return SERVER_ZONE.clone();
    }
    configured.to_string()
}

/// The RRULE subset: FREQ, INTERVAL, BYDAY, BYMONTHDAY, and UNTIL or COUNT.
#[derive(Debug)]
struct Rule {
    freq: Freq,
    interval: u32,
    by_day: Vec<Weekday>,
    by_month_day: Option<u32>,
    until: Option<Until>,
    count: Option<u32>,
}

/// Kept in the rule's own terms: the parser has no zone, and a date-only UNTIL read as UTC drops a
/// day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Until {
    /// `UNTIL=20260901T235959Z`: an instant, as written, in UTC.
    Instant(i64),
    /// Floating DATE-TIME, read in the reader's zone per RFC 5545.
    Local(chrono::NaiveDateTime),
    /// The last calendar day, read in the zone the occurrences resolve in.
    LastDay(NaiveDate),
}

/// `next_after` walks day by day with a bound scaling with the interval; larger is a typo or an
/// attack.
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
                // Deduped, which also bounds it: `next_after` scans `BYDAY` once per day walked.
                for d in v.split(',') {
                    let day = match d {
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
                    };
                    if !r.by_day.contains(&day) {
                        r.by_day.push(day);
                    }
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
                // `...Z` is a UTC instant; a bare date is a calendar day and a bare DATE-TIME is
                // floating (RFC 5545), both read in the reader's zone, or every zone west of
                // Greenwich loses the last day.
                r.until = Some(
                    chrono::NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%SZ")
                        .map(|dt| Until::Instant(dt.and_utc().timestamp()))
                        .or_else(|_| {
                            chrono::NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S")
                                .map(Until::Local)
                        })
                        .or_else(|_| NaiveDate::parse_from_str(v, "%Y%m%d").map(Until::LastDay))
                        .map_err(|_| format!("UNTIL={v} is not a date"))?,
                );
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
    // Refused rather than ignored: BYDAY under YEARLY needs the ordinals this subset refuses.
    if !r.by_day.is_empty() && r.freq == Freq::Yearly {
        return Err("BYDAY under FREQ=YEARLY is outside the subset".into());
    }
    // `next_after`'s Daily and Weekly arms never read `by_month_day`; RFC 5545 forbids the WEEKLY
    // pairing.
    if r.by_month_day.is_some() && matches!(r.freq, Freq::Daily | Freq::Weekly) {
        return Err("BYMONTHDAY under FREQ=DAILY or FREQ=WEEKLY is outside the subset".into());
    }
    Ok(r)
}

pub fn validate_rule(rule: &str) -> Result<(), String> {
    parse_rule(rule).map(|_| ())
}

/// `None` for open-ended; enforced by `complete_moment` counting the rule's rows.
pub fn rule_count(rule: &str) -> Option<u32> {
    parse_rule(rule).ok()?.count
}

/// Every reader of a moment can do arithmetic in this range; `resolve_local` and `api::set_moment`
/// hold to it.
pub const YEAR_ONE: i64 = -62_135_596_800;
pub const END_OF_9999: i64 = 253_402_300_799;

/// An ambiguous fall-back hour takes its earlier reading; a spring-forward gap rolls to the first
/// valid instant. `None` outside years 1 to 9999: `%Y` parses six-digit years and an unchecked add
/// at chrono's ceiling panics.
pub(crate) fn resolve_local(dt: chrono::NaiveDateTime, tz: Tz) -> Option<i64> {
    let stamped = |d: chrono::DateTime<Tz>| {
        let ts = d.timestamp();
        (YEAR_ONE..=END_OF_9999).contains(&ts).then_some(ts)
    };
    if let Some(d) = tz.from_local_datetime(&dt).earliest() {
        return stamped(d);
    }
    // Gaps are 30 or 60 minutes almost everywhere; three hours of quarter-hour steps covers
    // historical 2 h ones.
    (1..=12)
        .find_map(|q| {
            dt.checked_add_signed(chrono::Duration::minutes(15 * q))
                .and_then(|shifted| tz.from_local_datetime(&shifted).earliest())
        })
        .and_then(stamped)
}

/// Strictly after `at`, keeping `at`'s wall-clock in `tz`. COUNT is enforced by `complete_moment`.
pub fn next_after(rule: &str, at: i64, tz: Tz) -> Option<i64> {
    next_after_anchored(rule, at, tz, None)
}

/// Takes the wall-clock from `anchor`, so a time rolled out of a DST gap does not become the new
/// time. See `Store::series_anchor`; `None` reads it off `at`.
pub fn next_after_anchored(rule: &str, at: i64, tz: Tz, anchor: Option<i64>) -> Option<i64> {
    let r = parse_rule(rule).ok()?;
    let start = tz.timestamp_opt(at, 0).single()?;
    let time = anchor
        .and_then(|a| tz.timestamp_opt(a, 0).single())
        .map(|a| a.time())
        .unwrap_or_else(|| start.time());
    let origin = start.date_naive();
    let mut date = origin;
    // Bounded: one interval step never needs more than four years of days (a yearly 29 Feb).
    for _ in 0..(366 * 4 * r.interval as usize + 1) {
        date += chrono::Duration::days(1);
        let hit = match r.freq {
            // BYDAY narrows a daily rule rather than expanding it (RFC 5545).
            Freq::Daily => {
                (date - origin).num_days() % r.interval as i64 == 0
                    && (r.by_day.is_empty() || r.by_day.contains(&date.weekday()))
            }
            Freq::Weekly => {
                let own = [origin.weekday()];
                let days: &[Weekday] = if r.by_day.is_empty() { &own } else { &r.by_day };
                // RFC 5545 counts calendar weeks (WKST defaults to Monday), not 7-day blocks from
                // the origin.
                let monday = |d: chrono::NaiveDate| {
                    d - chrono::Duration::days(d.weekday().num_days_from_monday() as i64)
                };
                let weeks = (monday(date) - monday(origin)).num_days() / 7;
                days.contains(&date.weekday()) && weeks % r.interval as i64 == 0
            }
            Freq::Monthly => {
                let months = (date.year() - origin.year()) * 12
                    + (date.month() as i32 - origin.month() as i32);
                // BYDAY without an ordinal names every such weekday; with BYMONTHDAY the two
                // intersect.
                let day = match (r.by_month_day, r.by_day.is_empty()) {
                    (Some(dom), _) => date.day() == dom,
                    (None, false) => true,
                    (None, true) => date.day() == origin.day(),
                };
                day && (r.by_day.is_empty() || r.by_day.contains(&date.weekday()))
                    && months % r.interval as i32 == 0
            }
            // BYDAY is refused for YEARLY in `parse_rule`.
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
        // Each against its own clock: an instant against the instant, a named day against the
        // reader's day.
        let past = match r.until {
            Some(Until::Instant(u)) => ts > u,
            Some(Until::Local(u)) => dt > u,
            Some(Until::LastDay(d)) => date > d,
            None => false,
        };
        if past {
            return None;
        }
        return Some(ts);
    }
    None
}

impl crate::core::Core {
    /// `None` for a row from before `series_id`; a store error only costs the unanchored reading.
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
            // The claim and the gate: everything below must run once per completion.
            && self.store.mark_done(id, now).await?
        {
            // A bounded recurrence is bounded by its rows, the just-completed one included.
            if let (Some(rule), Some(at)) = (m.rule.as_deref(), m.at)
                && !self
                    .store
                    .rule_is_exhausted(&m.artifact_id, rule, m.series_id.as_deref())
                    .await?
                // Anchored, as `uncomplete_moment` is, so both look for the same successor instant.
                && let Some(next) = next_after_anchored(
                    rule,
                    at,
                    zone(Some(&m.tz)),
                    self.series_anchor_of(&m).await,
                )
                // Not `has_moment_at`, which also answers yes for a row's `moved_from`; arming
                // needs a parked row.
                && !self
                    .store
                    .moment_parked_at(&m.artifact_id, crate::store::moments::Kind::Due, Some(next))
                    .await?
            {
                // Adopt its own id as series head, or `rule_is_exhausted` would count the successor
                // alone.
                if m.series_id.is_none() {
                    self.store.adopt_into_own_series(&m.id).await?;
                }
                self.store
                    .insert_moment(&crate::store::moments::NewMoment {
                        artifact_id: m.artifact_id,
                        kind: crate::store::moments::Kind::Due,
                        at: Some(next),
                        tz: m.tz,
                        rule: m.rule.clone(),
                        // Not the parent's `cue`: a re-read would take a `cue` row for its own and
                        // delete it. See `Source::Armed`.
                        source: crate::store::moments::Source::Armed,
                        span: m.span,
                        // A NULL series is a pre-migration row, which is the head; `None` would
                        // restart the `COUNT`.
                        series_id: m.series_id.clone().or_else(|| Some(m.id.clone())),
                    })
                    .await?;
            }
            // Warn and carry on: `mark_done` has committed, and failing here would block the
            // retirement for good.
            if let Err(e) = self.store.rearm_remind().await {
                tracing::warn!(error = %e, moment_id = id, "could not re-arm Remind after a completion");
            }
            // Last, after the successor is armed: "no open reminder remains" is about the state
            // this leaves.
            if let Err(e) = self.retire_the_note_if_nothing_is_owed_on_it(id, now).await {
                tracing::warn!(error = %e, moment_id = id, "could not retire the note behind a completed reminder");
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Lifted out so `complete_moment` can run it best-effort.
    async fn retire_the_note_if_nothing_is_owed_on_it(
        &self,
        moment_id: &str,
        now: i64,
    ) -> crate::error::Result<()> {
        if let Some(cid) = self.store.corpus_of_moment(moment_id).await?
            && !self.store.has_open_reminder_for_corpus(&cid).await?
            && self.store.corpus_was_read_as_reminder(&cid).await?
        {
            self.store.retire_corpus(&cid, now).await?;
        }
        Ok(())
    }

    /// The exact inverse of `complete_moment`: the armed successor is deleted and the retired note
    /// restored.
    pub async fn uncomplete_moment(&self, id: &str) -> crate::error::Result<()> {
        let Some(m) = self.store.moment(id).await? else {
            return Ok(());
        };
        // While the row is still done, `next_after` from its `at` is the instant the successor was
        // given.
        if let (Some(rule), Some(at)) = (m.rule.as_deref(), m.at)
            && let Some(next) =
                next_after_anchored(rule, at, zone(Some(&m.tz)), self.series_anchor_of(&m).await)
        {
            self.store
                .delete_armed_occurrence(&m.artifact_id, rule, next)
                .await?;
        }
        self.store.undo_done(id).await?;
        // Unconditional: a corpus that was never retired is already NULL here.
        if let Some(cid) = self.store.corpus_of_moment(id).await? {
            self.store.unretire_corpus(&cid).await?;
        }
        self.store.rearm_remind().await?;
        Ok(())
    }

    /// The rule is restated on every save, so the row stops being the model's to re-read. Answers
    /// the id.
    pub async fn edit_reminder(
        &self,
        artifact_id: &str,
        at: i64,
        tz: &str,
        rule: Option<&str>,
    ) -> crate::error::Result<String> {
        if let Some(r) = rule {
            validate_rule(r).map_err(crate::error::Error::Validation)?;
        }
        let art = self.store.get_artifact(artifact_id).await?;
        let id = match self.store.open_due_for_artifact(artifact_id).await? {
            Some(m) => {
                if m.at != Some(at) || m.tz != tz {
                    self.store.move_moment(&m.id, at, tz).await?;
                }
                self.store.restate_rule(&m.id, rule).await?;
                m.id
            }
            None => {
                // The operator now makes it a reminder, so a past refusal is withdrawn rather than
                // left contradicting it.
                if let Some(cid) = art.corpus_id.as_deref() {
                    let mut meta = self.store.get_corpus(cid).await?.metadata;
                    if meta.is_object() && intent_refused(&meta, Intent::Remind) {
                        allow_intent(&mut meta, Intent::Remind);
                        self.store.set_corpus_metadata(cid, &meta).await?;
                    }
                }
                let id = self
                    .store
                    .insert_moment(&crate::store::moments::NewMoment {
                        artifact_id: artifact_id.to_string(),
                        kind: crate::store::moments::Kind::Due,
                        at: Some(at),
                        tz: tz.to_string(),
                        rule: rule.map(str::to_string),
                        source: crate::store::moments::Source::Set,
                        span: None,
                        series_id: None,
                    })
                    .await?;
                // A new reminder brings back the note a finished one retired.
                if let Some(cid) = self.store.corpus_of_moment(&id).await?
                    && self.store.is_retired(&cid).await?
                {
                    self.store.unretire_corpus(&cid).await?;
                }
                id
            }
        };
        self.store.rearm_remind().await?;
        Ok(id)
    }

    /// Every open row goes and the refusal is recorded, so a re-read does not file it again.
    pub async fn clear_reminder(&self, artifact_id: &str) -> crate::error::Result<bool> {
        let refused = self.set_reminder(artifact_id, false).await?;
        let rest = self.store.delete_open_due(artifact_id).await?;
        self.store.rearm_remind().await?;
        Ok(refused || rest > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn an_artifact(core: &crate::core::Core, text: &str) -> String {
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new(text, "ui"))
            .await
            .unwrap();
        crate::jobs::test_support::drain(core).await;
        core.store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id
    }

    #[tokio::test]
    async fn an_invented_rule_taken_off_by_hand_stays_off_and_done_retires() {
        use crate::store::moments::{Kind, NewMoment, Source};
        let core = crate::core::test_support::test_core().await;
        let aid = an_artifact(&core, "Reisenthel Rucksack kaufen").await;
        let at = berlin()
            .with_ymd_and_hms(2026, 10, 27, 19, 0, 0)
            .unwrap()
            .timestamp();
        let invented = "FREQ=DAILY;INTERVAL=1;BYDAY=TU";
        core.store
            .insert_moment(&NewMoment {
                artifact_id: aid.clone(),
                kind: Kind::Due,
                at: Some(at),
                tz: "Europe/Berlin".into(),
                rule: Some(invented.into()),
                source: Source::Classified,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();

        let id = core
            .edit_reminder(&aid, at, "Europe/Berlin", None)
            .await
            .unwrap();
        assert!(
            !core
                .store
                .set_rule_of_open_due(&aid, Some(at), invented)
                .await
                .unwrap(),
            "a re-read at the same instant leaves the operator's answer alone"
        );
        assert!(
            core.store
                .has_operator_moment(&aid, Kind::Due)
                .await
                .unwrap()
        );
        let m = core.store.moment(&id).await.unwrap().unwrap();
        assert_eq!(m.rule, None);
        assert_eq!(m.series_id, None);
        assert_eq!(
            m.source,
            Source::Classified,
            "how the date got here is unchanged"
        );

        assert!(core.complete_moment(&id).await.unwrap());
        assert!(
            core.store.open_due(0, i64::MAX).await.unwrap().is_empty(),
            "nothing armed"
        );
    }

    #[tokio::test]
    async fn an_entry_becomes_a_reminder_and_back() {
        use crate::store::moments::Source;
        let core = crate::core::test_support::test_core().await;
        let aid = an_artifact(&core, "Signal-Gruppe bereinigen").await;
        let at = berlin()
            .with_ymd_and_hms(2026, 10, 30, 11, 0, 0)
            .unwrap()
            .timestamp();

        let id = core
            .edit_reminder(&aid, at, "Europe/Berlin", Some("FREQ=WEEKLY"))
            .await
            .unwrap();
        let m = core.store.moment(&id).await.unwrap().unwrap();
        assert_eq!(m.source, Source::Set);
        assert_eq!(m.rule.as_deref(), Some("FREQ=WEEKLY"));
        assert_eq!(m.series_id.as_deref(), Some(id.as_str()));

        let later = at + 3_600;
        let again = core
            .edit_reminder(&aid, later, "Europe/Berlin", Some("FREQ=MONTHLY"))
            .await
            .unwrap();
        assert_eq!(again, id, "restated in place, not a second row");
        let m = core.store.moment(&id).await.unwrap().unwrap();
        assert_eq!(
            (m.at, m.rule.as_deref()),
            (Some(later), Some("FREQ=MONTHLY"))
        );

        assert!(
            core.edit_reminder(&aid, at, "Europe/Berlin", Some("FREQ=HOURLY"))
                .await
                .is_err(),
            "a rule outside the subset is refused, not stored"
        );

        assert!(core.clear_reminder(&aid).await.unwrap());
        assert!(
            core.store
                .open_due_for_artifact(&aid)
                .await
                .unwrap()
                .is_none()
        );
        let cid = core
            .store
            .get_artifact(&aid)
            .await
            .unwrap()
            .corpus_id
            .unwrap();
        let meta = core.store.get_corpus(&cid).await.unwrap().metadata;
        assert!(
            intent_refused(&meta, Intent::Remind),
            "a re-read does not file it again"
        );

        core.edit_reminder(&aid, at, "Europe/Berlin", None)
            .await
            .unwrap();
        let meta = core.store.get_corpus(&cid).await.unwrap().metadata;
        assert!(
            !intent_refused(&meta, Intent::Remind),
            "made a reminder by hand, the refusal is withdrawn"
        );
    }

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

    #[tokio::test]
    async fn a_counted_recurrence_with_several_rows_from_before_the_column_stops_on_its_count() {
        use crate::store::moments::{Kind, NewMoment, Source};
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new("Feed the cat", "ui"))
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
        let rule = "FREQ=DAILY;COUNT=3";
        let mut rows = Vec::new();
        for day in [30, 31] {
            rows.push(
                core.store
                    .insert_moment(&NewMoment {
                        artifact_id: aid.clone(),
                        kind: Kind::Due,
                        at: Some(
                            berlin()
                                .with_ymd_and_hms(2026, 8, day, 9, 0, 0)
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
                    .unwrap(),
            );
        }
        sqlx::query!(
            "UPDATE moments SET series_id = NULL WHERE artifact_id = ?",
            &aid
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        sqlx::query!(
            "UPDATE moments SET done_at = ? WHERE id = ?",
            crate::store::now() - 86_400,
            &rows[0]
        )
        .execute(&core.store.pool)
        .await
        .unwrap();

        core.complete_moment(&rows[1]).await.unwrap();
        let third = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(third.len(), 1, "the third occurrence is armed");
        core.complete_moment(&third[0].moment.id).await.unwrap();

        assert!(
            core.store.open_due(0, i64::MAX).await.unwrap().is_empty(),
            "three of three, and a fourth was armed"
        );
    }

    #[tokio::test]
    async fn a_counted_recurrence_on_an_upgraded_base_still_stops() {
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
        sqlx::query!("UPDATE moments SET series_id = NULL WHERE id = ?", &first)
            .execute(&core.store.pool)
            .await
            .unwrap();

        core.complete_moment(&first).await.unwrap();
        let second = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(second.len(), 1, "the second occurrence is armed");
        core.complete_moment(&second[0].moment.id).await.unwrap();

        assert!(
            core.store.open_due(0, i64::MAX).await.unwrap().is_empty(),
            "two of two, and the count started over"
        );
    }

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
        core.store.insert_moment(&row(day(3))).await.unwrap();
        assert!(core.complete_moment(&first).await.unwrap());
        let open = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(open.len(), 1, "no second row on the same instant: {open:?}");
        assert_eq!(open[0].moment.at, Some(day(3)));
    }

    #[test]
    fn examples_come_from_the_prototypes_in_the_readers_language() {
        let remind = examples_for("de-DE,de;q=0.9,en;q=0.8");
        assert!(remind.starts_with("erinnere mich"), "{remind}");

        let remind = examples_for("");
        assert!(
            remind.starts_with("remind me"),
            "English is the fallback: {remind}"
        );

        let remind = examples_for("xx-YY");
        assert!(
            remind.starts_with("remind me"),
            "an unknown language falls back too"
        );
    }

    #[test]
    fn every_language_carries_reminders_and_no_sentence_twice() {
        let langs: std::collections::BTreeSet<&str> =
            PROTOTYPES.iter().map(|(_, l, _)| *l).collect();
        assert!(
            langs.contains("en"),
            "English is the fallback and must exist"
        );
        for l in &langs {
            let n = PROTOTYPES
                .iter()
                .filter(|(i, x, _)| *i == Intent::Remind && x == l)
                .count();
            assert!(n >= 2, "{l} has {n} remind prototypes");
            assert!(!examples_for(l).is_empty(), "{l} resolves to an example");
        }
        let mut seen = std::collections::BTreeSet::new();
        for (_, _, p) in PROTOTYPES {
            assert!(seen.insert(*p), "duplicate prototype: {p}");
        }
    }

    #[test]
    fn a_refusal_is_recorded_and_withdrawn() {
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

        allow_intent(&mut meta, Intent::Remind);
        assert!(!intent_refused(&meta, Intent::Remind));
        assert!(
            meta.get("intent_refused").is_none(),
            "the last one takes the key with it"
        );

        let mut old = serde_json::json!({"intent_refused": ["journal"]});
        assert!(!intent_refused(&old, Intent::Remind));
        allow_intent(&mut old, Intent::Remind);
        assert_eq!(old["intent_refused"], serde_json::json!(["journal"]));
    }

    fn berlin() -> chrono_tz::Tz {
        chrono_tz::Tz::Europe__Berlin
    }

    #[test]
    fn a_year_no_calendar_can_spell_is_refused_rather_than_panicked_over() {
        let ceiling = chrono::NaiveDateTime::MAX;
        assert_eq!(resolve_local(ceiling, berlin()), None);
        assert_eq!(resolve_local(chrono::NaiveDateTime::MIN, berlin()), None);
        let past_9999 = chrono::NaiveDate::from_ymd_opt(20_000, 1, 1)
            .expect("chrono spells it even though a moment may not")
            .and_hms_opt(0, 0, 0)
            .unwrap();
        assert_eq!(resolve_local(past_9999, berlin()), None);
        // The bound is on the instant, not the local year, so 10000-01-01 east of UTC is still
        // allowed.
        let just_inside = chrono::NaiveDate::from_ymd_opt(10_000, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        assert!(resolve_local(just_inside, berlin()).is_some_and(|at| at <= END_OF_9999));
        let ordinary = chrono::NaiveDate::from_ymd_opt(2026, 9, 11)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        let at = resolve_local(ordinary, berlin()).expect("an ordinary date");
        assert!((YEAR_ONE..=END_OF_9999).contains(&at));
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
            "FREQ=DAILY;INTERVAL=0",
            "FREQ=DAILY;INTERVAL=4000000000",
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
    fn a_byday_list_is_bounded_by_the_days_that_exist() {
        assert_eq!(
            parse_rule("FREQ=WEEKLY;BYDAY=MO,MO,MO,TU,MO")
                .unwrap()
                .by_day,
            vec![Weekday::Mon, Weekday::Tue]
        );
        let huge = format!(
            "FREQ=MONTHLY;INTERVAL=100;BYDAY={}",
            "MO,".repeat(50_000) + "MO"
        );
        assert_eq!(parse_rule(&huge).unwrap().by_day, vec![Weekday::Mon]);
        let mon = berlin()
            .with_ymd_and_hms(2026, 8, 31, 9, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(
            local(next_after("FREQ=WEEKLY;BYDAY=MO,WE,MO,WE", mon, berlin()).unwrap()),
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
    fn a_date_only_until_is_the_readers_day_not_a_utc_instant() {
        let la: Tz = "America/Los_Angeles".parse().unwrap();
        let at = la
            .with_ymd_and_hms(2026, 8, 31, 18, 0, 0)
            .unwrap()
            .timestamp();
        let next = next_after("FREQ=DAILY;UNTIL=20260901", at, la)
            .expect("the last day the operator named was dropped");
        assert_eq!(
            la.timestamp_opt(next, 0)
                .unwrap()
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            "2026-09-01 18:00"
        );
        assert!(next_after("FREQ=DAILY;UNTIL=20260901", next, la).is_none());

        // The `Z` form means its UTC instant for everyone.
        assert!(next_after("FREQ=DAILY;UNTIL=20260901T000000Z", at, la).is_none());
    }

    #[test]
    fn a_floating_until_is_a_wall_clock_in_the_readers_zone() {
        let la: Tz = "America/Los_Angeles".parse().unwrap();
        let at = la
            .with_ymd_and_hms(2026, 8, 31, 18, 0, 0)
            .unwrap()
            .timestamp();
        let next = next_after("FREQ=DAILY;UNTIL=20260901T235959", at, la)
            .expect("a floating UNTIL is not a rule outside the subset");
        assert_eq!(
            la.timestamp_opt(next, 0)
                .unwrap()
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            "2026-09-01 18:00"
        );
        assert!(next_after("FREQ=DAILY;UNTIL=20260901T235959", next, la).is_none());
        assert!(next_after("FREQ=DAILY;UNTIL=20260901T235959", at, la).is_some());
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
        let dt = chrono::NaiveDate::from_ymd_opt(2027, 3, 28)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap();
        let ts = resolve_local(dt, berlin()).unwrap();
        assert_eq!(local(ts), "2027-03-28 03:00");
    }
}
