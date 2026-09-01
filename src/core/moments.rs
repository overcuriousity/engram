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

/// Whether a cue may decide on its own.
///
/// `remind me` and `dear diary` are unmistakable — nobody opens a note that
/// way by accident. The day words are not. *heute*, *hoy*, *oggi*, *bugün*,
/// *сегодня* open a diary entry and a to-do about equally often, and because
/// a journal cue only has to sit at the head of the text, `Heute den Bericht
/// abgeben` was filed as a diary entry on the strength of its first word.
/// `ingest.rs` names that collision and could only resolve it for a door that
/// had already forced `remind`, which the capture box never does.
///
/// So a weak cue stops overruling the vector and speaks last instead: it is
/// consulted only where the classifier declines to fire at all. Its recall is
/// kept — a note that resembles nothing and opens with *Heute* is still an
/// entry — and its veto is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strength {
    /// Decides, ahead of the classifier.
    Strong,
    /// Decides only where the classifier found nothing.
    Weak,
}

/// Lowercase, whole-word. A `journal` cue must sit at the head of the text.
pub const CUES: &[(Intent, &str, Strength)] = &[
    (Intent::Remind, "remind me", Strength::Strong),
    (Intent::Remind, "erinnere mich", Strength::Strong),
    (Intent::Remind, "erinner mich", Strength::Strong),
    (Intent::Remind, "rappelle-moi", Strength::Strong),
    (Intent::Remind, "rappelle moi", Strength::Strong),
    (Intent::Remind, "recuérdame", Strength::Strong),
    (Intent::Remind, "recuerdame", Strength::Strong),
    (Intent::Remind, "lembre-me", Strength::Strong),
    (Intent::Remind, "lembra-me", Strength::Strong),
    (Intent::Remind, "ricordami", Strength::Strong),
    (Intent::Remind, "herinner me", Strength::Strong),
    (Intent::Remind, "przypomnij mi", Strength::Strong),
    (Intent::Remind, "hatırlat", Strength::Strong),
    (Intent::Remind, "напомни", Strength::Strong),
    (Intent::Journal, "today i", Strength::Strong),
    (Intent::Journal, "dear diary", Strength::Strong),
    (Intent::Journal, "heute", Strength::Weak),
    (Intent::Journal, "liebes tagebuch", Strength::Strong),
    (Intent::Journal, "aujourd'hui", Strength::Weak),
    (Intent::Journal, "hoy", Strength::Weak),
    (Intent::Journal, "hoje", Strength::Weak),
    (Intent::Journal, "oggi", Strength::Weak),
    (Intent::Journal, "vandaag", Strength::Weak),
    (Intent::Journal, "dzisiaj", Strength::Weak),
    (Intent::Journal, "dziś", Strength::Weak),
    (Intent::Journal, "bugün", Strength::Weak),
    (Intent::Journal, "сегодня", Strength::Weak),
];

/// Sentence-shaped on purpose: the embedder places "remind me to X" near other
/// requests for future action, and a bare cue word near a dictionary entry.
///
/// Four phrasings per intent per language at minimum, and the last two of the
/// first four carry no cue word at all. That is where this table earns its
/// keep: `CUES` already catches *erinnere mich* by string match, and nothing
/// catches *nicht vergessen: am Freitag die Rechnung abschicken* except a
/// vector near one of these. `classify` takes the maximum over the table, so
/// a phrasing added here can only widen what is recognised — it cannot pull
/// an existing match off its prototype. What can is `DECOYS`, which every
/// match here has to out-score.
///
/// The language tag is carried in the row rather than in a parallel array: a
/// tag is not recoverable from a sentence, and at eighty rows two lists kept
/// in step by counting is a defect waiting for its first careless insert.
///
/// The **first** row of each (intent, language) pair is the one a reader sees
/// under the box — see `examples_for` — so it is written to be read, not only
/// to be embedded.
///
/// Adding a row invalidates the cached vectors on its own: `Core::prototypes`
/// re-embeds whenever the cache length and this length disagree.
pub const PROTOTYPES: &[(Intent, &str, &str)] = &[
    // ── en ──
    (Intent::Remind, "en", "remind me to send the invoice on friday"),
    (Intent::Remind, "en", "remind me next week to call the bank"),
    (Intent::Remind, "en", "don't forget to book the train tickets tomorrow"),
    (Intent::Remind, "en", "i need to renew my passport before the end of the month"),
    (Intent::Journal, "en", "today i finally got the migration working"),
    (Intent::Journal, "en", "long day, nothing got done, but the walk helped"),
    (Intent::Journal, "en", "slept badly again, the whole afternoon was a write-off"),
    (Intent::Journal, "en", "quiet evening, cooked properly for once and felt better for it"),
    // ── de ──
    (Intent::Remind, "de", "erinnere mich morgen an den zahnarzttermin"),
    (Intent::Remind, "de", "erinnere mich nächste woche an die steuererklärung"),
    (Intent::Remind, "de", "nicht vergessen: am freitag die rechnung abschicken"),
    (Intent::Remind, "de", "ich muss bis ende des monats den pass verlängern"),
    // Keyword-fragment style, the way a quick capture is actually typed:
    // trigger word, then noun phrases, then a day and time, with no verb
    // and no sentence structure tying them together.
    (Intent::Remind, "de", "erinnerung termin foto ausweis mittwoch 0900 zimmer a323"),
    (Intent::Journal, "de", "heute war ein langer tag und ich bin müde"),
    (Intent::Journal, "de", "heute morgen endlich den fehler gefunden"),
    (Intent::Journal, "de", "wieder schlecht geschlafen, der ganze nachmittag war für die katz"),
    (Intent::Journal, "de", "ruhiger abend, endlich mal richtig gekocht und es ging mir besser"),
    // ── fr ──
    (Intent::Remind, "fr", "rappelle-moi d'appeler la banque lundi"),
    (Intent::Remind, "fr", "rappelle-moi la semaine prochaine de renouveler l'assurance"),
    (Intent::Remind, "fr", "ne pas oublier d'envoyer la facture vendredi"),
    (Intent::Remind, "fr", "je dois refaire mon passeport avant la fin du mois"),
    (Intent::Journal, "fr", "aujourd'hui j'ai enfin terminé le rapport"),
    (Intent::Journal, "fr", "longue journée, rien d'avancé, mais la promenade m'a fait du bien"),
    (Intent::Journal, "fr", "encore mal dormi, tout l'après-midi est passé à côté"),
    (Intent::Journal, "fr", "soirée tranquille, j'ai enfin cuisiné correctement"),
    // ── es ──
    (Intent::Remind, "es", "recuérdame pagar el alquiler el día uno"),
    (Intent::Remind, "es", "recuérdame la semana que viene llamar al banco"),
    (Intent::Remind, "es", "no olvidar enviar la factura el viernes"),
    (Intent::Remind, "es", "tengo que renovar el pasaporte antes de fin de mes"),
    (Intent::Journal, "es", "hoy fue un día tranquilo, leí mucho"),
    (Intent::Journal, "es", "día largo, no avancé nada, pero el paseo ayudó"),
    (Intent::Journal, "es", "otra vez dormí mal, se me fue la tarde entera"),
    (Intent::Journal, "es", "por fin cociné bien esta noche y me sentó bien"),
    // ── pt ──
    (Intent::Remind, "pt", "lembre-me de renovar o passaporte em setembro"),
    (Intent::Remind, "pt", "lembre-me na próxima semana de ligar para o banco"),
    (Intent::Remind, "pt", "não esquecer de enviar a fatura na sexta"),
    (Intent::Remind, "pt", "preciso pagar o aluguel até o dia primeiro"),
    (Intent::Journal, "pt", "hoje acordei cedo e fui correr"),
    (Intent::Journal, "pt", "dia longo, não rendi nada, mas a caminhada ajudou"),
    (Intent::Journal, "pt", "dormi mal de novo, perdi a tarde inteira"),
    (Intent::Journal, "pt", "noite tranquila, cozinhei direito pela primeira vez em semanas"),
    // ── it ──
    (Intent::Remind, "it", "ricordami di comprare i biglietti domani"),
    (Intent::Remind, "it", "ricordami la settimana prossima di chiamare la banca"),
    (Intent::Remind, "it", "non dimenticare di mandare la fattura venerdì"),
    (Intent::Remind, "it", "devo rinnovare il passaporto entro fine mese"),
    (Intent::Journal, "it", "oggi è stata una giornata pesante"),
    (Intent::Journal, "it", "giornata lunga, non ho concluso niente, ma la passeggiata è servita"),
    (Intent::Journal, "it", "ho dormito male di nuovo, pomeriggio buttato"),
    (Intent::Journal, "it", "serata tranquilla, finalmente ho cucinato come si deve"),
    // ── nl ──
    (Intent::Remind, "nl", "herinner me eraan om de huur te betalen"),
    (Intent::Remind, "nl", "herinner me volgende week aan het gesprek met de bank"),
    (Intent::Remind, "nl", "niet vergeten om vrijdag de factuur te versturen"),
    (Intent::Remind, "nl", "ik moet mijn paspoort verlengen voor het eind van de maand"),
    (Intent::Journal, "nl", "vandaag eindelijk de tuin gedaan"),
    (Intent::Journal, "nl", "lange dag, niets afgekregen, maar die wandeling hielp"),
    (Intent::Journal, "nl", "weer slecht geslapen, de hele middag was verloren"),
    (Intent::Journal, "nl", "rustige avond, eindelijk weer eens goed gekookt"),
    // ── pl ──
    (Intent::Remind, "pl", "przypomnij mi jutro o spotkaniu z lekarzem"),
    (Intent::Remind, "pl", "przypomnij mi w przyszłym tygodniu zadzwonić do banku"),
    (Intent::Remind, "pl", "nie zapomnieć wysłać faktury w piątek"),
    (Intent::Remind, "pl", "muszę wyrobić paszport do końca miesiąca"),
    (Intent::Journal, "pl", "dzisiaj byłem u dentysty, poszło dobrze"),
    (Intent::Journal, "pl", "długi dzień, nic nie zrobiłem, ale spacer pomógł"),
    (Intent::Journal, "pl", "znowu źle spałem, całe popołudnie zmarnowane"),
    (Intent::Journal, "pl", "spokojny wieczór, w końcu porządnie ugotowałem"),
    // ── tr ──
    (Intent::Remind, "tr", "yarın bana faturayı ödemeyi hatırlat"),
    (Intent::Remind, "tr", "gelecek hafta bankayı aramayı hatırlat"),
    (Intent::Remind, "tr", "cuma günü faturayı göndermeyi unutma"),
    (Intent::Remind, "tr", "ay sonuna kadar pasaportu yenilemem lazım"),
    (Intent::Journal, "tr", "bugün çok yorucu bir gündü"),
    (Intent::Journal, "tr", "uzun bir gündü, hiçbir şey ilerlemedi ama yürüyüş iyi geldi"),
    (Intent::Journal, "tr", "yine kötü uyudum, bütün öğleden sonra boşa gitti"),
    (Intent::Journal, "tr", "sakin bir akşam, uzun zamandır ilk kez doğru düzgün yemek yaptım"),
    // ── ru ──
    (Intent::Remind, "ru", "напомни мне завтра позвонить маме"),
    (Intent::Remind, "ru", "напомни мне на следующей неделе про страховку"),
    (Intent::Remind, "ru", "не забыть отправить счёт в пятницу"),
    (Intent::Remind, "ru", "нужно продлить паспорт до конца месяца"),
    (Intent::Journal, "ru", "сегодня наконец закончил проект"),
    (Intent::Journal, "ru", "длинный день, ничего не сделал, но прогулка помогла"),
    (Intent::Journal, "ru", "опять плохо спал, весь день насмарку"),
    (Intent::Journal, "ru", "тихий вечер, наконец нормально приготовил ужин"),
];

/// One reminder and one journal example, in the reader's language where the
/// prototype table has it and in English where it does not.
///
/// Drawn from `PROTOTYPES` rather than written out again. These are examples
/// of what the classifier reads, and a second copy of them would drift from it
/// the first time a prototype is retuned — the page would then be teaching a
/// phrasing the base no longer recognises, which is worse than teaching none.
///
/// `accept_language` is the raw header. Only the primary subtag of the first
/// entry is read: `de-DE,de;q=0.9,en;q=0.8` is a reader who wants German, and
/// weighing the rest to discover that is arithmetic for nothing.
pub fn examples_for(accept_language: &str) -> (&'static str, &'static str) {
    let want = accept_language
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
        .to_ascii_lowercase();
    let pick = |intent: Intent| -> &'static str {
        let of = |lang: &str| {
            PROTOTYPES.iter().find(|(i, l, _)| *i == intent && *l == lang).map(|(_, _, p)| *p)
        };
        of(&want).or_else(|| of("en")).unwrap_or("")
    };
    (pick(Intent::Remind), pick(Intent::Journal))
}

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

fn cue_of(text: &str, want: Strength) -> Option<Intent> {
    let opening = text.chars().take(OPENING_CHARS).collect::<String>().to_lowercase();
    CUES.iter()
        .filter(|(_, _, st)| *st == want)
        .find(|(intent, c, _)| contains_cue(&opening, c, *intent == Intent::Journal))
        .map(|(intent, _, _)| *intent)
}

/// A cue that decides. What the capture box echoes and what the stage reads
/// before it looks at a vector.
pub fn cue(text: &str) -> Option<Intent> {
    cue_of(text, Strength::Strong)
}

/// A cue that only speaks where the classifier found nothing — see
/// [`Strength`]. Never consulted before it.
pub fn weak_cue(text: &str) -> Option<Intent> {
    cue_of(text, Strength::Weak)
}

/// The nearest prototype whatever it scored — the number `classify` weighed,
/// with none of its guards. Recorded on the corpus so a verdict, and a
/// near-miss just under the line, are both legible afterwards; `classify`
/// threw this away and left nothing to tune against but argument.
pub fn nearest(vec: &[f32], p: &Protos) -> Option<(Intent, f32)> {
    p.vectors.iter().map(|(i, v)| (*i, cosine(vec, v))).max_by(|a, b| a.1.total_cmp(&b.1))
}

/// Whether the operator has already said this note is not that.
///
/// `metadata.intent_refused` is a list of intent names, and it has to outlive
/// a re-embed: the moments stage derives the intent again every time an
/// artifact is re-embedded, so without a record of the refusal a reindex or a
/// switched embed model quietly files the note again over somebody who had
/// said no. The journal side has worked this way for a while under
/// `entry_refused`; that key is still read here, so a base written before this
/// keeps its refusals.
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
    let mut all: Vec<serde_json::Value> =
        meta["intent_refused"].as_array().cloned().unwrap_or_default();
    all.push(serde_json::Value::String(intent.as_str().to_string()));
    meta["intent_refused"] = serde_json::Value::Array(all);
}

/// Withdraw one, the legacy key included — turning a filing back on by hand is
/// the operator saying the stage may read this note again.
pub fn allow_intent(meta: &mut serde_json::Value, intent: Intent) {
    if let Some(a) = meta["intent_refused"].as_array() {
        let kept: Vec<serde_json::Value> =
            a.iter().filter(|v| v.as_str() != Some(intent.as_str())).cloned().collect();
        if kept.is_empty() {
            if let Some(m) = meta.as_object_mut() {
                m.remove("intent_refused");
            }
        } else {
            meta["intent_refused"] = serde_json::Value::Array(kept);
        }
    }
    if intent == Intent::Journal && let Some(m) = meta.as_object_mut() {
        m.remove("entry_refused");
    }
}

/// Ordinary notes: what a base is mostly made of, and what neither intent may
/// claim.
///
/// `classify` scores the best prototype and, before these, had nothing for
/// that best to beat — a note only had to clear an absolute line, so a
/// technical note sitting a hair over it became a reminder with nothing
/// arguing the other way. There was no negative class; "ordinary capture" was
/// the residue. These are the negative class, and they cost one embed call
/// once per model: the winner now has to out-score every one of them, which
/// turns a threshold into a comparison.
///
/// Deliberately spread across the shapes a knowledge base actually holds — a
/// fact, a command, a connection string, a link, a shortcut — rather than
/// near-misses of the two intents. A decoy written to sit just under a
/// prototype would be tuning, and tuning against eight sentences is how a
/// threshold becomes a fixture nobody dares move.
pub const DECOYS: &[&str] = &[
    "the admin port is 8443 and the console listens on 9443",
    "mount the image read-only with ro,loop so nothing is written back",
    "ext4 replays its journal on mount, which writes to the device",
    "the staging connection string lives in the vault under db/staging",
    "the invoice template is on the shared drive under finance",
    "vitamin d is fat soluble and is stored in the liver",
    "https://example.org/handbook — the onboarding checklist",
    "ctrl+shift+p opens the command palette",
];

/// The best prototype, if it clears the line and beats every decoy.
///
/// Maximum over an intent's prototypes rather than a mean: a note matches one
/// phrasing, not the average of ten languages. The decoy test is the second
/// half of the same idea — the winner is the nearest sentence of any kind,
/// and a note nearer an ordinary one than to either intent is an ordinary
/// note.
pub fn classify(vec: &[f32], p: &Protos) -> Option<(Intent, f32)> {
    let best = p
        .vectors
        .iter()
        .map(|(i, v)| (*i, cosine(vec, v)))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    if best.1 < p.line {
        return None;
    }
    if p.decoys.iter().any(|d| cosine(vec, d) >= best.1) {
        return None;
    }
    Some(best)
}

/// The lowest a configured `time.intent_at` may pull the bar, whatever the
/// file says. A floor under the floor: `classify` already asks the winner to
/// beat every decoy, and this only stops a base that has set the line to
/// nothing from calling every note an intent.
pub const INTENT_LINE_FLOOR: f32 = 0.70;

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
    match iana_time_zone::get_timezone().ok().filter(|n| n.parse::<Tz>().is_ok()) {
        Some(n) => n,
        None => {
            tracing::info!("the platform cannot name its zone; dates from doors that send none are read in UTC");
            Tz::UTC.name().to_string()
        }
    }
});

/// The zone a door that sent none is read in: what `time.default_tz` names, or
/// — where it is empty, which is the shipped default — the server's own.
///
/// The empty default has always been documented as "the server's zone", in the
/// field's own doc and in `config.example.toml`. It resolved to UTC, because
/// `""` is not a zone `zone` can parse, so every capture from a door with no
/// zone of its own had its dates read, stored and rendered two hours off on a
/// Berlin server — silently, and with the moment then labelled `UTC` as though
/// somebody had asked for it.
pub fn default_zone_name(configured: &str) -> String {
    let configured = configured.trim();
    if configured.is_empty() {
        return SERVER_ZONE.clone();
    }
    configured.to_string()
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

/// Month words that are also everyday words, in the language they would be
/// read from. A bare `<number> <word>` is only a date for these when something
/// else in the sentence says so — see `named_month_is_supported`.
///
/// The list is short on purpose, and short in two directions.
///
/// It is about ambiguity and not about abbreviation: `sept`, `okt` and
/// `ноября` are month words and nothing else, and they go on reading bare.
/// `may` is an everyday English modal, `march` and `marches` are verbs, `mars`
/// is a planet, `mart` and `marche` are markets. Without this, "Section 3 may
/// be revised", "review 5 may need work" and "the last 2 march entries" each
/// put an event on the front page under "Coming up".
///
/// And it stops at words that are ambiguous in the language a reader would
/// actually be typing them in. `mai`, `maj` and `maja` are May and nothing else
/// where they are written, and *3 mai* is how that date is written in French —
/// demanding a preposition for it would cost more dates than it saved. Every
/// word here has an everyday English reading, because that is where the
/// collisions were.
///
/// Matched against the word as written, lowercased, and not against the month
/// it resolves to: it is the spelling that is ambiguous, not September.
const AMBIGUOUS_MONTH_WORDS: &[&str] =
    &["mar", "march", "marche", "marches", "mars", "mart", "may"];

/// Words that put a date after them: the last word before the match is one of
/// these, and the number that follows is a day rather than a count.
///
/// Prepositions and nothing else. A verb like *review* or a noun like *section*
/// is exactly what the ambiguous readings turned out to be sitting behind.
const DATE_MARKERS: &[&str] = &[
    "on", "at", "by", "from", "until", "till", "since", "before", "after",
    "am", "an", "ab", "bis", "seit", "vom", "den", "zum",
    "le", "du", "des", "jusqu'au", "dès",
    "el", "al", "desde", "hasta", "em", "no", "na", "até", "de",
    "il", "dal", "entro",
    "op", "vanaf", "tot",
    "w", "do", "od", "dnia",
    "в", "до", "с", "от",
];

/// Whether a `<number> <month word>` reading has something behind it beyond the
/// two tokens themselves: a dot (*3. Mai*, *Mar. 3*), a preposition in front
/// (*on 3 May*), a year behind it (*3 May 2027*), or a time (*3 May at 10*).
fn named_month_is_supported(text: &str, matched: &str, start: usize, year: Option<i32>, timed: bool) -> bool {
    if matched.contains('.') || year.is_some() || timed {
        return true;
    }
    text[..start]
        .split_whitespace()
        .next_back()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'').to_lowercase())
        .is_some_and(|w| DATE_MARKERS.contains(&w.as_str()))
}

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
/// A time directly after a date: `T14:30`, ` um 10 Uhr`, ` at 3pm`, `, 14:00`,
/// ` 13:45 Uhr`.
///
/// Accepted only with a marker — a joining word, a colon or a suffix — so
/// `01.03.2027 5 apples` does not read the five as an hour. The joining word
/// is one of three ways to be marked and not a requirement: *Freitag 13:45
/// Uhr* is as plainly a time as *Freitag um 13:45*, and demanding the *um*
/// silently returned nine in the morning for it.
static TIME_AFTER: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)^(?:T|\s*(?:(um|at|à|a las|alle|om|o|saat|в|,)\s*)?)(\d{1,2})(?::(\d{2}))?\s*(am|pm|uhr|h)?\b").unwrap()
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
    time_read(rest).unwrap_or((DEFAULT_HOUR, 0))
}

/// The same, saying whether it read anything at all. A caller with a second
/// place to look needs to tell "no time here" from "nine in the morning",
/// which the defaulted form has thrown away by the time it returns.
fn time_read(rest: &str) -> Option<(u32, u32)> {
    let c = TIME_AFTER.captures(rest)?;
    let marked = c.get(1).is_some() || c.get(3).is_some() || c.get(4).is_some() || rest.starts_with('T');
    if !marked {
        return None;
    }
    let mut hour: u32 = c[2].parse().ok()?;
    let suffix = c.get(4).map(|m| m.as_str().to_lowercase());
    if suffix.as_deref() == Some("pm") && hour < 12 {
        hour += 12;
    }
    if suffix.as_deref() == Some("am") && hour == 12 {
        hour = 0;
    }
    if hour > 23 {
        return None;
    }
    Some((hour, c.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(0)))
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
        let time = time_read(&text[end..]);
        // The one reading with no punctuation of its own to prove it is a date:
        // two words, a small number and a word that happens to begin a month
        // name. Where that word is also an everyday word, something else in the
        // sentence has to say so.
        if AMBIGUOUS_MONTH_WORDS.contains(&name.to_lowercase().as_str())
            && !named_month_is_supported(text, m.as_str(), m.start(), year, time.is_some())
        {
            continue;
        }
        let (h, mi) = time.unwrap_or((DEFAULT_HOUR, 0));
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
/// `in 10 minuten`, `in half an hour`, `daqui a 20 minutos`, `через час`, …
///
/// The preposition is required, and it is what separates an offset from a
/// duration: *das 30-minuten meeting* says how long something takes, *in 30
/// minuten* says when to be reminded. `IN_N` can do without one because a note
/// naming three days rarely means anything else.
///
/// The ten languages the month and weekday tables carry, minus Turkish, which
/// puts its word after the unit and is read by `SONRA` below.
static IN_CLOCK: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        // Longest first, so `dentro de` is not read as a bare `de`.
        r"(?i)\b(?:daqui a|dentro de|binnen|dans|über|in|en|em|tra|fra|over|za|через)\s+",
        // Half, where it stands before the count: *in half an hour*.
        r"(?P<h1>half\s+(?:an\s+)?|pół\s+)?",
        // The count, or the written-out one that stands in for it. Both are
        // optional: *через час* and *za godzinę* name the hour and nothing
        // else, which `bare` below is what allows.
        r"(?:(?P<n>\d{1,4})|(?P<art>an|a|une?|un['’]|eine[rm]?|un[ao]|uma|um|een|één|jedn[aey]))?",
        r"(?:['’]\s*|\s+)?",
        // And half where it stands after it: *in einer halben stunde*.
        r"(?P<h2>halb(?:e|en|es)?\s+)?",
        r"(?P<unit>minut(?:e|en|es|os?|y|ów|i)?|mins?\.?|минут(?:ы|у)?|dakika|",
        r"stunden?|std\.?|hours?|hrs?|h|heures?|horas?|ore|ora|uur|godzin[aeyę]?|saat|час(?:а|ов)?)\b",
    ))
    .unwrap()
});

/// Turkish puts it after the unit: *10 dakika sonra*, *bir saat sonra*.
static SONRA: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b(?:(\d{1,4})|bir|yarım)\s+(dakika|saat)\s+sonra\b").unwrap()
});

/// The hour, named alone. *In an hour* is idiomatic with no article in Russian
/// and Polish and elided in Italian, so the count may be missing — but only
/// for these, and only in the singular. A bare plural is a span of time being
/// described (*in Stunden*, *en horas*), never a time to be reminded at.
const BARE_ONE: [&str; 9] =
    ["час", "godzinę", "godzina", "ora", "stunde", "heure", "hour", "hora", "uur"];

static AT_TIME: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b(?:at|um|à|a las|alle|om|o|saat|в)\s+(\d{1,2})(?::(\d{2}))?\s*(am|pm|uhr|h)?\b").unwrap()
});

/// A bare four-digit clock, no colon and no marker: `Mittwoch 0900`. Unsafe as
/// a general rule — `TIME_AFTER` exists precisely to keep an unmarked number
/// from being read as an hour — but exactly four digits sitting right after
/// the day name is not a house number, and demanding a colon silently
/// returned nine in the morning for the one shape a phone keypad naturally
/// produces. Anchored to exactly four digits so a five-digit run (an ID, a
/// partial date) fails the trailing `\b` and is left alone.
static TIME_BARE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"(?i)^\s*(\d{2})(\d{2})\b").unwrap());

/// What may follow a bare clock and leave it a clock: the end of the note,
/// a mark of punctuation, or the word that names the hour outright. A letter
/// or a digit after it means the run was counting something.
static TIME_BARE_TAIL: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)^(?:\s*$|[^\p{L}\p{N}\s]|\s*(?:uhr|h)\b)").unwrap());

fn time_bare(rest: &str) -> Option<(u32, u32)> {
    let c = TIME_BARE.captures(rest)?;
    let hour: u32 = c[1].parse().ok()?;
    let minute: u32 = c[2].parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    // A four-digit run in this position is a clock only when something says
    // so. The keypad's own shape says it with a padded hour — `Mittwoch 0900`,
    // which is what this rule was written for; every other run has to be
    // followed by an ending, a punctuation mark, or `Uhr`. Left unguarded the
    // hour and minute bounds were the only test, so `Montag 1000 Euro
    // überweisen` was dated 10:00 and `Freitag 2026 Steuererklärung` 20:26 —
    // amounts and years sit in exactly this position, and often enough to
    // outnumber the shape being read for. What this sends back to the 09:00
    // default instead — `Mittwoch 1430 Meeting` — is wording the model path
    // reads better than any rule here could.
    if !c[1].starts_with('0') && !TIME_BARE_TAIL.is_match(&rest[c.get(0)?.end()..]) {
        return None;
    }
    Some((hour, minute))
}

/// The second an offset inside the day names, counted off the second the note
/// was captured — `in 10 minuten`, `in einer halben stunde`, `in 2 Std.`.
///
/// Arithmetic rather than a date: `relative_date` only ever considers a date
/// strictly after today, and `IN_N` counts nothing shorter than a day, so
/// before this the shape had nowhere to land. It is the rule path's answer
/// only; where a model dates reminders it is asked first, and a note whose
/// wording this misses — a typo, a quarter of an hour, *gleich* — is exactly
/// what the model is better at.
pub fn clock_offset(text: &str, captured_at: i64) -> Option<Found> {
    let lower = text.to_lowercase();
    if let Some(c) = SONRA.captures(&lower) {
        let n: i64 = c.get(1).map_or(Ok(1), |m| m.as_str().parse()).ok()?;
        let seconds = if &c[2] == "dakika" { n * 60 } else { n * 3_600 };
        // `yarım` is the half, and it stands where the count would.
        let seconds = if c.get(1).is_none() && lower.contains("yarım") { seconds / 2 } else { seconds };
        return (seconds > 0).then(|| Found { at: captured_at + seconds, span: c[0].to_string() });
    }
    let c = IN_CLOCK.captures(&lower)?;
    let unit = c["unit"].to_lowercase();
    // Nothing between the preposition and the unit: only the hour, and only
    // where naming it alone is how the language says it.
    if c.name("n").is_none() && c.name("art").is_none() && !BARE_ONE.contains(&unit.as_str()) {
        return None;
    }
    // No digits is the written-out one: *in an hour*, *in einer stunde*.
    let n: i64 = c.name("n").map_or(Ok(1), |m| m.as_str().parse()).ok()?;
    let minutes = unit.starts_with("min") || unit.starts_with("мин") || unit.starts_with("dak");
    let seconds = if minutes { n * 60 } else { n * 3_600 };
    // *Half an hour*, and only ever half of what was named: half of ten
    // minutes is a precision nobody writing this sentence meant.
    let seconds = if c.name("h1").is_some() || c.name("h2").is_some() { seconds / 2 } else { seconds };
    (seconds > 0).then(|| Found { at: captured_at + seconds, span: c[0].to_string() })
}

/// The nearest future date the relative words name, or none. A weekday, with
/// or without "next", is the next such day strictly after today.
pub fn relative_date(text: &str, captured_at: i64, tz: Tz) -> Option<Found> {
    let lower = text.to_lowercase();
    let today = tz.timestamp_opt(captured_at, 0).single()?.date_naive();
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
    // Right after the word that named the day first, the way a date's own time
    // is read — *Freitag 13:45 Uhr* is the same sentence as *12. September
    // 13:45 Uhr* and had no reason to parse differently. `AT_TIME` second,
    // because it demands a joining word and so cannot see that form at all;
    // it still covers *am Freitag den Termin um 14 Uhr*, where the time is not
    // adjacent to the day.
    let after_day = lower.find(span.as_str()).map(|i| &lower[i + span.len()..]);
    let (h, mi) = after_day
        .and_then(time_read)
        .or_else(|| AT_TIME.find(&lower).and_then(|m| time_read(&lower[m.start()..])))
        .or_else(|| after_day.and_then(time_bare))
        .unwrap_or((DEFAULT_HOUR, 0));
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
    let mut r =
        Rule { freq: Freq::Daily, interval: 1, by_day: vec![], by_month_day: None, until: None, count: None };
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
                r.count = Some(v.parse::<u32>().ok().filter(|n| *n >= 1).ok_or("COUNT must be a positive integer")?);
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

/// How many occurrences the rule allows in all, `None` for open-ended. A
/// bounded recurrence is bounded by its rows: see `Store::occurrences_of_rule`
/// and `complete_moment`, which is where this is enforced.
pub fn rule_count(rule: &str) -> Option<u32> {
    parse_rule(rule).ok()?.count
}

/// The next occurrence strictly after `at`, keeping `at`'s wall-clock time in
/// `tz`. None when the rule is exhausted or invalid. COUNT is enforced by
/// `complete_moment`, which counts the occurrences already on the artifact.
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

/// The intent prototypes as vectors under the running embed model, and the
/// line measured from the base's own artifacts.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
/// What `classify` reads: the intent prototypes, the decoys they have to beat,
/// and the floor under both.
///
/// `line` used to be measured — the 99th percentile of two hundred sampled
/// base vectors, clamped to a ceiling. That measurement was the only thing
/// standing between an ordinary note and an intent, and it was computed once
/// into a `OnceCell` and never again: a base that was empty at startup kept
/// the bare floor for the life of the process, and a base that grew never had
/// its line move. `DECOYS` does the job the calibration was approximating —
/// "is this nearer an ordinary note than an intent" — asked directly, per
/// note, with nothing to go stale. What is left is the configured floor.
pub struct Protos {
    pub vectors: Vec<(Intent, Vec<f32>)>,
    /// `DECOYS`, embedded. Empty is legal and simply disables the test.
    pub decoys: Vec<Vec<f32>>,
    pub line: f32,
}

impl crate::core::Core {
    /// A fixed list of sentences as vectors, embedded once per embed model and
    /// kept in `meta`. Re-embedded whenever the cached count and the list
    /// disagree, which is what makes adding a row to `PROTOTYPES` or `DECOYS`
    /// invalidate its own cache and nothing else.
    async fn embedded_texts(&self, key: &str, texts: &[&str]) -> crate::error::Result<Vec<Vec<f32>>> {
        let cached: Vec<Vec<f32>> = match self.store.meta_get(key).await? {
            Some(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            None => vec![],
        };
        if cached.len() == texts.len() {
            return Ok(cached);
        }
        let docs: Vec<crate::infer::EmbedDoc> = texts
            .iter()
            .map(|t| crate::infer::EmbedDoc { title: None, text: (*t).to_string() })
            .collect();
        let permit = self.gate.background_light().await;
        let embedded = self.embedder.embed_documents(&docs).await;
        permit.finished();
        let embedded = embedded?;
        self.store.meta_set(key, &serde_json::to_string(&embedded).unwrap_or_default()).await?;
        Ok(embedded)
    }

    /// Done, and — for a recurring moment — the next occurrence as a new row
    /// carrying the same rule and source. The done row stays: the history of
    /// a recurring reminder is its rows.
    pub async fn complete_moment(&self, id: &str) -> crate::error::Result<()> {
        let now = self.clock.now();
        if let Some(m) = self.store.moment(id).await? {
            self.store.mark_done(id, now).await?;
            // A bounded recurrence is bounded by its rows: the done row stays,
            // so the occurrences that have existed are the ones on the
            // artifact carrying this rule — including the one just completed.
            // At COUNT, nothing further is armed.
            if let (Some(rule), Some(at)) = (m.rule.as_deref(), m.at)
                && !self.store.rule_is_exhausted(&m.artifact_id, rule).await?
                && let Some(next) = next_after(rule, at, zone(Some(&m.tz)))
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
                        // row wearing `cue` here is one the moments stage takes
                        // for its own on the next re-embed and deletes — after
                        // which the re-read finds the original instant, sees it
                        // done, and arms nothing. The recurrence ended at its
                        // first completion, silently. See `Source::Armed`.
                        source: crate::store::moments::Source::Armed,
                        span: m.span,
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
        }
        Ok(())
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
        let Some(m) = self.store.moment(id).await? else { return Ok(()) };
        // Before the row is reopened: while it is still done, it is the row
        // whose completion armed the successor, and `next_after` from its own
        // `at` is the instant that successor was given.
        if let (Some(rule), Some(at)) = (m.rule.as_deref(), m.at)
            && let Some(next) = next_after(rule, at, zone(Some(&m.tz)))
        {
            self.store.delete_armed_occurrence(&m.artifact_id, rule, next).await?;
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

    /// Embeds the prototypes once per embed model and measures the line from
    /// the base's own vectors. Cached in `meta` so a restart pays nothing, and
    /// held in `protos` so a process reads `meta` once. Keyed by model, so a
    /// switched embedder re-embeds and a reindex under the same one need not.
    pub async fn prototypes(&self) -> crate::error::Result<&Protos> {
        self.protos
            .get_or_try_init(|| async {
                let model = self.embedder.model();
                // Two caches, two keys, each invalidating on its own length —
                // adding a prototype must not force the decoys to be embedded
                // again, and the reverse. Both are keyed by embed model,
                // because a vector from one model says nothing under another.
                let vectors: Vec<(Intent, Vec<f32>)> = self
                    .embedded_texts(
                        &format!("moments.prototypes.{model}"),
                        &PROTOTYPES.iter().map(|(_, _, p)| *p).collect::<Vec<_>>(),
                    )
                    .await?
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| (PROTOTYPES[i].0, v))
                    .collect();
                let decoys =
                    self.embedded_texts(&format!("moments.decoys.{model}"), DECOYS).await?;
                Ok(Protos { vectors, decoys, line: self.time.intent_at.max(INTENT_LINE_FLOOR) })
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_counted_recurrence_stops_at_its_count() {
        use crate::store::moments::{Kind, NewMoment, Source};
        let core = crate::core::test_support::test_core().await;
        let out = core.ingest_capture(crate::core::ingest::Capture::new("Water the plants", "ui")).await.unwrap();
        crate::jobs::test_support::drain(&core).await;
        let aid = core.store.artifacts_for_corpus(&out.id).await.unwrap()[0].id.clone();
        let rule = "FREQ=DAILY;COUNT=2";
        let first = core
            .store
            .insert_moment(&NewMoment {
                artifact_id: aid.clone(),
                kind: Kind::Due,
                at: Some(berlin().with_ymd_and_hms(2026, 8, 31, 9, 0, 0).unwrap().timestamp()),
                tz: "Europe/Berlin".into(),
                rule: Some(rule.into()),
                source: Source::Cue,
                span: None,
            })
            .await
            .unwrap();

        core.complete_moment(&first).await.unwrap();
        assert_eq!(core.store.occurrences_of_rule(&aid, rule).await.unwrap(), 2, "the second occurrence is armed");
        let second = core.store.open_due(0, i64::MAX).await.unwrap();
        assert_eq!(second.len(), 1);
        core.complete_moment(&second[0].moment.id).await.unwrap();
        assert_eq!(core.store.occurrences_of_rule(&aid, rule).await.unwrap(), 2, "two of two, and no third");
        assert!(core.store.open_due(0, i64::MAX).await.unwrap().is_empty());
    }

    #[test]
    fn examples_come_from_the_prototypes_in_the_readers_language() {
        let (remind, journal) = examples_for("de-DE,de;q=0.9,en;q=0.8");
        assert!(remind.starts_with("erinnere mich"), "{remind}");
        assert!(journal.starts_with("heute"), "{journal}");

        let (remind, journal) = examples_for("");
        assert!(remind.starts_with("remind me"), "English is the fallback: {remind}");
        assert!(journal.starts_with("today i"), "{journal}");

        let (remind, _) = examples_for("xx-YY");
        assert!(remind.starts_with("remind me"), "an unknown language falls back too");
    }

    /// The table is read by index in two places and by language in a third, so
    /// its shape is a contract rather than a convention: every language carries
    /// both intents, no sentence is written twice, and nothing is left in a
    /// language `examples_for` cannot reach.
    #[test]
    fn every_language_carries_both_intents_and_no_sentence_twice() {
        let langs: std::collections::BTreeSet<&str> =
            PROTOTYPES.iter().map(|(_, l, _)| *l).collect();
        assert!(langs.contains("en"), "English is the fallback and must exist");
        for l in &langs {
            for intent in [Intent::Remind, Intent::Journal] {
                let n = PROTOTYPES.iter().filter(|(i, x, _)| i == &intent && x == l).count();
                assert!(n >= 2, "{l} has {n} {} prototypes", intent.as_str());
            }
            // The hint under the box reads the first row of the pair. If a
            // language is in the table at all, both of its examples resolve.
            let (r, j) = examples_for(l);
            assert!(!r.is_empty() && !j.is_empty(), "{l} resolves to examples");
        }
        let mut seen = std::collections::BTreeSet::new();
        for (_, _, p) in PROTOTYPES {
            assert!(seen.insert(*p), "duplicate prototype: {p}");
        }
    }

    #[test]
    fn a_remind_cue_fires_anywhere_in_the_opening() {
        assert_eq!(cue("Please remind me to send the invoice"), Some(Intent::Remind));
        assert_eq!(cue("Erinnere mich morgen an den Zahnarzt"), Some(Intent::Remind));
        assert_eq!(cue("Напомни мне позвонить"), Some(Intent::Remind));
    }

    #[test]
    fn a_journal_cue_counts_only_at_the_head_of_a_note() {
        assert_eq!(cue("Today I finally fixed the build"), Some(Intent::Journal));
        // *heute* is a weak cue now, so the head is where `weak_cue` looks.
        assert_eq!(weak_cue("Heute war ein langer Tag."), Some(Intent::Journal));
        assert_eq!(weak_cue("Das Meeting ist heute um drei"), None, "mid-sentence heute is a word");
        assert_eq!(cue("Heute war ein langer Tag."), None, "and it never decides on its own");
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

    fn protos(vectors: Vec<(Intent, Vec<f32>)>, decoys: Vec<Vec<f32>>, line: f32) -> Protos {
        Protos { vectors, decoys, line }
    }

    #[test]
    fn the_best_intent_above_the_line_fires_by_maximum_not_mean() {
        let p = protos(
            vec![
                (Intent::Remind, unit(0, 4)),
                (Intent::Remind, unit(1, 4)),
                (Intent::Remind, unit(2, 4)),
                (Intent::Journal, unit(3, 4)),
            ],
            vec![],
            0.8,
        );
        let note = vec![0.95, 0.05, 0.0, 0.0];
        let (intent, score) = classify(&note, &p).expect("fires");
        assert_eq!(intent, Intent::Remind);
        assert!(score > 0.9);
    }

    #[test]
    fn below_the_line_nothing_fires() {
        let p = protos(vec![(Intent::Remind, unit(0, 4))], vec![], 0.8);
        assert_eq!(classify(&[0.5, 0.5, 0.5, 0.5], &p), None);
    }

    #[test]
    fn a_note_nearer_an_ordinary_one_than_to_an_intent_is_an_ordinary_note() {
        // The hole the decoys close: with nothing to beat, clearing the line
        // was the whole test, so a note that merely leaned towards a
        // prototype became an intent. Here it leans — comfortably over 0.8 —
        // and a decoy leans harder.
        let p = protos(vec![(Intent::Remind, unit(0, 4))], vec![vec![0.8, 0.6, 0.0, 0.0]], 0.8);
        let note = vec![0.9, 0.5, 0.0, 0.0];
        assert!(cosine(&note, &unit(0, 4)) > 0.8, "it does clear the line");
        assert_eq!(classify(&note, &p), None, "and still loses to an ordinary note");

        // And the decoys do not simply suppress everything: a note squarely on
        // a prototype still fires with the same decoy present.
        assert_eq!(classify(&unit(0, 4), &p).map(|(i, _)| i), Some(Intent::Remind));
    }

    #[test]
    fn the_line_is_the_configured_floor_and_never_below_the_constant() {
        // What replaced the calibrated line: no sample, no percentile, no
        // ceiling, and nothing that can go stale in a `OnceCell`. The decoys
        // do the discriminating; this is only a floor.
        let p = protos(vec![(Intent::Remind, unit(0, 4))], vec![], 0.95_f32.max(INTENT_LINE_FLOOR));
        assert_eq!(p.line, 0.95, "a configured floor stands as it was set");
        assert_eq!(0.0_f32.max(INTENT_LINE_FLOOR), INTENT_LINE_FLOOR, "and never below the constant");
        let note = vec![0.9, 0.5, 0.0, 0.0];
        assert!(cosine(&note, &unit(0, 4)) < 0.95);
        assert_eq!(classify(&note, &p), None, "under the floor, nothing fires");
    }

    #[test]
    fn a_weak_cue_never_outranks_the_vector_and_a_strong_one_always_does() {
        // The collision the split exists for: a to-do that opens with a day
        // word. `cue` no longer sees it, so the classifier gets the note;
        // `weak_cue` still does, for the reading that happens only when the
        // classifier declines.
        assert_eq!(cue("Heute den Bericht abgeben"), None);
        assert_eq!(weak_cue("Heute den Bericht abgeben"), Some(Intent::Journal));
        // The unmistakable ones decide as they always did.
        assert_eq!(cue("Dear diary, the move is finally over"), Some(Intent::Journal));
        assert_eq!(cue("Today i finally got the migration working"), Some(Intent::Journal));
        assert_eq!(cue("Remind me to send the invoice"), Some(Intent::Remind));
        assert_eq!(weak_cue("Remind me to send the invoice"), None, "a remind cue is never weak");
        // And a weak journal cue still has to open the note.
        assert_eq!(weak_cue("Der Bericht ist heute fällig"), None);
    }

    #[test]
    fn a_refusal_is_recorded_per_intent_and_reads_the_key_it_replaced() {
        let mut meta = serde_json::json!({});
        assert!(!intent_refused(&meta, Intent::Remind));
        refuse_intent(&mut meta, Intent::Remind);
        refuse_intent(&mut meta, Intent::Remind);
        assert_eq!(meta["intent_refused"], serde_json::json!(["remind"]), "idempotent");
        assert!(intent_refused(&meta, Intent::Remind));
        assert!(!intent_refused(&meta, Intent::Journal), "one refusal is not the other");

        refuse_intent(&mut meta, Intent::Journal);
        allow_intent(&mut meta, Intent::Remind);
        assert!(!intent_refused(&meta, Intent::Remind));
        assert!(intent_refused(&meta, Intent::Journal), "and the other one stands");
        allow_intent(&mut meta, Intent::Journal);
        assert!(meta.get("intent_refused").is_none(), "the last one takes the key with it");

        // A base written before this keeps its refusals, and withdrawing one
        // clears the old key too — otherwise the undo would appear to work and
        // the stage would go on refusing.
        let mut old = serde_json::json!({"entry_refused": true});
        assert!(intent_refused(&old, Intent::Journal));
        assert!(!intent_refused(&old, Intent::Remind));
        allow_intent(&mut old, Intent::Journal);
        assert!(!intent_refused(&old, Intent::Journal));
    }

    #[test]
    fn every_language_has_a_cue_and_prototypes_for_both_intents() {
        for intent in [Intent::Remind, Intent::Journal] {
            assert!(CUES.iter().filter(|(i, _, _)| *i == intent).count() >= 10);
            assert!(PROTOTYPES.iter().filter(|(i, _, _)| *i == intent).count() >= 10);
        }
        for (_, _, p) in PROTOTYPES {
            assert!(p.split_whitespace().count() >= 3, "a prototype is a sentence, not a word: {p}");
        }
        // A decoy is a sentence for the same reason a prototype is, and there
        // have to be enough of them to cover more than one shape of note.
        assert!(DECOYS.len() >= 6);
        for d in DECOYS {
            assert!(d.split_whitespace().count() >= 3, "a decoy is a note, not a word: {d}");
        }
        // Every remind cue decides; the day words never do.
        assert!(CUES.iter().filter(|(i, _, _)| *i == Intent::Remind).all(|(_, _, s)| *s == Strength::Strong));
        assert!(CUES.iter().any(|(i, _, s)| *i == Intent::Journal && *s == Strength::Strong));
        assert!(CUES.iter().any(|(i, _, s)| *i == Intent::Journal && *s == Strength::Weak));
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
    fn an_everyday_word_that_begins_a_month_name_is_not_a_date_on_its_own() {
        // `may` is an English modal, `march` a verb, and `month_of` matches any
        // word of three letters or more that *prefixes* a month name. Each of
        // these put an event on the front page under "Coming up".
        for text in [
            "Section 3 may be revised",
            "review 5 may need work",
            "the last 2 march entries",
            "chapter 7 mars the argument",
            "put 9 marches on the calendar",
        ] {
            assert!(absolute_dates(text, captured(), berlin(), false).is_empty(), "{text}");
        }
    }

    #[test]
    fn an_ambiguous_month_word_still_reads_when_the_sentence_says_it_is_a_date() {
        // The guard asks for one supporting signal, and there are four: a dot,
        // a preposition in front, a year behind, or a time.
        for (text, want) in [
            ("on 3 May", "2027-05-03 09:00"),
            ("Termin am 3. Mai", "2027-05-03 09:00"),
            ("due 3 May 2027", "2027-05-03 09:00"),
            ("3 May at 10", "2027-05-03 10:00"),
        ] {
            let f = absolute_dates(text, captured(), berlin(), false);
            assert_eq!(f.first().map(|x| local(x.at)).as_deref(), Some(want), "{text}");
        }
    }

    #[test]
    fn an_unambiguous_month_name_needs_nothing_behind_it() {
        // The list is about ambiguity and not about abbreviation: a word that
        // is a month word and nothing else goes on reading bare.
        for text in ["meeting 12 September", "spotkanie 5 października", "встреча 7 ноября"] {
            assert!(!absolute_dates(text, captured(), berlin(), false).is_empty(), "{text}");
        }
    }

    #[test]
    fn an_empty_default_zone_is_the_servers_own() {
        // `""` is documented — in the field's doc and in config.example.toml —
        // as the server's zone, and resolved to UTC, because `zone("")` cannot
        // parse it. Every door that sent no zone had its dates read two hours
        // off on a Berlin server.
        let named = default_zone_name("Europe/Berlin");
        assert_eq!(named, "Europe/Berlin", "a named zone is used as it stands");
        let empty = default_zone_name("");
        assert!(empty.parse::<Tz>().is_ok(), "and the fallback is a zone the table knows: {empty}");
        assert_eq!(default_zone_name("  "), empty, "whitespace is empty");
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

    /// The reported shape: *erinnere mich in 10 minuten an xy* landed on the
    /// next day at the right clock time, and only saying *heute* fixed it. An
    /// offset this small is arithmetic on the second it was captured, and
    /// nothing here could do it: `IN_N` counts days and weeks, and
    /// `relative_date` only ever considers a date strictly after today.
    #[test]
    fn an_offset_inside_the_day_is_read_off_the_second_it_was_captured() {
        for (text, want) in [
            ("erinnere mich in 10 minuten an den ofen", 600),
            ("erinnere mich in 10 Minuten an den Ofen", 600),
            ("remind me in 25 minutes to check the oven", 1_500),
            ("in 90 min den ofen prüfen", 5_400),
            ("erinnere mich in 2 stunden", 7_200),
            ("in 2 Std. nachsehen", 7_200),
            ("remind me in an hour", 3_600),
            ("erinnere mich in einer stunde", 3_600),
            ("erinnere mich in einer halben stunde", 1_800),
            ("remind me in half an hour", 1_800),
            ("rappelle-moi dans 20 minutes", 1_200),
            ("через 15 минут", 900),
        ] {
            assert_eq!(
                clock_offset(text, captured()).map(|f| f.at - captured()),
                Some(want),
                "{text}"
            );
        }
    }

    /// The same ten languages the month and weekday tables carry. Turkish puts
    /// its word after the unit, and Russian, Polish and Italian name the hour
    /// with no count at all, which is what `BARE_ONE` is for.
    #[test]
    fn an_offset_reads_in_ten_languages() {
        for (text, want) in [
            ("remind me in 10 minutes", 600),
            ("erinnere mich in 10 minuten", 600),
            ("rappelle-moi dans 10 minutes", 600),
            ("recuérdame en 10 minutos", 600),
            ("lembra-me daqui a 10 minutos", 600),
            ("dentro de 10 minutos", 600),
            ("ricordami tra 10 minuti", 600),
            ("herinner me over 10 minuten", 600),
            ("przypomnij mi za 10 minut", 600),
            ("10 dakika sonra hatırlat", 600),
            ("напомни через 10 минут", 600),
            // The hour, and the several ways of naming one of it.
            ("remind me in an hour", 3_600),
            ("in einer stunde", 3_600),
            ("dans une heure", 3_600),
            ("in un'ora", 3_600),
            ("dentro de una hora", 3_600),
            ("em uma hora", 3_600),
            ("over een uur", 3_600),
            ("za godzinę", 3_600),
            ("bir saat sonra", 3_600),
            ("через час", 3_600),
            // And half of one.
            ("remind me in half an hour", 1_800),
            ("erinnere mich in einer halben stunde", 1_800),
            ("yarım saat sonra", 1_800),
        ] {
            assert_eq!(
                clock_offset(text, captured()).map(|f| f.at - captured()),
                Some(want),
                "{text}"
            );
        }
    }

    /// What it must not read. A duration is not an offset: the note names how
    /// long something takes, or which meeting it is, and neither says when to
    /// be reminded. The preposition is what separates the two, and it is the
    /// reason this asks for one where `IN_N` does not.
    #[test]
    fn a_duration_is_not_an_offset() {
        for text in [
            "erinnere mich morgen an das 30-minuten meeting",
            "the standup is 15 minutes, remind me tomorrow",
            "erinnere mich morgen an den zahnarzttermin",
            "in 3 days",
            "in 2 Wochen",
            // A span being described, not a time to be reminded at. The count
            // is missing and the unit is plural, which is the pair `BARE_ONE`
            // refuses.
            "das dauert noch in stunden gerechnet ewig",
            "esto se mide en horas",
            "it is measured in minutes",
            "nothing here",
        ] {
            assert_eq!(clock_offset(text, captured()), None, "{text}");
        }
    }

    /// The reported shape: a weekday, then a bare time. `AT_TIME` demands a
    /// joining word and could not see it, so the sentence a person actually
    /// types came back at nine in the morning with nothing saying why.
    #[test]
    fn a_time_beside_the_day_needs_no_joining_word() {
        for (text, want) in [
            ("erinnere mich an den termin, Freitag 13:45 uhr.", "2026-09-04 13:45"),
            ("Freitag um 13:45", "2026-09-04 13:45"),
            ("tomorrow 8:30", "2026-08-31 08:30"),
            ("tomorrow 5pm", "2026-08-31 17:00"),
            ("übermorgen 18 Uhr", "2026-09-01 18:00"),
            // Not adjacent to the day: `AT_TIME` is the second look and still
            // finds it.
            ("am Freitag den Termin um 14 Uhr", "2026-09-04 14:00"),
            // A bare number with no colon and no suffix is not a time, which
            // is the whole reason the marker rule exists.
            ("in 3 days 5 apples", "2026-09-02 09:00"),
        ] {
            let f = relative_date(text, captured(), berlin());
            assert_eq!(f.map(|x| local(x.at)).as_deref(), Some(want), "{text}");
        }
    }

    /// The one shape `TIME_AFTER` refuses on purpose — no colon, no marker —
    /// is still a time when it is exactly four digits beside the day.
    #[test]
    fn a_bare_four_digit_clock_beside_the_day_is_still_a_time() {
        for (text, want) in [
            ("Mittwoch 0900 Zimmer A323", "2026-09-02 09:00"),
            ("Freitag 1430", "2026-09-04 14:30"),
            // Five digits is not a clock and falls back to the default hour.
            ("Freitag 14305", "2026-09-04 09:00"),
            // An out-of-range hour is not a clock either.
            ("Freitag 2500", "2026-09-04 09:00"),
            // The padded hour is the keypad's own shape, so it needs nothing
            // after it; the marker and the sentence's end say it outright.
            ("Mittwoch 0030 Nachtschicht", "2026-09-02 00:30"),
            ("Mittwoch 1430 Uhr", "2026-09-02 14:30"),
            ("Freitag 1745.", "2026-09-04 17:45"),
        ] {
            let f = relative_date(text, captured(), berlin());
            assert_eq!(f.map(|x| local(x.at)).as_deref(), Some(want), "{text}");
        }
    }

    /// The other half of that rule: an unpadded run with a word after it is
    /// something being counted, and reading it as an hour dated a note the
    /// operator never dated.
    #[test]
    fn an_amount_or_a_year_beside_the_day_is_not_a_clock() {
        for (text, want) in [
            ("Erinnere mich Montag 1000 Euro zu überweisen", "2026-08-31 09:00"),
            ("Freitag 2026 Steuererklärung abgeben", "2026-09-04 09:00"),
            ("Montag 5000 Schritte laufen", "2026-08-31 09:00"),
            // The cost of the rule, and the wording the model path reads.
            ("Mittwoch 1430 Meeting", "2026-09-02 09:00"),
        ] {
            let f = relative_date(text, captured(), berlin());
            assert_eq!(f.map(|x| local(x.at)).as_deref(), Some(want), "{text}");
        }
    }

    #[test]
    fn a_date_takes_a_bare_time_beside_it_too() {
        for (text, want) in [
            ("Termin 12. September 13:45 Uhr", "2026-09-12 13:45"),
            ("Termin 12. September, 14:00", "2026-09-12 14:00"),
            ("meeting 12 September 2026 at 3pm", "2026-09-12 15:00"),
            ("01.03.2027 5 apples", "2027-03-01 09:00"),
        ] {
            let f = absolute_dates(text, captured(), berlin(), false);
            assert_eq!(f.first().map(|x| local(x.at)).as_deref(), Some(want), "{text}");
        }
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
        ] {
            assert!(validate_rule(bad).is_err(), "{bad}");
        }
        assert_eq!(rule_count("FREQ=DAILY;COUNT=5"), Some(5), "COUNT is kept, not discarded");
        assert_eq!(rule_count("FREQ=DAILY"), None);
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

    #[tokio::test]
    async fn the_prototypes_are_embedded_once_and_cached_under_the_model() {
        let (mut core, embedder) = crate::core::test_support::test_core_counting_embed_calls().await;
        core.protos = std::sync::Arc::new(tokio::sync::OnceCell::new());
        let before = embedder.calls();
        let first = core.prototypes().await.unwrap();
        assert_eq!(first.vectors.len(), PROTOTYPES.len());
        assert_eq!(first.decoys.len(), DECOYS.len());
        assert_eq!(first.line, core.time.intent_at, "the configured floor, and nothing measured");
        assert_eq!(embedder.calls(), before + 2, "one batch each for the prototypes and the decoys");
        core.prototypes().await.unwrap();
        assert_eq!(embedder.calls(), before + 2, "held on the core after that");
        // Two keys, so that adding a prototype does not force the decoys to be
        // embedded again, or the reverse.
        for (key, want) in [
            (format!("moments.prototypes.{}", core.embedder.model()), PROTOTYPES.len()),
            (format!("moments.decoys.{}", core.embedder.model()), DECOYS.len()),
        ] {
            let cached = core.store.meta_get(&key).await.unwrap().expect("cached in meta");
            let parsed: Vec<Vec<f32>> = serde_json::from_str(&cached).unwrap();
            assert_eq!(parsed.len(), want, "{key}");
        }
        // A fresh process with the same model reads the cache and embeds nothing.
        core.protos = std::sync::Arc::new(tokio::sync::OnceCell::new());
        core.prototypes().await.unwrap();
        assert_eq!(embedder.calls(), before + 2);
    }
}
