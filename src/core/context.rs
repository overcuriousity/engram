//! The situation a page view happened in: the clock it happened on, the bundle
//! the browser sent, and the fixed-length vector both become.
//!
//! Everything here is a function of its arguments. That is deliberate: the
//! encoder is the one part of this feature that can be silently wrong — a block
//! that quietly outweighs another produces plausible recommendations for the
//! wrong reason — and a pure function is the only shape that can be pinned by a
//! table of cases.

/// Where this feature reads the time. `System` everywhere but the tests, which
/// need a seventh Friday at 14:52 to exist on demand.
///
/// Held by the sweep, the encoder's entry point and the endpoint, and by
/// nothing else. The other `now()` call sites in the tree are not touched:
/// they work, and rewriting them would be a diff across the tree for nothing.
#[derive(Debug, Clone, Copy)]
pub enum Clock {
    System,
    Fixed(i64),
}

impl Clock {
    pub fn now(&self) -> i64 {
        match self {
            Clock::System => crate::store::now(),
            Clock::Fixed(t) => *t,
        }
    }
}

/// A moment as the device experienced it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalTime {
    /// Fractional, 0.0..24.0 — 15:30 is 15.5. Fractional because the hour is
    /// encoded as an angle, and rounding to the hour would put 14:55 and 15:05
    /// ten minutes apart on a circle they are five minutes apart on.
    pub hour: f32,
    /// 0 = Monday.
    pub weekday: u32,
    /// 1-based day of the month.
    pub day: u32,
    /// 1-based. Only the display reads it — "like 08.08., 15:04" — but it is
    /// derived here because this is the one place that knows which zone the
    /// moment is being read in.
    pub month: u32,
    pub days_in_month: u32,
}

/// The device's own reading of `at`.
///
/// The zone comes from the client, never from config:
/// `Intl.DateTimeFormat().resolvedOptions().timeZone` is correct per device and
/// carries DST with it, which a stored offset cannot. The offset is the fallback
/// for a browser that reports one and no zone; UTC is the fallback for a row
/// that has neither, which is every row written before this feature existed.
/// That last case is not a pretence that the operator lives in London — it is
/// the only reading available, and §12 leans on it: an old event still carries
/// a weekday and an hour, and those two blocks are what stand from the first
/// sweep.
pub fn local_time(at: i64, tz: Option<&str>, offset_mins: Option<i32>) -> LocalTime {
    use chrono::{DateTime, Datelike, TimeZone, Timelike};

    let utc = DateTime::from_timestamp(at, 0).unwrap_or_default();
    let naive = match tz.and_then(|z| z.parse::<chrono_tz::Tz>().ok()) {
        Some(z) => z.from_utc_datetime(&utc.naive_utc()).naive_local(),
        // `checked_mul` because the offset arrives from the browser and a
        // browser may say anything: `tz_offset_mins` is a bare `i32` off the
        // wire, and any value past ±35 791 394 overflows the seconds it is
        // converted to — a panic in a debug build, inside a handler whose whole
        // rule is that nothing a browser sends may take a page view down. An
        // overflow reads as "no offset given", which is the same fallback as a
        // browser that reported nothing: UTC.
        None => match offset_mins
            .and_then(|m| m.checked_mul(60))
            .and_then(chrono::FixedOffset::east_opt)
        {
            Some(o) => o.from_utc_datetime(&utc.naive_utc()).naive_local(),
            None => utc.naive_utc(),
        },
    };
    LocalTime {
        hour: naive.hour() as f32 + naive.minute() as f32 / 60.0,
        weekday: naive.weekday().num_days_from_monday(),
        day: naive.day(),
        month: naive.month(),
        days_in_month: days_in_month(naive.year(), naive.month()),
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    use chrono::NaiveDate;
    let (y, m) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first = NaiveDate::from_ymd_opt(year, month, 1);
    let next = NaiveDate::from_ymd_opt(y, m, 1);
    match (first, next) {
        (Some(a), Some(b)) => (b - a).num_days() as u32,
        // Unreachable for a date chrono just produced; 30 rather than a panic,
        // because a month length is a scaling factor for one block worth 0.0 by
        // default and nothing here is worth taking a page view down for.
        _ => 30,
    }
}

/// One named span of the context vector.
///
/// Named because the explanation falls out of the naming: each block is scored
/// separately at read time, so "weekday, hour, device" is three lookups in this
/// table rather than three sentences somebody had to write. A new block brings
/// its label and is done.
#[derive(Debug, Clone, Copy)]
pub struct Block {
    /// The config key its weight is read under. See `BlockWeights::of`.
    pub name: &'static str,
    /// What the line under the offer prints. No sentences and no values in
    /// prose — generated prose per block was the first draft and was cut,
    /// because it coupled every new dimension to a sentence template.
    pub label: &'static str,
    pub at: usize,
    pub dims: usize,
}

/// The layout, in order. Every block describes the situation and nothing
/// describes who is in it: each user has their own database and their own
/// collection, and `Core::offer` cuts foreign clusters by an exact match on top
/// of that.
///
/// Circular where there is a circle, one-hot where there is not. The hour is a
/// circle, so 23:30 and 00:30 are an hour apart. The weekday is *not* a useful
/// circle — "Friday is three from Tuesday" means nothing, and the pattern is
/// exactly Friday — so it is one-hot, with a separate weak weekday/weekend
/// block for the part that genuinely is gradual.
pub const BLOCKS: [Block; 10] = [
    Block {
        name: "time_of_day",
        label: "hour",
        at: 0,
        dims: 2,
    },
    Block {
        name: "weekday",
        label: "weekday",
        at: 2,
        dims: 7,
    },
    Block {
        name: "weekend",
        label: "weekend",
        at: 9,
        dims: 2,
    },
    Block {
        name: "device",
        label: "device",
        at: 11,
        dims: 8,
    },
    Block {
        name: "viewport",
        label: "screen",
        at: 19,
        dims: 4,
    },
    Block {
        name: "locale",
        label: "language",
        at: 23,
        dims: 8,
    },
    Block {
        name: "network",
        label: "network",
        at: 31,
        dims: 4,
    },
    Block {
        name: "power",
        label: "battery",
        at: 35,
        dims: 3,
    },
    Block {
        name: "environment",
        label: "surroundings",
        at: 38,
        dims: 5,
    },
    Block {
        name: "month_cycle",
        label: "month",
        at: 43,
        dims: 2,
    },
];

/// Fixed at collection creation, so a new block invalidates every stored
/// centroid. That is what `encoder_version` and whole-bundle storage are for:
/// the sweep rebuilds from the raw bundles rather than losing the history.
///
/// A *changed* width is the heavier case, because a named vector cannot be
/// resized in place: an existing collection keeps the size it was created with
/// and rejects the new one. `--reindex` is the migration — it creates the next
/// generation with this size and copies the dense vectors across, so nothing is
/// re-embedded, and sets of the old width are discarded rather than reinterpreted
/// (see `ctx_of` in `vector/qdrant.rs`).
pub const CTX_DIM: usize = 45;

/// Bumped whenever `BLOCKS` changes in any way — a width, an order, or what a
/// slot means. Not what is stored: see `encoder_version`.
///
/// 2: the `scope` block dropped. It existed to keep one person's situations from
/// being ranked first for another while everyone shared a collection, and each
/// user has had their own since. Inside one collection it was the same direction
/// of magnitude 10 in every stored vector and in the query — ordering nothing,
/// and under cosine compressing the differences the ten blocks that describe the
/// situation are able to make.
pub const LAYOUT_VERSION: i64 = 2;

/// What a stored cluster carries, and what the read path insists on matching.
///
/// The layout is half of it. The other half is `[recommend.weights]`, because
/// every block is scaled by its weight before anything is compared — a weight
/// is not a knob on top of the encoding, it *is* the geometry. An operator who
/// edits one and restarts leaves the store full of centroids built under the
/// old numbers, while `offer` encodes the present situation under the new ones
/// and compares the two; for the six hours until the next sweep, `context_score`
/// and the argmax are computed across two different encodings. That is exactly
/// the recommendation nobody can account for that `BlockWeights::of` refuses a
/// typo to avoid.
///
/// So the weights are hashed into the version. A cluster written under other
/// numbers is skipped by the read path — the area falls to its floor rather
/// than explaining a hit with an encoding that no longer exists — and the next
/// sweep, which rebuilds every profile from the raw bundles, replaces it.
pub fn encoder_version(w: &crate::config::BlockWeights) -> i64 {
    let mut key = format!("layout{LAYOUT_VERSION}");
    for b in BLOCKS {
        // The bits, not the printed value: two weights that differ below what
        // `{}` prints are two different geometries.
        key.push_str(&format!("|{}={:08x}", b.name, w.of(b.name).to_bits()));
    }
    // Shifted down one bit so the version is always positive: it goes into a
    // signed column, and a negative version is a value no operator reading the
    // table would recognise as one.
    (fnv1a(&key) >> 1) as i64
}

/// What the browser said about the situation, as received.
///
/// Every field optional, because every field is optional in a browser: the
/// Battery API does not exist on the desktop, `connection` is Chromium-only,
/// and a hardened browser withholds several. Absent is not zero — see `encode`.
///
/// Deliberately **not** collected: canvas, WebGL, font enumeration, plugin
/// lists. Not out of squeamishness — they are the wrong tool. Those are what
/// identify a device across a population, and here the population is one
/// authenticated person, so they are constant and say nothing about *which
/// situation* this is. They are also randomised per session and origin by a
/// hardened browser, so a device identity built on them would rotate and every
/// day would look like a new device.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Bundle {
    /// IANA, from `Intl.DateTimeFormat().resolvedOptions().timeZone`.
    pub tz: Option<String>,
    pub tz_offset_mins: Option<i32>,
    pub language: Option<String>,
    /// The full preference list. Stored, not encoded — see the module note on
    /// fields the encoder ignores.
    pub languages: Vec<String>,
    pub viewport_w: Option<f32>,
    pub viewport_h: Option<f32>,
    pub screen_w: Option<f32>,
    pub screen_h: Option<f32>,
    pub dpr: Option<f32>,
    /// `dark` | `light`.
    pub color_scheme: Option<String>,
    pub platform: Option<String>,
    /// The UA client hint's brand, or the family parsed from the UA string.
    pub ua_family: Option<String>,
    pub cores: Option<f32>,
    pub memory_gb: Option<f32>,
    pub touch: Option<bool>,
    /// `portrait` | `landscape`.
    pub orientation: Option<String>,
    /// 0.0..1.0.
    pub battery_level: Option<f32>,
    pub charging: Option<bool>,
    /// `wifi` | `cellular` | `wired` | anything else.
    pub network: Option<String>,
    pub audio_outputs: Option<u32>,
}

/// A bundle from whatever the browser posted.
///
/// Lenient on purpose: nothing a browser sends may take a page view down, and
/// an empty bundle is a working one — the weekday and the hour come from the
/// server's own clock and still stand. Unknown fields are dropped here but the
/// raw string is what `context_events.bundle` stores, so nothing is lost.
pub fn parse_bundle(raw: &str) -> Bundle {
    serde_json::from_str(raw).unwrap_or_default()
}

/// What machine this is, over the fields that do not move.
///
/// Platform, browser family, screen, cores, memory, language — and nothing the
/// situation changes. A phone that rotates, unplugs or joins a different
/// network is the same phone; a key that moved with any of those would make
/// every session look like a new device and no pattern could ever form.
///
/// `None` when the browser said nothing identifying at all, which `encode`
/// gives its own slot rather than treating as an absence.
pub fn device_key(b: &Bundle) -> Option<String> {
    let parts = [
        b.platform.clone(),
        b.ua_family.clone(),
        b.screen_w.map(|v| v.to_string()),
        b.screen_h.map(|v| v.to_string()),
        b.cores.map(|v| v.to_string()),
        b.memory_gb.map(|v| v.to_string()),
        b.language.clone(),
    ];
    if parts.iter().all(Option::is_none) {
        return None;
    }
    let joined = parts
        .iter()
        .map(|p| p.as_deref().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("|");
    Some(format!("{:016x}", fnv1a(&joined)))
}

/// FNV-1a, written out rather than reached for.
///
/// `DefaultHasher` is seeded per process, so a bucket chosen with it would move
/// on every restart and every stored centroid would be indexed under a slot
/// that no longer means what it meant. This is stable across runs, machines and
/// releases, which is the only property being asked of it.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.as_bytes() {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn bucket(s: &str, n: usize) -> usize {
    (fnv1a(s) % n as u64) as usize
}

/// One situation as a vector.
///
/// Each block is filled with raw values, **normalised to length 1, then scaled
/// by its weight**. That order is the whole design: a block contributes exactly
/// its weight however many dimensions it uses, which is what puts the weighting
/// back into config as named numbers instead of leaving it hidden in how the
/// encoding happened to be written.
///
/// A block whose values are all zero is left at zero rather than normalised —
/// dividing by nothing is how absence turns into a manufactured direction.
pub fn encode(at: i64, b: &Bundle, w: &crate::config::BlockWeights) -> Vec<f32> {
    let t = local_time(at, b.tz.as_deref(), b.tz_offset_mins);
    let mut v = vec![0.0f32; CTX_DIM];

    for block in BLOCKS {
        let slot = &mut v[block.at..block.at + block.dims];
        fill(block.name, slot, &t, b);
        scale(slot, w.of(block.name));
    }
    v
}

/// Raw values into one block's slots. Every arm either fills or leaves zeros;
/// leaving zeros is how "the browser did not say" is expressed.
fn fill(name: &str, s: &mut [f32], t: &LocalTime, b: &Bundle) {
    use std::f32::consts::TAU;
    match name {
        "time_of_day" => {
            let a = TAU * t.hour / 24.0;
            s[0] = a.sin();
            s[1] = a.cos();
        }
        "weekday" => s[(t.weekday as usize).min(6)] = 1.0,
        "weekend" => {
            let idx = usize::from(t.weekday >= 5);
            s[idx] = 1.0;
        }
        "device" => match device_key(b) {
            // The last slot is "nothing identifying was sent" — a state, not an
            // absence. A hardened browser is a situation that recurs, unlike a
            // battery that does not exist.
            Some(k) => s[bucket(&k, s.len() - 1)] = 1.0,
            None => {
                let last = s.len() - 1;
                s[last] = 1.0;
            }
        },
        "viewport" => {
            // Logs *centred* on a thousand pixels, not raw. Raw logs put every
            // screen ever built between 6.5 and 8, so any two of them scored
            // 0.999 against each other and the block said nothing. Centred, a
            // phone in portrait and a desktop in landscape point in genuinely
            // different directions.
            if let (Some(vw), Some(vh)) = (b.viewport_w, b.viewport_h)
                && vw > 0.0
                && vh > 0.0
            {
                s[0] = (vw / 1000.0).ln();
                s[1] = (vh / 1000.0).ln();
                s[2] = (vw / vh).ln();
                s[3] = b.dpr.filter(|d| *d > 0.0).map(f32::ln).unwrap_or(0.0);
            }
        }
        "locale" => {
            // Two halves in one block, four slots each: what language this
            // browser is in, and what zone it is in. They move together — a
            // trip changes both — and neither is worth a weight of its own.
            let half = s.len() / 2;
            if let Some(l) = &b.language {
                s[bucket(l, half)] = 1.0;
            }
            if let Some(z) = &b.tz {
                s[half + bucket(z, half)] = 1.0;
            }
        }
        "network" => {
            // Four named states including unknown, so a browser that does not
            // expose `connection` is grouped with the others that do not rather
            // than with none of them.
            let idx = match b.network.as_deref() {
                Some("wifi") => 0,
                Some("cellular") => 1,
                Some("wired" | "ethernet") => 2,
                _ => 3,
            };
            s[idx] = 1.0;
        }
        "power" => {
            // All three zero when there is no Battery API at all. A desktop
            // must not read as agreeing with a phone that happens to sit at
            // whatever default would otherwise have been invented here.
            if let Some(level) = b.battery_level {
                match b.charging {
                    Some(true) => s[0] = 1.0,
                    Some(false) => s[1] = 1.0,
                    None => {}
                }
                s[2] = level.clamp(0.0, 1.0);
            }
        }
        "environment" => {
            match b.color_scheme.as_deref() {
                Some("dark") => s[0] = 1.0,
                Some("light") => s[1] = 1.0,
                _ => {}
            }
            if b.touch == Some(true) {
                s[2] = 1.0;
            }
            // One signed slot rather than two, because the three states are
            // portrait, landscape and "did not say" — and zero is already the
            // third.
            s[3] = match b.orientation.as_deref() {
                Some("portrait") => 1.0,
                Some("landscape") => -1.0,
                _ => 0.0,
            };
            if let Some(n) = b.audio_outputs {
                s[4] = (n.min(4) as f32) / 4.0;
            }
        }
        "month_cycle" => {
            let a = TAU * (t.day.saturating_sub(1)) as f32 / t.days_in_month.max(1) as f32;
            s[0] = a.sin();
            s[1] = a.cos();
        }
        // Unreachable while `BLOCKS` and this match are edited together, and a
        // block silently left at zero is the safe way to be wrong: it
        // contributes nothing rather than contributing noise.
        _ => {}
    }
}

/// Normalise to length 1, then scale. A block that is all zeros stays all
/// zeros — there is no direction to normalise, and inventing one is exactly
/// what "absent is not zero-valued" forbids.
fn scale(s: &mut [f32], weight: f32) {
    if weight == 0.0 {
        s.fill(0.0);
        return;
    }
    let n = s.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n == 0.0 {
        return;
    }
    let k = weight / n;
    for x in s.iter_mut() {
        *x *= k;
    }
}

/// How well the situation matches.
///
/// One number, and the same one the store ranked on. It was two while the
/// `scope` block dominated the full cosine at weight 10 against a total of under
/// 5: counting it would have dragged every same-subject pair above 0.95 and left
/// `strong_at` and `weak_at` four hundredths apart, so the choice was made on
/// the full vector and the rung on the rest of it. With that block gone the two
/// scales are one, and `strong_at`/`weak_at` keep the values they were
/// calibrated at — they never saw the block that went.
pub fn context_score(now: &[f32], cluster: &[f32]) -> f32 {
    crate::vector::cosine(now, cluster)
}

/// What each named block contributed, largest first.
///
/// `w_b * cos(block_now, block_cluster)`. Because blocks are named and
/// separately normalised, the per-dimension breakdown that a weighted sum of
/// named terms would have produced by construction falls out of the vector as a
/// by-product — which is the answer to the one thing that approach did better.
///
/// These do **not** sum to `context_score`, and are not meant to: the score is
/// one cosine over the whole vector and each of these is a cosine over a slice,
/// with a different denominator. They rank which blocks decided it, which is
/// what the line under the offer needs and all it claims.
pub fn contributions(
    now: &[f32],
    cluster: &[f32],
    w: &crate::config::BlockWeights,
) -> Vec<(&'static str, f32)> {
    let mut out: Vec<(&'static str, f32)> = BLOCKS
        .iter()
        .filter(|b| now.len() >= b.at + b.dims && cluster.len() >= b.at + b.dims)
        .map(|b| {
            let a = &now[b.at..b.at + b.dims];
            let c = &cluster[b.at..b.at + b.dims];
            (b.label, w.of(b.name) * crate::vector::cosine(a, c))
        })
        .collect();
    // Ties break on the label, so which of two equally strong blocks is named
    // first does not depend on the order `BLOCKS` happens to be written in.
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weights() -> crate::config::BlockWeights {
        crate::config::BlockWeights::default()
    }

    /// A bundle a phone in Berlin would send.
    fn phone() -> Bundle {
        Bundle {
            tz: Some("Europe/Berlin".into()),
            tz_offset_mins: Some(120),
            language: Some("de-DE".into()),
            viewport_w: Some(390.0),
            viewport_h: Some(844.0),
            screen_w: Some(390.0),
            screen_h: Some(844.0),
            dpr: Some(3.0),
            color_scheme: Some("dark".into()),
            platform: Some("Android".into()),
            ua_family: Some("Chrome".into()),
            cores: Some(8.0),
            memory_gb: Some(4.0),
            touch: Some(true),
            orientation: Some("portrait".into()),
            battery_level: Some(0.4),
            charging: Some(false),
            network: Some("cellular".into()),
            audio_outputs: Some(1),
            ..Default::default()
        }
    }

    /// A bundle a desktop in London would send. Nothing about it agrees with
    /// `phone()`, which is what makes it useful.
    fn desktop() -> Bundle {
        Bundle {
            platform: Some("macOS".into()),
            ua_family: Some("Firefox".into()),
            touch: Some(false),
            orientation: Some("landscape".into()),
            network: Some("wired".into()),
            color_scheme: Some("light".into()),
            language: Some("en-GB".into()),
            viewport_w: Some(1920.0),
            viewport_h: Some(1080.0),
            screen_w: Some(2560.0),
            screen_h: Some(1440.0),
            dpr: Some(2.0),
            // No Battery API on a desktop, so that block says nothing at all
            // rather than agreeing with a phone at some invented level.
            battery_level: None,
            charging: None,
            ..phone()
        }
    }

    fn slice_of<'a>(v: &'a [f32], name: &str) -> &'a [f32] {
        let b = BLOCKS.iter().find(|b| b.name == name).unwrap();
        &v[b.at..b.at + b.dims]
    }

    fn norm(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    // 2026-08-21T13:52:00Z, a Friday. 15:52 in Berlin.
    const FRIDAY: i64 = FRIDAY_1352_UTC;

    #[test]
    fn the_browser_sends_exactly_the_fields_this_struct_reads() {
        // The one seam in this feature with a compiler on neither side: the
        // bundle is assembled in `assets/app.js` and parsed here, and a field
        // renamed on one side is silently `None` on the other — a block that
        // quietly stops contributing, with nothing failing anywhere.
        //
        // Both files are in the repository, so the check is just reading them.
        let js = include_str!("../../assets/app.js");
        let sent: std::collections::BTreeSet<&str> = js
            .lines()
            .filter_map(|l| l.trim().strip_prefix("b."))
            .filter_map(|l| l.split_once(" = "))
            .map(|(name, _)| name)
            .collect();
        assert!(!sent.is_empty(), "found no bundle assignments in app.js");

        // The struct's own field list, read off the source rather than kept by
        // hand — a hand-kept list is the thing everyone forgets to append to.
        let src = include_str!("context.rs");
        let body = src
            .split_once("pub struct Bundle {")
            .expect("Bundle struct")
            .1
            .split_once("\n}")
            .expect("end of Bundle")
            .0;
        let expected: std::collections::BTreeSet<&str> = body
            .lines()
            .filter_map(|l| l.trim().strip_prefix("pub "))
            .filter_map(|l| l.split_once(':'))
            .map(|(name, _)| name)
            .collect();

        assert_eq!(
            sent, expected,
            "app.js and Bundle disagree about what a situation is"
        );
    }

    #[test]
    fn the_layout_adds_up() {
        // The one invariant everything else rests on: a block that overlaps its
        // neighbour would silently mix two meanings into one dimension, and
        // every recommendation after it would be explained by the wrong block.
        let mut at = 0;
        for b in BLOCKS {
            assert_eq!(b.at, at, "{} starts where the last block ended", b.name);
            at += b.dims;
        }
        assert_eq!(at, CTX_DIM);
    }

    #[test]
    fn half_past_eleven_at_night_is_near_half_past_midnight() {
        // The hour is a circle, so the two are an hour apart rather than
        // twenty-three. A one-hot hour would have called them maximally
        // different, which is the whole reason this block is an angle.
        let late = encode(FRIDAY - 2 * 3600 - 22 * 60, &phone(), &weights());
        let early = encode(FRIDAY - 3600 - 22 * 60, &phone(), &weights());
        let c = crate::vector::cosine(
            slice_of(&late, "time_of_day"),
            slice_of(&early, "time_of_day"),
        );
        assert!(c > 0.96, "23:30 against 00:30 scored {c}");
    }

    #[test]
    fn five_past_three_costs_almost_nothing_against_a_three_o_clock_pattern() {
        let at_three = encode(FRIDAY - 52 * 60, &phone(), &weights());
        let five_past = encode(FRIDAY - 47 * 60, &phone(), &weights());
        let c = crate::vector::cosine(
            slice_of(&at_three, "time_of_day"),
            slice_of(&five_past, "time_of_day"),
        );
        assert!(c > 0.999, "15:00 against 15:05 scored {c}");
    }

    #[test]
    fn a_seven_slot_block_contributes_exactly_its_weight() {
        // The rule the whole design rests on. Seven one-hot slots for the
        // weekday do not outweigh two for the hour because there are seven of
        // them: each block is normalised and *then* scaled.
        let v = encode(FRIDAY, &phone(), &weights());
        let w = weights();
        assert!((norm(slice_of(&v, "weekday")) - w.weekday).abs() < 1e-5);
        assert!((norm(slice_of(&v, "time_of_day")) - w.time_of_day).abs() < 1e-5);
        assert!((norm(slice_of(&v, "device")) - w.device).abs() < 1e-5);
    }

    #[test]
    fn a_block_switched_off_contributes_nothing() {
        let mut w = weights();
        w.month_cycle = 0.0;
        let v = encode(FRIDAY, &phone(), &w);
        assert_eq!(norm(slice_of(&v, "month_cycle")), 0.0);
    }

    #[test]
    fn a_missing_value_zeroes_its_block_rather_than_inventing_a_default() {
        // The Battery API does not exist on the desktop. An invented default
        // would manufacture similarity between every desktop and every phone
        // that happened to sit at that level.
        let mut b = phone();
        b.battery_level = None;
        b.charging = None;
        let v = encode(FRIDAY, &b, &weights());
        assert_eq!(norm(slice_of(&v, "power")), 0.0, "power says nothing");
        assert!(
            norm(slice_of(&v, "weekday")) > 0.0,
            "and nothing else changed"
        );
    }

    #[test]
    fn a_block_that_says_nothing_scores_zero_rather_than_opposed() {
        // Two desktops with no battery must not read as *agreeing* about the
        // battery, and must not read as disagreeing either. `cosine` returns
        // 0.0 for a zero vector, which is the answer that means "no opinion".
        let mut b = phone();
        b.battery_level = None;
        b.charging = None;
        let v = encode(FRIDAY, &b, &weights());
        let c = contributions(&v, &v, &weights());
        let power = c.iter().find(|(n, _)| *n == "battery").unwrap();
        assert_eq!(power.1, 0.0);
    }

    #[test]
    fn an_unidentifiable_device_is_a_state_rather_than_an_absence() {
        // Unlike the battery, "this browser tells us nothing about itself" is
        // itself stable and recurring, so it gets a slot of its own — a
        // hardened browser is a situation, not a gap.
        let bare = Bundle::default();
        assert!(device_key(&bare).is_none());
        let v = encode(FRIDAY, &bare, &weights());
        assert!((norm(slice_of(&v, "device")) - weights().device).abs() < 1e-5);
    }

    #[test]
    fn a_device_key_is_stable_and_ignores_the_situation() {
        // It hashes what the machine *is*, never what it is doing: a phone that
        // rotates or unplugs is the same phone. A key that moved would make
        // every session look like a new device and no pattern could ever form.
        let mut later = phone();
        later.orientation = Some("landscape".into());
        later.battery_level = Some(0.9);
        later.viewport_w = Some(844.0);
        assert_eq!(device_key(&phone()), device_key(&later));

        let mut other = phone();
        other.platform = Some("macOS".into());
        assert_ne!(device_key(&phone()), device_key(&other));
    }

    #[test]
    fn nothing_in_the_vector_says_who_is_asking() {
        // Each user has their own database and their own Qdrant collection, and
        // the read path cuts foreign clusters by an exact match on top of that.
        // Inside one collection every cluster carries the same subject, so a
        // block for it is the same direction in every stored vector and in the
        // query: it orders nothing and, under cosine, compresses the
        // differences the blocks that *do* describe the situation can make.
        assert!(
            !BLOCKS.iter().any(|b| b.name == "scope"),
            "the situation is what the browser reports, not who is holding it"
        );
        assert_eq!(CTX_DIM, 45, "eight dimensions of constant went with it");
    }

    #[test]
    fn the_situation_is_the_whole_vector() {
        // Two numbers were needed while one block dominated the full cosine and
        // was sliced off the gate. With that block gone there is one number:
        // what ranks in the store is what `strong_at` and `weak_at` read.
        let friday_phone = encode(FRIDAY, &phone(), &weights());
        let monday_desk = encode(FRIDAY + 3 * 86_400 - 8 * 3600, &desktop(), &weights());
        let full = crate::vector::cosine(&friday_phone, &monday_desk);
        let scored = context_score(&friday_phone, &monday_desk);
        assert!(
            (full - scored).abs() < 1e-6,
            "the rank and the rung read the same evidence: {full} against {scored}"
        );
        assert!(
            scored < 0.4,
            "a Monday at a desk does not resemble a Friday on a phone, got {scored}"
        );
    }

    #[test]
    fn the_reason_names_the_blocks_that_decided_it() {
        let a = encode(FRIDAY, &phone(), &weights());
        let c = contributions(&a, &a, &weights());
        assert_eq!(c.len(), BLOCKS.len());
        for pair in c.windows(2) {
            assert!(
                pair[0].1 >= pair[1].1,
                "sorted, so the top three are the top three"
            );
        }
        // Every block carries a `&'static str` and nothing generates prose.
        assert!(BLOCKS.iter().all(|b| !b.label.is_empty()));
    }

    #[test]
    fn a_bundle_that_is_not_json_is_an_empty_one_rather_than_an_error() {
        // The bundle comes from a browser, and nothing a browser sends may take
        // a page view down. An empty bundle zeroes the blocks it would have
        // filled; the weekday and the hour still stand.
        let b = parse_bundle("}{ not json");
        assert!(b.tz.is_none());
        let v = encode(FRIDAY, &b, &weights());
        assert_eq!(v.len(), CTX_DIM);
        assert!(norm(slice_of(&v, "weekday")) > 0.0);
    }

    #[test]
    fn a_fixed_clock_does_not_move() {
        assert_eq!(Clock::Fixed(1_000).now(), 1_000);
        assert_eq!(Clock::Fixed(1_000).now(), 1_000);
    }

    // 2026-08-21T13:52:00Z is a Friday.
    const FRIDAY_1352_UTC: i64 = 1_787_320_320;

    #[test]
    fn an_iana_zone_beats_a_stored_offset() {
        // Berlin is UTC+2 in August. The offset argument is deliberately
        // wrong: a zone, when there is one, is the authority.
        let t = local_time(FRIDAY_1352_UTC, Some("Europe/Berlin"), Some(0));
        assert_eq!(t.weekday, 4, "Friday");
        assert!((t.hour - 15.866_666).abs() < 0.001, "15:52, got {}", t.hour);
    }

    #[test]
    fn an_offset_answers_when_there_is_no_zone() {
        let t = local_time(FRIDAY_1352_UTC, None, Some(120));
        assert!((t.hour - 15.866_666).abs() < 0.001, "got {}", t.hour);
    }

    #[test]
    fn an_unknown_zone_falls_back_rather_than_failing() {
        // A device can send anything. Nothing here may panic on it, and UTC is
        // the honest answer rather than a guess.
        let t = local_time(FRIDAY_1352_UTC, Some("Mars/Olympus"), None);
        assert!((t.hour - 13.866_666).abs() < 0.001, "got {}", t.hour);
    }

    #[test]
    fn the_month_is_carried_with_its_length() {
        let t = local_time(FRIDAY_1352_UTC, Some("UTC"), None);
        assert_eq!(t.day, 21);
        assert_eq!(t.month, 8);
        assert_eq!(t.days_in_month, 31);
    }

    #[test]
    fn an_offset_no_zone_could_hold_falls_back_rather_than_overflowing() {
        // `tz_offset_mins` is a bare `i32` off the wire and a browser may say
        // anything. Past ±35 791 394 the conversion to seconds overflows —
        // a panic in a debug build, inside a handler whose whole rule is that
        // nothing a browser sends may take a page view down.
        for m in [i32::MAX, i32::MIN, 35_791_395, -35_791_395] {
            let t = local_time(FRIDAY_1352_UTC, None, Some(m));
            assert!(
                (t.hour - 13.866_666).abs() < 0.001,
                "offset {m} did not fall back to UTC: got {}",
                t.hour
            );
        }
    }

    #[test]
    fn the_stored_version_moves_when_a_weight_does() {
        // A weight is not a knob on top of the encoding — every block is scaled
        // by it before anything is compared, so it *is* the geometry. An
        // operator who edits one leaves the store full of centroids built under
        // the old numbers while `offer` encodes the present situation under the
        // new ones, and for the six hours until the next sweep the two are
        // compared across different encodings.
        let a = weights();
        let mut b = weights();
        b.network += 0.01;
        assert_ne!(
            encoder_version(&a),
            encoder_version(&b),
            "an edited weight left the version standing"
        );
        // Even a change below what `{}` would print: two weights that differ at
        // all are two geometries.
        let mut c = weights();
        c.power += f32::EPSILON;
        assert_ne!(encoder_version(&a), encoder_version(&c));
        // Stable across calls, and positive — it goes into a signed column and
        // a negative version is a value no operator would recognise as one.
        assert_eq!(encoder_version(&a), encoder_version(&weights()));
        assert!(encoder_version(&a) > 0);
    }

    #[test]
    fn a_block_that_agreed_on_nothing_is_not_one_of_the_reasons() {
        // `cosine` returns zero for a zero vector — which is what an absent
        // block is — and zero sorts above every negative contribution. Naming
        // the top three unconditionally printed "battery" on a pair where
        // neither side ever sent a battery reading.
        let w = weights();
        let bare = Bundle {
            tz: Some("Europe/Berlin".into()),
            ..Default::default()
        };
        let v = encode(FRIDAY_1352_UTC, &bare, &w);
        let named = contributions(&v, &v, &w);
        // Every block is still in the list — the `<details>` pane is where
        // exactness lives — but the ones the browser said nothing about are
        // worth zero, and the line above it takes only what is worth something.
        // `device`, `network` and `language` are not among them: their last
        // slot means "nothing identifying was sent", which is a state that
        // recurs and not an absence.
        assert_eq!(named.len(), BLOCKS.len(), "{named:?}");
        let spoken: Vec<&str> = named
            .iter()
            .filter(|(_, c)| *c > 0.0)
            .map(|(l, _)| *l)
            .collect();
        assert!(spoken.contains(&"weekday"), "{named:?}");
        for absent in ["battery", "month", "screen", "surroundings"] {
            assert!(
                !spoken.contains(&absent),
                "{absent} was named on a bundle that never carried it: {named:?}"
            );
        }
    }
}
