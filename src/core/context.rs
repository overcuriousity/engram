//! The situation a page view happened in, and the fixed-length vector it becomes.
//! Pure functions, so the encoder can be pinned by a table of cases.

/// Where this feature reads the time; tests need a seventh Friday at 14:52 on demand.
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
    /// Fractional 0.0..24.0, because the hour is encoded as an angle.
    pub hour: f32,
    /// 0 = Monday.
    pub weekday: u32,
    /// 1-based day of the month.
    pub day: u32,
    /// 1-based; derived here because this is the one place that knows the zone.
    pub month: u32,
    pub days_in_month: u32,
}

/// The zone comes from the client, never config: it carries DST, which an offset cannot.
/// The offset is the fallback, then UTC, which is every row written before this feature.
pub fn local_time(at: i64, tz: Option<&str>, offset_mins: Option<i32>) -> LocalTime {
    use chrono::{DateTime, Datelike, TimeZone, Timelike};

    let utc = DateTime::from_timestamp(at, 0).unwrap_or_default();
    let naive = match tz.and_then(|z| z.parse::<chrono_tz::Tz>().ok()) {
        Some(z) => z.from_utc_datetime(&utc.naive_utc()).naive_local(),
        // `checked_mul`: a browser-sent offset past ±35 791 394 overflows; that reads as no offset.
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
        // Unreachable for a date chrono just produced; nothing here may take a page view down.
        _ => 30,
    }
}

/// One named span of the context vector, scored separately so the explanation is a lookup.
#[derive(Debug, Clone, Copy)]
pub struct Block {
    /// The config key its weight is read under. See `BlockWeights::of`.
    pub name: &'static str,
    /// What the line under the offer prints.
    pub label: &'static str,
    pub at: usize,
    pub dims: usize,
}

/// The layout, in order. Nothing describes who is in it: each user has their own collection.
/// The hour is a circle; the weekday is one-hot, since the pattern is exactly Friday.
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

/// A changed width needs `--reindex`: a named vector cannot be resized in place.
/// `ensure_collection` refuses a mismatched `ctx` width, which otherwise fails silently.
pub const CTX_DIM: usize = 45;

/// Bumped whenever `BLOCKS` changes in any way: a width, an order, or what a slot means.
pub const LAYOUT_VERSION: i64 = 2;

/// The layout version and a hash of `[recommend.weights]`: a weight is the geometry, so clusters
/// built under other weights are skipped until the next sweep rebuilds them.
pub fn encoder_version(w: &crate::config::BlockWeights) -> i64 {
    let mut key = format!("layout{LAYOUT_VERSION}");
    for b in BLOCKS {
        // The bits, not the printed value: weights differing below `{}` are different geometries.
        key.push_str(&format!("|{}={:08x}", b.name, w.of(b.name).to_bits()));
    }
    // Positive: it goes into a signed column.
    (fnv1a(&key) >> 1) as i64
}

/// What the browser said about the situation, as received; every field optional, absent is not
/// zero. Fingerprinting signals (canvas, WebGL, fonts) are deliberately not collected: constant for
/// one person.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Bundle {
    /// IANA, from `Intl.DateTimeFormat().resolvedOptions().timeZone`.
    pub tz: Option<String>,
    pub tz_offset_mins: Option<i32>,
    pub language: Option<String>,
    /// Stored, not encoded.
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

    // Stored in `context_events.bundle` but read by no block yet; encoding one bumps
    // `LAYOUT_VERSION`.
    /// Geohash, 6 characters (about 1 km). Only with the place switch on.
    pub place: Option<String>,
    /// `slow-2g` … `4g`, from `connection.effectiveType`.
    pub net_effective: Option<String>,
    /// Mbit/s estimate.
    pub net_downlink: Option<f32>,
    /// Round-trip estimate, ms.
    pub rtt: Option<f32>,
    pub save_data: Option<bool>,
    pub reduced_motion: Option<bool>,
    pub high_contrast: Option<bool>,
    /// `coarse` | `fine`.
    pub pointer: Option<String>,
    pub hover: Option<bool>,
    /// `standalone` | `browser` from the web; `app` from the phone.
    pub display_mode: Option<String>,
    /// `navigate` | `reload` | `back_forward`.
    pub nav_type: Option<String>,
    /// `maximised` | `windowed`.
    pub window_state: Option<String>,
    pub focused: Option<bool>,
    /// Seconds since this sender last built a bundle.
    pub since_last_view_s: Option<f32>,
    /// Bundles this sender built since its local midnight.
    pub views_today: Option<u32>,
    pub screen_x: Option<f32>,
    pub screen_y: Option<f32>,
    pub screens: Option<u32>,
    pub avail_w: Option<f32>,
    pub avail_h: Option<f32>,
    pub video_inputs: Option<u32>,
    pub audio_inputs: Option<u32>,
    /// `visualViewport.scale`.
    pub zoom: Option<f32>,
    pub fullscreen: Option<bool>,
    /// `none` | `same_origin` | `external`.
    pub referrer_kind: Option<String>,
    pub online: Option<bool>,
    /// The Chromium keyboard map's layout, where a browser offers one.
    pub keyboard_layout: Option<String>,
    /// `h12` | `h23`.
    pub hour_cycle: Option<String>,
    // Phone only: a browser writes `null` for these, so the app.js test still sees every name.
    /// `speaker` | `wired` | `bluetooth` | `car`.
    pub audio_route: Option<String>,
    pub dnd: Option<bool>,
    /// `normal` | `vibrate` | `silent`.
    pub ringer: Option<String>,
    pub power_save: Option<bool>,
    /// 0.0..1.0.
    pub brightness: Option<f32>,
    /// Ambient light, last sensor reading.
    pub lux: Option<f32>,
    pub docked: Option<bool>,
    pub headset: Option<bool>,
}

/// Lenient: an empty bundle still works, and the raw string is stored whole.
pub fn parse_bundle(raw: &str) -> Bundle {
    serde_json::from_str(raw).unwrap_or_default()
}

/// The fields that do not move with the situation, so a rotated or unplugged phone is the same
/// phone. `None` when nothing identifying was sent, which `encode` gives its own slot.
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

/// Stable across runs and machines, unlike the per-process-seeded `DefaultHasher`.
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

/// Each block is normalised to length 1, then scaled by its weight, so it contributes exactly its
/// weight. An all-zero block stays zero: normalising it would manufacture a direction.
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

/// Leaving zeros is how "the browser did not say" is expressed.
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
            // The last slot means nothing identifying was sent: a recurring state, not an absence.
            Some(k) => s[bucket(&k, s.len() - 1)] = 1.0,
            None => {
                let last = s.len() - 1;
                s[last] = 1.0;
            }
        },
        "viewport" => {
            // Logs centred on a thousand pixels; raw logs score every screen 0.999 against every
            // other.
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
            // Language and zone halves, four slots each; they move together.
            let half = s.len() / 2;
            if let Some(l) = &b.language {
                s[bucket(l, half)] = 1.0;
            }
            if let Some(z) = &b.tz {
                s[half + bucket(z, half)] = 1.0;
            }
        }
        "network" => {
            // Four named states including unknown, so browsers without `connection` group together.
            let idx = match b.network.as_deref() {
                Some("wifi") => 0,
                Some("cellular") => 1,
                Some("wired" | "ethernet") => 2,
                _ => 3,
            };
            s[idx] = 1.0;
        }
        "power" => {
            // All zero without a Battery API, so a desktop does not agree with a phone at an
            // invented level.
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
            // One signed slot: portrait, landscape, and zero for "did not say".
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
        // A block left at zero contributes nothing rather than noise.
        _ => {}
    }
}

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

/// One cosine over the whole vector, the same number the store ranked on.
/// A centroid of the wrong width scores zero: `cosine` fails closed on a length mismatch.
pub fn context_score(now: &[f32], cluster: &[f32]) -> f32 {
    crate::vector::cosine(now, cluster)
}

/// `w_b * cos(block_now, block_cluster)`, largest first. These do not sum to `context_score`.
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
    // Ties break on the label, so naming order does not depend on how `BLOCKS` is written.
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
        // The bundle is assembled in `assets/app.js` with no compiler on either side; a rename is
        // silently `None`.
        let js = include_str!("../../assets/app.js");
        let sent: std::collections::BTreeSet<&str> = js
            .lines()
            .filter_map(|l| l.trim().strip_prefix("b."))
            .filter_map(|l| l.split_once(" = "))
            .map(|(name, _)| name)
            .collect();
        assert!(!sent.is_empty(), "found no bundle assignments in app.js");

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
        let mut at = 0;
        for b in BLOCKS {
            assert_eq!(b.at, at, "{} starts where the last block ended", b.name);
            at += b.dims;
        }
        assert_eq!(at, CTX_DIM);
    }

    #[test]
    fn half_past_eleven_at_night_is_near_half_past_midnight() {
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
        let bare = Bundle::default();
        assert!(device_key(&bare).is_none());
        let v = encode(FRIDAY, &bare, &weights());
        assert!((norm(slice_of(&v, "device")) - weights().device).abs() < 1e-5);
    }

    #[test]
    fn a_device_key_is_stable_and_ignores_the_situation() {
        let mut later = phone();
        later.orientation = Some("landscape".into());
        later.battery_level = Some(0.9);
        later.viewport_w = Some(844.0);
        assert_eq!(device_key(&phone()), device_key(&later));

        let mut other = phone();
        other.platform = Some("macOS".into());
        assert_ne!(device_key(&phone()), device_key(&other));
    }

    fn bundle_field_names() -> std::collections::BTreeSet<&'static str> {
        let src = include_str!("context.rs");
        let body = src
            .split_once("pub struct Bundle {")
            .expect("Bundle struct")
            .1
            .split_once("\n}")
            .expect("end of Bundle")
            .0;
        body.lines()
            .filter_map(|l| l.trim().strip_prefix("pub "))
            .filter_map(|l| l.split_once(':'))
            .map(|(name, _)| name)
            .collect()
    }

    #[test]
    fn the_phone_reads_the_same_vocabulary_as_the_struct() {
        // The Kotlin tests read this fixture; this keeps it honest. One name per line, sorted.
        let fixture = include_str!("../../android/core/src/test/resources/bundle-fields.txt");
        let listed: std::collections::BTreeSet<&str> = fixture
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(
            listed,
            bundle_field_names(),
            "bundle-fields.txt and Bundle disagree about what a situation is"
        );
    }

    #[test]
    fn the_device_key_ignores_every_field_that_describes_the_moment() {
        let a = phone();
        let b = Bundle {
            place: Some("u33dc0".into()),
            net_effective: Some("4g".into()),
            net_downlink: Some(12.5),
            rtt: Some(40.0),
            save_data: Some(true),
            reduced_motion: Some(true),
            high_contrast: Some(true),
            pointer: Some("fine".into()),
            hover: Some(true),
            display_mode: Some("standalone".into()),
            nav_type: Some("reload".into()),
            window_state: Some("maximised".into()),
            focused: Some(false),
            since_last_view_s: Some(3600.0),
            views_today: Some(9),
            screen_x: Some(1920.0),
            screen_y: Some(0.0),
            screens: Some(2),
            avail_w: Some(1900.0),
            avail_h: Some(1000.0),
            video_inputs: Some(1),
            audio_inputs: Some(2),
            zoom: Some(1.5),
            fullscreen: Some(true),
            referrer_kind: Some("external".into()),
            online: Some(false),
            keyboard_layout: Some("de".into()),
            hour_cycle: Some("h23".into()),
            audio_route: Some("car".into()),
            dnd: Some(true),
            ringer: Some("silent".into()),
            power_save: Some(true),
            brightness: Some(0.2),
            lux: Some(3.0),
            docked: Some(true),
            headset: Some(true),
            ..phone()
        };
        assert_eq!(device_key(&a), device_key(&b));
    }

    #[test]
    fn a_bundle_with_the_wider_vocabulary_parses_every_field() {
        let raw = r#"{"tz":"Europe/Berlin","place":"u33dc0","audio_route":"car",
            "views_today":3,"lux":12.5,"online":true,"hour_cycle":"h23"}"#;
        let b = parse_bundle(raw);
        assert_eq!(b.place.as_deref(), Some("u33dc0"));
        assert_eq!(b.audio_route.as_deref(), Some("car"));
        assert_eq!(b.views_today, Some(3));
        assert_eq!(b.lux, Some(12.5));
        assert_eq!(b.online, Some(true));
        assert_eq!(b.hour_cycle.as_deref(), Some("h23"));
    }

    #[test]
    fn nothing_in_the_vector_says_who_is_asking() {
        assert!(
            !BLOCKS.iter().any(|b| b.name == "scope"),
            "the situation is what the browser reports, not who is holding it"
        );
        assert_eq!(CTX_DIM, 45, "eight dimensions of constant went with it");
    }

    #[test]
    fn the_situation_is_the_whole_vector() {
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
        assert!(BLOCKS.iter().all(|b| !b.label.is_empty()));
    }

    #[test]
    fn a_bundle_that_is_not_json_is_an_empty_one_rather_than_an_error() {
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
        // The offset is deliberately wrong: a zone, when there is one, is the authority.
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
        let a = weights();
        let mut b = weights();
        b.network += 0.01;
        assert_ne!(
            encoder_version(&a),
            encoder_version(&b),
            "an edited weight left the version standing"
        );
        let mut c = weights();
        c.power += f32::EPSILON;
        assert_ne!(encoder_version(&a), encoder_version(&c));
        assert_eq!(encoder_version(&a), encoder_version(&weights()));
        assert!(encoder_version(&a) > 0);
    }

    #[test]
    fn a_block_that_agreed_on_nothing_is_not_one_of_the_reasons() {
        let w = weights();
        let bare = Bundle {
            tz: Some("Europe/Berlin".into()),
            ..Default::default()
        };
        let v = encode(FRIDAY_1352_UTC, &bare, &w);
        let named = contributions(&v, &v, &w);
        // `device`, `network` and `language` keep a "nothing identifying" slot, which is not an
        // absence.
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
