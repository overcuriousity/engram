use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    /// Defaulted rather than required: every field under it already has a
    /// default, and the file that names five sections and one mode should not
    /// be refused over an empty table naming none of them.
    #[serde(default)]
    pub store: StoreConfig,
    pub vector: VectorConfig,
    pub infer: InferConfig,
    pub auth: AuthConfig,
    #[serde(default)]
    pub consolidate: ConsolidateConfig,
    #[serde(default)]
    pub learn: LearnConfig,
    #[serde(default)]
    pub feedback: FeedbackConfig,
    #[serde(default)]
    pub pacing: PacingConfig,
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default)]
    pub associate: AssociateConfig,
    #[serde(default)]
    pub activation: ActivationConfig,
    #[serde(default)]
    pub promote: PromoteConfig,
    #[serde(default)]
    pub pursuit: PursuitConfig,
    #[serde(default)]
    pub schedule: ScheduleConfig,
    #[serde(default)]
    pub sitting: SittingConfig,
    #[serde(default)]
    pub recommend: RecommendConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

/// What the two supplied-from-outside capture paths are allowed to cost.
///
/// The fetch limits are deliberately separate from `MAX_BODY_BYTES`: that one
/// bounds what a client may send us, and says nothing about what we go and
/// retrieve on their behalf.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct CaptureConfig {
    /// Ceiling on a server-side GET. Generous, but it is a network fetch and
    /// not a local model call, so it is not measured in minutes.
    pub fetch_timeout_secs: u64,
    /// Bytes read from a fetched URL before the transfer is abandoned.
    pub fetch_max_bytes: usize,
    /// Characters an extraction must yield to count as a capture. Below this,
    /// the page reduced to navigation and boilerplate: report it, store
    /// nothing. A corpus that silently holds a cookie banner instead of the
    /// document is the failure this whole path is shaped to prevent.
    pub min_extracted_chars: usize,
    /// Bytes an uploaded image may weigh. A phone photo is 3–8 MB; this is the
    /// per-route ceiling for the image door only, the global body limit stays.
    pub image_max_bytes: usize,
    /// Longest edge, in pixels, of the preview the vision model is shown and
    /// the UI displays. The original is stored untouched regardless.
    pub image_preview_edge: u32,
    /// Bytes an uploaded PDF may weigh. A book is tens of megabytes; this is
    /// the per-route ceiling for the upload door, the global body limit stays.
    /// Nothing else bounds a PDF — no page cap: feeding a book to engram is a
    /// deliberate act, and the queue behind it is already throttled.
    pub pdf_max_bytes: usize,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            fetch_timeout_secs: 30,
            fetch_max_bytes: 8 * 1024 * 1024,
            min_extracted_chars: 200,
            image_max_bytes: 25 * 1024 * 1024,
            image_preview_edge: 2048,
            pdf_max_bytes: 50 * 1024 * 1024,
        }
    }
}

/// Pacing for every generating inference call, not just synthesis.
///
/// The roles share one GPU, so a per-role gap could not bound total load: three
/// roles each honouring their own cooldown still interleave into unbroken work.
/// One gap in front of all of them is the only version of this setting that
/// means what it says.
///
/// Generating is the word that carries the exception. Embedding takes its turn
/// like everything else — one call at a time is what bounds the GPU — but it
/// neither serves the gap nor starts one, because the gap is measured against
/// a generation and an encoder is not one. Pacing a batch that answers in a
/// second behind thirty seconds of silence spends almost the whole budget on
/// the one role that needs no protecting; at `synthesis = "earned"`, where
/// capture generates nothing, that was most of what the setting did.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct PacingConfig {
    /// Minimum seconds between the end of one background generation and the
    /// start of the next. Zero disables pacing. `ask` ignores it: a person is
    /// waiting, and the pacer exists to protect the GPU from batch work, not
    /// from them. Embedding ignores it too, for the opposite reason — it is
    /// batch work the gap was never sized against. See `InferenceGate::
    /// background_light`.
    pub cooldown_secs: u64,
}

/// The one switch over everything learned from what happens here.
///
/// Recording searches, learning links from co-retrieval, and writing a pursuit
/// were three flags that only ever meant something together: association reads
/// recorded searches, and a pursuit is swept on the associative pass. Two of
/// the three combinations were refused at startup and the third was a warning,
/// which is a way of saying they were never really three settings. They are
/// one now.
///
/// On by default. Everything downstream of it — activation, promotion at
/// `synthesis = "earned"`, the associative spread, `[recommend]` — moves only
/// while the log is being written, so off is the deliberate act. The wording of
/// a query is personal and nothing here leaves the machine; this is the switch
/// for the operator who wants none of it kept, and turning it off stops the
/// recording as well as everything read from it.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct LearnConfig {
    /// The named bundle. Every key it stands for is still a key, and a key
    /// written in the file wins over what the mode would have said.
    pub mode: LearnMode,
    pub enabled: bool,
    /// What the mode decided, in the order it decided it, for
    /// `--print-config` to show. Not a setting: it is filled in during
    /// `load` and skipped by deserialization, so a file naming it is
    /// ignored the way any unknown key is.
    #[serde(skip)]
    pub resolved: Vec<(&'static str, String)>,
}

impl Default for LearnConfig {
    fn default() -> Self {
        Self {
            mode: LearnMode::default(),
            enabled: true,
            resolved: Vec::new(),
        }
    }
}

/// The dial: one word for a coherent bundle of the keys below it.
///
/// The keys did not go anywhere. What the mode does is decide the ones the
/// file does not, which is what makes `off` a line rather than a page: the
/// half-dozen settings that have to agree for "learn nothing" to mean
/// anything are settings that only ever agreed by hand before, and the
/// combinations that did not agree were refused at startup or warned about.
/// A mode cannot disagree with itself.
///
/// `learning` is the one that did not exist. It is the mode the roadmap's own
/// rule asks for — a default that changes ranking moves only after the harness
/// has been run — and it is the only way to run the harness honestly: the log
/// is written, activation and links accumulate, and nothing reads any of it on
/// the query path, so a sweep compares a ranking against itself rather than
/// against a ranking the sweep's own inputs have already moved.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LearnMode {
    /// Record nothing, learn nothing, prime nothing, promote nothing;
    /// consolidate only the exact and near duplicates capture finds for a
    /// hash. What is left is capture, hybrid search and ask.
    Off,
    /// Record and accumulate, read none of it on the query path. The mode to
    /// run the harness in before any of this is allowed to move a rank.
    Learning,
    /// Today's defaults, unchanged.
    #[default]
    Full,
}

impl LearnMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            LearnMode::Off => "off",
            LearnMode::Learning => "learning",
            LearnMode::Full => "full",
        }
    }
}

/// The one UI concern with settings of its own: the vector background.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct UiConfig {
    pub background: BackgroundConfig,
}

/// The rotating point cloud behind the pages, sampled from the vector store.
///
/// Decorative, and held to a decoration's budget: the client keeps the cloud it
/// was given and asks once per page load whether the store still matches it,
/// which costs a count. Only a store that has changed costs a scroll. On by
/// default: it is the machine showing its own shape, and off is one line for
/// the operator who wants the pages plain.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct BackgroundConfig {
    pub enabled: bool,
    /// Vectors sampled per snapshot. 2000 points read as a cloud; far fewer
    /// read as noise, far more cost the phone drawing them.
    pub sample_size: usize,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_size: 2000,
        }
    }
}

/// Recording real searches so they can be judged later.
///
/// The queries a benchmark needs cannot be written from memory: phrased while
/// looking at an artifact, they reuse its vocabulary, and every retrieval system
/// passes such a pair. Only a search made in earnest, before anything came back,
/// is worth scoring against.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct FeedbackConfig {
    /// Candidates stored per event. Wider than the answer on purpose — search
    /// over-fetches anyway, so the extra rows are free, and they are what lets a
    /// buried hit be confirmed later.
    pub candidates: usize,
    /// Window in which another query from the same searcher replaces the
    /// previous event instead of starting a new one. `0` turns folding off.
    ///
    /// Sized for a typing burst and not for a train of thought. Any rewording
    /// inside the window folds, so the window is also how long a finished
    /// search has left to live: search, read the titles, search again, and the
    /// first one is gone — including the case where nothing was opened, which
    /// is exactly what the unmatched-gap sweep exists to see.
    pub coalesce_secs: i64,
    /// Days captured searches are kept. `0` keeps them forever.
    pub retain_days: i64,
    /// How often the retention sweep runs. Hours rather than minutes because
    /// `retain_days` is the only thing it enforces: a window measured in days
    /// does not need checking more than a few times a day.
    pub sweep_hours: u64,
    pub tune: TuneConfig,
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            candidates: 20,
            coalesce_secs: 5,
            retain_days: 0,
            sweep_hours: 6,
            tune: TuneConfig::default(),
        }
    }
}

/// When judgements are spent on a parameter sweep, and how often.
///
/// The floor is statistical rather than cautious: with ten pairs recall@10
/// moves in ten-point steps, so a sweep under it recommends the quirks of ten
/// queries with the same confidence as a real improvement. Below
/// `min_judgements` nothing runs; after that a sweep re-runs once
/// `resweep_after` further verdicts have been given.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct TuneConfig {
    pub min_judgements: i64,
    pub resweep_after: i64,
}

impl Default for TuneConfig {
    fn default() -> Self {
        Self {
            min_judgements: 50,
            resweep_after: 10,
        }
    }
}

/// Links learned from co-retrieval, and what they are allowed to do.
///
/// Every threshold here is a weight in the same units: one co-appearance is
/// `+1`, one confirmed answer is `+2`, and a half-life of thirty days is what
/// makes those numbers mean "lately" rather than "ever".
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct AssociateConfig {
    pub interval_mins: u64,
    pub half_life_days: f64,
    /// Decayed weight under which a `learning` link is deleted.
    pub prune_below: f64,
    /// Decayed weight at which a link is worth showing.
    pub show_min: f64,
    /// ...and at which it is worth one model call.
    pub judge_min: f64,
    /// Distinct binding questions a link needs before it is judged. One question
    /// asked six times is one question.
    pub judge_min_queries: i64,
    pub judge_per_sweep: i64,
    /// How many of the top ranked hits are asked what they are linked to.
    pub spread_from: usize,
    /// How many associated hits may be appended, outside `limit`.
    pub spread_max: usize,
    /// How much more activated a hit must be than the one above it to pass it.
    /// Normalised within one result list, so this is a fraction, not a weight.
    pub prime_margin: f64,
    /// Positions a hit may climb. `0` turns priming off, and it ships off: an
    /// unmeasured feature that reorders results and makes a claim about the
    /// person should not be on until the harness has run with it off and on.
    pub prime_lift: usize,
}

impl Default for AssociateConfig {
    fn default() -> Self {
        Self {
            interval_mins: 30,
            half_life_days: 30.0,
            prune_below: 0.5,
            show_min: 2.0,
            judge_min: 4.0,
            judge_min_queries: 3,
            judge_per_sweep: 10,
            spread_from: 3,
            spread_max: 3,
            prime_margin: 0.5,
            prime_lift: 0,
        }
    }
}

/// When a passage has earned its window a synthesis call, and when an eager
/// artifact has earned a second one.
///
/// `activation_above` is read against `[activation]` and *above the capture
/// baseline*: `retrieved = 0`, `opened = 1.0`, `confirmed = 3.0`, half-life 14
/// days — so `3.0` is one confirmation, or three openings. The baseline every
/// artifact carries decays at the same rate as what use adds, so it is
/// subtracted decayed before the comparison; a threshold read against the raw
/// sum meant something different at every age, and the `4.0` this was could
/// only be reached by a confirmation at essentially zero elapsed time. Checked
/// with `>=` after the bump, decay folded in, and only at the engagement bumps
/// — opened, confirmed, cited — never at retrieved. With `retrieved` at zero
/// that is a guarantee rather than a habit: a passage that merely keeps
/// appearing in result lists cannot fill the tank for one open to fire,
/// however often it is listed.
///
/// `resynthesize_after_unconfirmed` is the `eager` counterpart: an artifact
/// shown this many times with no confirmation recorded against it is
/// re-synthesised from its segment. `0` disables it, and it ships disabled —
/// re-synthesising changes what an existing base contains without anyone
/// asking, so it is a default the harness moves.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct PromoteConfig {
    pub activation_above: f64,
    pub resynthesize_after_unconfirmed: i64,
}

impl Default for PromoteConfig {
    fn default() -> Self {
        Self {
            activation_above: 3.0,
            resynthesize_after_unconfirmed: 0,
        }
    }
}

/// What a coherent run of searches — a pursuit — may earn: one generated
/// artifact, written from what was engaged with, when the base did not answer
/// or the answer was assembled by hand. Runs behind `[learn]`, which is the
/// switch: the events it reads are the ones recording writes, and the sweep it
/// rides on is the associative pass. The grouping line is not a key: it is
/// measured, like the gap clusters', by `core::gaps::link_threshold`.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct PursuitConfig {
    /// A pursuit is over when nothing has happened for this long.
    pub idle_secs: u64,
    /// Fewer engaged artifacts than this is a promotion case, not generation.
    pub min_sources: usize,
    /// Total engagement weight a pursuit needs before it is worth a call.
    pub min_engagement: f64,
}

impl Default for PursuitConfig {
    fn default() -> Self {
        Self {
            idle_secs: 900,
            min_sources: 2,
            min_engagement: 3.0,
        }
    }
}

/// How accessible an artifact is, and what raises it.
///
/// Being surfaced *because* of activation raises nothing: `resurface` and
/// association both leave it alone. Loops that reinforce themselves are the
/// failure mode of this whole idea, and they are closed by construction.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ActivationConfig {
    pub half_life_days: f64,
    /// Returned by a search the caller marked as seen. Zero: being listed is
    /// exposure, and activation is read — by priming, by promotion — as use.
    /// At `1.0` it was the strongest per-event signal there was, because it
    /// was the most common thing that happened to an artifact, and "you reach
    /// this one often" was said of artifacts nobody had ever opened.
    pub retrieved: f64,
    /// Opened in the detail pane. The unit the others are measured in.
    pub opened: f64,
    /// Judged the answer to a real question. The strong signal.
    pub confirmed: f64,
    /// Cited by an answer. A model's use of an artifact, not a person's, which
    /// is why it weighs what an open weighs rather than what a confirmation
    /// does: nothing here verified that the answer was right.
    pub cited: f64,
}

impl Default for ActivationConfig {
    fn default() -> Self {
        Self {
            half_life_days: 14.0,
            retrieved: 0.0,
            opened: 1.0,
            confirmed: 3.0,
            cited: 0.5,
        }
    }
}

/// The live sitting: what this session has touched, carried between the doors.
///
/// One key, because carrying changes no order and needs no permission. What
/// moves ranking is priming, and that is what this switches.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct SittingConfig {
    /// Let what this sitting has touched lift a result.
    ///
    /// Off until the harness says otherwise. It is the only part of the sitting
    /// that moves an order, and the same query ranking differently in two
    /// sittings is exactly what is disorienting about it — so it ships off, the
    /// lift is bounded by the same budget activation's is, and rank 0 never
    /// moves.
    pub prime: bool,
}

#[allow(clippy::derivable_impls)]
impl Default for SittingConfig {
    fn default() -> Self {
        // Spelled out rather than derived: `false` here is a decision with a
        // reason above it, and a derived `Default` would put that reason a
        // refactor away from the value it explains.
        Self { prime: false }
    }
}

/// What each named block of the context vector is worth.
///
/// This is the whole of the encoder's argument in config form. Each block is
/// normalised to length 1 and *then* scaled by its weight, so a block
/// contributes exactly its weight however many dimensions it uses — seven
/// one-hot slots for the weekday do not outweigh two for the hour because there
/// are seven of them. That is what turns the encoding's implicit weighting,
/// which nobody can tune, back into named numbers an operator can change.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct BlockWeights {
    /// The hour, as an angle. See `core::context::encode`.
    pub time_of_day: f32,
    pub weekday: f32,
    /// The part of the weekday that genuinely is gradual, kept apart from the
    /// one-hot and kept weak.
    pub weekend: f32,
    pub device: f32,
    pub viewport: f32,
    pub locale: f32,
    pub network: f32,
    pub power: f32,
    pub environment: f32,
    /// Off. A monthly rhythm is real — rent, invoices — but nothing has shown
    /// one here yet, and a block at zero costs two dimensions and no reasoning.
    pub month_cycle: f32,
}

impl Default for BlockWeights {
    fn default() -> Self {
        Self {
            time_of_day: 1.0,
            weekday: 1.0,
            weekend: 0.3,
            device: 0.8,
            viewport: 0.4,
            locale: 0.3,
            network: 0.6,
            power: 0.2,
            environment: 0.2,
            month_cycle: 0.0,
        }
    }
}

impl BlockWeights {
    /// The weight of a block by name. A name nothing knows is worth nothing,
    /// rather than a default: the block table and this lookup are edited
    /// together, and a typo that silently gave a block weight 1.0 would be a
    /// recommendation nobody could account for.
    pub fn of(&self, block: &str) -> f32 {
        match block {
            "time_of_day" => self.time_of_day,
            "weekday" => self.weekday,
            "weekend" => self.weekend,
            "device" => self.device,
            "viewport" => self.viewport,
            "locale" => self.locale,
            "network" => self.network,
            "power" => self.power,
            "environment" => self.environment,
            "month_cycle" => self.month_cycle,
            _ => 0.0,
        }
    }
}

/// Offering an artifact before it is asked for, from the situation the page was
/// opened in.
///
/// One gate and a table of numbers, on purpose: eight gates over one faculty
/// is the objection recorded in #72, and this does not add a ninth. The
/// learning cadence is not here either — see
/// `jobs::context::INTERVAL_HOURS`.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct RecommendConfig {
    /// On, with the floor of the ladder as the honest answer while the base is
    /// young. A weekly pattern needs weeks, so a new base sees the random card
    /// for a fortnight — that card claims nothing, and an area that says
    /// nothing until it has something to say is an area nobody discovers.
    /// Needs `learn.enabled`: the situations are read from the same log.
    pub enabled: bool,
    /// Cosine above which an event joins a cluster rather than opening its own.
    pub cluster_merge_at: f32,
    /// Per (scope, artifact). Multiple clusters are the point: a thing looked
    /// up on Friday afternoons *and* occasionally on Monday mornings is two
    /// situations, and their mean is a situation that never happened.
    pub max_clusters: usize,
    /// A pattern that stops fades rather than standing for ever.
    pub half_life_days: f64,
    /// A cluster below this is dropped.
    ///
    /// Low enough that a single recent event survives, because a thing done
    /// twice is worth saying so about — it just is not worth calling a
    /// pattern. What separates the two is `firm_at`, and what protects against
    /// the single accident is that a thin cluster has to match the situation
    /// *better* before anything is offered at all.
    pub min_weight: f64,
    /// Weight at or above which a cluster is spoken of as established.
    ///
    /// Below it the offer says how many times it has happened instead —
    /// "Twice before" — and demands a strong situational match before saying
    /// anything. At the default half-life this is the third repetition.
    pub firm_at: f64,
    /// Context score at or above which the offer is called a pattern.
    ///
    /// The same number the store ranked on. These two lived on a scale of their
    /// own while a `scope` block dominated the full cosine at weight 10 against
    /// a total of under 5 — counting it would have dragged every same-subject
    /// pair above 0.95 and left them four hundredths apart. That block is gone
    /// and these values are unchanged: the gate never read it.
    pub strong_at: f32,
    /// And above which it is called a resemblance. Below it the ladder falls
    /// through to the sitting, and then to what has been forgotten.
    pub weak_at: f32,
    /// What an open of something this feature offered counts for, back into the
    /// profile. Zero, because without it the first lucky guess grows into a
    /// habit the system taught itself.
    pub self_weight: f64,
    /// See `BlockWeights`.
    pub weights: BlockWeights,
}

impl Default for RecommendConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cluster_merge_at: 0.82,
            max_clusters: 5,
            half_life_days: 45.0,
            min_weight: 0.9,
            firm_at: 2.5,
            strong_at: 0.75,
            weak_at: 0.45,
            self_weight: 0.0,
            weights: BlockWeights::default(),
        }
    }
}

/// What the queue does with work nobody is waiting on.
///
/// One key, because there is one thing to decide. `jobs.class` says whether
/// somebody is standing in front of a unit, and that answer is a constant per
/// stage rather than a setting — a priority the operator can set wrong presents
/// as "the capture is hanging", with nothing anywhere saying why. What is left
/// to configure is the one number that keeps priority from becoming starvation.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ScheduleConfig {
    /// A background unit that has waited longer than this becomes foreground.
    ///
    /// Without it, one long ingest keeps night work off the workers
    /// indefinitely, which is the exact failure a priority scheduler is
    /// expected to have an answer for. The default is a guess; `sweep_runs` on
    /// Ops is how the guess gets checked, since a sweep whose runs thin out is
    /// visible there rather than silent.
    pub age_after_mins: i64,
    /// The ceiling on how long a sweep that keeps finding nothing waits.
    ///
    /// A base with nothing to do wakes, queries, finds nothing and sleeps
    /// again — for ever, by construction, once per interval per sweep per
    /// tenant. Each consecutive empty run doubles the wait up to this, and any
    /// new data cancels it outright, because every producer already calls
    /// `arm_now`. So the cost of being wrong here is bounded on both sides: a
    /// sweep on a quiet base runs late, never not at all.
    pub backoff_max_hours: u64,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            age_after_mins: 60,
            backoff_max_hours: 24,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ConsolidateConfig {
    /// Whether the background sweep runs at all. Capture-time near-duplicate
    /// detection is separate and always on: it costs a hash, not a query.
    pub enabled: bool,
    /// Estimated Jaccard over word shingles above which a capture is parked as
    /// a near-duplicate of an existing corpus.
    pub near_dupe_min: f64,
    /// Cosine at or above which a pair is worth an operator's attention.
    pub review_min: f32,
    /// Cosine at or above which a pair is judged *first* — a fast lane to the
    /// dedupe judge, no longer a hide. It used to supersede the older artifact
    /// on the score alone; embeddings barely distinguish negation, and "runs
    /// on ext4" / "does not run on ext4" sit far above any realistic
    /// threshold, so the judge's `losses` check now stands behind every hide.
    /// Still validated above `review_min`.
    pub auto_supersede: f32,
    /// Neighbours considered per artifact when it looks for duplicates.
    pub per_point: usize,
    /// How often the sweep is queued.
    pub interval_hours: u64,
    /// How often the dedupe ticker arms units, in minutes.
    ///
    /// Its own ticker rather than a passenger on the sweep. `max_judgements`
    /// bounded what *one sweep* armed, which was right while the sweep was the
    /// only producer of pairs; the relate units file them continuously now, so a
    /// number per 24-hour tick is not a budget but a queue that only grows.
    ///
    /// The fixed quantity in this system is neither the base nor the sweep — it
    /// is what the hardware can get through. So the budget is a rate.
    pub dedupe_interval_mins: u64,
    /// Units armed per tick. With the default interval that is a ceiling of
    /// twenty calls an hour, whatever the base has grown to.
    ///
    /// Zero switches the model off entirely: pairs are still found, recorded and
    /// clustered — all of which is free — and nothing is ever asked about.
    pub max_dedupe_per_tick: usize,
    /// An active artifact not confirmed accurate (`last_verified_at`) in this
    /// many days becomes a deprecation-review candidate — never anything more
    /// automatic than that. See `stale_max_hits`.
    pub stale_after_days: u32,
    /// ...and retrieved at most this many times since. Both conditions must
    /// hold: staleness alone is not suspicious for a rare topic, and
    /// popularity alone says nothing about accuracy. This is read-only input
    /// to the candidate list — it never feeds search scoring, or a frequently
    /// shown result would keep boosting its own visibility.
    pub stale_max_hits: i64,
}

impl Default for ConsolidateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            near_dupe_min: 0.90,
            review_min: 0.88,
            auto_supersede: 0.95,
            per_point: 5,
            interval_hours: 24,
            dedupe_interval_mins: 15,
            max_dedupe_per_tick: 5,
            stale_after_days: 365,
            stale_max_hits: 0,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub bind: String,
    #[serde(default = "default_workers")]
    pub workers: usize,
}
fn default_workers() -> usize {
    2
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct StoreConfig {
    /// The instance-wide control database: identity, and the job queue.
    pub control_path: String,
    /// Where per-tenant databases live, one `{slug}.db` per user.
    pub dir: String,
    /// How many tenants may be open at once. An open tenant costs a SQLite
    /// pool and a background queue; the rest are opened on demand, and
    /// eviction is transparent because the next request reopens the same file
    /// — and, through `Tenants::working_for`, over the same sitting. Reopening
    /// the file was never the whole of that promise: the working memory search
    /// and ask carry lives in the `Core` the cache miss rebuilds, so until it
    /// was held across eviction this cap was visible to any user unlucky
    /// enough to be evicted between two requests. See `core::Working`.
    pub max_open_tenants: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            control_path: "engram-control.db".into(),
            dir: "data/users".into(),
            max_open_tenants: 32,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct VectorConfig {
    pub url: String,
    pub collection: String,
    #[serde(default)]
    pub api_key: Option<String>,
    /// How much a result's age counts against it. Fused ranks land between
    /// roughly 0.1 and 1.0, so the default breaks near-ties in favour of the
    /// newer note without ever overturning a clearly better match. `0.0` turns
    /// recency off entirely.
    #[serde(default = "default_recency_weight")]
    pub recency_weight: f32,
    /// Age at which a chunk has lost half of that boost.
    #[serde(default = "default_recency_half_life_days")]
    pub recency_half_life_days: u32,
    /// Extra score for a chunk carrying the `pinned` tag, so something you
    /// decided matters can outrank the decay curve.
    #[serde(default = "default_pinned_boost")]
    pub pinned_boost: f32,
    /// Cosine similarity below which a result is only loosely related to the
    /// query, and is labelled as such rather than presented like a real answer.
    ///
    /// This is a similarity, not a rank: hybrid retrieval returns reciprocal
    /// rank fusion values, which say where a result placed and nothing about
    /// how close it was, so the top hit for a typo scores exactly like the top
    /// hit for a perfect match. The similarity is read separately — see
    /// `VectorStore::search` — and compared here.
    ///
    /// Normalised embeddings put unrelated text around 0.0–0.2 and genuinely
    /// related text well above 0.4, so the default sits between them. Raise it
    /// to be told more often that nothing really matched; `0.0` turns the
    /// labelling off.
    #[serde(default = "default_weak_below")]
    pub weak_below: f32,
    /// Chunks one document may contribute to a result list. `0` lets a single
    /// document fill it.
    ///
    /// A setting rather than the constant it was, because the tuning sweep
    /// measures it and applying a recommendation writes it back here — the
    /// file stays the one place the running configuration can be read.
    #[serde(default = "default_per_source_cap")]
    pub per_source_cap: usize,
}
fn default_recency_weight() -> f32 {
    0.05
}
fn default_per_source_cap() -> usize {
    crate::core::search::MAX_PER_CORPUS
}
fn default_recency_half_life_days() -> u32 {
    180
}
fn default_pinned_boost() -> f32 {
    0.15
}
fn default_weak_below() -> f32 {
    0.35
}

/// A named endpoint and its defaults. Roles point at one instead of each
/// carrying its own, so "which model is this call worth" is a decision made
/// once rather than repeated per role.
#[derive(Debug, Deserialize, Clone)]
pub struct TierConfig {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub ceiling_param: Option<CeilingParam>,
    #[serde(default = "default_true")]
    pub structured_output: bool,
}

/// How much inference capture spends. `Off` embeds the source text verbatim
/// and calls nothing; `Earned` does the same at capture and synthesizes later
/// where use has shown it is worth it; `Eager` is one synthesis call per
/// segment at capture — what engram did before the other two existed.
///
/// `Earned` is the default. What a base is for is answering, and capture
/// cannot know which of ten thousand paragraphs will ever be asked about — so
/// synthesising all of them spends a model call per segment on text most of
/// which is never retrieved, and replaces the operator's own words with a
/// rewrite before anyone has asked for one. At `earned` the source goes in as
/// it was written and stays that way; a window is rewritten when reading has
/// shown it is worth rewriting, and every artifact in the base can name the
/// use that earned it. `eager` remains supported for a base that wants
/// everything pre-written and is willing to pay for it up front.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SynthesisMode {
    Off,
    #[default]
    Earned,
    Eager,
}

impl SynthesisMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SynthesisMode::Off => "off",
            SynthesisMode::Earned => "earned",
            SynthesisMode::Eager => "eager",
        }
    }
}

/// The window budget when no synthesizer is configured to derive one from.
/// Estimator tokens. A synthesizer configured later whose context is smaller
/// than the windows already stored means re-capturing; there is no migration.
pub const DEFAULT_SEGMENT_TOKENS: usize = 4096;
/// The retrieval unit. A target, not a ceiling: see the spec on why it is
/// fixed rather than derived from the embedder's capacity.
pub const DEFAULT_CHUNK_TOKENS: usize = 384;

fn default_segment_tokens() -> usize {
    DEFAULT_SEGMENT_TOKENS
}
fn default_chunk_tokens() -> usize {
    DEFAULT_CHUNK_TOKENS
}

/// The resolved roles. Deserialised through [`RawInferConfig`] so that tiers
/// are flattened away before anything downstream sees a role: `HttpCompleter`
/// and friends keep taking a struct whose every field is concrete, and a tier
/// stays a spelling of the config file rather than a concept the call path has
/// to know about.
#[derive(Debug, Deserialize, Clone)]
#[serde(try_from = "RawInferConfig")]
pub struct InferConfig {
    pub synthesis: SynthesisMode,
    pub segment_tokens: usize,
    /// `None` is allowed only at `synthesis = "off"`.
    pub synthesize: Option<SynthesizeRole>,
    pub embed: EmbedRole,
    /// `None` closes the ask door: no page, no nav entry, no tool.
    pub ask: Option<AskRole>,
    pub rerank: Option<RerankRole>,
    pub vision: Option<VisionRole>,
}

/// The file's shape, before tiers are folded into the roles. Every endpoint
/// field on a role is optional here: it comes from the tier unless the role
/// overrides it.
#[derive(Debug, Deserialize)]
pub struct RawInferConfig {
    #[serde(default)]
    tiers: HashMap<String, TierConfig>,
    #[serde(default)]
    synthesis: SynthesisMode,
    #[serde(default = "default_segment_tokens")]
    segment_tokens: usize,
    #[serde(default)]
    synthesize: Option<RawSynthesizeRole>,
    embed: EmbedRole,
    #[serde(default)]
    ask: Option<RawAskRole>,
    #[serde(default)]
    rerank: Option<RerankRole>,
    #[serde(default)]
    vision: Option<RawVisionRole>,
}

#[derive(Debug, Deserialize)]
struct RawSynthesizeRole {
    tier: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    context_tokens: Option<usize>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    ceiling_param: Option<CeilingParam>,
    #[serde(default)]
    structured_output: Option<bool>,
    // Role-only, unchanged.
    #[serde(default = "default_output_ratio")]
    output_ratio: f32,
    #[serde(default = "default_context_opening_tokens")]
    context_opening_tokens: usize,
    #[serde(default = "default_context_overlap_tokens")]
    context_overlap_tokens: usize,
}

#[derive(Debug, Deserialize)]
struct RawAskRole {
    tier: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    context_tokens: Option<usize>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    ceiling_param: Option<CeilingParam>,
    // Role-only.
    /// Named `plan` since the call became "which subjects are missing" rather
    /// than "what is the one thing missing". The old name is still accepted:
    /// a renamed key that silently reverts to its default is an operator whose
    /// switch stopped working without anything saying so.
    #[serde(default = "default_plan", alias = "follow_up")]
    plan: bool,
    #[serde(default, alias = "follow_up_tier")]
    plan_tier: Option<String>,
}

/// On. The fan-out is what asking means now: a question that spans several
/// subjects is retrieved for all of them or answered from whichever one the
/// single ranked list happened to favour. The cost is one cheap call per
/// question, and `plan_tier` is where that cost is placed.
fn default_plan() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct RawVisionRole {
    model: String,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
    #[serde(default)]
    ceiling_param: Option<CeilingParam>,
}

/// The endpoint a role runs on: the tier it names.
///
/// A name that matches nothing is refused rather than defaulted. What that
/// prevents is a typo running every call of one role against a different model
/// than the operator wrote down — a divergence no later stage could notice,
/// let alone report.
fn resolve_endpoint(
    role: &str,
    tier_name: &str,
    tiers: &HashMap<String, TierConfig>,
) -> Result<TierConfig, String> {
    tiers.get(tier_name).cloned().ok_or_else(|| {
        let mut known: Vec<&str> = tiers.keys().map(String::as_str).collect();
        known.sort_unstable();
        format!(
            "[infer.{role}] points at tier `{tier_name}`, which is not defined. \
             Known tiers: {}. Define it under [infer.tiers.{tier_name}].",
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        )
    })
}

impl TryFrom<RawInferConfig> for InferConfig {
    type Error = String;

    fn try_from(raw: RawInferConfig) -> Result<Self, Self::Error> {
        let tiers = &raw.tiers;

        let synthesize = match raw.synthesize {
            None => None,
            Some(s) => {
                let st = resolve_endpoint("synthesize", &s.tier, tiers)?;
                Some(SynthesizeRole {
                    base_url: s.base_url.unwrap_or(st.base_url),
                    model: s.model.unwrap_or(st.model),
                    api_key: s.api_key.or(st.api_key),
                    context_tokens: s.context_tokens.unwrap_or(st.context_tokens),
                    max_output_tokens: s.max_output_tokens.unwrap_or(st.max_output_tokens),
                    output_ratio: s.output_ratio,
                    reasoning_effort: s.reasoning_effort.or(st.reasoning_effort),
                    ceiling_param: s.ceiling_param.or(st.ceiling_param),
                    timeout_secs: s.timeout_secs.unwrap_or(st.timeout_secs),
                    structured_output: s.structured_output.unwrap_or(st.structured_output),
                    context_opening_tokens: s.context_opening_tokens,
                    context_overlap_tokens: s.context_overlap_tokens,
                })
            }
        };

        let ask = match raw.ask {
            None => None,
            Some(a) => {
                let at = resolve_endpoint("ask", &a.tier, tiers)?;
                // Resolved here rather than where it is used, so a typo in the name is
                // a startup failure like every other tier name instead of a surprise on
                // the first question someone asks.
                let plan_endpoint = match a.plan_tier.as_deref() {
                    Some(name) => Some(resolve_endpoint("ask.plan_tier", name, tiers)?),
                    None => None,
                };
                Some(AskRole {
                    base_url: a.base_url.unwrap_or(at.base_url),
                    model: a.model.unwrap_or(at.model),
                    api_key: a.api_key.or(at.api_key),
                    context_tokens: a.context_tokens.unwrap_or(at.context_tokens),
                    max_output_tokens: a.max_output_tokens.unwrap_or(at.max_output_tokens),
                    timeout_secs: a.timeout_secs.unwrap_or(at.timeout_secs),
                    reasoning_effort: a.reasoning_effort.or(at.reasoning_effort),
                    ceiling_param: a.ceiling_param.or(at.ceiling_param),
                    structured_output: at.structured_output,
                    plan: a.plan,
                    plan_endpoint,
                })
            }
        };

        // Vision is the one role whose endpoint may legitimately be absent:
        // `None` there means the synthesize endpoint, which `VisionRole::resolve`
        // reads later. So it is folded by hand rather than through
        // `resolve_endpoint`, which would refuse that as underspecified.
        let vision = match raw.vision {
            None => None,
            Some(v) => {
                let vt = match v.tier.as_deref() {
                    Some(name) => Some(resolve_endpoint("vision", name, tiers)?),
                    None => None,
                };
                Some(VisionRole {
                    model: v.model,
                    base_url: v
                        .base_url
                        .or_else(|| vt.as_ref().map(|t| t.base_url.clone())),
                    api_key: v
                        .api_key
                        .or_else(|| vt.as_ref().and_then(|t| t.api_key.clone())),
                    // The tier's timeout only where there is a tier: this
                    // role's own default is two minutes, which is about one
                    // image and not about whatever endpoint serves it.
                    timeout_secs: v
                        .timeout_secs
                        .or_else(|| vt.as_ref().map(|t| t.timeout_secs))
                        .unwrap_or_else(default_vision_timeout_secs),
                    // Not inherited: a tier's ceiling is sized for the role it
                    // was written for, and a description is stored as a corpus.
                    max_output_tokens: v
                        .max_output_tokens
                        .unwrap_or_else(default_vision_max_output_tokens),
                    ceiling_param: v
                        .ceiling_param
                        .or_else(|| vt.as_ref().and_then(|t| t.ceiling_param)),
                })
            }
        };

        Ok(InferConfig {
            synthesis: raw.synthesis,
            segment_tokens: raw.segment_tokens,
            synthesize,
            embed: raw.embed,
            ask,
            rerank: raw.rerank,
            vision,
        })
    }
}

/// Seconds an inference request may take before the client gives up.
///
/// Fifteen minutes, which is absurd for a hosted API and about right for the
/// case engram is built for: a small reasoning model on one consumer GPU,
/// where a single segmentation window has been measured at seven minutes and
/// 8000 output tokens. A timeout there is indistinguishable from a dead
/// endpoint to the job runner — the call fails, the job retries, and it fails
/// again at the same wall, forever.
///
/// The cost of setting it too high is a stuck job holding a worker until it
/// gives up. The cost of setting it too low is a corpus that never finishes
/// segmenting, which is worse, so the default errs long. Hosted endpoints
/// should lower it per role.
pub const DEFAULT_TIMEOUT_SECS: u64 = 900;

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

#[derive(Debug, Deserialize, Clone)]
pub struct SynthesizeRole {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    pub output_ratio: f32,
    /// Sent as `reasoning_effort` when set. A reasoning model spends output
    /// budget thinking before it writes any JSON, and that budget is the same
    /// one the chunk list has to fit in — on a small local model the thinking
    /// is what truncates the answer.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Which name this endpoint takes the output ceiling under. See
    /// [`CeilingParam`]. Unset means: infer it, then correct the guess from the
    /// endpoint's own 400.
    #[serde(default)]
    pub ceiling_param: Option<CeilingParam>,
    /// Seconds to wait on one call before giving up on it.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Whether to send the reply's JSON Schema as an OpenAI `json_schema`
    /// response format, so the endpoint constrains decoding to it.
    ///
    /// On by default, because unconstrained is not a safe default here: a 9B
    /// model asked for JSON will close an array with a brace, or omit a
    /// required field, often enough that windows fail permanently — and the
    /// only symptom is a parse error that reads exactly like a truncated reply.
    /// llama.cpp, vLLM and the hosted APIs all honour it. Turn it off for an
    /// endpoint that rejects the field outright.
    ///
    /// Governs the dedupe judge as well, which runs on this endpoint.
    #[serde(default = "default_true")]
    pub structured_output: bool,
    /// Tokens of the document's verbatim opening prepended to every window, so
    /// an artifact from deep in a long document still knows what product and
    /// version it belongs to. Zero disables it.
    #[serde(default = "default_context_opening_tokens")]
    pub context_opening_tokens: usize,
    /// Tokens of each neighbouring window carried on both sides, so a window
    /// that opens mid-procedure can still resolve what its pronouns point at.
    /// Zero disables it.
    #[serde(default = "default_context_overlap_tokens")]
    pub context_overlap_tokens: usize,
}

fn default_true() -> bool {
    true
}

/// The example file's number, so a minimal config does not fail at startup on
/// the one field in this block that had no default. Sized for a small local
/// model, which is what a first instance runs; see the example file for when
/// to lower it.
fn default_output_ratio() -> f32 {
    8.0
}

fn default_context_opening_tokens() -> usize {
    200
}

fn default_context_overlap_tokens() -> usize {
    150
}

#[derive(Debug, Deserialize, Clone)]
pub struct EmbedRole {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub dim: usize,
    pub max_input_tokens: usize,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// See `EmbedTemplates`. Flat on the role rather than nested, so the TOML
    /// reads `[infer.embed] query_template = ...` beside `model`, which is the
    /// other half of the same identity.
    #[serde(default = "default_query_template")]
    pub query_template: String,
    #[serde(default = "default_document_template")]
    pub document_template: String,
    #[serde(default = "default_document_template_untitled")]
    pub document_template_untitled: String,
    /// Passage size at `synthesis = "off"`/`"earned"`, in estimator tokens.
    /// Under `embed` because it is sized to the retrieval unit, not to a
    /// model's context. Clamped to what the embedder will take — see
    /// `effective_chunk_tokens`.
    #[serde(default = "default_chunk_tokens")]
    pub chunk_tokens: usize,
}

impl EmbedRole {
    /// What a stored vector means, in one string: the model, the dimension,
    /// and the three templates the text is rendered through before the call.
    ///
    /// All five are one identity. A vector embedded under `task: search
    /// result | {title}\n{text}` is not comparable with one embedded under
    /// bare `{text}`, and nothing in the vector says which it was — the
    /// collection keeps its name, the model field is unchanged, and the only
    /// symptom is retrieval quietly getting worse. Recorded at boot so a
    /// change can at least be *said*; the answer to it is a re-capture.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(
            format!(
                "{}\n{}\n{}\n{}\n{}",
                self.model,
                self.dim,
                self.query_template,
                self.document_template,
                self.document_template_untitled,
            )
            .as_bytes(),
        ))
    }

    /// `chunk_tokens`, never above the embed path's own ceiling
    /// (`max_input_tokens * 0.8`).
    ///
    /// A ceiling on the passage, not a promise about the rendered document.
    /// What `embed` measures is `render(chunk)` — the title and the template
    /// around it as well as the text — against that same number, so a config
    /// that sets `chunk_tokens` at or near the ceiling still produces passages
    /// that measure oversize once rendered. That costs one split round-trip,
    /// not a loop: `split_oversize` cuts against `limit - envelope_cost`, so
    /// the pieces it makes fit with the envelope on. The envelope is a title
    /// and a template this type has neither of, which is why the allowance is
    /// left where it can be measured rather than guessed at here.
    pub fn effective_chunk_tokens(&self) -> usize {
        let ceiling = (self.max_input_tokens as f32 * 0.8) as usize;
        self.chunk_tokens.min(ceiling).max(1)
    }

    pub fn templates(&self) -> EmbedTemplates {
        EmbedTemplates {
            query_template: self.query_template.clone(),
            document_template: self.document_template.clone(),
            document_template_untitled: self.document_template_untitled.clone(),
        }
    }
}

/// The three strings that, with `model`, fix what a stored vector means.
///
/// EmbeddingGemma is asymmetric: a query and a document are embedded through
/// different prompts, and a document without a title substitutes the literal
/// `none` rather than an empty field. Changing any of these invalidates every
/// stored vector as thoroughly as changing `model` does; there is no fingerprint
/// or rebuild path because there is no base to look after yet — the answer is
/// to drop the collection and re-capture.
///
/// Kept in config rather than code so that a symmetric embedder — `bge-m3`,
/// which engram shipped with — stays one TOML block away: see `legacy()`.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedTemplates {
    /// `{text}` is the query.
    pub query_template: String,
    /// `{title}` and `{text}`.
    pub document_template: String,
    /// `{text}` only; used when the document has no title.
    pub document_template_untitled: String,
}

pub(crate) fn default_query_template() -> String {
    "task: search result | query: {text}".into()
}
pub(crate) fn default_document_template() -> String {
    "title: {title} | text: {text}".into()
}
pub(crate) fn default_document_template_untitled() -> String {
    "title: none | text: {text}".into()
}

impl Default for EmbedTemplates {
    fn default() -> Self {
        Self {
            query_template: default_query_template(),
            document_template: default_document_template(),
            document_template_untitled: default_document_template_untitled(),
        }
    }
}

impl EmbedTemplates {
    /// What `embed_text` produced before templates existed: the title on its
    /// own line above the body, the query as typed. The right block for a
    /// symmetric embedder, and what every test double renders with, so a test
    /// that queries with `"title\ntext"` lands on the document it seeded.
    pub fn legacy() -> Self {
        Self {
            query_template: "{text}".into(),
            document_template: "{title}\n{text}".into(),
            document_template_untitled: "{text}".into(),
        }
    }

    pub fn render_query(&self, query: &str) -> String {
        substitute(&self.query_template, &[("text", query)])
    }

    pub fn render_document(&self, title: Option<&str>, text: &str) -> String {
        match title {
            Some(t) => substitute(&self.document_template, &[("title", t), ("text", text)]),
            None => substitute(&self.document_template_untitled, &[("text", text)]),
        }
    }

    /// Every template must be able to carry what it is for. Checked at config
    /// load; a template that drops `{text}` would embed the same string for
    /// every document and nothing downstream would notice.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.query_template.contains("{text}") {
            return Err("infer.embed.query_template must contain {text}".into());
        }
        if !self.document_template.contains("{text}") {
            return Err("infer.embed.document_template must contain {text}".into());
        }
        if !self.document_template.contains("{title}") {
            return Err("infer.embed.document_template must contain {title}; \
                        use document_template_untitled for the case with no title"
                .into());
        }
        if !self.document_template_untitled.contains("{text}") {
            return Err("infer.embed.document_template_untitled must contain {text}".into());
        }
        Ok(())
    }
}

/// Fill `{name}` placeholders in one left-to-right pass.
///
/// One pass rather than chained `replace` calls: a value that itself contains
/// a placeholder — a heading that reads "about {text}" — must land verbatim,
/// not have the next value spliced into it. A `{name}` that matches no
/// variable is left as written.
pub(crate) fn substitute(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                match vars.iter().find(|(k, _)| *k == name) {
                    Some((_, v)) => out.push_str(v),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push_str(&rest[open..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[derive(Debug, Deserialize, Clone)]
pub struct AskRole {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub context_tokens: usize,
    /// Hard cap on output tokens per answer. An answer is prose for a person to
    /// read, so this is a generous bound on the longest one worth waiting for
    /// rather than a tuning knob — but it must be sent, because an endpoint
    /// asked for no ceiling applies its own, and the model's own stopping is
    /// the only thing between the two.
    #[serde(default = "default_ask_max_output_tokens")]
    pub max_output_tokens: usize,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// See `SynthesizeRole::reasoning_effort`.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// See `SynthesizeRole::ceiling_param`.
    #[serde(default)]
    pub ceiling_param: Option<CeilingParam>,
    /// See `TierConfig::structured_output`. The ask call itself never sends a
    /// response format — it answers in prose — but the planning call does,
    /// and when no `plan_tier` is named it runs on this endpoint, whose flag
    /// it has to honour rather than assume.
    pub structured_output: bool,
    /// Whether one round of planned, fanned-out retrieval follows the first.
    ///
    /// On by default. It is what lets a question that spans several subjects be
    /// retrieved for all of them, and that is the ordinary case rather than a
    /// refinement. The switch survives for the operator with one slow endpoint
    /// and no cheap tier to put the planning call on.
    pub plan: bool,
    /// The resolved endpoint the "which subjects are missing" call runs on,
    /// from `plan_tier`. `None` falls back to this role's own endpoint.
    ///
    /// A `TierConfig` rather than a role, because that is honestly what it is:
    /// an endpoint and its ceilings, handed straight to a completer. That call
    /// is a cheap classification and belongs on the efficient model even when
    /// the answer it feeds belongs on the deep one — which is the capability
    /// the tier names exist to express.
    pub plan_endpoint: Option<TierConfig>,
}

impl AskRole {
    /// The endpoint the planning call runs on.
    ///
    /// The fallback lives here, at config time, and not at call time. A caller
    /// that reached for `Core::completer` when no plan tier was named would
    /// spend an ask-endpoint call on a question the operator may have turned
    /// off — the gate is `plan`, and nothing downstream of it may invent an
    /// endpoint.
    ///
    /// The fallback is this endpoint as its tier described it, `structured_output`
    /// included: a tier that says it takes no response format must not be
    /// sent one by the planning call, which would 400 on every ask and degrade
    /// to the single-round answer — quietly, and one wasted call each time.
    pub fn plan_on(&self) -> TierConfig {
        self.plan_endpoint.clone().unwrap_or_else(|| TierConfig {
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            api_key: self.api_key.clone(),
            context_tokens: self.context_tokens,
            max_output_tokens: self.max_output_tokens,
            timeout_secs: self.timeout_secs,
            reasoning_effort: self.reasoning_effort.clone(),
            ceiling_param: self.ceiling_param,
            structured_output: self.structured_output,
        })
    }
}

fn default_ask_max_output_tokens() -> usize {
    4096
}

/// The request field an endpoint takes the output ceiling under.
///
/// The two names mean the same ceiling and no endpoint accepts both: OpenAI's
/// reasoning models answer a `max_tokens` with a 400 naming
/// `max_completion_tokens`, while a llama.cpp or vLLM build reads `max_tokens`
/// and silently ignores anything it does not recognise — which is the dangerous
/// half, because an unrecognised ceiling is no ceiling at all and the reply is
/// bounded only by the server's own limit.
///
/// Left unset, the name is inferred from `reasoning_effort` and then corrected
/// from the endpoint's 400 if the guess was wrong. That inference is a guess and
/// not a fact: `reasoning_effort = "none"` is exactly what the local-endpoint
/// documentation recommends, and a local endpoint wants `max_tokens`. Set this
/// when the guess is wrong and the endpoint is one of the silent ones, which
/// have no 400 for the probe to learn from.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CeilingParam {
    MaxTokens,
    MaxCompletionTokens,
}

impl CeilingParam {
    /// The name to send the ceiling under when the operator has not said.
    ///
    /// A reasoning model refuses `max_tokens` outright — a 400 naming
    /// `max_completion_tokens` — and `reasoning_effort` is the only signal
    /// available that one is on the other end.
    ///
    /// The two ways of guessing wrong do not cost the same, which is what
    /// decides the ambiguous value. Guess `max_completion_tokens` at a local
    /// endpoint and the field is silently ignored: no ceiling at all, no 400 to
    /// learn from, and a reply bounded only by the server's own limit. Guess
    /// `max_tokens` at a reasoning endpoint and it says so, in a 400 the
    /// transport reads once and remembers. Where the signal is ambiguous, the
    /// safe guess is the one that self-corrects.
    ///
    /// `"none"` is exactly that case. It is what the local-endpoint
    /// documentation — and `config.example.toml` — recommend for suppressing a
    /// local model's thinking, and it is *also* a value hosted reasoning models
    /// accept, so it says nothing either way. Reading it as `max_tokens` costs
    /// a hosted endpoint one self-correcting 400 and saves a silent local one
    /// from running with no ceiling at all.
    pub fn inferred_from(effort: Option<&str>) -> Self {
        match effort.map(str::trim) {
            None => Self::MaxTokens,
            Some(e) if e.eq_ignore_ascii_case("none") => Self::MaxTokens,
            Some(_) => Self::MaxCompletionTokens,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }

    /// The other name — what a rejected ceiling is retried under.
    pub fn flipped(self) -> Self {
        match self {
            Self::MaxTokens => Self::MaxCompletionTokens,
            Self::MaxCompletionTokens => Self::MaxTokens,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct RerankRole {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub style: RerankStyle,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Where the reranker is consulted. Both places unless narrowed: whoever
    /// configured the endpoint wants it working, and `apply = ["ask"]` is the
    /// opt-out for search, where the rerank call is latency a typing operator
    /// would otherwise wait on.
    #[serde(default = "default_rerank_apply")]
    pub apply: Vec<RerankApply>,
}

impl RerankRole {
    pub fn applies_to(&self, place: RerankApply) -> bool {
        self.apply.contains(&place)
    }
}

fn default_rerank_apply() -> Vec<RerankApply> {
    vec![RerankApply::Ask, RerankApply::Search]
}

/// A place the reranker can be consulted. `Ask` is retrieval behind a
/// question; `Search` is every ranked list a caller sees directly — the UI
/// rail, the API, MCP, the extension, and the judging view.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RerankApply {
    Ask,
    Search,
}

/// The vision model that reads a captured image into text. Optional: absent
/// means the image door is closed. `base_url` and `api_key` are optional
/// because the common case is the synthesize endpoint serving a multimodal
/// model too — then only `model` needs saying.
#[derive(Debug, Deserialize, Clone)]
pub struct VisionRole {
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_vision_timeout_secs")]
    pub timeout_secs: u64,
    /// Hard cap on output tokens per description, sent on every call.
    ///
    /// A description is markdown for a person and for the segmenter that reads
    /// it next, so this is a generous bound on the longest one worth waiting
    /// for — but it must be sent, for the reason every other role sends one: an
    /// endpoint asked for no ceiling applies its own, which is far larger, and
    /// a vision model handed a dense screenshot is exactly the kind of caller
    /// that keeps writing. What it produces is stored as a corpus, so an
    /// unbounded reply is not merely a slow call.
    #[serde(default = "default_vision_max_output_tokens")]
    pub max_output_tokens: usize,
    /// See `SynthesizeRole::ceiling_param`. Inherited from the synthesize role
    /// when this one has no endpoint of its own, since then it is that endpoint.
    #[serde(default)]
    pub ceiling_param: Option<CeilingParam>,
}

fn default_vision_timeout_secs() -> u64 {
    120
}

fn default_vision_max_output_tokens() -> usize {
    4096
}

impl VisionRole {
    /// The endpoint and key this role actually calls: its own where given,
    /// the synthesize role's otherwise.
    pub fn resolve(&self, synth: Option<&SynthesizeRole>) -> (String, Option<String>) {
        (
            self.base_url
                .clone()
                .or_else(|| synth.map(|s| s.base_url.clone()))
                // Validation refuses a vision role with neither; this arm is
                // unreachable after `Config::load` and exists so the type
                // system does not have to be argued with here.
                .unwrap_or_default(),
            self.api_key
                .clone()
                .or_else(|| synth.and_then(|s| s.api_key.clone())),
        )
    }

    /// Which name to send the output ceiling under. Inherited only when this
    /// role borrows the synthesize endpoint: a setting about how one server
    /// reads a request says nothing about a different server.
    pub fn ceiling_param(&self, synth: Option<&SynthesizeRole>) -> Option<CeilingParam> {
        self.ceiling_param.or_else(|| {
            self.base_url
                .is_none()
                .then(|| synth.and_then(|s| s.ceiling_param))
                .flatten()
        })
    }

    /// The effort the ceiling name is inferred from when neither role names it,
    /// inherited on the same condition and for the same reason as
    /// [`Self::ceiling_param`].
    ///
    /// Without this, a synthesize role that sets `reasoning_effort` and leaves
    /// `ceiling_param` unset has nothing to inherit, and the two roles guess
    /// different names for the one server they share. This role never sends
    /// `reasoning_effort` itself — it is read here only as the signal about how
    /// that server reads a request.
    pub fn inherited_reasoning_effort<'a>(
        &self,
        synth: Option<&'a SynthesizeRole>,
    ) -> Option<&'a str> {
        self.base_url
            .is_none()
            .then(|| synth.and_then(|s| s.reasoning_effort.as_deref()))
            .flatten()
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RerankStyle {
    Tei,
    Cohere,
    Vllm,
}

impl RerankStyle {
    /// Where the startup probe checks this server is up. The probe has to
    /// carry the same prefix its style's *request* path does, because that is
    /// what the configured `base_url` is written against: vLLM posts to
    /// `v1/rerank` off a bare host, so the probe asks `v1/models`; Cohere
    /// posts to a bare `rerank`, so its `base_url` already ends in `/v1` and
    /// the probe must ask `models` — `v1/models` would request `/v1/v1/models`
    /// and warn "rerank unreachable" at every startup. TEI has no model-list
    /// endpoint; `info` is the one it actually serves.
    pub fn probe_path(self) -> &'static str {
        match self {
            RerankStyle::Tei => "info",
            RerankStyle::Cohere => "models",
            RerankStyle::Vllm => "v1/models",
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct AuthConfig {
    pub mode: AuthMode,
    #[serde(default)]
    pub oidc: Option<OidcConfig>,
    #[serde(default)]
    pub local: Option<LocalConfig>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    Oidc,
    Local,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OidcConfig {
    pub issuer_url: String,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    pub redirect_url: String,
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// Admit everyone the identity provider authenticates, with no list to
    /// name them in.
    ///
    /// Off by default, and deliberately something an operator has to write
    /// down. A first request from a subject engram has never seen provisions a
    /// tenant — a control row, a SQLite file and a Qdrant collection — so
    /// against a provider that lets anyone self-register, an open door is
    /// unbounded resource creation by strangers. Nothing else in the path caps
    /// it. Setting this says the provider's own registration is the gate and
    /// that is understood.
    ///
    /// Ignored when any of the three lists below is non-empty: a listed
    /// deployment already has a narrower door than this could open.
    #[serde(default)]
    pub open_registration: bool,
    /// Subjects admitted by name. Empty, with the other two lists empty as
    /// well, admits everyone the provider authenticates only when
    /// `open_registration` says so — see [`crate::auth::oidc::is_allowed`].
    #[serde(default)]
    pub allowed_subs: Vec<String>,
    #[serde(default)]
    pub allowed_emails: Vec<String>,
    /// Group names from the provider's `groups` claim. Nextcloud's OIDC
    /// provider app only sends this when the admin has turned on group
    /// provisioning for the client; without it the claim is simply absent; and
    /// a subject in a listed group is admitted the same as one listed by
    /// subject or email.
    #[serde(default)]
    pub allowed_groups: Vec<String>,
}
fn default_scopes() -> Vec<String> {
    vec!["openid".into(), "profile".into(), "email".into()]
}

#[derive(Debug, Deserialize, Clone)]
pub struct LocalConfig {
    pub username: String,
    pub password_hash: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config: {0}")]
    Load(#[from] config::ConfigError),
    #[error("config: {0}")]
    Invalid(String),
}

/// Write the two runtime-tunable keys back into the file they came from.
///
/// An edit to the parsed document rather than a re-serialisation of the whole
/// configuration: every comment, every blank line and every key this does not
/// name comes back byte for byte. A file is where an operator explains their
/// own choices to themselves, and handing it back as a machine's would cost
/// more than the setting is worth.
///
/// A missing file is refused rather than created. The apply path promises that
/// memory and disk agree, and a configuration invented by the server is one
/// nobody wrote.
pub fn write_ranking(path: &Path, p: &crate::core::ranking::RankingParams) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(std::io::Error::other)?;
    // Three decimals is the whole resolution the grid has. Widened to f64
    // verbatim, 0.05 writes as 0.05000000074505806 — the file claiming a
    // precision the sweep never measured.
    let weight = (f64::from(p.recency_weight) * 1000.0).round() / 1000.0;
    doc["vector"]["recency_weight"] = toml_edit::value(weight);
    doc["vector"]["per_source_cap"] = toml_edit::value(p.per_source_cap.map_or(0, |n| n as i64));
    write_beside_and_rename(path, &doc.to_string())
}

/// Whichever of the two swept keys the environment is currently setting.
///
/// `load` layers `ENGRAM__*` *after* the file, so an operator who set one of
/// these where the server starts gets that value back on the next boot whatever
/// was just written — while the tuning history goes on naming settings that
/// stopped being in force at the restart. The write is still the right thing to
/// do and the running server does use the new values; what cannot be promised
/// is that they survive. Saying so is the whole of what is available from here:
/// the environment belongs to whoever starts the process, and this is a page
/// with a button on it, not an installer.
pub fn ranking_keys_in_env() -> Vec<String> {
    // Read back rather than assumed, and matched without case, because the
    // config crate lowercases before it compares and an operator's compose file
    // is under no obligation to shout.
    std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| {
            matches!(
                k.to_ascii_uppercase().as_str(),
                "ENGRAM__VECTOR__RECENCY_WEIGHT" | "ENGRAM__VECTOR__PER_SOURCE_CAP"
            )
        })
        .collect()
}

/// Replace `path` with `body` in one step, or leave it exactly as it was.
///
/// `fs::write` truncates and then writes. A crash or a full disk in between
/// leaves the operator holding a half-written configuration — and since a
/// configuration that will not parse is refused rather than ignored, a server
/// that will not start. The file this function exists to preserve byte for
/// byte would be destroyed by the one failure it is most likely to meet.
///
/// The temporary file is a sibling so the rename stays within one filesystem,
/// and it carries the original's permissions: a config file holding a password
/// hash or a client secret must not come back world-readable because it was
/// rewritten.
/// The temporary file, born with no permissions to spare.
///
/// `File::create` opens at `0666 & ~umask` — 0644 on an ordinary host — and
/// widening it first to narrow it a line later leaves a window in which anyone
/// on the box may open it. The bytes land after that, but a descriptor is
/// checked when it is opened and not when it is read, so the window is enough
/// to walk away with a password hash. The mode is set by the same call that
/// makes the file, and the copy of the original's permissions still follows:
/// this decides what the file may never have been, that decides what it ends up
/// as. Truncating rather than `create_new`, so a `.tmp` left behind by an
/// earlier crash does not wedge every write after it.
#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .mode(0o600)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::create(path)
}

fn write_beside_and_rename(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);

    let written = (|| -> std::io::Result<()> {
        let mut f = create_private(&tmp)?;
        #[cfg(unix)]
        f.set_permissions(std::fs::metadata(path)?.permissions())?;
        f.write_all(body.as_bytes())?;
        // Before the rename, not after it: a rename that reaches the disk ahead
        // of the bytes it points at is the same lost file by a slower route.
        f.sync_all()
    })();
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        // Nothing was touched, so the only thing to undo is the temporary file.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
        let mut builder = config::Config::builder();
        if let Some(p) = path {
            builder = builder.add_source(config::File::from(p).required(true));
        } else {
            builder = builder.add_source(config::File::with_name("config").required(false));
        }
        let raw = builder
            .add_source(
                config::Environment::with_prefix("ENGRAM")
                    .separator("__")
                    .list_separator(","),
            )
            .build()?;
        Self::refuse_removed_keys(&raw)?;
        let mut cfg: Config = raw.clone().try_deserialize()?;
        cfg.apply_learn_mode(&raw);
        cfg.normalize();
        cfg.validate()?;
        cfg.warn_on_file_secrets(path);
        cfg.warn_on_defaulted_store(&raw);
        cfg.warn_on_inert_settings();
        cfg.warn_on_inferred_ceiling_param();
        cfg.warn_on_unplaced_plan_cost();
        Ok(cfg)
    }

    /// Keys that used to mean something, and no longer exist.
    ///
    /// Refused rather than ignored, and refused rather than folded into
    /// `[learn]`. Deserialization drops what it does not recognise, so an
    /// upgraded base whose file still says `[feedback] enabled = false` would
    /// parse without complaint and start recording again — the one key whose
    /// whole purpose was to say "keep none of this" turned into a key that
    /// says nothing. Silence is the wrong answer to that.
    ///
    /// An alias would be the other answer, and it cannot be written: the three
    /// old flags were independent, so `feedback.enabled = true` beside
    /// `pursuit.enabled = false` has no single `[learn]` value that means what
    /// the file meant. Naming them and stopping is what leaves the decision
    /// with the person who wrote them.
    ///
    /// This is `migrate`'s rule for a database that is behind the schema,
    /// applied to the file: read first, say what is wrong, change nothing.
    fn refuse_removed_keys(raw: &config::Config) -> Result<(), ConfigError> {
        // `[learn]` is the replacement for all three: recording, the links
        // learned from it, and the pursuits that read both.
        const REMOVED: [(&str, &str); 3] = [
            ("feedback.enabled", "[learn] enabled"),
            ("associate.enabled", "[learn] enabled"),
            ("pursuit.enabled", "[learn] enabled"),
        ];
        let found: Vec<String> = REMOVED
            .iter()
            .filter(|(key, _)| raw.get::<config::Value>(key).is_ok())
            .map(|(key, now)| format!("  {key} — see {now}"))
            .collect();
        if found.is_empty() {
            return Ok(());
        }
        Err(ConfigError::Invalid(format!(
            "this config sets keys that no longer exist:\n{}\nRemove them, or set [learn] to \
             what you mean. They were three switches over one faculty and are one switch now; \
             an upgrade cannot guess which of them you meant, and ignoring them would turn a \
             setting that says \"keep none of this\" into a setting that says nothing.",
            found.join("\n")
        )))
    }

    /// The dial, resolved to the keys it stands for.
    ///
    /// Runs after deserialization and before validation, so what the mode
    /// decides is what the rest of the file is checked against: `off` turns
    /// synthesis off, and the config that names five sections and one mode is
    /// then a config that starts, rather than one refused for having no
    /// `[infer.synthesize]` behind a promotion that can no longer happen.
    ///
    /// A key written in the file — or given in the environment, which lands in
    /// the same merged source — is never touched. That is the whole of the
    /// promise that nobody loses a knob: the mode fills in what was left
    /// unsaid, and says so afterwards through `--print-config`.
    ///
    /// `full` decides nothing, because `full` is the defaults. That is not a
    /// special case so much as the definition: the mode exists to describe the
    /// two bundles that were previously a page of agreeing settings.
    fn apply_learn_mode(&mut self, raw: &config::Config) {
        let mut resolved: Vec<(&'static str, String)> = Vec::new();
        macro_rules! resolve {
            ($key:literal, $field:expr, $value:expr) => {
                if raw.get::<config::Value>($key).is_err() {
                    $field = $value;
                    resolved.push(($key, format!("{}", $field)));
                }
            };
        }
        match self.learn.mode {
            LearnMode::Full => {}
            LearnMode::Off | LearnMode::Learning => {
                // Nothing reads the log on the query path in either mode.
                // Priming and the associative spread are the two things that
                // reorder or extend a result list from what was learned; the
                // sitting's lift is the third, and the offers under the search
                // box are read from the same clusters.
                resolve!("associate.spread_max", self.associate.spread_max, 0);
                resolve!("associate.prime_lift", self.associate.prime_lift, 0);
                resolve!("sitting.prime", self.sitting.prime, false);
                // Promotion is the other reader. It is gated on a threshold
                // rather than a switch, so the way to say "never" in the keys
                // that exist is a threshold no activation reaches — which is
                // also what `--print-config` then shows, in the units the
                // operator would have typed.
                resolve!(
                    "promote.activation_above",
                    self.promote.activation_above,
                    f64::INFINITY
                );
                // A pursuit is the third writer, and the one that writes a new
                // artifact rather than reordering existing ones. Neither mode
                // may grow the corpus from what was recorded: at `learning`
                // that is the whole promise — a sweep that generates while it
                // measures is measuring a corpus its own inputs have moved —
                // and at `off` it follows from the rest. Said in the same
                // shape as promotion, because it is the same kind of gate: an
                // engagement total no pursuit reaches.
                resolve!(
                    "pursuit.min_engagement",
                    self.pursuit.min_engagement,
                    f64::INFINITY
                );
            }
        }
        match self.learn.mode {
            LearnMode::Off => {
                resolve!("learn.enabled", self.learn.enabled, false);
                // Capture-time near-duplicate detection is not this switch: it
                // costs a hash and stays on. What stops is the background
                // sweep, which is a stream of model calls about pairs.
                resolve!("consolidate.enabled", self.consolidate.enabled, false);
                // With nothing promoted, `earned` is `off` with a synthesizer
                // requirement attached. Resolving it here is what lets the
                // five-section config start.
                if raw.get::<config::Value>("infer.synthesis").is_err() {
                    self.infer.synthesis = SynthesisMode::Off;
                    resolved.push(("infer.synthesis", SynthesisMode::Off.as_str().into()));
                }
            }
            LearnMode::Learning => {
                resolve!("learn.enabled", self.learn.enabled, true);
                // The offers under the search box are read from the recorded
                // clusters, so they are a query-path reader and go off here.
                // Not in the shared block: at `off` this key is what arms the
                // retention sweep that ages the recorded rows out
                // (`core/background.rs`), and `recommends()` already ands
                // `learn.enabled`, so resolving it there would buy nothing and
                // freeze every situation and interaction already written down
                // in the database for ever.
                resolve!("recommend.enabled", self.recommend.enabled, false);
            }
            LearnMode::Full => {}
        }
        self.learn.resolved = resolved;
    }

    /// Values that would make a feature quietly useless, put back rather than
    /// refused.
    ///
    /// `feedback.candidates = 0` stores an empty pool for every captured
    /// search: every card renders with nothing to choose, every judgement is
    /// forced through "none of these", and every one of those is recorded as a
    /// find — a ranking failure that never happened, permanently in the
    /// dataset. Nobody types a zero meaning that. It goes back to the default
    /// with a line in the log rather than stopping a server over a number that
    /// only affects an optional feature.
    ///
    /// The ceiling is the other end of the same argument. A captured search
    /// fetches at least `candidates` vectors whatever the caller asked for, so
    /// the number is the width of every search through a captured door, not
    /// just the depth of the pool stored behind it. Left unbounded, a four-digit
    /// value read as "keep plenty" turns every API call into a four-digit vector
    /// fetch, and nothing in the file says so. The ceiling is what the widest
    /// legal search already costs: `MAX_LIMIT` results over-fetched by the
    /// candidate multiplier.
    fn normalize(&mut self) {
        if self.feedback.candidates == 0 {
            let d = FeedbackConfig::default().candidates;
            self.feedback.candidates = d;
            tracing::warn!(
                using = d,
                "feedback.candidates = 0 would store an empty pool for every captured search; \
                 using the default"
            );
        }
        let ceiling = crate::core::search::MAX_LIMIT * crate::core::search::CANDIDATE_MULTIPLIER;
        if self.feedback.candidates > ceiling {
            tracing::warn!(
                configured = self.feedback.candidates,
                using = ceiling,
                "feedback.candidates is the fetch width of every captured search; \
                 capping it at the widest ordinary search"
            );
            self.feedback.candidates = ceiling;
        }
        // The association widths multiply each other on the search path to
        // size one SQL `LIMIT`, and `interval_mins` is multiplied by sixty to
        // make a `Duration`. None of the three has a ceiling that comes from
        // anywhere else, so it is stated here: past these, the number has
        // stopped describing a search someone would run or a rhythm someone
        // would wait for, and the arithmetic is the only thing still reading
        // it. Clamped rather than refused — an oversized width is a typo, not
        // a config that destroys anything.
        const MAX_SPREAD_FROM: usize = 64;
        const MAX_SPREAD_MAX: usize = 64;
        const MAX_PRIME_LIFT: usize = 64;
        // A year. Longer than this and the ticker fires once and effectively
        // never again, which the operator can say by setting `enabled = false`.
        const MAX_INTERVAL_MINS: u64 = 525_600;
        for (name, value, ceiling) in [
            (
                "associate.spread_from",
                &mut self.associate.spread_from,
                MAX_SPREAD_FROM,
            ),
            (
                "associate.spread_max",
                &mut self.associate.spread_max,
                MAX_SPREAD_MAX,
            ),
            (
                "associate.prime_lift",
                &mut self.associate.prime_lift,
                MAX_PRIME_LIFT,
            ),
        ] {
            if *value > ceiling {
                tracing::warn!(
                    setting = name,
                    configured = *value,
                    using = ceiling,
                    "association width is far past anything a result list can use; capping it"
                );
                *value = ceiling;
            }
        }
        if self.associate.interval_mins > MAX_INTERVAL_MINS {
            tracing::warn!(
                configured = self.associate.interval_mins,
                using = MAX_INTERVAL_MINS,
                "associate.interval_mins is longer than a year; capping it"
            );
            self.associate.interval_mins = MAX_INTERVAL_MINS;
        }
        // The backdrop is a decoration, and every other thing about it is
        // budgeted — a capped device pixel ratio, a frame ceiling, one fetch
        // per refresh window. `sample_size` is the one number that reaches
        // both a Qdrant scroll limit and a `dim × n` projection loop, so a
        // typo'd extra zero turns a page load into a scroll of the whole
        // collection with its dense vectors materialized in memory.
        const MAX_SAMPLE_SIZE: usize = 20_000;
        if self.ui.background.sample_size > MAX_SAMPLE_SIZE {
            tracing::warn!(
                configured = self.ui.background.sample_size,
                using = MAX_SAMPLE_SIZE,
                "ui.background.sample_size is far past what a decorative cloud can draw; capping it"
            );
            self.ui.background.sample_size = MAX_SAMPLE_SIZE;
        }
    }

    /// Rules that a config can satisfy syntactically and still be wrong.
    ///
    /// The thresholds are the only ones so far. `auto_supersede` no longer
    /// hides anything on the score alone — it marks the band the sweep asks
    /// about *first*, ordering `pairs_to_judge` — but it is still refused at or
    /// below `review_min`, because a fast lane that starts below the floor for
    /// looking at a pair at all is a number that cannot mean anything. The
    /// operator who typed it means one of the two, and neither is what they
    /// would get.
    fn validate(&self) -> Result<(), ConfigError> {
        let c = &self.consolidate;
        if c.auto_supersede <= c.review_min {
            return Err(ConfigError::Invalid(format!(
                "consolidate.auto_supersede ({}) must be above consolidate.review_min ({}): \
                 it marks the pairs asked about first, and cannot sit below the score at \
                 which a pair is worth asking about",
                c.auto_supersede, c.review_min
            )));
        }
        if self.infer.synthesis != SynthesisMode::Off && self.infer.synthesize.is_none() {
            return Err(ConfigError::Invalid(format!(
                "infer.synthesis = \"{}\" needs [infer.synthesize]; only \"off\" runs without a \
                 synthesizer",
                self.infer.synthesis.as_str()
            )));
        }
        if let Some(v) = &self.infer.vision
            && v.base_url.is_none()
            && self.infer.synthesize.is_none()
        {
            return Err(ConfigError::Invalid(
                "infer.vision has no base_url or tier and there is no [infer.synthesize] to \
                 borrow an endpoint from"
                    .into(),
            ));
        }
        self.infer
            .embed
            .templates()
            .validate()
            .map_err(ConfigError::Invalid)?;
        Ok(())
    }

    /// Settings that are on but cannot act: say so once at startup rather than
    /// letting an operator discover a faculty has been idle since they turned
    /// `[learn]` off. The combinations that used to be refused here are gone —
    /// there is one switch now, and it cannot disagree with itself.
    /// `[store]` is defaulted rather than required, and a defaulted store is
    /// indistinguishable at runtime from the one the operator meant.
    ///
    /// A missing section is the ordinary case — the five-section file this
    /// mode exists for names none of it — but so is a mistyped `[stroe]` or a
    /// section lost in an edit, and those start on a fresh control database in
    /// the process's working directory rather than the base the operator has.
    /// An empty base and a refusal are both survivable; an empty base that
    /// says nothing is not. Say which paths are in force, once, at startup.
    fn warn_on_defaulted_store(&self, raw: &config::Config) {
        if raw.get::<config::Value>("store").is_ok() {
            return;
        }
        tracing::info!(
            control_path = %self.store.control_path,
            dir = %self.store.dir,
            "no [store] section: using the default paths, resolved against the working \
             directory. If this base has data elsewhere, the section is missing or misspelt."
        );
    }

    fn warn_on_inert_settings(&self) {
        if self.infer.synthesis == SynthesisMode::Earned && !self.learn.enabled {
            tracing::warn!(
                "infer.synthesis = \"earned\" with learn.enabled = false: activation never \
                 moves, so nothing is ever promoted — this is `off` under another name."
            );
        }
        if self.infer.synthesis == SynthesisMode::Earned
            && self.learn.mode == LearnMode::Learning
            && self.promote.activation_above.is_infinite()
        {
            tracing::warn!(
                "infer.synthesis = \"earned\" at learn.mode = \"learning\": activation is \
                 recorded but nothing reads it, so no window is ever promoted. That is what \
                 the mode is for — run the harness here, then move to \"full\" — but the \
                 synthesizer is idle until you do."
            );
        }
    }

    /// The output ceiling's name is a guess whenever `reasoning_effort` is set
    /// and `ceiling_param` is not, and one of the two ways it can be wrong is
    /// silent: an endpoint that ignores the field it does not recognise applies
    /// no ceiling and returns no error, so nothing downstream ever finds out.
    ///
    /// Say which way it guessed, at startup, where an operator can compare it
    /// against the server they know they are running. The 400 that corrects the
    /// other direction needs no warning — it corrects itself.
    fn warn_on_inferred_ceiling_param(&self) {
        for (role, effort) in self.inferred_ceiling_params() {
            tracing::warn!(
                role,
                reasoning_effort = effort,
                guessing = CeilingParam::inferred_from(Some(effort)).as_str(),
                "{role}.ceiling_param is unset, so the output ceiling's name is inferred from \
                 reasoning_effort. If this endpoint takes the other name and ignores unknown \
                 fields — llama.cpp and older vLLM builds do — replies run with no ceiling at \
                 all and nothing reports it. Set {role}.ceiling_param to say."
            );
        }
    }

    /// The roles whose output-ceiling name is a guess: `reasoning_effort` set,
    /// `ceiling_param` not. Each with the effort the guess is made from.
    fn inferred_ceiling_params(&self) -> Vec<(&'static str, &str)> {
        let synth = self.infer.synthesize.as_ref();
        let mut roles: Vec<(&str, Option<&str>, Option<CeilingParam>)> = Vec::new();
        if let Some(s) = synth {
            roles.push((
                "infer.synthesize",
                s.reasoning_effort.as_deref(),
                s.ceiling_param,
            ));
        }
        if let Some(a) = &self.infer.ask {
            roles.push(("infer.ask", a.reasoning_effort.as_deref(), a.ceiling_param));
        }
        // Vision resolves a name the same way, off values it may have inherited
        // from synthesize — and it is the role a dropped ceiling costs most,
        // since what it writes is stored as a corpus and segmented again.
        if let Some(v) = &self.infer.vision {
            roles.push((
                "infer.vision",
                v.inherited_reasoning_effort(synth),
                v.ceiling_param(synth),
            ));
        }
        // The planning call infers its name the same way (`for_plan`), off a
        // tier of its own — so a guess there is not covered by the ask line
        // above. Without a named tier it runs on ask's endpoint under ask's
        // values, and the ask line is the one that speaks for it.
        if let Some(f) = self
            .infer
            .ask
            .as_ref()
            .and_then(|a| a.plan_endpoint.as_ref())
        {
            roles.push((
                "infer.ask.plan_tier",
                f.reasoning_effort.as_deref(),
                f.ceiling_param,
            ));
        }
        roles
            .into_iter()
            .filter_map(|(role, effort, configured)| match (effort, configured) {
                (Some(effort), None) => Some((role, effort)),
                _ => None,
            })
            .collect()
    }

    /// The planning call has nowhere cheap to go.
    ///
    /// `plan` defaults on, and an operator who never wrote the key gets the
    /// fan-out without asking for it — which is the intent. What is not the
    /// intent is where the call lands: with no `plan_tier`, `plan_on` falls
    /// back to the ask role's own endpoint, so every question now pays a full
    /// deep-model completion in front of its answer. On a local deep model
    /// that is tens of seconds of added latency per ask, arriving on an upgrade
    /// with no config change to point at.
    ///
    /// A warning rather than a different default, because the feature is worth
    /// having and the operator is the only one who knows which of their tiers
    /// is the efficient one. Said once, at startup, where a latency change has
    /// somewhere to be explained.
    fn warn_on_unplaced_plan_cost(&self) {
        let Some(a) = self.infer.ask.as_ref() else {
            return;
        };
        if a.plan && a.plan_endpoint.is_none() {
            tracing::warn!(
                model = %a.model,
                "infer.ask.plan is on with no infer.ask.plan_tier: the planning call \
                 runs on the ask endpoint, one extra completion per question. Name a \
                 cheaper tier there, or set plan = false"
            );
        }
    }

    /// Secrets belong in the environment. A secret sitting in the config file
    /// is a real risk (it gets committed), so say so loudly rather than
    /// rejecting a config that otherwise works.
    fn warn_on_file_secrets(&self, path: Option<&Path>) {
        let Some(p) = path else { return };
        let Ok(body) = std::fs::read_to_string(p) else {
            return;
        };
        for key in ["client_secret", "api_key", "password_hash"] {
            if body.contains(key) {
                tracing::warn!(
                    key,
                    file = %p.display(),
                    "secret found in config file; prefer the ENGRAM__ environment variable"
                );
            }
        }
    }

    pub fn redacted(&self) -> String {
        let mut c = self.clone();
        const R: &str = "REDACTED";
        c.vector.api_key = c.vector.api_key.map(|_| R.into());
        if let Some(s) = c.infer.synthesize.as_mut() {
            s.api_key = s.api_key.as_ref().map(|_| R.into());
        }
        c.infer.embed.api_key = c.infer.embed.api_key.map(|_| R.into());
        if let Some(a) = c.infer.ask.as_mut() {
            a.api_key = a.api_key.as_ref().map(|_| R.into());
            if let Some(f) = a.plan_endpoint.as_mut() {
                f.api_key = f.api_key.as_ref().map(|_| R.into());
            }
        }
        if let Some(r) = c.infer.rerank.as_mut() {
            r.api_key = r.api_key.as_ref().map(|_| R.into());
        }
        if let Some(v) = c.infer.vision.as_mut() {
            v.api_key = v.api_key.as_ref().map(|_| R.into());
        }
        if let Some(o) = c.auth.oidc.as_mut() {
            o.client_secret = o.client_secret.as_ref().map(|_| R.into());
        }
        if let Some(l) = c.auth.local.as_mut() {
            l.password_hash = R.into();
        }
        // The dial first, and in the file's own spelling. The dump below it is
        // the resolved config, so the values are all in there — but reading a
        // hundred fields to work out which of them the mode decided is the
        // question this line answers directly.
        let mut head = format!("# learn.mode = \"{}\"\n", c.learn.mode.as_str());
        if c.learn.resolved.is_empty() {
            head.push_str("# nothing was resolved from it: every key it stands for is set\n");
        } else {
            head.push_str("# resolved from it, because the file did not say:\n");
            for (key, value) in &c.learn.resolved {
                head.push_str(&format!("#   {key} = {value}\n"));
            }
        }
        format!("{head}\n{c:#?}")
    }
}

impl Config {
    /// A `Config` with every role configured and nothing reachable.
    ///
    /// Lives here rather than in the binary's tests because the tenant
    /// registry needs one too, and two fixtures drifting apart is how a test
    /// starts asserting against a config the binary never builds. Not behind
    /// `cfg(test)`: the binary's own tests compile against this crate as a
    /// dependency, where that flag is not set.
    #[doc(hidden)]
    pub fn test_default() -> Config {
        Config {
            server: ServerConfig {
                bind: "127.0.0.1:8080".into(),
                workers: 2,
            },
            store: StoreConfig::default(),
            vector: VectorConfig {
                url: "http://localhost:6333".into(),
                collection: "chunks".into(),
                api_key: None,
                recency_weight: 0.05,
                recency_half_life_days: 180,
                pinned_boost: 0.15,
                weak_below: 0.35,
                per_source_cap: 3,
            },
            infer: InferConfig {
                synthesis: SynthesisMode::Eager,
                segment_tokens: DEFAULT_SEGMENT_TOKENS,
                synthesize: Some(SynthesizeRole {
                    base_url: "http://localhost:8000/v1".into(),
                    model: "m".into(),
                    api_key: None,
                    context_tokens: 32768,
                    max_output_tokens: 8192,
                    output_ratio: 1.4,
                    timeout_secs: DEFAULT_TIMEOUT_SECS,
                    reasoning_effort: None,
                    ceiling_param: None,
                    structured_output: true,
                    context_opening_tokens: 200,
                    context_overlap_tokens: 150,
                }),
                embed: EmbedRole {
                    base_url: "http://localhost:8000/v1".into(),
                    model: "e".into(),
                    api_key: None,
                    dim: 1024,
                    max_input_tokens: 8192,
                    timeout_secs: DEFAULT_TIMEOUT_SECS,
                    query_template: EmbedTemplates::default().query_template,
                    document_template: EmbedTemplates::default().document_template,
                    document_template_untitled: EmbedTemplates::default()
                        .document_template_untitled,
                    chunk_tokens: DEFAULT_CHUNK_TOKENS,
                },
                ask: Some(AskRole {
                    base_url: "http://localhost:8000/v1".into(),
                    model: "m".into(),
                    api_key: None,
                    context_tokens: 32768,
                    max_output_tokens: 4096,
                    timeout_secs: DEFAULT_TIMEOUT_SECS,
                    reasoning_effort: None,
                    ceiling_param: None,
                    plan: false,
                    structured_output: true,
                    plan_endpoint: None,
                }),
                rerank: None,
                vision: None,
            },
            auth: AuthConfig {
                mode: AuthMode::Local,
                oidc: None,
                local: Some(LocalConfig {
                    username: "dev".into(),
                    password_hash: "$argon2id$v=19$m=1,t=1,p=1$c2FsdA$aaaa".into(),
                }),
            },
            consolidate: ConsolidateConfig::default(),
            learn: LearnConfig::default(),
            feedback: FeedbackConfig::default(),
            capture: CaptureConfig::default(),
            pacing: PacingConfig::default(),
            associate: AssociateConfig::default(),
            activation: ActivationConfig::default(),
            promote: PromoteConfig::default(),
            pursuit: PursuitConfig::default(),
            schedule: ScheduleConfig::default(),
            sitting: SittingConfig::default(),
            recommend: RecommendConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Environment variables are process-global, but `cargo test` runs tests on
    /// parallel threads. Without this, the env-override test mutates `ENGRAM__*`
    /// while another test is deserializing config and the two race.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn the_capture_defaults_are_the_documented_ones() {
        let c = CaptureConfig::default();
        assert_eq!(c.fetch_timeout_secs, 30);
        assert_eq!(c.fetch_max_bytes, 8 * 1024 * 1024);
        // The floor below which extraction is reported as a failure rather
        // than stored as a corpus.
        assert_eq!(c.min_extracted_chars, 200);
    }

    #[test]
    fn the_background_ships_on() {
        let b = UiConfig::default().background;
        assert!(b.enabled);
        assert_eq!(b.sample_size, 2000);
    }

    #[test]
    fn the_example_config_carries_the_background_block() {
        // Read as text first, then parsed — a load-and-compare test alone would
        // pass with the block deleted, because `#[serde(default)]` fills in the
        // same numbers. See `the_example_config_carries_the_recommend_block`.
        let raw = std::fs::read_to_string("config.example.toml").unwrap();
        assert!(
            raw.contains("\n[ui.background]\n"),
            "the block is documented"
        );
    }

    #[test]
    fn the_recommender_ships_on_with_its_weights_named() {
        let r = RecommendConfig::default();
        // On, with the floor of the ladder as the honest answer while the base
        // is young. It still needs `[learn]`, which is where the log it reads
        // is switched on — see `Core::recommends`.
        assert!(r.enabled);
        assert_eq!(r.weights.of("weekday"), 1.0);
        // Nothing weights who is asking. Isolation is a database and a
        // collection per user, plus the exact cut in `Core::offer` — never a
        // direction in a vector.
        assert_eq!(r.weights.of("scope"), 0.0);
        assert_eq!(r.weights.of("month_cycle"), 0.0, "off by default");
        // A block nobody named contributes nothing rather than a default. The
        // block table and this lookup are edited together, and a typo that
        // silently gave a block weight 1.0 would be a recommendation nobody
        // could account for.
        assert_eq!(r.weights.of("phase_of_the_moon"), 0.0);
        // The two rungs are far enough apart to mean different things.
        assert!(r.strong_at > r.weak_at + 0.2);
        assert_eq!(r.self_weight, 0.0, "the offer does not teach itself");
    }

    #[test]
    fn the_example_config_carries_the_recommend_block() {
        // Every value here equals its struct default, which is the point — the
        // example file documents what ships. That also makes a load-and-compare
        // test vacuous on its own: it would pass with the block deleted, because
        // `#[serde(default)]` would fill in the same numbers. So the file is
        // read as text first, and only then parsed.
        let path = std::path::Path::new("config.example.toml");
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(raw.contains("\n[recommend]\n"), "the block is documented");
        assert!(
            raw.contains("\n[recommend.weights]\n"),
            "and so is every weight the offer rests on"
        );
        for block in [
            "time_of_day",
            "weekday",
            "weekend",
            "device",
            "viewport",
            "locale",
            "network",
            "power",
            "environment",
            "month_cycle",
        ] {
            assert!(raw.contains(&format!("\n{block} = ")), "{block} is unnamed");
        }

        // And it parses: a mistyped value or a table the loader cannot reach
        // fails here rather than at somebody's boot.
        let cfg = Config::load(Some(path)).unwrap();
        assert!(cfg.recommend.enabled);
        // And the one switch it runs behind, in the file rather than only in
        // the defaults: an operator turning the layer off must be able to find
        // the key without reading the source.
        assert!(raw.contains("\n[learn]\n"), "the example names no [learn]");
        assert!(cfg.learn.enabled);
        assert_eq!(cfg.recommend.max_clusters, 5);
        assert_eq!(cfg.recommend.weights.of("network"), 0.6);
    }

    #[test]
    fn the_example_config_carries_the_capture_block() {
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert_eq!(cfg.capture.min_extracted_chars, 200);
    }

    /// The setting exists for the endpoint the guess gets wrong, so the value an
    /// operator types has to be the value the wire carries. Unset stays unset —
    /// that is what leaves the guess and its correction in charge.
    #[test]
    fn the_ceiling_parameter_is_named_in_config_the_way_it_is_named_on_the_wire() {
        for (typed, want) in [
            ("max_tokens", CeilingParam::MaxTokens),
            ("max_completion_tokens", CeilingParam::MaxCompletionTokens),
        ] {
            let p: CeilingParam = serde_json::from_str(&format!("\"{typed}\"")).unwrap();
            assert_eq!(p, want);
            assert_eq!(p.as_str(), typed);
            assert_eq!(p.flipped().flipped(), p);
            assert_ne!(p.flipped(), p);
        }
        // And the two names share no substring, which is what lets a rejection
        // naming one of them be read unambiguously.
        assert!(
            !CeilingParam::MaxCompletionTokens
                .as_str()
                .contains(CeilingParam::MaxTokens.as_str())
        );

        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert!(
            cfg.infer.ask.as_ref().unwrap().ceiling_param.is_none()
                && cfg
                    .infer
                    .synthesize
                    .as_ref()
                    .unwrap()
                    .ceiling_param
                    .is_none(),
            "the example config pins a ceiling name it should be leaving to the guess"
        );
    }

    #[test]
    fn the_default_timeout_survives_a_slow_local_model() {
        // A segmentation window against a 9B model on one consumer GPU has been
        // measured at seven minutes. Anything shorter turns a working setup
        // into an endless retry loop, so this number is load-bearing rather
        // than arbitrary.
        const {
            assert!(
                DEFAULT_TIMEOUT_SECS >= 600,
                "the default must outlast a local reasoning model's slowest window"
            )
        };
    }

    #[test]
    fn a_config_without_timeouts_still_gets_them() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(
            cfg.infer.synthesize.as_ref().unwrap().timeout_secs,
            DEFAULT_TIMEOUT_SECS
        );
        assert_eq!(cfg.infer.embed.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(
            cfg.infer.ask.as_ref().unwrap().timeout_secs,
            DEFAULT_TIMEOUT_SECS
        );
        assert_eq!(
            cfg.infer.synthesize.as_ref().unwrap().reasoning_effort,
            None
        );
    }

    #[test]
    fn a_key_that_no_longer_exists_stops_the_start_rather_than_being_ignored() {
        // Deserialization drops what it does not recognise, so this file used
        // to load in silence — and `[feedback] enabled = false` is the one key
        // whose entire purpose is to say "keep none of this". Ignored, it
        // became a key that said nothing, and an upgrade turned recording back
        // on for the operator who had most explicitly refused it.
        let dir = tempfile::tempdir().unwrap();
        for key in ["[feedback]\nenabled = false", "[pursuit]\nenabled = false"] {
            let p = write(&dir, &format!("{MINIMAL}\n{key}\n"));
            let err = Config::load(Some(&p)).unwrap_err().to_string();
            assert!(
                err.contains("no longer exist") && err.contains("[learn]"),
                "a removed key loaded without saying so: {err}"
            );
        }
        // `true` is refused as well. It is not a safe no-op to leave lying in a
        // file: the next person to read it would take it for the live switch.
        let p = write(&dir, &format!("{MINIMAL}\n[associate]\nenabled = true\n"));
        assert!(Config::load(Some(&p)).is_err());
        // And a file that has been brought up to date loads.
        let p = write(&dir, &format!("{MINIMAL}\n[learn]\nenabled = false\n"));
        assert!(!Config::load(Some(&p)).unwrap().learn.enabled);
    }

    #[test]
    fn a_zero_candidate_pool_is_put_back_to_the_default() {
        // Zero would store an empty pool for every captured search: nothing to
        // choose on any card, so every judgement is forced through "none of
        // these" and recorded as a find that never happened.
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, &format!("{MINIMAL}\n[feedback]\ncandidates = 0\n"));
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(
            cfg.feedback.candidates,
            FeedbackConfig::default().candidates
        );
        assert!(cfg.learn.enabled, "the rest of the section was dropped");
    }

    #[test]
    fn an_oversized_candidate_pool_is_capped_at_the_widest_ordinary_search() {
        // A captured search fetches at least this many vectors whatever the
        // caller asked for, so the number is the width of every UI, API and MCP
        // search — not just the depth of the pool stored behind it. Four digits
        // here silently made every API call a four-digit vector fetch.
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, &format!("{MINIMAL}\n[feedback]\ncandidates = 2000\n"));
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(
            cfg.feedback.candidates,
            crate::core::search::MAX_LIMIT * crate::core::search::CANDIDATE_MULTIPLIER
        );
    }

    #[test]
    fn an_oversized_background_sample_is_capped() {
        // The number reaches a Qdrant scroll limit and a `dim × n` projection
        // loop at once, for a decoration. An extra zero should not turn a page
        // load into a scroll of the whole collection with its dense vectors.
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[ui.background]\nsample_size = 2000000\n"),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(cfg.ui.background.sample_size, 20_000);
    }

    #[test]
    fn a_deliberate_background_sample_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[ui.background]\nsample_size = 500\n"),
        );
        assert_eq!(
            Config::load(Some(&p)).unwrap().ui.background.sample_size,
            500
        );
    }

    #[test]
    fn a_deliberate_candidate_count_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, &format!("{MINIMAL}\n[feedback]\ncandidates = 5\n"));
        assert_eq!(Config::load(Some(&p)).unwrap().feedback.candidates, 5);
    }

    #[test]
    fn applying_a_recommendation_edits_the_file_and_leaves_the_rest_of_it_alone() {
        // The file is the operator's, not the server's: a rewrite that dropped
        // their comments would be a worse answer than refusing to write at all.
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            "# the note I left myself\n\
             [vector]\n\
             url = \"http://localhost:6333\"   # and this one\n\
             recency_weight = 0.05\n\
             pinned_boost = 0.15\n",
        );
        let params = crate::core::ranking::RankingParams {
            recency_weight: 0.1,
            per_source_cap: None,
        };
        write_ranking(&p, &params).unwrap();

        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("# the note I left myself"), "{out}");
        assert!(out.contains("# and this one"), "{out}");
        assert!(out.contains("pinned_boost = 0.15"), "{out}");
        assert!(out.contains("recency_weight = 0.1"), "{out}");
        assert!(out.contains("per_source_cap = 0"), "no cap is written as 0");
    }

    #[test]
    fn applying_leaves_no_half_written_file_and_no_widened_permissions() {
        // `fs::write` truncates and then writes. A crash or a full disk in
        // between left the operator with an empty configuration and a server
        // that refuses to start on the next boot — the one file this whole
        // `toml_edit` approach exists to preserve. Written beside and renamed
        // over, so it is either the old file or the new one.
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "[vector]\nrecency_weight = 0.05\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let params = crate::core::ranking::RankingParams {
            recency_weight: 0.1,
            per_source_cap: Some(2),
        };
        write_ranking(&p, &params).unwrap();

        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1, "a temporary file was left behind: {left:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600,
                "a file that may hold a password hash came back readable to everyone"
            );
        }
    }

    #[test]
    fn a_config_that_is_not_there_is_refused_rather_than_invented() {
        // A server that writes a configuration nobody wrote is a server with
        // two authors, and the apply path promises the file and memory agree.
        let dir = tempfile::tempdir().unwrap();
        let params = crate::core::ranking::RankingParams {
            recency_weight: 0.1,
            per_source_cap: Some(2),
        };
        assert!(write_ranking(&dir.path().join("absent.toml"), &params).is_err());
    }

    #[test]
    fn a_swept_weight_is_written_at_the_grids_resolution() {
        // f32 to f64 verbatim writes 0.05000000074505806, which is the file
        // saying a precision the sweep never had.
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "[vector]\nrecency_weight = 0.15\n");
        write_ranking(
            &p,
            &crate::core::ranking::RankingParams {
                recency_weight: 0.05,
                per_source_cap: Some(3),
            },
        )
        .unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("recency_weight = 0.05"), "{out}");
        assert!(out.contains("per_source_cap = 3"), "{out}");
    }

    #[test]
    fn tune_defaults_and_file_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(cfg.feedback.tune.min_judgements, 50);
        assert_eq!(cfg.feedback.tune.resweep_after, 10);
        assert_eq!(cfg.vector.per_source_cap, 3);

        let tuned = MINIMAL.replace(
            "collection = \"chunks\"",
            "collection = \"chunks\"\nper_source_cap = 0",
        );
        let p = write(
            &dir,
            &format!("{tuned}\n[feedback.tune]\nmin_judgements = 20\nresweep_after = 5\n"),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(cfg.feedback.tune.min_judgements, 20);
        assert_eq!(cfg.feedback.tune.resweep_after, 5);
        assert_eq!(cfg.vector.per_source_cap, 0);
    }

    fn write(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let p = dir.path().join("config.toml");
        std::fs::write(&p, body).unwrap();
        p
    }

    const MINIMAL: &str = r#"
[server]
bind = "127.0.0.1:8080"

[store]
path = "engram.db"

[vector]
url = "http://localhost:6334"
collection = "chunks"

[infer.tiers.efficient]
base_url = "http://localhost:8000/v1"
model = "qwen"
context_tokens = 32768
max_output_tokens = 8192

[infer.synthesize]
tier = "efficient"
output_ratio = 1.4

[infer.embed]
base_url = "http://localhost:8000/v1"
model = "bge-m3"
dim = 1024
max_input_tokens = 8192

[infer.ask]
tier = "efficient"

[auth]
mode = "local"

[auth.local]
username = "dev"
password_hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaa"
"#;

    /// The five sections the issue names, one mode, and nothing else. No
    /// synthesizer, no tiers, no ask: the base this starts is capture, hybrid
    /// search and whatever `[infer.embed]` can reach.
    const FIVE_SECTIONS: &str = r#"
[server]
bind = "127.0.0.1:8080"

[vector]
url = "http://localhost:6334"
collection = "chunks"

[infer.embed]
base_url = "http://localhost:8000/v1"
model = "bge-m3"
dim = 1024
max_input_tokens = 8192

[auth]
mode = "local"

[auth.local]
username = "dev"
password_hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaa"

[learn]
mode = "off"
"#;

    #[test]
    fn five_sections_and_one_mode_start() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, FIVE_SECTIONS);
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(cfg.learn.mode, LearnMode::Off);
        assert!(!cfg.learn.enabled);
        // `earned` would have refused this file for having no synthesizer, and
        // the promotion it wanted one for cannot happen at `off` anyway.
        assert_eq!(cfg.infer.synthesis, SynthesisMode::Off);
        assert!(cfg.infer.synthesize.is_none());
        assert!(!cfg.consolidate.enabled);
        assert!(!cfg.sitting.prime);
        assert_eq!(cfg.associate.spread_max, 0);
        assert_eq!(cfg.associate.prime_lift, 0);
        assert!(cfg.promote.activation_above.is_infinite());
        assert!(cfg.pursuit.min_engagement.is_infinite());
        // Left alone on purpose. `recommends()` is already false through
        // `learn.enabled`, and this key is what arms the retention sweep — an
        // operator switching learning off is exactly who needs the situations
        // and interactions already recorded to keep ageing out.
        assert!(cfg.recommend.enabled);
    }

    #[test]
    fn off_leaves_the_key_that_ages_recorded_rows_out_alone() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, &format!("{MINIMAL}\n[learn]\nmode = \"off\"\n"));
        let cfg = Config::load(Some(&p)).unwrap();
        // `core::background::periodic_units` arms `Stage::Retention` on
        // `feedback.retain_days > 0 || learn.enabled || recommend.enabled`.
        // At `off` the first two are false, so this key is the whole of what
        // keeps `expire_context_events` and `expire_interactions` running.
        assert!(!cfg.learn.enabled);
        assert_eq!(cfg.feedback.retain_days, 0);
        assert!(cfg.recommend.enabled);
        // And the mode never says so, so nothing about it reads as decided.
        assert!(
            !cfg.learn
                .resolved
                .iter()
                .any(|(k, _)| *k == "recommend.enabled")
        );
    }

    #[test]
    fn learning_records_everything_and_reads_none_of_it() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, &format!("{MINIMAL}\n[learn]\nmode = \"learning\"\n"));
        let cfg = Config::load(Some(&p)).unwrap();
        // Written: the log, and the activation and links read from it.
        assert!(cfg.learn.enabled);
        // Read on the query path: none of it.
        assert_eq!(cfg.associate.spread_max, 0);
        assert_eq!(cfg.associate.prime_lift, 0);
        assert!(!cfg.sitting.prime);
        assert!(!cfg.recommend.enabled);
        assert!(cfg.promote.activation_above.is_infinite());
        // Nothing new is written into the corpus either: a sweep that
        // generates while it measures is measuring its own inputs.
        assert!(cfg.pursuit.min_engagement.is_infinite());
        // Synthesis is not the mode's business here: `learning` is about what
        // moves a rank, and an eager base still writes what it always wrote.
        assert_eq!(cfg.infer.synthesis, SynthesisMode::Earned);
        assert!(cfg.consolidate.enabled);
    }

    #[test]
    fn full_is_todays_defaults_and_resolves_nothing() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let with = write(&dir, &format!("{MINIMAL}\n[learn]\nmode = \"full\"\n"));
        let cfg = Config::load(Some(&with)).unwrap();
        assert!(cfg.learn.resolved.is_empty(), "{:?}", cfg.learn.resolved);
        let plain = Config::load(Some(&write(&tempfile::tempdir().unwrap(), MINIMAL))).unwrap();
        assert_eq!(cfg.learn.mode, plain.learn.mode);
        assert_eq!(cfg.learn.enabled, plain.learn.enabled);
        assert_eq!(cfg.recommend.enabled, plain.recommend.enabled);
        assert_eq!(cfg.associate.spread_max, plain.associate.spread_max);
        assert_eq!(cfg.promote.activation_above, plain.promote.activation_above);
    }

    #[test]
    fn a_key_in_the_file_beats_the_mode() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        // `off` with three of its keys contradicted: the mode fills in what
        // was left unsaid and touches nothing else.
        let p = write(
            &dir,
            &format!(
                "{MINIMAL}\n[learn]\nmode = \"off\"\nenabled = true\n\
                 [recommend]\nenabled = true\n[associate]\nspread_max = 2\n"
            ),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        assert!(cfg.learn.enabled);
        assert!(cfg.recommend.enabled);
        assert_eq!(cfg.associate.spread_max, 2);
        // ...and the rest of the bundle still applies.
        assert!(!cfg.consolidate.enabled);
        assert!(cfg.promote.activation_above.is_infinite());
        let named: Vec<&str> = cfg.learn.resolved.iter().map(|(k, _)| *k).collect();
        assert!(!named.contains(&"learn.enabled"), "{named:?}");
        assert!(!named.contains(&"associate.spread_max"), "{named:?}");
        assert!(named.contains(&"consolidate.enabled"), "{named:?}");
    }

    #[test]
    fn print_config_names_what_the_mode_decided() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, FIVE_SECTIONS);
        let dump = Config::load(Some(&p)).unwrap().redacted();
        assert!(dump.contains("learn.mode = \"off\""), "{dump}");
        assert!(dump.contains("consolidate.enabled = false"), "{dump}");
        assert!(dump.contains("infer.synthesis = off"), "{dump}");
        assert!(dump.contains("promote.activation_above = inf"), "{dump}");
    }

    #[test]
    fn loads_minimal_config() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(cfg.infer.embed.dim, 1024);
        assert_eq!(cfg.vector.collection, "chunks");
        assert!(
            cfg.infer.rerank.is_none(),
            "rerank must default to disabled"
        );
    }

    #[test]
    fn rerank_applies_everywhere_unless_narrowed() {
        // A configured reranker is used for both ask and search unless `apply`
        // names fewer places: whoever set up the endpoint wants it working,
        // and the narrowing is the opt-out for search's latency.
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let rerank = "\n[infer.rerank]\nbase_url = \"http://localhost:8081\"\nmodel = \"bge-reranker-v2-m3\"\nstyle = \"tei\"\n";
        let p = write(&dir, &format!("{MINIMAL}{rerank}"));
        let role = Config::load(Some(&p)).unwrap().infer.rerank.unwrap();
        assert!(role.applies_to(RerankApply::Ask));
        assert!(role.applies_to(RerankApply::Search));

        let p = write(&dir, &format!("{MINIMAL}{rerank}apply = [\"ask\"]\n"));
        let role = Config::load(Some(&p)).unwrap().infer.rerank.unwrap();
        assert!(role.applies_to(RerankApply::Ask));
        assert!(
            !role.applies_to(RerankApply::Search),
            "apply = [\"ask\"] must switch the reranker off for search"
        );
    }

    #[test]
    fn env_overrides_file() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        temp_env::with_var("ENGRAM__INFER__EMBED__DIM", Some("768"), || {
            let cfg = Config::load(Some(&p)).unwrap();
            assert_eq!(cfg.infer.embed.dim, 768);
        });
    }

    #[test]
    fn an_unknown_key_does_not_stop_a_config_that_otherwise_works() {
        // Unknown keys are ignored rather than refused: a server that will not
        // start is a worse answer than one that runs on the keys it knows.
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[consolidate]\nnot_a_setting = 20\n"),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        assert_eq!(
            cfg.consolidate.max_dedupe_per_tick,
            ConsolidateConfig::default().max_dedupe_per_tick
        );
    }

    #[test]
    fn thresholds_that_leave_no_review_band_are_refused() {
        // `auto_supersede` marks the pairs the sweep asks about first. Below
        // `review_min` — the score at which a pair is worth asking about at
        // all — it names a lane no pair can be in, which is a number that
        // means neither of the two things the operator could have intended.
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[consolidate]\nreview_min = 0.88\nauto_supersede = 0.85\n"),
        );
        assert!(matches!(
            Config::load(Some(&p)),
            Err(ConfigError::Invalid(_))
        ));
    }

    #[test]
    fn redacted_hides_secrets() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        let cfg = Config::load(Some(&p)).unwrap();
        let dump = cfg.redacted();
        assert!(!dump.contains("$argon2id$"), "password hash leaked: {dump}");
        assert!(dump.contains("REDACTED"));
    }

    #[test]
    fn vision_is_off_unless_configured() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        let cfg = Config::load(Some(&p)).unwrap();
        assert!(
            cfg.infer.vision.is_none(),
            "vision must default to disabled"
        );
        assert_eq!(cfg.capture.image_max_bytes, 25 * 1024 * 1024);
        assert_eq!(cfg.capture.pdf_max_bytes, 50 * 1024 * 1024);
        assert_eq!(cfg.capture.image_preview_edge, 2048);
    }

    #[test]
    fn a_vision_role_without_its_own_endpoint_inherits_synthesize() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[infer.vision]\nmodel = \"qwen-vl\"\n"),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        let v = cfg.infer.vision.as_ref().expect("configured");
        assert_eq!(v.model, "qwen-vl");
        assert_eq!(v.timeout_secs, 120);
        // Its own ceiling, sent on every call like every other role's.
        assert_eq!(v.max_output_tokens, 4096);
        let (url, key) = v.resolve(cfg.infer.synthesize.as_ref());
        assert_eq!(url, cfg.infer.synthesize.as_ref().unwrap().base_url);
        assert_eq!(key, cfg.infer.synthesize.as_ref().unwrap().api_key);
    }

    /// How a request is read is a property of the server reading it, so the
    /// borrowed name travels exactly as far as the borrowed endpoint does.
    #[test]
    fn the_ceiling_name_is_inherited_only_by_a_vision_role_sharing_the_endpoint() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[infer.vision]\nmodel = \"qwen-vl\"\n"),
        );
        let mut cfg = Config::load(Some(&p)).unwrap();
        cfg.infer.synthesize.as_mut().unwrap().ceiling_param = Some(CeilingParam::MaxTokens);
        let synth = cfg.infer.synthesize.as_ref().unwrap().clone();
        let v = cfg.infer.vision.as_mut().expect("configured");

        assert_eq!(v.ceiling_param(Some(&synth)), Some(CeilingParam::MaxTokens));

        // Its own endpoint: a different server, and nothing carries over.
        v.base_url = Some("http://vision:9000/v1".into());
        assert_eq!(v.ceiling_param(Some(&synth)), None);

        // Unless it says so itself, which beats both.
        v.ceiling_param = Some(CeilingParam::MaxCompletionTokens);
        assert_eq!(
            v.ceiling_param(Some(&synth)),
            Some(CeilingParam::MaxCompletionTokens)
        );
    }

    /// The setting is not the only thing a borrowed endpoint has to inherit.
    /// With `ceiling_param` unset there is nothing explicit to carry, and the
    /// name is inferred from `reasoning_effort` — so a role that borrows the
    /// endpoint has to borrow the signal too, or the two guess differently about
    /// one server.
    #[test]
    fn the_effort_the_name_is_guessed_from_travels_with_the_borrowed_endpoint() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!("{MINIMAL}\n[infer.vision]\nmodel = \"qwen-vl\"\n"),
        );
        let mut cfg = Config::load(Some(&p)).unwrap();
        cfg.infer.synthesize.as_mut().unwrap().reasoning_effort = Some("high".into());
        let synth = cfg.infer.synthesize.as_ref().unwrap().clone();
        let v = cfg.infer.vision.as_mut().expect("configured");

        assert_eq!(
            v.ceiling_param(Some(&synth)),
            None,
            "nothing explicit to inherit"
        );
        assert_eq!(v.inherited_reasoning_effort(Some(&synth)), Some("high"));

        // Its own address is its own server, and the signal stops there.
        v.base_url = Some("http://vision:9000/v1".into());
        assert_eq!(v.inherited_reasoning_effort(Some(&synth)), None);
    }

    /// The ceiling's name is guessed from `reasoning_effort`, and the two ways
    /// of guessing wrong do not cost the same: a hosted endpoint refuses the
    /// wrong name in a 400 the transport learns from, while a local one ignores
    /// it silently and applies no ceiling at all. `"none"` is the value that
    /// says nothing either way — both kinds accept it, and it is what this
    /// project recommends for suppressing local thinking — so it has to fall to
    /// the side that self-corrects.
    #[test]
    fn the_ambiguous_effort_is_guessed_the_way_that_corrects_itself() {
        assert_eq!(CeilingParam::inferred_from(None), CeilingParam::MaxTokens);
        for ambiguous in ["none", "None", " none "] {
            assert_eq!(
                CeilingParam::inferred_from(Some(ambiguous)),
                CeilingParam::MaxTokens,
                "{ambiguous} was read as a hosted reasoning endpoint"
            );
        }
        for hosted in ["minimal", "low", "medium", "high"] {
            assert_eq!(
                CeilingParam::inferred_from(Some(hosted)),
                CeilingParam::MaxCompletionTokens,
                "{hosted}"
            );
        }
    }

    #[test]
    fn a_dedicated_vision_endpoint_wins_over_synthesize() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            &dir,
            &format!(
                "{MINIMAL}\n[infer.vision]\nmodel = \"qwen-vl\"\nbase_url = \"http://vision:9000/v1\"\napi_key = \"vk\"\n"
            ),
        );
        let cfg = Config::load(Some(&p)).unwrap();
        let (url, key) = cfg
            .infer
            .vision
            .as_ref()
            .unwrap()
            .resolve(cfg.infer.synthesize.as_ref());
        assert_eq!(url, "http://vision:9000/v1");
        assert_eq!(key.as_deref(), Some("vk"));
        assert!(!cfg.redacted().contains("\"vk\""), "vision key leaked");
    }

    #[test]
    fn the_example_config_documents_the_vision_role() {
        let text = std::fs::read_to_string("config.example.toml").unwrap();
        assert!(
            text.contains("[infer.vision]"),
            "example config must show the vision block"
        );
        assert!(text.contains("image_max_bytes"));
        assert!(text.contains("pdf_max_bytes"));
    }

    #[test]
    fn the_association_defaults_are_the_documented_ones() {
        let a = AssociateConfig::default();
        assert_eq!(a.interval_mins, 30);
        assert_eq!(a.half_life_days, 30.0);
        assert_eq!((a.show_min, a.judge_min, a.prune_below), (2.0, 4.0, 0.5));
        assert_eq!((a.spread_from, a.spread_max), (3, 3));
        assert_eq!((a.prime_margin, a.prime_lift), (0.5, 0));
        let v = ActivationConfig::default();
        assert_eq!(v.half_life_days, 14.0);
        assert_eq!((v.retrieved, v.opened, v.confirmed), (0.0, 1.0, 3.0));
    }

    #[test]
    fn a_config_with_no_association_block_still_gets_one() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, MINIMAL);
        let cfg = Config::load(Some(&p)).unwrap();
        assert!(cfg.learn.enabled);
        // ...and recording is on too, so the feature is live out of the box:
        // promotion reads activation, and activation moves only while
        // searches are recorded.
        assert!(cfg.learn.enabled);
    }

    #[test]
    fn the_example_config_carries_the_association_block() {
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert_eq!(cfg.associate.spread_max, 3);
    }
    /// The tier tests below say nothing about auth, but a `Config` cannot be
    /// loaded without one; kept out of the fixtures so they read as the shapes
    /// they are actually about.
    const AUTH_TAIL: &str = r#"
        [auth]
        mode = "local"
        [auth.local]
        username = "dev"
        password_hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaa"
    "#;

    fn load_infer(body: &str) -> Result<Config, ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        Config::load(Some(&write(&dir, &format!("{body}{AUTH_TAIL}"))))
    }

    /// A role takes its endpoint from the tier it names.
    #[test]
    fn a_role_resolves_its_endpoint_from_its_tier() {
        let _guard = env_guard();
        let cfg = load_infer(
            r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.efficient]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        [infer.synthesize]
        tier = "efficient"
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "bge-m3"
        dim = 1024
        max_input_tokens = 1024
        [infer.ask]
        tier = "efficient"
        "#,
        )
        .expect("tiered config parses");

        // A minimal block: the one field that had no default has one now.
        assert_eq!(cfg.infer.synthesize.as_ref().unwrap().output_ratio, 8.0);

        assert_eq!(
            cfg.infer.synthesize.as_ref().unwrap().base_url,
            "http://localhost:8000/v1"
        );
        assert_eq!(cfg.infer.synthesize.as_ref().unwrap().model, "qwen");
        assert_eq!(cfg.infer.synthesize.as_ref().unwrap().context_tokens, 32768);
        assert_eq!(
            cfg.infer.synthesize.as_ref().unwrap().max_output_tokens,
            16384
        );
        assert_eq!(
            cfg.infer.ask.as_ref().unwrap().base_url,
            "http://localhost:8000/v1"
        );
        assert_eq!(cfg.infer.ask.as_ref().unwrap().model, "qwen");
    }

    /// A role may override any field its tier defines. Without this the two tiers
    /// would have to multiply by every ceiling an operator wants.
    #[test]
    fn a_role_field_overrides_the_tier_it_points_at() {
        let _guard = env_guard();
        let cfg = load_infer(
            r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.deep]
        base_url = "http://localhost:8000/v1"
        model = "big"
        context_tokens = 131072
        max_output_tokens = 16384
        [infer.synthesize]
        tier = "deep"
        output_ratio = 8.0
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "bge-m3"
        dim = 1024
        max_input_tokens = 1024
        [infer.ask]
        tier = "deep"
        max_output_tokens = 4096
        "#,
        )
        .unwrap();
        assert_eq!(
            cfg.infer.ask.as_ref().unwrap().max_output_tokens,
            4096,
            "the role's value wins"
        );
        assert_eq!(
            cfg.infer.ask.as_ref().unwrap().context_tokens,
            131072,
            "unset fields come from the tier"
        );
        assert_eq!(
            cfg.infer.synthesize.as_ref().unwrap().max_output_tokens,
            16384
        );
    }

    /// A typo in a tier name must be a startup failure naming the typo, never a
    /// silent fallback to some other model.
    #[test]
    fn a_role_pointing_at_a_tier_that_does_not_exist_is_refused() {
        let _guard = env_guard();
        let err = load_infer(
            r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.efficient]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        [infer.synthesize]
        tier = "efficent"
        output_ratio = 8.0
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "bge-m3"
        dim = 1024
        max_input_tokens = 1024
        [infer.ask]
        tier = "efficient"
        "#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("efficent"),
            "the error must name the typo: {err}"
        );
        assert!(err.contains("efficient"), "and what was available: {err}");
    }

    /// The example config is the documentation, so it has to be the shape the
    /// parser accepts and resolve to what the roles expect.
    #[test]
    fn the_example_config_reaches_its_endpoints_through_tiers() {
        let text = std::fs::read_to_string("config.example.toml").unwrap();
        assert!(
            text.contains("[infer.tiers."),
            "the example must show a tier"
        );
        let cfg = Config::load(Some(std::path::Path::new("config.example.toml"))).unwrap();
        assert_eq!(cfg.infer.synthesize.as_ref().unwrap().context_tokens, 32768);
        assert_eq!(
            cfg.infer.synthesize.as_ref().unwrap().max_output_tokens,
            16384
        );
        assert_eq!(cfg.infer.ask.as_ref().unwrap().context_tokens, 32768);
        assert_eq!(cfg.infer.ask.as_ref().unwrap().max_output_tokens, 4096);
    }
    /// A tiered config with one endpoint everything can point at, so the tests
    /// below say only the thing they are about. `[infer.ask]` is last, which is
    /// what lets a test append a key to it.
    const TIERED: &str = r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.efficient]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        api_key = "tier-key"
        context_tokens = 32768
        max_output_tokens = 16384
        timeout_secs = 600
        ceiling_param = "max_completion_tokens"
        [infer.synthesize]
        tier = "efficient"
        output_ratio = 8.0
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "bge-m3"
        dim = 1024
        max_input_tokens = 1024
        [infer.ask]
        tier = "efficient"
    "#;

    /// Vision is the one role folded by hand rather than through
    /// `resolve_endpoint`, and the two halves of that are asymmetric on
    /// purpose: a timeout describes the server, so it travels with the tier,
    /// and an output ceiling describes what the reply is for, so it does not.
    /// A later tidy-up that ran vision through the shared path would give it a
    /// fifteen-minute timeout and the segmenter's ceiling with a green suite,
    /// so the asymmetry is pinned here rather than left to a comment.
    #[test]
    fn a_vision_role_takes_its_endpoint_from_a_tier_but_not_its_output_ceiling() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{TIERED}\n[infer.vision]\nmodel = \"qwen-vl\"\ntier = \"efficient\"\n"
        ))
        .unwrap();
        let v = cfg.infer.vision.as_ref().expect("configured");
        assert_eq!(v.base_url.as_deref(), Some("http://localhost:8000/v1"));
        assert_eq!(v.api_key.as_deref(), Some("tier-key"));
        assert_eq!(
            v.timeout_secs, 600,
            "a timeout is a property of the server, so it comes from the tier"
        );
        assert_eq!(
            v.ceiling_param,
            Some(CeilingParam::MaxCompletionTokens),
            "so is the name the ceiling is sent under"
        );
        assert_eq!(
            v.max_output_tokens,
            default_vision_max_output_tokens(),
            "the tier's 16384 is sized for the segmenter; a description is \
             stored as a corpus and keeps its own bound"
        );
        assert!(!cfg.redacted().contains("tier-key"), "tier key leaked");
    }

    /// A vision block with no endpoint at all is the documented common case:
    /// one server hosting a multimodal model too.
    #[test]
    fn a_vision_role_borrowing_the_synthesize_endpoint_is_the_common_case() {
        let _guard = env_guard();
        let cfg = load_infer(&format!("{TIERED}\n[infer.vision]\nmodel = \"qwen-vl\"\n")).unwrap();
        let v = cfg.infer.vision.as_ref().expect("configured");
        assert!(
            v.base_url.is_none(),
            "`None` is what `resolve` reads as: use the synthesize endpoint"
        );
        assert_eq!(
            v.timeout_secs,
            default_vision_timeout_secs(),
            "with no tier to take one from, the role keeps its own two minutes"
        );
        assert_eq!(
            v.resolve(cfg.infer.synthesize.as_ref()).0,
            cfg.infer.synthesize.as_ref().unwrap().base_url
        );
    }

    /// A vision block may name its own endpoint directly, for the case where a
    /// separate server serves the images.
    #[test]
    fn a_vision_role_may_carry_its_own_endpoint() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{TIERED}\n[infer.vision]\nmodel = \"qwen-vl\"\nbase_url = \"http://vision:9000/v1\"\n"
        ))
        .unwrap();
        // And the endpoint it named is still the one it calls.
        let v = cfg.infer.vision.as_ref().expect("configured");
        assert_eq!(
            v.resolve(cfg.infer.synthesize.as_ref()).0,
            "http://vision:9000/v1"
        );
    }

    /// The planning call's whole reason to name a tier is to run somewhere
    /// cheaper than the answer it feeds, so the endpoint has to arrive resolved
    /// and complete — the completer is handed this, not a role.
    #[test]
    fn a_plan_tier_resolves_to_a_complete_endpoint() {
        let _guard = env_guard();
        let cfg = load_infer(&format!("{TIERED}\nplan_tier = \"efficient\"\n")).unwrap();
        let f = cfg
            .infer
            .ask
            .as_ref()
            .unwrap()
            .plan_endpoint
            .as_ref()
            .expect("the named tier resolved");
        assert_eq!(f.base_url, "http://localhost:8000/v1");
        assert_eq!(f.model, "qwen");
        assert_eq!(f.max_output_tokens, 16384);
        assert_eq!(f.api_key.as_deref(), Some("tier-key"));
        assert!(
            !cfg.redacted().contains("tier-key"),
            "the planning endpoint's key leaked"
        );
    }

    /// With no plan tier named, the call runs on the ask endpoint — the whole
    /// of it, including whether that endpoint takes a response format.
    /// Assuming it does sends a schema to a server configured not to want one,
    /// which 400s every ask's planning call and wastes it, silently.
    #[test]
    fn the_plan_fallback_carries_the_ask_tiers_structured_output_flag() {
        let _guard = env_guard();
        let toml = TIERED.replace(
            "ceiling_param = \"max_completion_tokens\"",
            "ceiling_param = \"max_completion_tokens\"\nstructured_output = false",
        );
        let cfg = load_infer(&toml).unwrap();
        assert!(cfg.infer.ask.as_ref().unwrap().plan_endpoint.is_none());
        assert!(
            !cfg.infer.ask.as_ref().unwrap().plan_on().structured_output,
            "the tier said no response format; the fallback endpoint said yes"
        );
        // And the default remains on, as it is for every tier.
        let cfg = load_infer(TIERED).unwrap();
        assert!(cfg.infer.ask.as_ref().unwrap().plan_on().structured_output);
    }

    /// The fan-out is what asking means now, so the switch ships on. An
    /// operator who wrote the old key meant to turn something on, and a rename
    /// that silently reverted them to the default would be indistinguishable
    /// from the feature never having worked.
    #[test]
    fn planning_ships_on_and_the_old_key_still_speaks() {
        let _guard = env_guard();
        let on = |toml: &str| load_infer(toml).unwrap().infer.ask.unwrap().plan;
        assert!(on(TIERED), "the fan-out has to be the default");
        assert!(!on(&format!("{TIERED}\nplan = false\n")));
        assert!(
            !on(&format!("{TIERED}\nfollow_up = false\n")),
            "the old key stopped being read"
        );
    }

    /// A named plan tier infers its ceiling name the same way every role does,
    /// and the warning that says so at startup has to cover it — that endpoint
    /// is exactly the cheap local one that ignores an unknown name.
    #[test]
    fn a_plan_tier_guessing_its_ceiling_name_is_warned_about() {
        let _guard = env_guard();
        let toml = format!(
            "{TIERED}\nplan_tier = \"quick\"\n\
             [infer.tiers.quick]\nbase_url = \"http://localhost:8001/v1\"\nmodel = \"small\"\n\
             context_tokens = 8192\nmax_output_tokens = 512\nreasoning_effort = \"none\"\n"
        );
        let cfg = load_infer(&toml).unwrap();
        let guessed = cfg.inferred_ceiling_params();
        assert!(
            guessed.contains(&("infer.ask.plan_tier", "none")),
            "the planning endpoint's guess went unreported: {guessed:?}"
        );
        // Ask itself is on a tier that names the parameter, so it is not listed.
        assert!(
            !guessed.iter().any(|(r, _)| *r == "infer.ask"),
            "{guessed:?}"
        );
    }

    /// Resolved at startup for the same reason every other tier name is: a typo
    /// must fail where the operator can see it, not on the first question
    /// somebody asks.
    #[test]
    fn a_plan_tier_that_does_not_exist_is_refused_like_any_other() {
        let _guard = env_guard();
        let err = load_infer(&format!("{TIERED}\nplan_tier = \"efficent\"\n"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("plan_tier"), "name the key: {err}");
        assert!(
            err.contains("efficent"),
            "the error must name the typo: {err}"
        );
        assert!(err.contains("efficient"), "and what was available: {err}");
    }

    #[test]
    fn query_and_document_render_differently_for_the_same_text() {
        let t = EmbedTemplates::default();
        let q = t.render_query("how do I recover deleted entries");
        let d = t.render_document(None, "how do I recover deleted entries");
        assert_ne!(q, d);
        assert_eq!(
            q,
            "task: search result | query: how do I recover deleted entries"
        );
        assert_eq!(d, "title: none | text: how do I recover deleted entries");
    }

    #[test]
    fn a_titled_and_an_untitled_document_take_different_templates() {
        let t = EmbedTemplates::default();
        assert_eq!(
            t.render_document(Some("Recovering deleted entries"), "run fsck first"),
            "title: Recovering deleted entries | text: run fsck first"
        );
        assert_eq!(
            t.render_document(None, "run fsck first"),
            "title: none | text: run fsck first"
        );
    }

    #[test]
    fn the_legacy_templates_reproduce_the_old_join() {
        // What `embed_text` produced before templates existed, and what a
        // bge-m3 operator configures. The test doubles default to it so every
        // retrieval test that queries with "title\ntext" keeps matching.
        let t = EmbedTemplates::legacy();
        assert_eq!(t.render_query("t0\nalpha"), "t0\nalpha");
        assert_eq!(t.render_document(Some("t0"), "alpha"), "t0\nalpha");
        assert_eq!(t.render_document(None, "alpha"), "alpha");
    }

    #[test]
    fn substitution_is_one_pass_so_a_value_cannot_be_substituted_again() {
        // A title that happens to contain the text placeholder must not have
        // the body spliced into it. Two chained `replace` calls would.
        let t = EmbedTemplates::default();
        assert_eq!(
            t.render_document(Some("about {text}"), "body"),
            "title: about {text} | text: body"
        );
        assert_eq!(
            substitute("a {x} b {y} c {x}", &[("x", "1"), ("y", "2")]),
            "a 1 b 2 c 1"
        );
        // An unknown placeholder is left as written rather than eaten.
        assert_eq!(substitute("{nope} {x}", &[("x", "1")]), "{nope} 1");
    }

    #[test]
    fn a_template_without_its_placeholder_is_rejected() {
        assert!(EmbedTemplates::default().validate().is_ok());
        let t = EmbedTemplates {
            query_template: "task: search result | query: ".into(),
            ..EmbedTemplates::default()
        };
        assert!(t.validate().unwrap_err().contains("query_template"));
        let t = EmbedTemplates {
            document_template: "text: {text}".into(),
            ..EmbedTemplates::default()
        };
        assert!(t.validate().unwrap_err().contains("{title}"));
        let t = EmbedTemplates {
            document_template_untitled: "title: none | text: ".into(),
            ..EmbedTemplates::default()
        };
        assert!(
            t.validate()
                .unwrap_err()
                .contains("document_template_untitled")
        );
    }

    /// The preamble every `[infer.embed]` test below shares: a valid config
    /// with the embed block left for the test to write.
    const EMBED_PREAMBLE: &str = r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.efficient]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        [infer.synthesize]
        tier = "efficient"
        output_ratio = 8.0
        [infer.ask]
        tier = "efficient"
    "#;

    #[test]
    fn embed_templates_default_to_embeddinggemma_when_unset() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{EMBED_PREAMBLE}
            [infer.embed]
            base_url = \"http://localhost:8000/v1\"
            model = \"embeddinggemma\"
            dim = 768
            max_input_tokens = 2048
            "
        ))
        .unwrap();
        assert_eq!(cfg.infer.embed.templates(), EmbedTemplates::default());
    }

    #[test]
    fn embed_templates_are_read_from_config() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{EMBED_PREAMBLE}
            [infer.embed]
            base_url = \"http://localhost:8000/v1\"
            model = \"bge-m3\"
            dim = 1024
            max_input_tokens = 1024
            query_template = \"{{text}}\"
            document_template = \"{{title}}\\n{{text}}\"
            document_template_untitled = \"{{text}}\"
            "
        ))
        .unwrap();
        assert_eq!(cfg.infer.embed.templates(), EmbedTemplates::legacy());
    }

    #[test]
    fn a_template_missing_its_placeholder_fails_at_load() {
        let _guard = env_guard();
        let err = load_infer(&format!(
            "{EMBED_PREAMBLE}
            [infer.embed]
            base_url = \"http://localhost:8000/v1\"
            model = \"embeddinggemma\"
            dim = 768
            max_input_tokens = 2048
            query_template = \"task: search result | query: \"
            "
        ))
        .unwrap_err();
        assert!(err.to_string().contains("query_template"), "got: {err}");
    }

    /// Everything but `[infer.*]` roles: the tests below write those themselves.
    const BARE_PREAMBLE: &str = r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.efficient]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "embeddinggemma"
        dim = 768
        max_input_tokens = 2048
    "#;

    /// `BARE_PREAMBLE` without its `[infer.embed]` table, for a test that
    /// writes its own.
    const BARE_PREAMBLE_NO_EMBED: &str = r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.efficient]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
    "#;

    #[test]
    fn synthesis_defaults_to_earned_and_parses_the_three_modes() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}
            [infer.synthesize]
            tier = \"efficient\"
            output_ratio = 8.0
            [infer.ask]
            tier = \"efficient\"
            "
        ))
        .unwrap();
        assert_eq!(cfg.infer.synthesis, SynthesisMode::Earned);
        assert_eq!(cfg.infer.segment_tokens, DEFAULT_SEGMENT_TOKENS);
        for (word, mode) in [
            ("off", SynthesisMode::Off),
            ("earned", SynthesisMode::Earned),
            ("eager", SynthesisMode::Eager),
        ] {
            let cfg = load_infer(&format!(
                "{BARE_PREAMBLE}
                [infer]
                synthesis = \"{word}\"
                [infer.synthesize]
                tier = \"efficient\"
                output_ratio = 8.0
                [infer.ask]
                tier = \"efficient\"
                "
            ))
            .unwrap();
            assert_eq!(cfg.infer.synthesis, mode, "{word}");
        }
    }

    #[test]
    fn off_needs_neither_synthesize_nor_ask() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}
            [infer]
            synthesis = \"off\"
            segment_tokens = 2048
            "
        ))
        .unwrap();
        assert!(cfg.infer.synthesize.is_none());
        assert!(cfg.infer.ask.is_none());
        assert_eq!(cfg.infer.segment_tokens, 2048);
    }

    #[test]
    fn earned_and_eager_refuse_to_start_without_a_synthesizer() {
        let _guard = env_guard();
        for word in ["earned", "eager"] {
            let err = load_infer(&format!(
                "{BARE_PREAMBLE}
                [infer]
                synthesis = \"{word}\"
                "
            ))
            .unwrap_err()
            .to_string();
            assert!(err.contains("infer.synthesize"), "{word}: {err}");
            assert!(err.contains(word), "{word}: {err}");
        }
    }

    #[test]
    fn vision_without_an_endpoint_of_its_own_needs_the_synthesizer() {
        let _guard = env_guard();
        let err = load_infer(&format!(
            "{BARE_PREAMBLE}
            [infer]
            synthesis = \"off\"
            [infer.vision]
            model = \"llava\"
            "
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("infer.vision"), "{err}");
        // With its own address it stands alone.
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}
            [infer]
            synthesis = \"off\"
            [infer.vision]
            model = \"llava\"
            base_url = \"http://localhost:9000/v1\"
            "
        ))
        .unwrap();
        let v = cfg.infer.vision.as_ref().unwrap();
        assert_eq!(v.resolve(None).0, "http://localhost:9000/v1");
    }

    #[test]
    fn chunk_tokens_defaults_to_384_and_is_clamped_to_the_embedder() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}
            [infer]
            synthesis = \"off\"
            "
        ))
        .unwrap();
        assert_eq!(cfg.infer.embed.chunk_tokens, DEFAULT_CHUNK_TOKENS);
        assert_eq!(cfg.infer.embed.effective_chunk_tokens(), 384);
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE_NO_EMBED}
            [infer]
            synthesis = \"off\"
            [infer.embed]
            base_url = \"http://localhost:8000/v1\"
            model = \"small\"
            dim = 384
            max_input_tokens = 256
            chunk_tokens = 1000
            "
        ))
        .unwrap();
        // 256 * 0.8 = 204: what the embedder will take wins over what was asked.
        assert_eq!(cfg.infer.embed.effective_chunk_tokens(), 204);
    }

    #[test]
    fn promote_defaults_and_feedback_ships_on() {
        let _guard = env_guard();
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}
            [infer.synthesize]
            tier = \"efficient\"
            output_ratio = 8.0
            [infer.ask]
            tier = \"efficient\"
            "
        ))
        .unwrap();
        assert_eq!(cfg.promote.activation_above, 3.0);
        assert_eq!(cfg.promote.resynthesize_after_unconfirmed, 0);
        // Opt-out now: promotion reads activation, and activation only moves
        // while searches are recorded.
        assert!(cfg.learn.enabled);
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}
            [infer.synthesize]
            tier = \"efficient\"
            output_ratio = 8.0
            [infer.ask]
            tier = \"efficient\"
            [promote]
            activation_above = 2.5
            resynthesize_after_unconfirmed = 12
            [learn]
            enabled = false
            "
        ))
        .unwrap();
        assert_eq!(cfg.promote.activation_above, 2.5);
        assert_eq!(cfg.promote.resynthesize_after_unconfirmed, 12);
        assert!(!cfg.learn.enabled);
    }

    #[test]
    fn the_learning_layer_is_one_switch_that_nothing_can_contradict() {
        let _guard = env_guard();
        let roles = "[infer.synthesize]\ntier = \"efficient\"\noutput_ratio = 8.0\n[infer.ask]\ntier = \"efficient\"\n";
        let cfg = load_infer(&format!("{BARE_PREAMBLE}\n{roles}")).unwrap();
        // On, and the sections it governs carry no switch of their own. There
        // used to be three — `feedback`, `associate`, `pursuit` — and two of
        // their eight combinations were refused at startup while a third was a
        // warning, which is how you find out they were one setting written
        // three times. Nothing to refuse now: a config cannot express the
        // combination that had to be rejected.
        assert!(cfg.learn.enabled);
        assert_eq!(cfg.pursuit.idle_secs, 900);
        assert_eq!(cfg.pursuit.min_sources, 2);
        assert_eq!(cfg.pursuit.min_engagement, 3.0);
        assert_eq!(cfg.associate.interval_mins, 30);
        assert_eq!(cfg.feedback.candidates, 20);

        // One key turns the whole layer off, including the recording that the
        // rest of it reads.
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}\n{roles}[learn]\nenabled = false\n"
        ))
        .unwrap();
        assert!(!cfg.learn.enabled);
        // And the tuning under each faculty still loads with it off: a section
        // is thresholds now, not a gate.
        let cfg = load_infer(&format!(
            "{BARE_PREAMBLE}\n{roles}[learn]\nenabled = false\n[pursuit]\nidle_secs = 60\n"
        ))
        .unwrap();
        assert_eq!(cfg.pursuit.idle_secs, 60);
        assert!(!cfg.learn.enabled);
    }

    #[test]
    fn the_rerank_probe_shares_the_prefix_of_its_style_request_path() {
        // The probe is written against the same `base_url` the request is, so
        // a style whose request path carries no `v1` must not have the probe
        // add one: Cohere's `base_url` already ends in `/v1`, and `v1/models`
        // asked for `/v1/v1/models` and warned "rerank unreachable" at every
        // startup.
        assert_eq!(RerankStyle::Cohere.probe_path(), "models");
        assert_eq!(RerankStyle::Vllm.probe_path(), "v1/models");
        assert_eq!(RerankStyle::Tei.probe_path(), "info");
    }
}
