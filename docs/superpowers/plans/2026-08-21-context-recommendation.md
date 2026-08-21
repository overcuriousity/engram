# A recommendation under the search box, from context — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Learn which situations recur for which artifact from the interaction log, and offer that artifact under the search box before it is asked for, with the reasons visible.

**Architecture:** A browser bundle recorded on every page view (`context_events`); a sweep that agglomerates the situations an artifact was opened in into a handful of decayed clusters (`context_clusters`); each centroid stored as one element of a `ctx` multivector on the artifact's existing Qdrant point, scored with `max_sim`; one vector query at page load, its winning cluster re-derived locally so the display can name the blocks that decided it. No model call and no embedding anywhere on this path.

**Tech Stack:** Rust 2024, axum 0.8, sqlx/SQLite, Qdrant over REST, askama templates, htmx, chrono + chrono-tz.

**Spec:** `docs/superpowers/specs/2026-08-21-context-recommendation-design.md`

## Global Constraints

- **No model call and no embedding at read time.** One vector query per page view, one exact scan over the profiled subset. Nothing here may add an inference call.
- **The sweep writes vectors through `POST /points/vectors`, never `upsert`.** `upsert` replaces the whole payload; clearing `status` or `last_seen_at` puts hidden artifacts back into search (`src/vector/qdrant.rs:1095-1101`).
- **This adds no stage to search ranking.** The recommendation is its own surface and disappears the moment a query exists.
- **`scope` isolation is load-bearing.** Until per-user collections exist, the `scope` block's weight is the only thing keeping one person's clusters from being offered to another. Task 4 and Task 8 each carry a test for it.
- **The reason must match the hit.** The local recomputation and the store must pick the same artifact. Task 8 carries the test.
- **One gate.** Everything under `[recommend]` is `enabled` plus numbers. No second boolean.
- **`schema.sql` is declarative, not a migration chain** (`src/store/mod.rs:60-80`). New tables are `CREATE TABLE IF NOT EXISTS`; a new *column* on an existing table means the boot check names it as missing and the operator recreates the database. That is the existing contract — do not add `ALTER TABLE`.
- **Rust 1.94, edition 2024.** `cargo fmt` and `cargo clippy` must be clean.
- Commit style: conventional prefix, lowercase subject in the repo's voice (`feat(context): …`).

## Two refinements to the spec, decided here

Both are consequences of the spec's own numbers. They are called out so an executor does not read them as drift.

1. **The rung is scored without the `scope` block.** §6 sets `scope` to weight 10 against a total block weight of ~4.6, so a same-scope cosine never falls below ≈0.957 whatever the situation. `strong_at` and `weak_at` would have to live in a band four hundredths wide. So: Qdrant's `max_sim` over the **full** vector decides *which* artifact is offered (that is what the scope block is for — a foreign cluster can never win it), and the **rung** is decided by the cosine over the vector with the `scope` block sliced off. The two numbers have different jobs and now have different scales.
2. **`context_clusters.representative` stores `{"at": <unix>, "bundle": {…}}`.** The display quotes the representative event's timestamp, and the raw bundle does not carry one. One TEXT column still, as the spec's schema fixes it.

## File Structure

**Created:**
- `src/core/context.rs` — `Clock`, `Bundle`, `LocalTime`, the block table, `CTX_DIM`, `encode`, `contributions`, `device_key`. One pure module: everything here is a function of its arguments, which is what makes Task 4's table-driven tests possible.
- `src/core/recommend.rs` — the read path: query the store, reload the candidates' clusters, reproduce the argmax, pick the rung, build the reason.
- `src/store/context.rs` — `context_events` and `context_clusters` reads and writes.
- `src/jobs/context.rs` — the sweep: bridge join, agglomeration, decay, slot allocation, centroid write.
- `src/web/templates/_context.html` — the fragment under the search box.

**Modified:**
- `Cargo.toml` — `chrono`, `chrono-tz`.
- `src/store/schema.sql` — `artifacts.updated_at`, `context_events`, `context_clusters`.
- `src/store/artifacts.rs:747-812` — three UPDATEs bump `updated_at` beside `embed_rev`.
- `src/store/pursuits.rs` — a `recommended_*` writer and the Ops rollup.
- `src/config.rs` — `RecommendConfig`, `BlockWeights`, wired into `Config`.
- `src/core/mod.rs` — `pub mod context; pub mod recommend;`, `Core::{clock, recommend}`, `Core::recommends()`, test builder.
- `src/vector/mod.rs` — two trait methods.
- `src/vector/memory.rs` — both, including `max_sim`.
- `src/vector/qdrant.rs` — `CTX` const, `collection_body`, `reindex`, both trait methods.
- `src/store/jobs.rs` — `Stage::Context`.
- `src/core/background.rs`, `src/jobs/mod.rs` — the periodic unit.
- `src/jobs/pursuit.rs:520` — exclude `recommended_shown` from engagement.
- `src/jobs/retention.rs` — trim `context_events` on its own window.
- `src/web/ui.rs` — `POST /ui/context`, `SearchTemplate::recommend`, the `rec=` branch in `artifact_detail`, the Ops rollup.
- `src/web/templates/search.html`, `src/web/templates/ops.html`.
- `assets/app.js`, `assets/css/40-search.css`.
- `config.example.toml`, `ROADMAP.md`.

---

### Task 1: Time — `chrono`, a `Clock`, and `artifacts.updated_at`

Storage stays Unix seconds. Nothing existing changes format, and the other
`now()` call sites in the tree are **not** touched: they work, and rewriting
them is a diff across the tree for nothing.

**Files:**
- Modify: `Cargo.toml`
- Create: `src/core/context.rs`
- Modify: `src/core/mod.rs` (module declaration, `Core::clock`, test builder)
- Modify: `src/store/schema.sql:66-120` (one column on `artifacts`)
- Modify: `src/store/artifacts.rs:747-812` (three UPDATEs)
- Test: in `src/core/context.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `crate::core::context::{Clock, LocalTime, local_time}`.
  - `enum Clock { System, Fixed(i64) }`, `impl Clock { pub fn now(&self) -> i64 }`, `Copy + Clone + Debug`.
  - `struct LocalTime { pub hour: f32, pub weekday: u32, pub day: u32, pub days_in_month: u32 }` — `hour` fractional 0.0..24.0, `weekday` 0 = Monday, `day` 1-based.
  - `pub fn local_time(at: i64, tz: Option<&str>, offset_mins: Option<i32>) -> LocalTime`
  - `Core::clock: Clock`.

- [ ] **Step 1: Add the two crates**

```bash
cargo add chrono --no-default-features --features clock,std
cargo add chrono-tz --no-default-features --features std
```

Then edit `Cargo.toml` so the two lines carry their reason, in the style of the
file's other comments (put them just after the `url = "2"` line):

```toml
# Local time, so "Friday at 15:00" is a question the base can ask of its own
# log. Storage stays Unix seconds; this is only ever applied at the edge.
chrono = { version = "0.4.45", default-features = false, features = ["clock", "std"] }
# The IANA database, because the zone comes from the device
# (`Intl.DateTimeFormat().resolvedOptions().timeZone`) and a stored offset
# cannot answer what happens across a DST boundary.
chrono-tz = { version = "0.10.4", default-features = false, features = ["std"] }
```

- [ ] **Step 2: Write the failing tests**

Create `src/core/context.rs`:

```rust
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib core::context 2>&1 | tail -20`
Expected: FAIL — `cannot find function local_time in this scope`.

- [ ] **Step 4: Implement `local_time`**

Add above the `#[cfg(test)]` block in `src/core/context.rs`:

```rust
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
        None => match offset_mins.and_then(|m| chrono::FixedOffset::east_opt(m * 60)) {
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
    let (y, m) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
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
```

- [ ] **Step 5: Declare the module and hang the clock on `Core`**

In `src/core/mod.rs`, add `pub mod context;` to the module list at the top
(alphabetical, between `pub mod ask;` and `pub mod extract;` — after
`pub mod background;`).

Add to the `Core` struct, just after `pub background: Arc<Background>,`:

```rust
    /// Where this feature reads the time. `System` in the binary; the
    /// recommendation tests set a fixed one so a seventh Friday at 14:52
    /// exists on demand. Nothing else in the tree reads it.
    pub clock: crate::core::context::Clock,
```

Add to `Core::from_config`'s literal, beside `background`:

```rust
            clock: crate::core::context::Clock::System,
```

Add to `test_support::build`'s literal, in the same place:

```rust
            clock: crate::core::context::Clock::System,
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib core::context 2>&1 | tail -20`
Expected: PASS, 5 tests.

- [ ] **Step 7: Add `artifacts.updated_at`**

In `src/store/schema.sql`, inside `CREATE TABLE IF NOT EXISTS artifacts`,
immediately after the `embed_rev` column and its comment (`schema.sql:99-101`):

```sql
  -- Bumped in the same UPDATE that bumps `embed_rev`. Unrelated to
  -- recommendation, and the one question the base could not previously answer
  -- about itself: when did this artifact last change. `created_at` answers when
  -- it arrived, and `last_verified_at` answers when someone vouched for it;
  -- neither says whether the text on screen is the text that was captured.
  updated_at       INTEGER NOT NULL DEFAULT 0,
```

- [ ] **Step 8: Bump it wherever `embed_rev` is bumped**

Three sites in `src/store/artifacts.rs`. In each, add `updated_at = ?` to the
`SET` list and bind `super::now()` as the *first* bind (before the existing
ones), matching each statement's placeholder order.

`reset_embed_state` (`:747`):

```rust
        sqlx::query(
            "UPDATE artifacts
             SET embed_state = 'pending', embed_model = NULL, embed_rev = embed_rev + 1,
                 updated_at = ?
             WHERE corpus_id = ?",
        )
        .bind(super::now())
        .bind(corpus_id)
```

`update_artifact_title` (`:784`):

```rust
            sqlx::query(
                "UPDATE artifacts
                 SET title = ?, embed_state = 'pending', embed_model = NULL,
                     embed_rev = embed_rev + 1, updated_at = ?
                 WHERE id = ?",
            )
            .bind(title)
            .bind(super::now())
            .bind(id)
```

`update_artifact_text` (`:799`):

```rust
            sqlx::query(
                "UPDATE artifacts
                 SET text = ?, embed_state = 'pending', embed_model = NULL,
                     embed_rev = embed_rev + 1, updated_at = ?
                 WHERE id = ?",
            )
            .bind(text)
            .bind(super::now())
            .bind(id)
```

- [ ] **Step 9: Write the failing test for the bump**

Append to the existing `mod tests` in `src/store/artifacts.rs`:

```rust
    #[tokio::test]
    async fn editing_an_artifact_stamps_when_it_changed() {
        let store = Store::memory().await.unwrap();
        let cid = seed_corpus(&store).await;
        let id = seed_artifact(&store, &cid, "before").await;

        let before: i64 = sqlx::query_scalar("SELECT updated_at FROM artifacts WHERE id = ?")
            .bind(&id)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(before, 0, "a fresh row has never been edited");

        store.update_artifact_text(&id, "after").await.unwrap();

        let after: i64 = sqlx::query_scalar("SELECT updated_at FROM artifacts WHERE id = ?")
            .bind(&id)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert!(after > 0, "an edit says when it happened");
    }
```

If `seed_corpus`/`seed_artifact` helpers do not already exist in that test
module, read the module's existing tests and use whatever they use to create a
corpus and an artifact — do not add new helpers.

- [ ] **Step 10: Run it**

Run: `cargo test --lib store::artifacts::tests::editing_an_artifact_stamps_when_it_changed 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 11: Whole suite, then commit**

Run: `cargo fmt && cargo clippy --all-targets 2>&1 | tail -20 && cargo test --lib 2>&1 | tail -20`
Expected: clean, all pass.

```bash
git add Cargo.toml Cargo.lock src/core/context.rs src/core/mod.rs \
        src/store/schema.sql src/store/artifacts.rs
git commit -m "feat(context): a clock that knows what Friday means, and a stamp for when an artifact changed"
```

---

### Task 2: Config — `[recommend]`, one gate and a table of numbers

`ROADMAP.md` under `[Core Platform]` already objects to eight gates over one
faculty. This adds one, and everything else is a number.

**Files:**
- Modify: `src/config.rs` (new structs, one field on `Config`)
- Modify: `src/core/mod.rs` (`Core::recommend`, `Core::recommends()`, both builders)
- Modify: `config.example.toml`
- Test: in `src/config.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `crate::config::BlockWeights { scope, time_of_day, weekday, weekend, device, viewport, locale, network, power, month_cycle, environment: f32 }` — `Debug + Deserialize + Clone`.
  - `crate::config::RecommendConfig { enabled: bool, cluster_merge_at: f32, max_clusters: usize, half_life_days: f64, min_weight: f64, strong_at: f32, weak_at: f32, self_weight: f64, weights: BlockWeights }` — `Debug + Deserialize + Clone`.
  - `impl BlockWeights { pub fn of(&self, block: &str) -> f32 }`
  - `Core::recommend: RecommendConfig`, `Core::recommends() -> bool`.

- [ ] **Step 1: Write the failing test**

Append to the existing `mod tests` in `src/config.rs`:

```rust
    #[test]
    fn the_recommender_ships_off_with_its_weights_named() {
        let r = RecommendConfig::default();
        assert!(!r.enabled, "a faculty that learns from a log ships off");
        // §6: the scope block dominates so a foreign cluster can never win
        // `max_sim`. When each user has their own collection this goes to 0
        // and nothing else changes.
        assert_eq!(r.weights.of("scope"), 10.0);
        assert_eq!(r.weights.of("weekday"), 1.0);
        assert_eq!(r.weights.of("month_cycle"), 0.0, "off by default");
        // A block nobody named contributes nothing rather than a default.
        assert_eq!(r.weights.of("phase_of_the_moon"), 0.0);
    }

    #[test]
    fn a_partial_recommend_table_keeps_the_rest_of_the_defaults() {
        // `#[serde(default)]` on both structs is what makes an operator able to
        // move one weight without restating ten.
        let cfg: RecommendConfig = toml::from_str(
            "enabled = true\n[weights]\ndevice = 2.5\n",
        )
        .unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.weights.of("device"), 2.5);
        assert_eq!(cfg.weights.of("weekday"), 1.0, "untouched");
        assert_eq!(cfg.max_clusters, 5, "untouched");
    }
```

If `toml` is not already a dev-dependency, use `config::Config::builder()` the
way the module's neighbouring tests parse a fragment; read them first and match
whichever they use rather than adding a crate.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib config::tests::the_recommender 2>&1 | tail -20`
Expected: FAIL — `cannot find type RecommendConfig`.

- [ ] **Step 3: Implement the two structs**

Add to `src/config.rs`, after `SittingConfig`'s `impl Default` (around `:311`):

```rust
/// What each named block of the context vector is worth.
///
/// This is the whole of §6's argument in config form. Each block is normalised
/// to length 1 and *then* scaled by its weight, so a block contributes exactly
/// its weight however many dimensions it happens to use — seven one-hot slots
/// for the weekday do not outweigh two for the hour because there are seven of
/// them. That is what turns the encoding's implicit weighting, which nobody can
/// tune, back into named numbers an operator can change.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct BlockWeights {
    /// Interim, and load-bearing. Every scope shares one collection today, and
    /// a point's multivector mixes the clusters of everyone who opened that
    /// artifact — a payload filter cannot help, because it acts on the point
    /// and not on elements of the set. A dominating `scope` block is what keeps
    /// a foreign cluster from ever winning `max_sim`. When each user gets their
    /// own collection this goes to 0 and nothing else changes.
    pub scope: f32,
    /// The hour, as an angle. See `encode`.
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
            scope: 10.0,
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
            "scope" => self.scope,
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
/// One gate and a table of numbers, on purpose: `ROADMAP.md` under
/// `[Core Platform]` objects to eight gates over one faculty, and this does not
/// add a ninth. The learning cadence is not here either — see
/// `jobs::context::INTERVAL_HOURS`.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct RecommendConfig {
    /// Off until there is a base with weeks of situations in it. A weekly
    /// pattern needs weeks; shipping this on would show the bottom rung of the
    /// ladder for a fortnight and teach the operator to ignore the area.
    pub enabled: bool,
    /// Cosine above which an event joins a cluster rather than opening its own.
    pub cluster_merge_at: f32,
    /// Per (scope, artifact). Multiple clusters are the point: a thing looked
    /// up on Friday afternoons *and* occasionally on Monday mornings is two
    /// situations, and their mean is a situation that never happened.
    pub max_clusters: usize,
    /// A pattern that stops fades rather than standing for ever.
    pub half_life_days: f64,
    /// A cluster below this is dropped. Also what protects against the single
    /// accident: one event never reaches it.
    pub min_weight: f64,
    /// Context score (the vector without its `scope` block — see the plan's
    /// note) at or above which the offer is called a pattern.
    pub strong_at: f32,
    /// And above which it is called a resemblance. Below it the ladder falls
    /// through to the sitting, and then to what has been forgotten.
    pub weak_at: f32,
    /// What an open of something this feature offered counts for, back into the
    /// profile. Zero, because without it the first lucky guess grows into a
    /// habit the system taught itself.
    pub self_weight: f64,
}

impl Default for RecommendConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cluster_merge_at: 0.82,
            max_clusters: 5,
            half_life_days: 45.0,
            min_weight: 2.0,
            strong_at: 0.75,
            weak_at: 0.45,
            self_weight: 0.0,
        }
    }
}
```

Note the `weights` field is deliberately *not* in the struct above — add it now,
as the last field of `RecommendConfig`, so the TOML nests as
`[recommend.weights]`:

```rust
    /// See `BlockWeights`.
    pub weights: BlockWeights,
```

and in `Default`:

```rust
            weights: BlockWeights::default(),
```

- [ ] **Step 4: Wire it into `Config`**

In `src/config.rs`, add to the `Config` struct after `pub sitting: SittingConfig,`:

```rust
    #[serde(default)]
    pub recommend: RecommendConfig,
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib config:: 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Hang it on `Core`**

In `src/core/mod.rs`, add to the `Core` struct after `pub sitting: …`:

```rust
    /// Whether and how the area under the search box is filled. Read by the
    /// sweep and on the page-view path, so it lives here rather than being
    /// threaded down.
    pub recommend: crate::config::RecommendConfig,
```

In `from_config`'s literal, beside `sitting: cfg.sitting.clone(),`:

```rust
            recommend: cfg.recommend.clone(),
```

In `test_support::build`'s literal, beside `sitting: …`:

```rust
            // Off, like the shipped default. The recommendation tests switch it
            // on; every other test asserts nothing is offered and nothing is
            // recorded.
            recommend: crate::config::RecommendConfig::default(),
```

Add beside `Core::asks` (around `:303`):

```rust
    /// Is the area under the search box filled? `false` means the placeholder
    /// is not rendered, the endpoint records nothing, and the sweep does not
    /// run — one gate, in one place.
    pub fn recommends(&self) -> bool {
        self.recommend.enabled
    }
```

- [ ] **Step 7: Document it in `config.example.toml`**

Append after the `[pursuit]` block (`config.example.toml:474-482`):

```toml
# ── A recommendation under the search box ────────────────────────────────────
# The area under the search box, filled from the situation the page was opened
# in: the time zone and local time the browser reports, the device, the
# viewport, the network, the battery. A sweep learns which situations recur for
# which artifact; a page view is one vector query against what it learned. No
# model call and no embedding on this path, ever.
#
# Off until there is a base with weeks of situations in it — a weekly pattern
# needs weeks, and until then every offer would sit on the bottom rung.
[recommend]
enabled = false
# Cosine above which an event joins a cluster rather than opening its own.
cluster_merge_at = 0.82
# Situations kept per artifact, per person. More than one is the point: a thing
# looked up on Friday afternoons and occasionally on Monday mornings is two
# situations, and their mean is a situation that never happened.
max_clusters = 5
# A pattern that stops fades rather than standing for ever.
half_life_days = 45.0
# A cluster lighter than this is dropped. Also what protects against the single
# accident: one event never reaches it.
min_weight = 2.0
# Score at or above which the offer is called a pattern, and above which it is
# called a resemblance. Below the second, the ladder falls through to what this
# sitting has touched, and then to what has been forgotten. Scored without the
# `scope` block, which decides *who* may be offered something rather than how
# well the situation matches.
strong_at = 0.75
weak_at = 0.45
# What an open of something this offered counts for, back into the profile.
# Zero: without it, the first lucky guess grows into a habit the system taught
# itself.
self_weight = 0.0

# What each named block of the situation is worth. Each block is normalised to
# length 1 and then scaled by its weight, so a block contributes exactly its
# weight however many dimensions it uses — seven one-hot slots for the weekday
# do not outweigh two for the hour because there are seven of them.
[recommend.weights]
# Interim, and load-bearing: every person shares one collection today, so this
# is what keeps one person's situations from being offered to another. It goes
# to 0 when each user has their own collection.
scope = 10.0
# The hour, as an angle — 23:30 and 00:30 are an hour apart, and 14:55 against a
# 15:00 pattern costs almost nothing.
time_of_day = 1.0
# One-hot, because "Friday is three from Tuesday" means nothing and the pattern
# is exactly Friday.
weekday = 1.0
# The part of the weekday that genuinely is gradual, kept separate and weak.
weekend = 0.3
device = 0.8
viewport = 0.4
locale = 0.3
network = 0.6
power = 0.2
environment = 0.2
# Off. A monthly rhythm is real, but nothing has shown one here yet.
month_cycle = 0.0
```

- [ ] **Step 8: Verify the example file still parses**

Run: `cargo test --lib config:: 2>&1 | tail -20`
Expected: PASS. (`src/config.rs` has a test that loads `config.example.toml`; if
it does not, run `cargo run -- --help` after pointing `ENGRAM_CONFIG` at the
example to confirm it loads.)

- [ ] **Step 9: Commit**

```bash
git add src/config.rs src/core/mod.rs config.example.toml
git commit -m "feat(recommend): one gate over the offer, and every weight it rests on named"
```

---

### Task 3: Two tables and the store that reads them

`context_events` holds the bundle **whole**, including fields the encoder does
not read today. That is what makes §6's versioning cheap: a new block is a
reindex plus a sweep, not the loss of history.

**Files:**
- Modify: `src/store/schema.sql` (two tables, two indexes, at the end of the feedback section)
- Create: `src/store/context.rs`
- Modify: `src/store/mod.rs` (module declaration)
- Test: in `src/store/context.rs`

**Interfaces:**
- Consumes: `crate::store::feedback::{blob_to_vec, vec_to_blob}`.
- Produces:
  - `pub struct ContextEvent { pub id: i64, pub scope: Option<String>, pub at: i64, pub bundle: String, pub device_key: Option<String>, pub local_hour: Option<i64>, pub weekday: Option<i64>, pub tz: Option<String> }`
  - `pub struct StoredCluster { pub scope: Option<String>, pub artifact_id: String, pub slot: i64, pub centroid: Vec<f32>, pub weight: f64, pub last_at: i64, pub encoder_version: i64, pub representative: String }`
  - `pub const RETAIN_DAYS: i64 = 400;`
  - `Store::record_context(&self, ev: &ContextEvent) -> Result<i64>` (the `id` on the argument is ignored; the row's own is returned)
  - `Store::context_events_since(&self, since: i64) -> Result<Vec<ContextEvent>>` — oldest first
  - `Store::expire_context_events(&self, retain_days: i64) -> Result<u64>`
  - `Store::replace_context_clusters(&self, artifact_id: &str, clusters: &[StoredCluster]) -> Result<()>`
  - `Store::context_clusters_of(&self, artifact_ids: &[String]) -> Result<HashMap<String, Vec<StoredCluster>>>`

- [ ] **Step 1: Add the tables**

In `src/store/schema.sql`, after the `interaction_events` index
(`schema.sql:476`) and before the `pursuits` comment block:

```sql
-- ── The situation a page view happened in ────────────────────────────────────
-- Joined to `search_events` and `interaction_events` through `scope` and `at`,
-- never through a stored id — the same rule `interaction_events` states just
-- above for pursuits. The clustering decides what belongs together, and
-- re-clustering never has to rewrite these.
CREATE TABLE IF NOT EXISTS context_events (
  id          INTEGER PRIMARY KEY,
  scope       TEXT,
  at          INTEGER NOT NULL,
  -- The whole bundle as received, including fields the encoder ignores. That
  -- is what makes a new block cheap: a reindex plus a sweep, rather than the
  -- loss of every situation recorded before it existed.
  bundle      TEXT NOT NULL,
  -- Hash over the stable fields only: platform, UA family, screen dimensions,
  -- hardwareConcurrency, deviceMemory, language. Not canvas, WebGL or fonts —
  -- those are what identify a device across a population, and here the
  -- population is one authenticated person, so they are constant and say
  -- nothing about *which situation* this is. They are also randomised per
  -- session and origin by a hardened browser, so every day would look like a
  -- new device.
  device_key  TEXT,
  -- Denormalised because the sweep reads them on every row.
  local_hour  INTEGER,
  weekday     INTEGER,
  tz          TEXT
);
CREATE INDEX IF NOT EXISTS idx_context_scope_at ON context_events(scope, at);

-- The situations one artifact is opened in, agglomerated. The centroids
-- themselves live in the vector store as the `ctx` multivector; this is the
-- bookkeeping, for two reasons: Qdrant holds numbers and cannot produce a
-- reason, and this table survives a `--reindex` while the vectors are rewritten.
CREATE TABLE IF NOT EXISTS context_clusters (
  id              INTEGER PRIMARY KEY,
  scope           TEXT,
  artifact_id     TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  -- Position of this centroid within the point's `ctx` multivector. Unique per
  -- artifact and NOT per (scope, artifact): the multivector is one array on one
  -- point, shared by every scope that has opened this artifact, so a slot
  -- numbered per scope would have two owners writing index 0.
  slot            INTEGER NOT NULL,
  centroid        BLOB NOT NULL,
  weight          REAL NOT NULL,
  last_at         INTEGER NOT NULL,
  -- What layout `centroid` was written under. A reader that does not recognise
  -- it skips the cluster rather than explaining a hit with the wrong blocks.
  encoder_version INTEGER NOT NULL,
  -- The member nearest the centroid, as `{"at": <unix>, "bundle": {…}}` — what
  -- the display quotes. The stamp is carried with it because a bundle does not
  -- contain one and the line says "like 08.08., 15:04".
  representative  TEXT NOT NULL,
  UNIQUE (artifact_id, slot)
);
CREATE INDEX IF NOT EXISTS idx_context_clusters_artifact ON context_clusters(artifact_id);
```

- [ ] **Step 2: Write the failing tests**

Create `src/store/context.rs`:

```rust
//! What situation a page view happened in, and what the sweep made of them.
//!
//! Two tables with one rule between them: nothing here is joined by a stored
//! id. A context event is matched to an open through `scope` and `at`, the way
//! `interaction_events` is matched to a pursuit — so re-clustering never has to
//! rewrite a row, and a sweep run under a new encoder starts from the raw
//! bundles rather than from what the last one concluded.

use super::{Store, now};
use crate::error::Result;
use crate::store::feedback::{blob_to_vec, vec_to_blob};
use sqlx::Row;
use std::collections::HashMap;

/// How long a situation is kept.
///
/// Not `feedback.retain_days`, and not a setting. A weekly pattern needs weeks
/// and a monthly one needs months, so the window this feature needs is not the
/// window an operator sets for their query log — and it is longer than either
/// default. Housekeeping about how long the base may remember a Friday
/// afternoon is not a preference; it is what the feature costs to work at all.
pub const RETAIN_DAYS: i64 = 400;

/// One page view, as the browser described it.
#[derive(Debug, Clone, Default)]
pub struct ContextEvent {
    pub id: i64,
    pub scope: Option<String>,
    pub at: i64,
    /// The bundle as received, JSON.
    pub bundle: String,
    pub device_key: Option<String>,
    pub local_hour: Option<i64>,
    pub weekday: Option<i64>,
    pub tz: Option<String>,
}

/// One learned situation, as SQLite holds it.
#[derive(Debug, Clone)]
pub struct StoredCluster {
    pub scope: Option<String>,
    pub artifact_id: String,
    pub slot: i64,
    pub centroid: Vec<f32>,
    pub weight: f64,
    pub last_at: i64,
    pub encoder_version: i64,
    pub representative: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(scope: &str, at: i64) -> ContextEvent {
        ContextEvent {
            id: 0,
            scope: Some(scope.into()),
            at,
            bundle: r#"{"tz":"Europe/Berlin"}"#.into(),
            device_key: Some("phone".into()),
            local_hour: Some(15),
            weekday: Some(4),
            tz: Some("Europe/Berlin".into()),
        }
    }

    #[tokio::test]
    async fn a_recorded_situation_comes_back_whole() {
        let store = Store::memory().await.unwrap();
        store.record_context(&event("alice", 1_000)).await.unwrap();

        let out = store.context_events_since(0).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].scope.as_deref(), Some("alice"));
        assert_eq!(out[0].bundle, r#"{"tz":"Europe/Berlin"}"#);
        assert_eq!(out[0].weekday, Some(4));
        assert!(out[0].id > 0, "the row's own id, not the argument's");
    }

    #[tokio::test]
    async fn situations_come_back_oldest_first() {
        let store = Store::memory().await.unwrap();
        for at in [3_000, 1_000, 2_000] {
            store.record_context(&event("alice", at)).await.unwrap();
        }
        let ats: Vec<i64> = store
            .context_events_since(0)
            .await
            .unwrap()
            .iter()
            .map(|e| e.at)
            .collect();
        assert_eq!(ats, vec![1_000, 2_000, 3_000]);
    }

    #[tokio::test]
    async fn expiry_uses_this_features_own_window() {
        let store = Store::memory().await.unwrap();
        let day = 86_400;
        store.record_context(&event("alice", now() - 500 * day)).await.unwrap();
        store.record_context(&event("alice", now() - 10 * day)).await.unwrap();

        let dropped = store.expire_context_events(RETAIN_DAYS).await.unwrap();
        assert_eq!(dropped, 1);
        assert_eq!(store.context_events_since(0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn clusters_are_replaced_wholesale_not_merged() {
        // The sweep is a full rebuild per artifact. A write that merged would
        // leave a slot from a previous encoder standing beside fresh ones, and
        // the multivector written from them would not match the table.
        let store = Store::memory().await.unwrap();
        let aid = seed_artifact(&store).await;

        store
            .replace_context_clusters(&aid, &[cluster(&aid, 0, 3.0), cluster(&aid, 1, 2.0)])
            .await
            .unwrap();
        store
            .replace_context_clusters(&aid, &[cluster(&aid, 0, 9.0)])
            .await
            .unwrap();

        let back = store.context_clusters_of(&[aid.clone()]).await.unwrap();
        let mine = &back[&aid];
        assert_eq!(mine.len(), 1, "slot 1 is gone, not stale");
        assert_eq!(mine[0].weight, 9.0);
    }

    #[tokio::test]
    async fn a_centroid_survives_the_round_trip() {
        let store = Store::memory().await.unwrap();
        let aid = seed_artifact(&store).await;
        let mut c = cluster(&aid, 0, 1.0);
        c.centroid = vec![0.5, -0.25, 0.125];
        store.replace_context_clusters(&aid, &[c]).await.unwrap();

        let back = store.context_clusters_of(&[aid.clone()]).await.unwrap();
        assert_eq!(back[&aid][0].centroid, vec![0.5, -0.25, 0.125]);
    }

    #[tokio::test]
    async fn an_artifact_with_no_clusters_is_absent_rather_than_empty() {
        // The read path asks for the ten ids the store returned and expects to
        // learn which of them it knows nothing about.
        let store = Store::memory().await.unwrap();
        let back = store
            .context_clusters_of(&["nobody".to_string()])
            .await
            .unwrap();
        assert!(back.is_empty());
    }

    #[tokio::test]
    async fn deleting_an_artifact_takes_its_situations_with_it() {
        let store = Store::memory().await.unwrap();
        let aid = seed_artifact(&store).await;
        store
            .replace_context_clusters(&aid, &[cluster(&aid, 0, 1.0)])
            .await
            .unwrap();

        sqlx::query("DELETE FROM artifacts WHERE id = ?")
            .bind(&aid)
            .execute(&store.pool)
            .await
            .unwrap();

        let back = store.context_clusters_of(&[aid]).await.unwrap();
        assert!(back.is_empty(), "ON DELETE CASCADE");
    }

    fn cluster(artifact_id: &str, slot: i64, weight: f64) -> StoredCluster {
        StoredCluster {
            scope: Some("alice".into()),
            artifact_id: artifact_id.into(),
            slot,
            centroid: vec![1.0, 0.0],
            weight,
            last_at: 1_000,
            encoder_version: 1,
            representative: r#"{"at":1000,"bundle":{}}"#.into(),
        }
    }

    /// A corpus and one artifact in it, because `context_clusters.artifact_id`
    /// is a foreign key and SQLite enforces it.
    async fn seed_artifact(store: &Store) -> String {
        let cid = crate::store::new_id();
        sqlx::query(
            "INSERT INTO corpora (id, title, text, status, created_at)
             VALUES (?, 'c', 't', 'ready', 0)",
        )
        .bind(&cid)
        .execute(&store.pool)
        .await
        .unwrap();
        let aid = crate::store::new_id();
        sqlx::query(
            "INSERT INTO artifacts (id, corpus_id, ordinal, text, created_at)
             VALUES (?, ?, 0, 'a', 0)",
        )
        .bind(&aid)
        .bind(&cid)
        .execute(&store.pool)
        .await
        .unwrap();
        aid
    }
}
```

Before running: open `src/store/corpora.rs` and confirm the `corpora` column
names used in `seed_artifact` match the real schema (`schema.sql:15-43`). Fix
the two INSERTs to match rather than changing the schema.

- [ ] **Step 3: Declare the module and run the tests to verify they fail**

In `src/store/mod.rs`, add `pub mod context;` to the module list (alphabetical,
after `pub mod corpora;`... place it before `corpora` so the list stays sorted).

Run: `cargo test --lib store::context 2>&1 | tail -30`
Expected: FAIL — `no method named record_context`.

- [ ] **Step 4: Implement the store**

Insert between the `StoredCluster` definition and `#[cfg(test)]`:

```rust
impl Store {
    /// Record one page view's situation. Returns the row's own id.
    pub async fn record_context(&self, ev: &ContextEvent) -> Result<i64> {
        let res = sqlx::query(
            "INSERT INTO context_events
                 (scope, at, bundle, device_key, local_hour, weekday, tz)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&ev.scope)
        .bind(ev.at)
        .bind(&ev.bundle)
        .bind(&ev.device_key)
        .bind(ev.local_hour)
        .bind(ev.weekday)
        .bind(&ev.tz)
        .execute(&self.pool)
        .await?;
        Ok(res.last_insert_rowid())
    }

    /// Every situation recorded at or after `since`, oldest first.
    ///
    /// Unbounded on purpose, and bounded in practice by `RETAIN_DAYS`: the one
    /// caller is the sweep, which rebuilds every profile from the raw bundles
    /// and therefore needs all of them. Paging it would mean holding a cursor
    /// across a rebuild that is only correct when it sees the whole window.
    pub async fn context_events_since(&self, since: i64) -> Result<Vec<ContextEvent>> {
        let rows = sqlx::query(
            "SELECT id, scope, at, bundle, device_key, local_hour, weekday, tz
               FROM context_events
              WHERE at >= ?
              ORDER BY at, id",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| ContextEvent {
                id: r.get("id"),
                scope: r.get("scope"),
                at: r.get("at"),
                bundle: r.get("bundle"),
                device_key: r.get("device_key"),
                local_hour: r.get("local_hour"),
                weekday: r.get("weekday"),
                tz: r.get("tz"),
            })
            .collect())
    }

    /// Drop situations past keeping. See `RETAIN_DAYS`.
    pub async fn expire_context_events(&self, retain_days: i64) -> Result<u64> {
        let cutoff = now() - retain_days * 86_400;
        Ok(sqlx::query("DELETE FROM context_events WHERE at < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// Replace everything this artifact has learned, in one transaction.
    ///
    /// Wholesale, never merged. The sweep rebuilds a profile from the raw
    /// bundles every run, so a merge would leave a slot from a previous run
    /// standing beside fresh ones — and the multivector written from the fresh
    /// ones would then not match the table the reason is read out of. An empty
    /// list is a clear, which is what an artifact whose every cluster fell
    /// below `min_weight` needs.
    pub async fn replace_context_clusters(
        &self,
        artifact_id: &str,
        clusters: &[StoredCluster],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM context_clusters WHERE artifact_id = ?")
            .bind(artifact_id)
            .execute(&mut *tx)
            .await?;
        for c in clusters {
            sqlx::query(
                "INSERT INTO context_clusters
                     (scope, artifact_id, slot, centroid, weight, last_at,
                      encoder_version, representative)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&c.scope)
            .bind(artifact_id)
            .bind(c.slot)
            .bind(vec_to_blob(&c.centroid))
            .bind(c.weight)
            .bind(c.last_at)
            .bind(c.encoder_version)
            .bind(&c.representative)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// What these artifacts have learned, keyed by artifact, in slot order.
    /// Ids with no clusters are absent from the answer rather than present and
    /// empty — the read path asks for the ten the vector store returned and
    /// needs to know which of them it can say nothing about.
    pub async fn context_clusters_of(
        &self,
        artifact_ids: &[String],
    ) -> Result<HashMap<String, Vec<StoredCluster>>> {
        if artifact_ids.is_empty() {
            return Ok(HashMap::new());
        }
        // Built by hand because sqlx has no list binding for SQLite. The values
        // are bound, never spliced; only the placeholders are generated.
        let marks = std::iter::repeat_n("?", artifact_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT scope, artifact_id, slot, centroid, weight, last_at,
                    encoder_version, representative
               FROM context_clusters
              WHERE artifact_id IN ({marks})
              ORDER BY artifact_id, slot"
        );
        let mut q = sqlx::query(&sql);
        for id in artifact_ids {
            q = q.bind(id);
        }
        let mut out: HashMap<String, Vec<StoredCluster>> = HashMap::new();
        for r in q.fetch_all(&self.pool).await? {
            let artifact_id: String = r.get("artifact_id");
            out.entry(artifact_id.clone())
                .or_default()
                .push(StoredCluster {
                    scope: r.get("scope"),
                    artifact_id,
                    slot: r.get("slot"),
                    centroid: blob_to_vec(&r.get::<Vec<u8>, _>("centroid")),
                    weight: r.get("weight"),
                    last_at: r.get("last_at"),
                    encoder_version: r.get("encoder_version"),
                    representative: r.get("representative"),
                });
        }
        Ok(out)
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib store::context 2>&1 | tail -30`
Expected: PASS, 7 tests.

If `deleting_an_artifact_takes_its_situations_with_it` fails, foreign keys are
off on the test pool — check `Store::memory` for `PRAGMA foreign_keys` and
follow whatever the other cascade tests in the tree do rather than changing the
pragma.

- [ ] **Step 6: Trim the log on its own window**

In `src/jobs/retention.rs`, add a third field to `Report`:

```rust
    /// Situations dropped for being past `store::context::RETAIN_DAYS`.
    pub contexts: u64,
```

and a third pass in `run`, after the gaps block and before the closing comment:

```rust
    // Its own window, and behind no key. §4: a weekly pattern needs weeks, and
    // `feedback.retain_days` defaults to keeping for ever but is an operator
    // switch — an operator who shortens their query log is not asking the base
    // to forget what Friday afternoon looks like. Runs whenever this unit runs,
    // which is why `periodic_units` now also arms it for `recommend.enabled`.
    match core
        .store
        .expire_context_events(crate::store::context::RETAIN_DAYS)
        .await
    {
        Ok(n) => {
            if n > 0 {
                tracing::info!(dropped = n, "expired recorded situations");
            }
            report.contexts = n;
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not expire recorded situations");
            failure.get_or_insert(e);
        }
    }
```

In `src/core/background.rs`, extend the `Retention` gate (`background.rs:122`):

```rust
    if core.feedback.retain_days > 0 || core.feedback.enabled || core.recommends() {
        out.push((Stage::Retention, CONSOLIDATE_TARGET));
    }
```

- [ ] **Step 7: Run the retention tests, then the whole suite**

Run: `cargo test --lib jobs::retention core::background 2>&1 | tail -20`
Expected: PASS.

Run: `cargo fmt && cargo clippy --all-targets 2>&1 | tail -20 && cargo test --lib 2>&1 | tail -10`
Expected: clean, all pass.

- [ ] **Step 8: Commit**

```bash
git add src/store/schema.sql src/store/context.rs src/store/mod.rs \
        src/jobs/retention.rs src/core/background.rs
git commit -m "feat(context): where a situation is written down, and how long it is kept"
```

---

### Task 4: The encoder

A bundle becomes a fixed-length `f32` vector composed of named blocks. **Each
block is normalised to length 1, then scaled by its weight.** That one rule is
what makes the weights real numbers an operator can change instead of an
accident of how many dimensions a block happens to use.

**Files:**
- Modify: `src/core/context.rs` (everything below `local_time`)
- Test: in `src/core/context.rs`

**Interfaces:**
- Consumes: `crate::config::BlockWeights`, `crate::core::context::{LocalTime, local_time}`, `crate::vector::cosine`.
- Produces:
  - `pub const CTX_DIM: usize = 53;`
  - `pub const SCOPE_DIMS: usize = 8;` — the leading block, sliced off when scoring a rung.
  - `pub const ENCODER_VERSION: i64 = 1;`
  - `pub struct Block { pub name: &'static str, pub label: &'static str, pub at: usize, pub dims: usize }`
  - `pub const BLOCKS: [Block; 11]`
  - `pub struct Bundle { … }` — `Default + Clone + Debug + Serialize + Deserialize`, every field optional
  - `pub fn parse_bundle(raw: &str) -> Bundle`
  - `pub fn device_key(b: &Bundle) -> Option<String>`
  - `pub fn encode(at: i64, scope: Option<&str>, b: &Bundle, w: &BlockWeights) -> Vec<f32>`
  - `pub fn contributions(now: &[f32], cluster: &[f32], w: &BlockWeights) -> Vec<(&'static str, f32)>` — descending, `scope` excluded
  - `pub fn context_score(now: &[f32], cluster: &[f32]) -> f32` — cosine over the vector with `scope` sliced off

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` block in `src/core/context.rs` (the one Task 1
created):

```rust
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

    fn slice_of<'a>(v: &'a [f32], name: &str) -> &'a [f32] {
        let b = BLOCKS.iter().find(|b| b.name == name).unwrap();
        &v[b.at..b.at + b.dims]
    }

    fn norm(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    // 2026-08-21T13:52:00Z, a Friday. 15:52 in Berlin.
    const FRIDAY: i64 = 1_787_320_320;

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
        assert_eq!(BLOCKS[0].name, "scope");
        assert_eq!(BLOCKS[0].dims, SCOPE_DIMS);
    }

    #[test]
    fn half_past_eleven_at_night_is_near_half_past_midnight() {
        // The hour is a circle, so the two are an hour apart rather than
        // twenty-three. A one-hot hour would have called them maximally
        // different, which is the whole reason this block is an angle.
        let late = encode(FRIDAY - 2 * 3600 - 22 * 60, None, &phone(), &weights());
        let early = encode(FRIDAY - 3600 - 22 * 60, None, &phone(), &weights());
        let c = crate::vector::cosine(slice_of(&late, "time_of_day"), slice_of(&early, "time_of_day"));
        assert!(c > 0.96, "23:30 against 00:30 scored {c}");
    }

    #[test]
    fn five_past_three_costs_almost_nothing_against_a_three_o_clock_pattern() {
        let at_three = encode(FRIDAY - 52 * 60, None, &phone(), &weights());
        let five_past = encode(FRIDAY - 47 * 60, None, &phone(), &weights());
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
        let v = encode(FRIDAY, Some("alice"), &phone(), &weights());
        let w = weights();
        assert!((norm(slice_of(&v, "weekday")) - w.weekday).abs() < 1e-5);
        assert!((norm(slice_of(&v, "time_of_day")) - w.time_of_day).abs() < 1e-5);
        assert!((norm(slice_of(&v, "scope")) - w.scope).abs() < 1e-5);
    }

    #[test]
    fn a_block_switched_off_contributes_nothing() {
        let mut w = weights();
        w.month_cycle = 0.0;
        let v = encode(FRIDAY, None, &phone(), &w);
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
        let v = encode(FRIDAY, None, &b, &weights());
        assert_eq!(norm(slice_of(&v, "power")), 0.0, "power says nothing");
        assert!(norm(slice_of(&v, "weekday")) > 0.0, "and nothing else changed");
    }

    #[test]
    fn a_block_that_says_nothing_scores_zero_rather_than_opposed() {
        // Two desktops with no battery must not read as *agreeing* about the
        // battery, and must not read as disagreeing either. `cosine` returns
        // 0.0 for a zero vector, which is the answer that means "no opinion".
        let mut b = phone();
        b.battery_level = None;
        b.charging = None;
        let v = encode(FRIDAY, None, &b, &weights());
        let c = contributions(&v, &v, &weights());
        let power = c.iter().find(|(n, _)| *n == "power").unwrap();
        assert_eq!(power.1, 0.0);
    }

    #[test]
    fn an_unidentifiable_device_is_a_state_rather_than_an_absence() {
        // Unlike the battery, "this browser tells us nothing about itself" is
        // itself stable and recurring, so it gets a slot of its own — a
        // hardened browser is a situation, not a gap.
        let bare = Bundle::default();
        assert!(device_key(&bare).is_none());
        let v = encode(FRIDAY, None, &bare, &weights());
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
    fn two_people_in_the_same_situation_are_still_far_apart() {
        // §11: until each person has their own collection, the `scope` block is
        // the only thing keeping one person's situations from being offered to
        // another. It is load-bearing and this is the test that says so.
        let alice = encode(FRIDAY, Some("alice"), &phone(), &weights());
        let bob = encode(FRIDAY, Some("bob"), &phone(), &weights());
        let same = encode(FRIDAY, Some("alice"), &phone(), &weights());
        assert!(
            crate::vector::cosine(&alice, &bob) < crate::vector::cosine(&alice, &same),
            "a foreign scope must never win max_sim"
        );
    }

    #[test]
    fn the_rung_is_scored_without_the_scope_block() {
        // The scope block decides *who* may be offered something. If it counted
        // towards the rung too, it would drag every same-scope score above 0.95
        // and `strong_at` and `weak_at` would have to live four hundredths
        // apart. Two different jobs, two different scales.
        let friday_phone = encode(FRIDAY, Some("alice"), &phone(), &weights());
        let mut desktop = phone();
        desktop.platform = Some("macOS".into());
        desktop.touch = Some(false);
        desktop.network = Some("wired".into());
        desktop.battery_level = None;
        desktop.charging = None;
        // A Monday morning at a desk: nothing about the situation agrees.
        let monday_desk = encode(FRIDAY + 3 * 86_400 - 8 * 3600, Some("alice"), &desktop, &weights());

        assert!(crate::vector::cosine(&friday_phone, &monday_desk) > 0.9, "scope alone");
        assert!(
            context_score(&friday_phone, &monday_desk) < 0.4,
            "and the situation does not agree at all"
        );
    }

    #[test]
    fn the_reason_names_the_blocks_that_decided_it() {
        let a = encode(FRIDAY, Some("alice"), &phone(), &weights());
        let c = contributions(&a, &a, &weights());
        assert!(!c.iter().any(|(n, _)| *n == "scope"), "scope explains nothing");
        assert_eq!(c.len(), BLOCKS.len() - 1);
        for pair in c.windows(2) {
            assert!(pair[0].1 >= pair[1].1, "sorted, so the top three are the top three");
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
        let v = encode(FRIDAY, Some("alice"), &b, &weights());
        assert_eq!(v.len(), CTX_DIM);
        assert!(norm(slice_of(&v, "weekday")) > 0.0);
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib core::context 2>&1 | tail -20`
Expected: FAIL — `cannot find value BLOCKS in this scope`.

- [ ] **Step 3: Write the block table and the bundle**

Add to `src/core/context.rs`, above the `#[cfg(test)]` block:

```rust
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

/// The layout, in order. `scope` leads because `context_score` slices it off by
/// taking everything after it, which only works while it is first.
///
/// Circular where there is a circle, one-hot where there is not. The hour is a
/// circle, so 23:30 and 00:30 are an hour apart. The weekday is *not* a useful
/// circle — "Friday is three from Tuesday" means nothing, and the pattern is
/// exactly Friday — so it is one-hot, with a separate weak weekday/weekend
/// block for the part that genuinely is gradual.
pub const BLOCKS: [Block; 11] = [
    Block { name: "scope",        label: "who",           at: 0,  dims: 8 },
    Block { name: "time_of_day",  label: "hour",          at: 8,  dims: 2 },
    Block { name: "weekday",      label: "weekday",       at: 10, dims: 7 },
    Block { name: "weekend",      label: "weekend",       at: 17, dims: 2 },
    Block { name: "device",       label: "device",        at: 19, dims: 8 },
    Block { name: "viewport",     label: "screen",        at: 27, dims: 4 },
    Block { name: "locale",       label: "language",      at: 31, dims: 8 },
    Block { name: "network",      label: "network",       at: 39, dims: 4 },
    Block { name: "power",        label: "battery",       at: 43, dims: 3 },
    Block { name: "environment",  label: "surroundings",  at: 46, dims: 5 },
    Block { name: "month_cycle",  label: "month",         at: 51, dims: 2 },
];

/// Fixed at collection creation, so a new block invalidates every stored
/// centroid. That is what `ENCODER_VERSION` and §4's whole-bundle storage are
/// for: the sweep rebuilds from the raw bundles rather than losing the history.
pub const CTX_DIM: usize = 53;

/// The width of the leading `scope` block. See `context_score`.
pub const SCOPE_DIMS: usize = 8;

/// Bumped whenever `BLOCKS` changes in any way — a width, an order, or what a
/// slot means. A stored cluster carrying a different one is skipped by the read
/// path and rebuilt by the next sweep; explaining a hit with the wrong block is
/// worse than explaining nothing.
pub const ENCODER_VERSION: i64 = 1;

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
```

- [ ] **Step 4: Write `encode`**

Append, still above `#[cfg(test)]`:

```rust
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
pub fn encode(
    at: i64,
    scope: Option<&str>,
    b: &Bundle,
    w: &crate::config::BlockWeights,
) -> Vec<f32> {
    let t = local_time(at, b.tz.as_deref(), b.tz_offset_mins);
    let mut v = vec![0.0f32; CTX_DIM];

    for block in BLOCKS {
        let slot = &mut v[block.at..block.at + block.dims];
        fill(block.name, slot, &t, scope, b);
        scale(slot, w.of(block.name));
    }
    v
}

/// Raw values into one block's slots. Every arm either fills or leaves zeros;
/// leaving zeros is how "the browser did not say" is expressed.
fn fill(name: &str, s: &mut [f32], t: &LocalTime, scope: Option<&str>, b: &Bundle) {
    use std::f32::consts::TAU;
    match name {
        "scope" => {
            // Hashed into buckets rather than looked up in a registry: there is
            // no list of every subject that has ever searched, and one bucket
            // shared by two people is a collision the per-user collections in
            // §12 remove rather than a bug to work around here.
            if let Some(sc) = scope {
                s[bucket(sc, s.len())] = 1.0;
            }
        }
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
    let n = s.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n == 0.0 || weight == 0.0 {
        if weight == 0.0 {
            s.fill(0.0);
        }
        return;
    }
    let k = weight / n;
    for x in s.iter_mut() {
        *x *= k;
    }
}
```

- [ ] **Step 5: Write `contributions` and `context_score`**

```rust
/// How well the situation matches, ignoring who is asking.
///
/// The `scope` block decides *which* artifact may be offered — it is what keeps
/// a foreign cluster from ever winning `max_sim`, at weight 10 against a total
/// of under 5 — and that same dominance would drag every same-scope full cosine
/// above 0.95, leaving `strong_at` and `weak_at` four hundredths apart. So the
/// choice is made on the full vector and the rung is decided here. Two
/// questions, two scales.
pub fn context_score(now: &[f32], cluster: &[f32]) -> f32 {
    if now.len() < SCOPE_DIMS || cluster.len() < SCOPE_DIMS {
        return 0.0;
    }
    crate::vector::cosine(&now[SCOPE_DIMS..], &cluster[SCOPE_DIMS..])
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
///
/// `scope` is excluded. It is always either a perfect match or a rejection, so
/// it would lead every list while explaining nothing.
pub fn contributions(
    now: &[f32],
    cluster: &[f32],
    w: &crate::config::BlockWeights,
) -> Vec<(&'static str, f32)> {
    let mut out: Vec<(&'static str, f32)> = BLOCKS
        .iter()
        .filter(|b| b.name != "scope")
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
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib core::context 2>&1 | tail -30`
Expected: PASS, 18 tests.

If `the_rung_is_scored_without_the_scope_block` fails on its second assertion,
print `context_score` for the two vectors and check the `viewport` block — a raw
(uncentred) log there is the most likely cause of a score that will not fall.

- [ ] **Step 7: Commit**

```bash
git add src/core/context.rs
git commit -m "feat(context): a situation as a vector, each block worth exactly what it is named"
```

---

### Task 5: The trait, and `max_sim` in the in-memory store

Without an in-memory implementation no test of this feature runs without a live
Qdrant, which would mean the recommendation path is never tested at all.

**Files:**
- Modify: `src/vector/mod.rs` (two trait methods)
- Modify: `src/vector/memory.rs` (both, plus a second map)
- Test: in `src/vector/memory.rs`

**Interfaces:**
- Consumes: `crate::vector::cosine`, `SearchFilter`, `SearchHit`.
- Produces, on `VectorStore`:
  - `async fn set_context_vectors(&self, artifact_id: &str, vectors: Vec<Vec<f32>>) -> Result<()>` — an empty `vectors` removes the set.
  - `async fn context_query(&self, vector: &[f32], limit: usize, filter: &SearchFilter) -> Result<Vec<SearchHit>>` — `SearchHit::score` is the `max_sim`, `similarity` is `None`.

- [ ] **Step 1: Add the trait methods**

In `src/vector/mod.rs`, inside `pub trait VectorStore`, after `neighbours`
(`:320`):

```rust
    /// Replace this artifact's set of context centroids — the `ctx`
    /// multivector, scored with `max_sim`.
    ///
    /// A **vector** write, never a point write. `upsert` replaces the whole
    /// payload, and a writer that does not know when the artifact was last
    /// shown would clear `last_seen_at` and `status` with it — which puts every
    /// artifact the sweep hid straight back into search. See `qdrant.rs`'s
    /// `upsert` for the same hazard stated where it bites.
    ///
    /// An empty `vectors` removes the set. That is the ordinary case for an
    /// artifact whose every cluster has decayed below `min_weight`, not an
    /// error, and it must leave the point and its dense vector alone.
    ///
    /// An artifact with no point is not a failure: its embedding may never have
    /// run. There is nothing to attach a set to, and nothing to complain about.
    async fn set_context_vectors(&self, artifact_id: &str, vectors: Vec<Vec<f32>>) -> Result<()>;
    /// The artifacts whose learned situations most resemble this one.
    ///
    /// `max_sim` over each artifact's set: an artifact matches if *any* of its
    /// situations does, which is the whole reason the profile is a set rather
    /// than a mean. A thing looked up on Friday afternoons and occasionally on
    /// Monday mornings must match both, and their average is a situation that
    /// never happened.
    ///
    /// Points carrying no set are absent from the answer, so the candidates are
    /// "anything ever opened" without a filter saying so.
    ///
    /// `score` is the `max_sim`. `similarity` is `None`: this is not a query
    /// vector against a document vector, and calling it a similarity would
    /// invite it into a ranking it has no business in.
    async fn context_query(
        &self,
        vector: &[f32],
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>>;
```

- [ ] **Step 2: Write the failing tests**

Append to the `mod tests` block in `src/vector/memory.rs` (create one if the
file has none, following the pattern of another `src/vector/` test module):

```rust
    fn point(id: &str) -> VectorPoint {
        VectorPoint {
            vector: vec![1.0; 4],
            sparse: Default::default(),
            payload: VectorPayload {
                artifact_id: id.into(),
                corpus_id: "c".into(),
                text: "t".into(),
                title: None,
                category: None,
                tags: vec![],
                created_at: 0,
                last_seen_at: None,
                hit_count: None,
                status: None,
                last_verified_at: None,
                superseded_by: None,
                origin_corpora: vec![],
                provenance: None,
            },
        }
    }

    #[tokio::test]
    async fn an_artifact_matches_on_its_nearest_situation_not_its_average() {
        // Friday afternoon *and* Monday morning. Their mean is a situation that
        // never happened, and a store that scored the mean would answer neither.
        let v = MemoryVectors::new();
        v.upsert(vec![point("a")]).await.unwrap();
        v.set_context_vectors("a", vec![vec![1.0, 0.0], vec![0.0, 1.0]])
            .await
            .unwrap();

        let friday = v.context_query(&[1.0, 0.0], 5, &wide()).await.unwrap();
        assert_eq!(friday.len(), 1);
        assert!((friday[0].score - 1.0).abs() < 1e-5);

        let monday = v.context_query(&[0.0, 1.0], 5, &wide()).await.unwrap();
        assert!((monday[0].score - 1.0).abs() < 1e-5);
    }

    #[tokio::test]
    async fn an_artifact_with_no_situations_is_not_a_candidate() {
        // The candidate set is "anything ever opened", and that is expressed by
        // the absence of a set rather than by a filter.
        let v = MemoryVectors::new();
        v.upsert(vec![point("a"), point("b")]).await.unwrap();
        v.set_context_vectors("a", vec![vec![1.0, 0.0]]).await.unwrap();

        let out = v.context_query(&[1.0, 0.0], 5, &wide()).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].payload.artifact_id, "a");
    }

    #[tokio::test]
    async fn an_empty_write_removes_the_set_and_leaves_the_point() {
        let v = MemoryVectors::new();
        v.upsert(vec![point("a")]).await.unwrap();
        v.set_context_vectors("a", vec![vec![1.0, 0.0]]).await.unwrap();
        v.set_context_vectors("a", vec![]).await.unwrap();

        assert!(v.context_query(&[1.0, 0.0], 5, &wide()).await.unwrap().is_empty());
        assert_eq!(v.count().await.unwrap(), 1, "the point is still there");
    }

    #[tokio::test]
    async fn a_hidden_artifact_is_never_offered() {
        // The same rule search obeys: superseded and deprecated are out.
        let v = MemoryVectors::new();
        v.upsert(vec![point("a")]).await.unwrap();
        v.set_context_vectors("a", vec![vec![1.0, 0.0]]).await.unwrap();
        v.set_lifecycle("a", ArtifactStatus::Superseded, Some("b"))
            .await
            .unwrap();

        let out = v
            .context_query(&[1.0, 0.0], 5, &SearchFilter::default())
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn results_come_back_best_first_and_capped() {
        let v = MemoryVectors::new();
        v.upsert(vec![point("near"), point("far")]).await.unwrap();
        v.set_context_vectors("near", vec![vec![1.0, 0.0]]).await.unwrap();
        v.set_context_vectors("far", vec![vec![0.2, 1.0]]).await.unwrap();

        let out = v.context_query(&[1.0, 0.0], 1, &wide()).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].payload.artifact_id, "near");
    }

    #[tokio::test]
    async fn a_set_on_an_artifact_with_no_point_is_not_an_error() {
        // An artifact whose embedding never ran has nothing to attach a set to.
        // The sweep must not fail over one.
        let v = MemoryVectors::new();
        v.set_context_vectors("nobody", vec![vec![1.0, 0.0]])
            .await
            .unwrap();
        assert!(v.context_query(&[1.0, 0.0], 5, &wide()).await.unwrap().is_empty());
    }

    /// Superseded and deprecated included, so a test asserting the ordinary
    /// path is not silently asserting the filter instead.
    fn wide() -> SearchFilter {
        SearchFilter {
            include_superseded: true,
            include_deprecated: true,
            ..Default::default()
        }
    }
```

- [ ] **Step 3: Run them to verify they fail**

Run: `cargo test --lib vector::memory 2>&1 | tail -20`
Expected: FAIL — `not all trait items implemented: set_context_vectors, context_query`.

- [ ] **Step 4: Implement both in `memory.rs`**

Add the second map to the struct:

```rust
pub struct MemoryVectors {
    points: RwLock<HashMap<String, VectorPoint>>,
    /// The `ctx` multivector per artifact, held apart from the point rather
    /// than on it. A field on `VectorPoint` would put a context set in every
    /// signature that carries a point — the embed job's, the reindex's — for
    /// the sake of one caller that writes it and one that reads it.
    ctx: RwLock<HashMap<String, Vec<Vec<f32>>>>,
}
```

and to `new()`:

```rust
            ctx: RwLock::new(HashMap::new()),
```

Add the two methods to the `impl VectorStore for MemoryVectors` block, after
`neighbours`:

```rust
    async fn set_context_vectors(&self, artifact_id: &str, vectors: Vec<Vec<f32>>) -> Result<()> {
        let mut w = self.ctx.write().unwrap();
        if vectors.is_empty() {
            w.remove(artifact_id);
        } else {
            w.insert(artifact_id.to_string(), vectors);
        }
        Ok(())
    }

    async fn context_query(
        &self,
        vector: &[f32],
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        let points = self.points.read().unwrap();
        let ctx = self.ctx.read().unwrap();
        let mut hits: Vec<SearchHit> = ctx
            .iter()
            // An artifact with a set but no point cannot be offered: there is
            // no payload to render it from. That is the sweep having run
            // against an artifact whose embedding has not, not an error.
            .filter_map(|(id, set)| points.get(id).map(|p| (p, set)))
            .filter(|(p, _)| {
                let status = status_of(&p.payload);
                (filter.include_superseded || status != ArtifactStatus::Superseded)
                    && (filter.include_deprecated || status != ArtifactStatus::Deprecated)
            })
            .map(|(p, set)| SearchHit {
                payload: p.payload.clone(),
                // `max_sim`: the best of the artifact's situations, which is
                // what makes a set worth more than a mean.
                score: set
                    .iter()
                    .map(|c| cosine(vector, c))
                    .fold(f32::NEG_INFINITY, f32::max),
                // Not a query-to-document similarity, and calling it one would
                // invite it into a ranking it has no business in.
                similarity: None,
            })
            .collect();
        // Ties break on the id, so a HashMap's iteration order never decides
        // what is offered.
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.payload.artifact_id.cmp(&b.payload.artifact_id))
        });
        hits.truncate(limit);
        Ok(hits)
    }
```

Check `delete_artifacts` and `delete_by_corpus` in the same file and drop the
`ctx` entries there too — a set outliving its point would keep answering
`context_query` with a payload that no longer exists.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib vector::memory 2>&1 | tail -20`
Expected: PASS, 6 new tests.

- [ ] **Step 6: Commit**

```bash
git add src/vector/mod.rs src/vector/memory.rs
git commit -m "feat(vector): a set of situations per artifact, and max_sim over it"
```

---

### Task 6: Qdrant — one named vector, written without touching the payload

**Files:**
- Modify: `src/vector/qdrant.rs` (`CTX` const, `collection_body`, `ctx_of`, `reindex`, two trait methods)
- Test: in `src/vector/qdrant.rs`'s existing `mod tests` (unit tests over the
  request bodies — the live-Qdrant path is not exercised here, matching how
  `collection_body` is already tested at `:2144`)

**Interfaces:**
- Consumes: `crate::core::context::CTX_DIM`, the trait from Task 5.
- Produces: `pub const CTX: &str = "ctx";`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/vector/qdrant.rs`:

```rust
    #[test]
    fn the_collection_carries_a_context_multivector() {
        let body = collection_body(768);
        assert_eq!(body["vectors"][CTX]["size"], crate::core::context::CTX_DIM);
        assert_eq!(body["vectors"][CTX]["distance"], "Cosine");
        assert_eq!(body["vectors"][CTX]["multivector_config"]["comparator"], "max_sim");
        // No HNSW, an exact scan. Right here rather than thrifty: the
        // candidates are only artifacts ever opened — hundreds to a few
        // thousand at 53 dimensions — and an index would be rebuilt on every
        // sweep write to beat a scan it cannot beat at that size.
        assert_eq!(body["vectors"][CTX]["hnsw_config"]["m"], 0);
        // And nothing else moved.
        assert_eq!(body["vectors"][DENSE]["size"], 768);
        assert_eq!(body["sparse_vectors"][SPARSE]["modifier"], "idf");
    }

    #[test]
    fn a_context_set_is_copied_by_a_rebuild_when_it_still_fits() {
        let dim = crate::core::context::CTX_DIM;
        let stored = json!({ DENSE: [1.0, 2.0], CTX: [vec![0.5; dim], vec![0.25; dim]] });
        let copied = ctx_of(&stored).unwrap();
        assert_eq!(copied.len(), 2);
    }

    #[test]
    fn a_context_set_from_an_older_layout_is_dropped_rather_than_copied() {
        // A changed encoder layout changes CTX_DIM. The old sets are discarded
        // and the next sweep rebuilds them from the raw bundles in
        // `context_events` — which costs no embedding call either way.
        let stored = json!({ DENSE: [1.0, 2.0], CTX: [[0.5, 0.5, 0.5]] });
        assert!(ctx_of(&stored).is_none());
    }

    #[test]
    fn a_point_with_no_context_set_reindexes_without_one() {
        assert!(ctx_of(&json!({ DENSE: [1.0, 2.0] })).is_none());
        assert!(ctx_of(&json!([1.0, 2.0])).is_none());
    }

    #[test]
    fn the_offer_excludes_hidden_artifacts_by_must_not() {
        // `must_not` rather than a positive match, for the reason `build_filter`
        // gives: a point carrying no `status` key at all reads as active, and a
        // positive clause would drop every hand-written one.
        let f = build_filter(&SearchFilter::default()).unwrap();
        assert!(f.get("must").is_none());
        let not = f["must_not"].as_array().unwrap();
        assert_eq!(not.len(), 2);
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib vector::qdrant 2>&1 | tail -20`
Expected: FAIL — `cannot find value CTX in this scope`.

- [ ] **Step 3: Add the constant and the collection entry**

Beside `DENSE` and `SPARSE` (`qdrant.rs:36-40`):

```rust
/// The context multivector: one element per learned situation, scored with
/// `max_sim`. Not the multivector the roadmap cut — that was ColBERT-style
/// late-interaction reranking, one reduced-width vector per *token* and
/// thousands per artifact. This is two to five per artifact.
pub const CTX: &str = "ctx";
```

Replace `collection_body` (`:209-215`):

```rust
/// The schema every generation is created with.
fn collection_body(dim: usize) -> Value {
    json!({
        "vectors": {
            DENSE: { "size": dim, "distance": "Cosine" },
            CTX: {
                "size": crate::core::context::CTX_DIM,
                "distance": "Cosine",
                "multivector_config": { "comparator": "max_sim" },
                // No index, an exact scan. The candidates are only artifacts
                // ever opened, at 53 dimensions, and an HNSW graph would be
                // rebuilt on every sweep write to beat a scan it cannot beat at
                // that size.
                "hnsw_config": { "m": 0 },
            },
        },
        "sparse_vectors": { SPARSE: { "modifier": "idf" } },
    })
}
```

**A collection created before this change has no `ctx` named vector, and
writing one to it will fail.** `--reindex` is the migration: it creates a fresh
generation from this body. Say so in the commit message.

- [ ] **Step 4: Add `ctx_of` and carry sets through a rebuild**

Beside `dense_of` (`:308-315`):

```rust
/// A stored point's context set, when it is still readable under the running
/// encoder.
///
/// `None` covers three cases that a rebuild treats alike: no set at all, a
/// pre-alias point with a single unnamed vector, and a set written under an
/// older layout. The last is why the width is checked rather than trusted — a
/// changed `BLOCKS` changes `CTX_DIM`, and copying the old numbers into the new
/// space would give every artifact a profile assembled from the wrong blocks.
/// Discarded, and the next sweep rebuilds from the raw bundles.
fn ctx_of(vector: &Value) -> Option<Vec<Vec<f32>>> {
    let set = vector.as_object()?.get(CTX)?.as_array()?;
    if set.is_empty() {
        return None;
    }
    let out: Option<Vec<Vec<f32>>> = set
        .iter()
        .map(|e| {
            let row: Vec<f32> = e
                .as_array()?
                .iter()
                .map(|x| x.as_f64().map(|f| f as f32))
                .collect::<Option<_>>()?;
            (row.len() == crate::core::context::CTX_DIM).then_some(row)
        })
        .collect();
    out
}
```

In `reindex`'s per-point loop (`:850-857`), after the sparse line:

```rust
                let mut vector = json!({ DENSE: dense });
                if let Some(sp) = sparse_body(&sparse_of_payload(&p.payload)) {
                    vector[SPARSE] = sp;
                }
                // Copied when the dimension matches, skipped when it does not.
                // No embedding call in either case — a context vector is
                // assembled from a bundle, and the bundles are all still in
                // `context_events`.
                if let Some(set) = ctx_of(&p.vector) {
                    vector[CTX] = json!(set);
                }
```

- [ ] **Step 5: Implement the two trait methods**

In `impl VectorStore for QdrantVectors`, after `neighbours`:

```rust
    async fn set_context_vectors(&self, artifact_id: &str, vectors: Vec<Vec<f32>>) -> Result<()> {
        let id = point_uuid(artifact_id);
        // `points/vectors`, never `upsert`. A point write replaces the entire
        // payload — see the comment on `upsert` — and clearing `status` puts
        // every artifact the sweep hid straight back into search, on every
        // sweep run. This endpoint does not touch payload at all.
        let (path, body) = match vectors.is_empty() {
            true => (
                format!("/collections/{}/points/vectors/delete?wait=true", self.alias),
                json!({ "points": [id], "vector": [CTX] }),
            ),
            false => (
                format!("/collections/{}/points/vectors?wait=true", self.alias),
                json!({ "points": [{ "id": id, "vector": { CTX: vectors } }] }),
            ),
        };
        // Absent is not a failure, for the same reason `set_lifecycle` gives:
        // an artifact whose embedding never ran has no point, and a sweep that
        // errored on one would take the whole run down over an artifact that
        // has nothing to attach a set to.
        let _: Option<Value> = self
            .call_absent_point_as_none(Method::POST, &path, Some(body))
            .await?;
        Ok(())
    }

    async fn context_query(
        &self,
        vector: &[f32],
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        let mut body = json!({
            "query": vector,
            "using": CTX,
            "limit": limit,
            "with_payload": true,
        });
        if let Some(f) = build_filter(filter) {
            body["filter"] = f;
        }
        // No recency stage and no pinning over this. Those exist to reorder a
        // ranked list of answers to a question; there is no question here, and
        // an artifact captured today is not a better answer to "it is Friday
        // afternoon" than one captured last year.
        let res: QueryResult = self
            .call(
                Method::POST,
                &format!("/collections/{}/points/query", self.alias),
                Some(body),
            )
            .await?;
        // `hits_of` already skips points whose payload is not one of ours, and
        // sets `similarity` from nothing — which is correct here: `max_sim` is
        // not a query-to-document similarity.
        Ok(hits_of(res))
    }
```

Check `hits_of`'s signature and whether it sets `similarity: None`; if it sets
it from a map, call it the way `resurface` or `neighbours` does rather than
inventing a third path.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib vector::qdrant 2>&1 | tail -20`
Expected: PASS.

Run: `cargo clippy --all-targets 2>&1 | tail -20`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add src/vector/qdrant.rs
git commit -m "feat(vector): the ctx multivector, written through points/vectors so the payload survives

A collection created before this carries no `ctx` named vector. `--reindex` is
the migration: it creates a fresh generation from the new collection body and
copies every dense vector across, at no embedding cost."
```

---

### Task 7: The sweep

The learning half. One pass, no randomness, no model call: it reads the log,
agglomerates, decays, and writes centroids.

**Files:**
- Create: `src/jobs/context.rs`
- Modify: `src/jobs/mod.rs` (module declaration, dispatch, `run_accounted`)
- Modify: `src/store/jobs.rs` (`Stage::Context`)
- Modify: `src/store/context.rs` (one more read)
- Modify: `src/core/background.rs` (`periodic_units`, `periodic_period`)
- Test: in `src/jobs/context.rs`

**Interfaces:**
- Consumes: `Store::{context_events_since, replace_context_clusters, expire_context_events}`, `Store::interactions_between`, `Store::events_between`, `core::context::{encode, parse_bundle, device_key, local_time, ENCODER_VERSION, CTX_DIM}`, `VectorStore::set_context_vectors`, `Core::{clock, recommend}`.
- Produces:
  - `pub const INTERVAL_HOURS: u64 = 6;`
  - `pub const BRIDGE_SECS: i64 = 900;`
  - `pub const MATCH_SECS: i64 = 1800;`
  - `pub const MAX_SLOTS: usize = 16;`
  - `pub struct Member { pub vec: Vec<f32>, pub weight: f64, pub at: i64, pub bundle: String }`
  - `pub struct Cluster { pub centroid: Vec<f32>, pub weight: f64, pub last_at: i64, pub representative: String }`
  - `pub fn agglomerate(members: &[Member], merge_at: f32, max_clusters: usize, min_weight: f64) -> Vec<Cluster>`
  - `pub struct Report { pub events: usize, pub profiled: usize, pub clusters: usize, pub cleared: usize }` — `Serialize`
  - `pub async fn run(core: &Core) -> Result<Report>`
  - `Stage::Context`, `"context"`, class 1 (background)
  - `Store::artifacts_with_context_clusters(&self) -> Result<Vec<String>>`

- [ ] **Step 1: Add the stage**

In `src/store/jobs.rs`, add `Stage::Context` in four places, each beside
`Stage::Retention`: the `ALL` list (`:88`), `as_str` → `"context"` (`:108`),
the class match's background arm (`:141`), and `parse` → `"context" => Some(Stage::Context)`.

- [ ] **Step 2: Add the one missing store read**

Append to `impl Store` in `src/store/context.rs`:

```rust
    /// Every artifact that currently has a profile.
    ///
    /// What the sweep needs in order to *clear* one: an artifact whose every
    /// situation has decayed below `min_weight` produces no clusters this run,
    /// so nothing in the run's own output names it, and without this its old
    /// centroids would stand for ever — offering it on a pattern that stopped
    /// months ago.
    pub async fn artifacts_with_context_clusters(&self) -> Result<Vec<String>> {
        Ok(
            sqlx::query_scalar("SELECT DISTINCT artifact_id FROM context_clusters")
                .fetch_all(&self.pool)
                .await?,
        )
    }
```

- [ ] **Step 3: Write the failing tests for the clustering**

Create `src/jobs/context.rs` with the module doc, the constants, the types, and
this test module. Implementation comes in Step 5.

```rust
//! What situations recur for which artifact.
//!
//! One pass over the log, no randomness, no model call. It joins three sources
//! on `scope` and `at` — never on a stored id — agglomerates the situations an
//! artifact was opened in, decays them, and writes the surviving centroids to
//! the vector store as that artifact's `ctx` set.
//!
//! A full rebuild every run, from the raw bundles. That is what makes a change
//! to the encoder a sweep rather than a migration, and it is why the sweep
//! never reads what a previous run concluded.

use crate::core::Core;
use crate::error::Result;
use crate::vector::cosine;

/// How often this runs.
///
/// A constant rather than a setting, for the reason `REPAIR_INTERVAL_HOURS`
/// gives: how often a faculty learns is not a preference, and §9 fixes the
/// `[recommend]` keys as one gate and a table of weights. Six hours means a
/// situation recorded this morning is offered this evening, which is as fast as
/// anything here needs to be — the patterns being learned are weekly.
pub const INTERVAL_HOURS: u64 = 6;

/// How long after a search an open still counts as that search's.
///
/// The bridge: where an open followed a search, the event inherits the search's
/// identity, so a recurring search resolves to the artifact it led to rather
/// than to a rerun of the query. Fifteen minutes, which is the same order as
/// `pursuit.idle_secs` and deliberately not that key — pursuits may be off, and
/// this must still work.
pub const BRIDGE_SECS: i64 = 900;

/// How far from an open a recorded situation may sit and still be that open's.
///
/// Half an hour either way. A page view records a situation; the opens that
/// follow it belong to it. Wider than `BRIDGE_SECS` because a person can read
/// for a while after arriving, and a situation is a slower thing than a query.
pub const MATCH_SECS: i64 = 1800;

/// Centroids one artifact may carry, across every scope.
///
/// `max_clusters` bounds the situations per person per artifact; this bounds
/// the array itself, which is shared by every scope that ever opened the
/// artifact. On a base with one person it never binds. On one with many it is
/// what keeps a popular artifact's multivector from growing with the user
/// count — the heaviest survive, which is the same rule `min_weight` applies
/// one level down.
pub const MAX_SLOTS: usize = 16;

/// One situation an artifact was opened in, ready to cluster.
#[derive(Debug, Clone)]
pub struct Member {
    pub vec: Vec<f32>,
    /// Already decayed, and already multiplied by `self_weight` where this was
    /// an open of something this feature offered.
    pub weight: f64,
    pub at: i64,
    /// The raw bundle, carried so the winner can be quoted.
    pub bundle: String,
}

/// One learned situation.
#[derive(Debug, Clone)]
pub struct Cluster {
    pub centroid: Vec<f32>,
    pub weight: f64,
    pub last_at: i64,
    /// `{"at": <unix>, "bundle": {…}}` for the member nearest the centroid.
    pub representative: String,
}

/// What one run did.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Report {
    /// Opens that found a situation to be encoded from.
    pub events: usize,
    /// Artifacts that came out of this run with at least one situation.
    pub profiled: usize,
    pub clusters: usize,
    /// Artifacts whose every situation had decayed away.
    pub cleared: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(v: Vec<f32>, weight: f64, at: i64) -> Member {
        Member { vec: v, weight, at, bundle: format!(r#"{{"n":{at}}}"#) }
    }

    #[test]
    fn six_fridays_and_one_monday_leave_one_situation() {
        // §10.2, and the shape the whole feature is for. The outlier is real
        // and recorded; it is simply not yet a pattern, and `min_weight` is
        // what says so. Without it, one accident is a habit.
        let mut members: Vec<Member> = (0..6)
            .map(|i| member(vec![1.0, 0.02 * i as f32], 1.0, 1_000 + i))
            .collect();
        members.push(member(vec![0.0, 1.0], 1.0, 2_000));

        let out = agglomerate(&members, 0.82, 5, 2.0);
        assert_eq!(out.len(), 1, "the Monday is below the threshold");
        assert!((out[0].weight - 6.0).abs() < 1e-6);
        assert_eq!(out[0].last_at, 1_005, "the most recent member's stamp");
    }

    #[test]
    fn two_real_situations_are_two_clusters_not_their_mean() {
        // The recycling centre looked up on Friday afternoons *and*
        // occasionally on Monday mornings. A mean of them is a situation that
        // never happened, and it would match neither.
        let mut members: Vec<Member> = (0..4).map(|i| member(vec![1.0, 0.0], 1.0, 1_000 + i)).collect();
        members.extend((0..4).map(|i| member(vec![0.0, 1.0], 1.0, 2_000 + i)));

        let out = agglomerate(&members, 0.82, 5, 2.0);
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|c| c.centroid[0] > 0.9));
        assert!(out.iter().any(|c| c.centroid[1] > 0.9));
    }

    #[test]
    fn the_same_input_clusters_the_same_way_twice() {
        // One pass and no randomness, because otherwise it is not testable —
        // and because a recommendation that changed its reason between two
        // sweeps over identical data would be unaccountable.
        let members: Vec<Member> = (0..12)
            .map(|i| member(vec![(i % 3) as f32, ((i + 1) % 3) as f32, 1.0], 1.0, 1_000 + i))
            .collect();
        let a = agglomerate(&members, 0.82, 4, 0.0);
        let b = agglomerate(&members, 0.82, 4, 0.0);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.centroid, y.centroid);
            assert_eq!(x.representative, y.representative);
        }
    }

    #[test]
    fn the_count_never_exceeds_the_cap() {
        let members: Vec<Member> = (0..20)
            .map(|i| {
                let mut v = vec![0.0; 20];
                v[i] = 1.0;
                member(v, 1.0, 1_000 + i as i64)
            })
            .collect();
        let out = agglomerate(&members, 0.99, 5, 0.0);
        assert!(out.len() <= 5, "got {}", out.len());
    }

    #[test]
    fn a_cluster_quotes_the_member_nearest_its_centre() {
        // What the display shows is a real event that happened, not a
        // reconstruction of the average of several.
        let members = vec![
            member(vec![1.0, 0.0], 1.0, 1_000),
            member(vec![0.98, 0.2], 1.0, 1_001),
            member(vec![0.99, 0.1], 1.0, 1_002),
        ];
        let out = agglomerate(&members, 0.5, 5, 0.0);
        assert_eq!(out.len(), 1);
        let rep: serde_json::Value = serde_json::from_str(&out[0].representative).unwrap();
        assert!(rep["at"].is_i64(), "the stamp travels with the bundle");
        assert!(rep["bundle"].is_object());
    }

    #[test]
    fn heavier_members_pull_the_centroid_further() {
        let members = vec![
            member(vec![1.0, 0.0], 1.0, 1_000),
            member(vec![0.9, 0.44], 9.0, 1_001),
        ];
        let out = agglomerate(&members, 0.5, 5, 0.0);
        assert_eq!(out.len(), 1);
        assert!(out[0].centroid[1] > 0.3, "the heavy one dominates");
    }

    #[test]
    fn nothing_in_means_nothing_out() {
        assert!(agglomerate(&[], 0.82, 5, 2.0).is_empty());
    }
}
```

- [ ] **Step 4: Run them to verify they fail**

Run: `cargo test --lib jobs::context 2>&1 | tail -20`
Expected: FAIL — `cannot find function agglomerate`.

- [ ] **Step 5: Implement `agglomerate`**

Insert above `#[cfg(test)]`:

```rust
/// One deterministic pass, in event order.
///
/// An event joins the nearest cluster when cosine exceeds `merge_at`, otherwise
/// it opens its own; when the count exceeds `max_clusters` the two nearest are
/// merged. One pass and no randomness, because otherwise it is not testable —
/// and because a recommendation whose reason changed between two sweeps over
/// identical data could not be accounted for by anyone.
///
/// Members arrive already decayed. A cluster whose total weight falls below
/// `min_weight` is dropped, which is what protects against the single accident:
/// one event never reaches the threshold.
pub fn agglomerate(
    members: &[Member],
    merge_at: f32,
    max_clusters: usize,
    min_weight: f64,
) -> Vec<Cluster> {
    struct Building {
        centroid: Vec<f32>,
        weight: f64,
        last_at: i64,
        /// Indices into `members`, so the representative can be chosen against
        /// the *final* centroid rather than against whatever it was when each
        /// member arrived.
        members: Vec<usize>,
    }

    let cap = max_clusters.max(1);
    let mut built: Vec<Building> = Vec::new();

    for (i, m) in members.iter().enumerate() {
        if m.weight <= 0.0 {
            continue;
        }
        let nearest = built
            .iter()
            .enumerate()
            .map(|(k, c)| (k, cosine(&m.vec, &c.centroid)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        match nearest {
            Some((k, sim)) if sim > merge_at => {
                let c = &mut built[k];
                blend(&mut c.centroid, c.weight, &m.vec, m.weight);
                c.weight += m.weight;
                c.last_at = c.last_at.max(m.at);
                c.members.push(i);
            }
            _ => built.push(Building {
                centroid: m.vec.clone(),
                weight: m.weight,
                last_at: m.at,
                members: vec![i],
            }),
        }

        while built.len() > cap {
            let Some((a, b)) = closest_pair(&built.iter().map(|c| &c.centroid[..]).collect::<Vec<_>>())
            else {
                break;
            };
            let victim = built.remove(b);
            let host = &mut built[a];
            blend(&mut host.centroid, host.weight, &victim.centroid, victim.weight);
            host.weight += victim.weight;
            host.last_at = host.last_at.max(victim.last_at);
            host.members.extend(victim.members);
        }
    }

    let mut out: Vec<Cluster> = built
        .into_iter()
        .filter(|c| c.weight >= min_weight)
        .map(|c| {
            // The representative is the member nearest the *finished* centroid.
            // Ties break on the index, so which of two equally central events
            // is quoted does not depend on a float comparison going either way.
            let rep = c
                .members
                .iter()
                .map(|&i| (i, cosine(&members[i].vec, &c.centroid)))
                .max_by(|x, y| {
                    x.1.partial_cmp(&y.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| y.0.cmp(&x.0))
                })
                .map(|(i, _)| i);
            let representative = rep
                .map(|i| {
                    // The bundle plus its stamp, because a bundle carries no
                    // time of its own and the line says "like 08.08., 15:04".
                    // Stored as a string that is always valid JSON: a bundle
                    // that will not re-parse becomes an empty object rather
                    // than corrupting the row.
                    let bundle: serde_json::Value = serde_json::from_str(&members[i].bundle)
                        .unwrap_or_else(|_| serde_json::json!({}));
                    serde_json::json!({ "at": members[i].at, "bundle": bundle }).to_string()
                })
                .unwrap_or_else(|| r#"{"at":0,"bundle":{}}"#.to_string());
            Cluster {
                centroid: c.centroid,
                weight: c.weight,
                last_at: c.last_at,
                representative,
            }
        })
        .collect();
    // Heaviest first: this is the order slots are allocated in, and `MAX_SLOTS`
    // keeps the front of it.
    out.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.last_at.cmp(&a.last_at))
    });
    out
}

/// Weighted mean, in place.
fn blend(centroid: &mut [f32], have: f64, add: &[f32], w: f64) {
    let total = have + w;
    if total <= 0.0 {
        return;
    }
    for (i, c) in centroid.iter_mut().enumerate() {
        let other = add.get(i).copied().unwrap_or(0.0);
        *c = ((*c as f64 * have + other as f64 * w) / total) as f32;
    }
}

/// The two nearest centroids, as `(keep, drop)` with `keep < drop`.
///
/// Quadratic, over at most `max_clusters + 1` vectors of 53 dimensions. That is
/// a few hundred multiplications on a path that runs every six hours.
fn closest_pair(centroids: &[&[f32]]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize, f32)> = None;
    for a in 0..centroids.len() {
        for b in (a + 1)..centroids.len() {
            let sim = cosine(centroids[a], centroids[b]);
            if best.is_none_or(|(_, _, s)| sim > s) {
                best = Some((a, b, sim));
            }
        }
    }
    best.map(|(a, b, _)| (a, b))
}
```

- [ ] **Step 6: Run the clustering tests to verify they pass**

Run: `cargo test --lib jobs::context 2>&1 | tail -20`
Expected: PASS, 7 tests.

- [ ] **Step 7: Write the failing test for the whole sweep**

Append to `mod tests` in `src/jobs/context.rs`:

```rust
    use crate::core::context::{Bundle, CTX_DIM};
    use crate::core::test_support::test_core;

    /// A core with the recommender on and a fixed clock.
    async fn recommending_core(now: i64) -> Core {
        let mut core = test_core().await;
        core.recommend.enabled = true;
        core.clock = crate::core::context::Clock::Fixed(now);
        core
    }

    fn phone_bundle() -> String {
        serde_json::to_string(&Bundle {
            tz: Some("Europe/Berlin".into()),
            platform: Some("Android".into()),
            ua_family: Some("Chrome".into()),
            screen_w: Some(390.0),
            screen_h: Some(844.0),
            viewport_w: Some(390.0),
            viewport_h: Some(844.0),
            dpr: Some(3.0),
            cores: Some(8.0),
            memory_gb: Some(4.0),
            language: Some("de-DE".into()),
            touch: Some(true),
            orientation: Some("portrait".into()),
            network: Some("cellular".into()),
            ..Default::default()
        })
        .unwrap()
    }

    /// A Friday at 15:00 Berlin time, `weeks` weeks before `FRIDAY_SEVENTH`.
    const FRIDAY_SEVENTH: i64 = 1_787_320_320; // 2026-08-21T13:52Z
    fn friday(weeks_back: i64) -> i64 {
        FRIDAY_SEVENTH - weeks_back * 7 * 86_400 - 52 * 60
    }

    /// Six Fridays at 15:00 on the phone, opening `aid`.
    async fn seed_six_fridays(core: &Core, aid: &str) {
        for w in 1..=6 {
            let at = friday(w);
            core.store
                .record_context(&crate::store::context::ContextEvent {
                    id: 0,
                    scope: Some("alice".into()),
                    at,
                    bundle: phone_bundle(),
                    device_key: crate::core::context::device_key(
                        &crate::core::context::parse_bundle(&phone_bundle()),
                    ),
                    local_hour: Some(15),
                    weekday: Some(4),
                    tz: Some("Europe/Berlin".into()),
                })
                .await
                .unwrap();
            core.store
                .record_interaction(aid, "opened", None, Some("alice"), at + 5)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn six_fridays_become_one_stored_situation() {
        let core = recommending_core(FRIDAY_SEVENTH).await;
        let aid = seed_one_artifact(&core).await;
        seed_six_fridays(&core, &aid).await;

        let r = run(&core).await.unwrap();
        assert_eq!(r.events, 6);
        assert_eq!(r.profiled, 1);
        assert_eq!(r.clusters, 1);

        let stored = core.store.context_clusters_of(&[aid.clone()]).await.unwrap();
        let c = &stored[&aid][0];
        assert_eq!(c.slot, 0);
        assert_eq!(c.centroid.len(), CTX_DIM);
        assert_eq!(c.encoder_version, crate::core::context::ENCODER_VERSION);
        assert_eq!(c.scope.as_deref(), Some("alice"));

        // And the vector store agrees, which is what the read path queries.
        let hits = core
            .vectors
            .context_query(&c.centroid, 5, &Default::default())
            .await
            .unwrap();
        assert_eq!(hits[0].payload.artifact_id, aid);
    }

    #[tokio::test]
    async fn an_open_of_something_this_offered_raises_no_weight() {
        // §5, and the named test the guard needs — the sitting has one saying
        // it writes no activation, for the same reason. Without this, the first
        // lucky guess grows into a habit the system taught itself.
        let core = recommending_core(FRIDAY_SEVENTH).await;
        let aid = seed_one_artifact(&core).await;
        seed_six_fridays(&core, &aid).await;
        let before = run(&core).await.unwrap();

        // Six more Fridays, every one of them an open of the offer.
        for w in 7..=12 {
            let at = friday(w - 6) + 60;
            core.store
                .record_context(&crate::store::context::ContextEvent {
                    id: 0,
                    scope: Some("alice".into()),
                    at,
                    bundle: phone_bundle(),
                    device_key: None,
                    local_hour: Some(15),
                    weekday: Some(4),
                    tz: Some("Europe/Berlin".into()),
                })
                .await
                .unwrap();
            core.store
                .record_interaction(&aid, "recommended_open", None, Some("alice"), at + 5)
                .await
                .unwrap();
        }

        let after = run(&core).await.unwrap();
        assert_eq!(after.events, before.events, "not one of them counted");
        let stored = core.store.context_clusters_of(&[aid.clone()]).await.unwrap();
        assert!(
            (stored[&aid][0].weight - 6.0).abs() < 0.5,
            "weight {} moved",
            stored[&aid][0].weight
        );
    }

    #[tokio::test]
    async fn a_pattern_that_stopped_is_cleared_rather_than_left_standing() {
        // A year of silence at a 45-day half-life is 2^-8 of the weight it had.
        let core = recommending_core(FRIDAY_SEVENTH).await;
        let aid = seed_one_artifact(&core).await;
        seed_six_fridays(&core, &aid).await;
        run(&core).await.unwrap();
        assert!(!core.store.context_clusters_of(&[aid.clone()]).await.unwrap().is_empty());

        let later = recommending_core(FRIDAY_SEVENTH + 365 * 86_400).await;
        let later = Core { store: core.store.clone(), vectors: core.vectors.clone(), ..later };
        let r = run(&later).await.unwrap();
        assert_eq!(r.cleared, 1);
        assert!(core.store.context_clusters_of(&[aid.clone()]).await.unwrap().is_empty());
        assert!(
            later
                .vectors
                .context_query(&vec![0.1; CTX_DIM], 5, &Default::default())
                .await
                .unwrap()
                .is_empty(),
            "and the vector store was cleared too"
        );
    }

    #[tokio::test]
    async fn an_old_event_with_no_bundle_still_carries_a_weekday_and_an_hour() {
        // §12's cold start, which is not a backfill path: it is the ordinary
        // sweep reading older rows. Device and network contribute nothing
        // because §6 zeroes an absent block rather than defaulting it.
        let core = recommending_core(FRIDAY_SEVENTH).await;
        let aid = seed_one_artifact(&core).await;
        for w in 1..=6 {
            core.store
                .record_interaction(&aid, "opened", None, Some("alice"), friday(w))
                .await
                .unwrap();
        }
        let r = run(&core).await.unwrap();
        assert_eq!(r.events, 6, "no context event, and they still count");
        assert_eq!(r.clusters, 1);
    }

    #[tokio::test]
    async fn the_sweep_does_not_run_when_the_faculty_is_off() {
        let mut core = recommending_core(FRIDAY_SEVENTH).await;
        core.recommend.enabled = false;
        let aid = seed_one_artifact(&core).await;
        seed_six_fridays(&core, &aid).await;

        let r = run(&core).await.unwrap();
        assert_eq!(r.events, 0);
        assert!(core.store.context_clusters_of(&[aid]).await.unwrap().is_empty());
    }

    /// One corpus, one artifact, one point. Reuse whichever helper the
    /// neighbouring job test modules already use for this; write it here only
    /// if none exists.
    async fn seed_one_artifact(core: &Core) -> String {
        // See `src/jobs/pursuit.rs`'s test module for the shape this should
        // take against the real `Core` — it must leave a vector point behind,
        // or `context_query` has no payload to return.
        todo!("mirror jobs::pursuit's seeding helper")
    }
```

The `seed_one_artifact` helper is the one thing this plan does not spell out,
because `jobs/pursuit.rs`'s test module already has exactly it. Read that module
first and copy its approach; do not invent a third way to seed an artifact.

- [ ] **Step 8: Implement `run`**

```rust
/// One pass: read the log, encode, cluster, write.
pub async fn run(core: &Core) -> Result<Report> {
    let mut report = Report::default();
    if !core.recommends() {
        return Ok(report);
    }
    let cfg = &core.recommend;
    let now = core.clock.now();
    let since = now - crate::store::context::RETAIN_DAYS * 86_400;

    // Three sources, read whole. Bounded by the retention window rather than
    // paged: the sweep rebuilds every profile from scratch, and a rebuild is
    // only correct when it sees the whole window.
    let interactions = core.store.interactions_between(since, now).await?;
    let contexts = core.store.context_events_since(since).await?;
    // The bridge. `events_between` excludes the judge door already, which is
    // right here too: a benchmark run is not a situation anybody was in.
    let searches = core.store.events_between(since, now).await?;

    // (scope, artifact) -> the situations it was opened in.
    let mut by_pair: std::collections::BTreeMap<(String, String), Vec<Member>> =
        std::collections::BTreeMap::new();

    for i in &interactions {
        // `dwell` is not an open, and `recommended_shown` is not an
        // interaction at all — it is this feature's own offer, and counting it
        // would profile every artifact it ever guessed at.
        let self_made = match i.kind.as_str() {
            "opened" | "pivoted" => false,
            "recommended_open" => true,
            _ => continue,
        };
        let Some(artifact_id) = i.artifact_id.clone() else {
            continue;
        };
        let scope = i.scope.clone().unwrap_or_default();

        // Where the open followed a search, the situation is the search's, not
        // the open's: a recurring search resolves to the artifact it led to
        // rather than to a rerun of the query.
        let anchor = searches
            .iter()
            .filter(|s| {
                s.scope.as_deref().unwrap_or_default() == scope
                    && s.created_at <= i.at
                    && i.at - s.created_at <= BRIDGE_SECS
                    && s.shown.iter().any(|(id, _)| id == &artifact_id)
            })
            .map(|s| s.created_at)
            .max()
            .unwrap_or(i.at);

        // The nearest recorded situation, within half an hour either way.
        let matched = contexts
            .iter()
            .filter(|c| {
                c.scope.as_deref().unwrap_or_default() == scope
                    && (c.at - anchor).abs() <= MATCH_SECS
            })
            .min_by_key(|c| ((c.at - anchor).abs(), c.id));

        // No bundle is not no event. §12: `at` and `scope` were recorded from
        // the beginning, and because §6 zeroes an absent block rather than
        // defaulting it, an old row feeds in with no special handling —
        // weekday and hour contribute, device and network contribute nothing.
        let (raw, at) = match matched {
            Some(c) => (c.bundle.clone(), c.at),
            None => ("{}".to_string(), anchor),
        };
        let bundle = crate::core::context::parse_bundle(&raw);

        // Age decay, and the self-reinforcement guard as a multiplier. At
        // `self_weight = 0` a `recommended_open` produces a weightless member,
        // which the clusterer skips — so the first lucky guess cannot grow into
        // a habit the system taught itself.
        let age_days = ((now - at).max(0) as f64) / 86_400.0;
        let decayed = 0.5f64.powf(age_days / cfg.half_life_days.max(0.1));
        let weight = decayed * if self_made { cfg.self_weight } else { 1.0 };
        if weight <= 0.0 {
            continue;
        }

        let scope_arg = (!scope.is_empty()).then_some(scope.as_str());
        by_pair
            .entry((scope.clone(), artifact_id))
            .or_default()
            .push(Member {
                vec: crate::core::context::encode(at, scope_arg, &bundle, &cfg.weights),
                weight,
                at,
                bundle: raw,
            });
        report.events += 1;
    }

    // Slots are numbered per *artifact*, not per (scope, artifact): the
    // multivector is one array on one point, shared by every scope that has
    // opened this artifact, so a slot numbered per scope would have two owners
    // writing index 0.
    let mut per_artifact: std::collections::BTreeMap<String, Vec<(Option<String>, Cluster)>> =
        std::collections::BTreeMap::new();
    for ((scope, artifact_id), members) in by_pair {
        let scope = (!scope.is_empty()).then_some(scope);
        for c in agglomerate(&members, cfg.cluster_merge_at, cfg.max_clusters, cfg.min_weight) {
            per_artifact
                .entry(artifact_id.clone())
                .or_default()
                .push((scope.clone(), c));
        }
    }

    // Artifacts that had a profile and produced none this run. Their centroids
    // must go, or a pattern that stopped months ago would still be offered.
    let mut stale: std::collections::BTreeSet<String> = core
        .store
        .artifacts_with_context_clusters()
        .await?
        .into_iter()
        .collect();

    for (artifact_id, mut clusters) in per_artifact {
        stale.remove(&artifact_id);
        clusters.sort_by(|a, b| {
            b.1.weight
                .partial_cmp(&a.1.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.1.last_at.cmp(&a.1.last_at))
        });
        clusters.truncate(MAX_SLOTS);

        let rows: Vec<crate::store::context::StoredCluster> = clusters
            .iter()
            .enumerate()
            .map(|(slot, (scope, c))| crate::store::context::StoredCluster {
                scope: scope.clone(),
                artifact_id: artifact_id.clone(),
                slot: slot as i64,
                centroid: c.centroid.clone(),
                weight: c.weight,
                last_at: c.last_at,
                encoder_version: crate::core::context::ENCODER_VERSION,
                representative: c.representative.clone(),
            })
            .collect();

        // SQLite first, then the vector store. If the second write fails the
        // table names clusters the index does not carry — which offers nothing
        // and explains nothing, and the next run repairs it. The other order
        // would offer an artifact with no reason to show for it, which is the
        // one failure §11 says this must not have.
        core.store
            .replace_context_clusters(&artifact_id, &rows)
            .await?;
        let vectors: Vec<Vec<f32>> = rows.iter().map(|r| r.centroid.clone()).collect();
        core.vectors
            .set_context_vectors(&artifact_id, vectors)
            .await?;

        report.profiled += 1;
        report.clusters += rows.len();
    }

    for artifact_id in stale {
        core.store.replace_context_clusters(&artifact_id, &[]).await?;
        core.vectors.set_context_vectors(&artifact_id, vec![]).await?;
        report.cleared += 1;
    }

    Ok(report)
}
```

- [ ] **Step 9: Run the sweep tests to verify they pass**

Run: `cargo test --lib jobs::context 2>&1 | tail -30`
Expected: PASS, 12 tests.

- [ ] **Step 10: Wire it as a periodic unit**

`src/jobs/mod.rs` — add `pub mod context;` to the module list; add
`Stage::Context` to the periodic tuple match (`:113-118`), beside
`Stage::Retention`; and add an arm to `run_accounted` (`:234`):

```rust
        Stage::Context => context::run(core).await.and_then(detail),
```

`src/core/background.rs` — in `periodic_units`, after the `Retention` block:

```rust
    // Learning which situations recur for which artifact. Behind its own gate
    // and nothing else's: it reads the interaction log, which is not something
    // an operator switches off by switching off duplicate hygiene.
    if core.recommends() {
        out.push((Stage::Context, CONSOLIDATE_TARGET));
    }
```

and in `periodic_period`'s match:

```rust
        Stage::Context => crate::jobs::context::INTERVAL_HOURS.saturating_mul(3600),
```

- [ ] **Step 11: Write the failing test for the wiring**

Append to `mod tests` in `src/core/background.rs`:

```rust
    #[tokio::test]
    async fn the_context_sweep_is_armed_only_when_the_offer_is_on() {
        let mut core = crate::core::test_support::test_core().await;
        assert!(
            !periodic_units(&core).iter().any(|(s, _)| *s == Stage::Context),
            "off by default"
        );
        core.recommend.enabled = true;
        assert!(periodic_units(&core).iter().any(|(s, _)| *s == Stage::Context));
        assert_eq!(
            periodic_period(&core, Stage::Context),
            Some(std::time::Duration::from_secs(
                crate::jobs::context::INTERVAL_HOURS * 3600
            ))
        );
    }
```

- [ ] **Step 12: Run everything and commit**

Run: `cargo fmt && cargo clippy --all-targets 2>&1 | tail -20 && cargo test --lib 2>&1 | tail -10`
Expected: clean, all pass.

```bash
git add src/jobs/context.rs src/jobs/mod.rs src/store/jobs.rs \
        src/store/context.rs src/core/background.rs
git commit -m "feat(context): a sweep that learns what Friday afternoon looks like, and forgets when it stops"
```

---

### Task 8: The read path — the ladder, and the reason that matches the hit

One vector query, then arithmetic over at most fifty small vectors. No
embedding, no model call.

**Files:**
- Create: `src/core/recommend.rs`
- Modify: `src/core/mod.rs` (module declaration)
- Test: in `src/core/recommend.rs`

**Interfaces:**
- Consumes: `VectorStore::context_query`, `Store::context_clusters_of`, `core::context::{encode, contributions, context_score, ENCODER_VERSION, CTX_DIM}`, `Core::{clock, recommend, sittings, resurface}`.
- Produces:
  - `pub enum Rung { Pattern, Similar, Sitting, Forgotten }` with `pub fn as_str(&self) -> &'static str` and `pub fn line(&self) -> &'static str`
  - `pub struct Offer { pub artifact_id: String, pub title: String, pub rung: Rung, pub slot: Option<i64>, pub blocks: Vec<&'static str>, pub at: Option<i64>, pub detail: String }`
  - `pub const CANDIDATES: usize = 10;`
  - `pub const NAMED_BLOCKS: usize = 3;`
  - `Core::offer(&self, scope: Option<&str>, bundle: &Bundle, session: Option<&str>) -> Result<Option<Offer>>`

- [ ] **Step 1: Write the failing tests**

Create `src/core/recommend.rs` with the doc comment, types and this test module;
implement in Step 3.

```rust
//! What to offer under the search box, and why.
//!
//! One vector query against the `ctx` multivector, then the winning cluster
//! re-derived locally — because Qdrant returns the `max_sim` score and not
//! *which* element produced it, and the display needs the element.
//!
//! That places the same arithmetic in two places. It is the price of holding
//! the vectors in the index and the reason outside it, and a test pins that
//! both pick the same artifact: if they drift, the line under the offer
//! explains a hit it is not explaining.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::context::{Bundle, encode};
    use crate::core::test_support::test_core;

    const FRIDAY: i64 = 1_787_320_320;

    async fn core_at(now: i64) -> Core {
        let mut core = test_core().await;
        core.recommend.enabled = true;
        core.clock = crate::core::context::Clock::Fixed(now);
        core
    }

    fn phone() -> Bundle {
        Bundle {
            tz: Some("Europe/Berlin".into()),
            platform: Some("Android".into()),
            ua_family: Some("Chrome".into()),
            screen_w: Some(390.0),
            screen_h: Some(844.0),
            viewport_w: Some(390.0),
            viewport_h: Some(844.0),
            dpr: Some(3.0),
            cores: Some(8.0),
            memory_gb: Some(4.0),
            language: Some("de-DE".into()),
            touch: Some(true),
            orientation: Some("portrait".into()),
            network: Some("cellular".into()),
            ..Default::default()
        }
    }

    /// Give `aid` one learned situation, at `at`, in both stores.
    async fn learn(core: &Core, aid: &str, scope: &str, at: i64, b: &Bundle) {
        let v = encode(at, Some(scope), b, &core.recommend.weights);
        core.store
            .replace_context_clusters(
                aid,
                &[crate::store::context::StoredCluster {
                    scope: Some(scope.into()),
                    artifact_id: aid.into(),
                    slot: 0,
                    centroid: v.clone(),
                    weight: 6.0,
                    last_at: at,
                    encoder_version: crate::core::context::ENCODER_VERSION,
                    representative: serde_json::json!({ "at": at, "bundle": b }).to_string(),
                }],
            )
            .await
            .unwrap();
        core.vectors.set_context_vectors(aid, vec![v]).await.unwrap();
    }

    #[tokio::test]
    async fn the_reason_explains_the_artifact_that_was_offered() {
        // §10.3, and §11's last rule. If the local recomputation and the store
        // disagree, the line explains a different artifact than the one shown —
        // which is the one dishonesty this feature must not commit.
        let core = core_at(FRIDAY).await;
        let near = seed_one_artifact(&core).await;
        let far = seed_one_artifact(&core).await;
        learn(&core, &near, "alice", FRIDAY - 7 * 86_400, &phone()).await;
        let mut desk = phone();
        desk.platform = Some("macOS".into());
        desk.touch = Some(false);
        desk.network = Some("wired".into());
        learn(&core, &far, "alice", FRIDAY - 10 * 86_400 - 7 * 3600, &desk).await;

        let now = encode(FRIDAY, Some("alice"), &phone(), &core.recommend.weights);
        let from_store = core
            .vectors
            .context_query(&now, CANDIDATES, &Default::default())
            .await
            .unwrap();

        let offer = core.offer(Some("alice"), &phone(), None).await.unwrap().unwrap();
        assert_eq!(
            offer.artifact_id, from_store[0].payload.artifact_id,
            "the local argmax reproduces the store's"
        );
        assert_eq!(offer.artifact_id, near);
    }

    #[tokio::test]
    async fn a_recurring_situation_is_called_a_pattern_and_names_its_blocks() {
        let core = core_at(FRIDAY).await;
        let aid = seed_one_artifact(&core).await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone()).await;

        let offer = core.offer(Some("alice"), &phone(), None).await.unwrap().unwrap();
        assert_eq!(offer.rung, Rung::Pattern);
        assert_eq!(offer.slot, Some(0));
        assert_eq!(offer.blocks.len(), NAMED_BLOCKS);
        assert!(offer.blocks.contains(&"weekday"), "{:?}", offer.blocks);
        assert!(offer.blocks.contains(&"hour"), "{:?}", offer.blocks);
        assert!(offer.blocks.contains(&"device"), "{:?}", offer.blocks);
        assert_eq!(offer.at, Some(FRIDAY - 7 * 86_400));
    }

    #[tokio::test]
    async fn a_resemblance_is_not_called_a_pattern() {
        // The wording says what it rests on. The distance between "Fridays
        // around 15:00" and "similar to" is the whole honesty of the feature.
        let core = core_at(FRIDAY).await;
        let aid = seed_one_artifact(&core).await;
        // Same weekday and device, four hours off and on a different network.
        let mut other = phone();
        other.network = Some("wifi".into());
        other.orientation = Some("landscape".into());
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400 - 4 * 3600, &other).await;

        let offer = core.offer(Some("alice"), &phone(), None).await.unwrap().unwrap();
        assert_eq!(offer.rung, Rung::Similar);
    }

    #[tokio::test]
    async fn one_persons_situations_are_never_offered_to_another() {
        // §11. Until per-user collections exist, the `scope` block is the whole
        // of the isolation, and it needs a test that says so.
        let core = core_at(FRIDAY).await;
        let aid = seed_one_artifact(&core).await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone()).await;

        let offer = core.offer(Some("bob"), &phone(), None).await.unwrap();
        assert!(
            offer.as_ref().is_none_or(|o| o.rung != Rung::Pattern && o.rung != Rung::Similar),
            "alice's Friday is not bob's, got {offer:?}"
        );
    }

    #[tokio::test]
    async fn with_nothing_learned_the_ladder_falls_to_what_this_sitting_touched() {
        let core = core_at(FRIDAY).await;
        let aid = seed_one_artifact(&core).await;
        core.sittings.touched("sess-1", &aid, FRIDAY, 900);

        let offer = core
            .offer(Some("alice"), &phone(), Some("sess-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(offer.rung, Rung::Sitting);
        assert_eq!(offer.artifact_id, aid);
        assert!(offer.blocks.is_empty(), "nothing was matched, so nothing is named");
        assert_eq!(offer.slot, None);
    }

    #[tokio::test]
    async fn with_no_sitting_either_it_falls_to_what_has_been_forgotten() {
        // Deliberately not phrased like a pattern. This rung *is* `resurface`,
        // and it says so.
        let core = core_at(FRIDAY).await;
        let _aid = seed_old_artifact(&core).await;

        let offer = core.offer(Some("alice"), &phone(), None).await.unwrap().unwrap();
        assert_eq!(offer.rung, Rung::Forgotten);
        assert!(offer.rung.line().contains("long"), "{}", offer.rung.line());
    }

    #[tokio::test]
    async fn an_empty_base_is_offered_nothing_rather_than_a_lie() {
        let core = core_at(FRIDAY).await;
        assert!(core.offer(Some("alice"), &phone(), None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_cluster_from_an_older_encoder_explains_nothing() {
        // Its centroid may still be in the index — a rebuild copies a set whose
        // width matches — but the blocks it was built from are not the blocks
        // this reader knows. Skipped, rather than described with the wrong ones.
        let core = core_at(FRIDAY).await;
        let aid = seed_one_artifact(&core).await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone()).await;
        let mut rows = core.store.context_clusters_of(&[aid.clone()]).await.unwrap()[&aid].clone();
        rows[0].encoder_version = crate::core::context::ENCODER_VERSION + 1;
        core.store.replace_context_clusters(&aid, &rows).await.unwrap();

        let offer = core.offer(Some("alice"), &phone(), None).await.unwrap();
        assert!(offer.as_ref().is_none_or(|o| o.slot.is_none()));
    }

    #[tokio::test]
    async fn the_details_carry_the_bundle_and_the_numbers() {
        // "The parameters must be visible" — whoever wants to know exactly,
        // expands it. It is also the answer to what is being collected:
        // inspectable rather than promised.
        let core = core_at(FRIDAY).await;
        let aid = seed_one_artifact(&core).await;
        learn(&core, &aid, "alice", FRIDAY - 7 * 86_400, &phone()).await;

        let offer = core.offer(Some("alice"), &phone(), None).await.unwrap().unwrap();
        let d: serde_json::Value = serde_json::from_str(&offer.detail).unwrap();
        assert!(d["bundle"]["tz"].is_string());
        assert!(d["contributions"].is_object());
        assert!(d["score"].is_number());
    }

    /// See Task 7 Step 7: mirror `jobs::pursuit`'s seeding helper.
    async fn seed_one_artifact(core: &Core) -> String {
        todo!("mirror jobs::pursuit's seeding helper")
    }

    /// The same, with `created_at` far enough back that `resurface` returns it.
    async fn seed_old_artifact(core: &Core) -> String {
        todo!("as above, with created_at = 0 and last_seen_at unset")
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib core::recommend 2>&1 | tail -20`
Expected: FAIL — `cannot find type Rung`.

- [ ] **Step 3: Implement the ladder**

Insert above `#[cfg(test)]`:

```rust
use crate::core::Core;
use crate::core::context::{Bundle, contributions, context_score, encode};
use crate::error::Result;
use crate::vector::cosine;

/// How many artifacts the store is asked for.
///
/// Ten. Every one of them has its clusters reloaded and rescored locally, at
/// most five clusters of 53 dimensions apiece — a few thousand multiplications,
/// which is free next to the round trip that fetched them.
pub const CANDIDATES: usize = 10;

/// How many blocks the line names. Three, sorted by contribution: enough to say
/// what decided it, short enough to stay one line.
pub const NAMED_BLOCKS: usize = 3;

/// Which rung of the ladder the offer rests on.
///
/// Something is always shown, and the wording says what it rests on.
/// `Forgotten` is deliberately not phrased like a pattern: the distance between
/// "Fridays around 15:00" and "not seen in a long time" is the whole honesty of
/// this feature, and blurring it makes the area furniture within a fortnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    Pattern,
    Similar,
    Sitting,
    Forgotten,
}

impl Rung {
    /// For the recorded row, and for the Ops breakdown.
    pub fn as_str(&self) -> &'static str {
        match self {
            Rung::Pattern => "pattern",
            Rung::Similar => "similar",
            Rung::Sitting => "sitting",
            Rung::Forgotten => "forgotten",
        }
    }

    /// The four fixed strings the page prints. No prose is generated anywhere
    /// on this path — a new block in the encoder brings its own label and needs
    /// no sentence written for it.
    pub fn line(&self) -> &'static str {
        match self {
            Rung::Pattern => "Pattern",
            Rung::Similar => "Similar to",
            Rung::Sitting => "Touched in this sitting",
            Rung::Forgotten => "Not seen in a long time",
        }
    }
}

/// What is offered under the search box, and everything the page needs to say
/// why.
#[derive(Debug, Clone)]
pub struct Offer {
    pub artifact_id: String,
    pub title: String,
    pub rung: Rung,
    /// The winning cluster, for the recorded row. `None` on the two rungs that
    /// matched no situation.
    pub slot: Option<i64>,
    /// The blocks that decided it, largest contribution first, at most
    /// `NAMED_BLOCKS`. Empty on the lower two rungs, which matched nothing.
    pub blocks: Vec<&'static str>,
    /// The representative event's stamp. What "like 08.08., 15:04" prints.
    pub at: Option<i64>,
    /// The raw bundle and the contribution numbers, JSON, for the `<details>`.
    pub detail: String,
}

impl Core {
    /// What to offer, from the situation this page view happened in.
    ///
    /// One vector query and no embedding. The winning cluster is re-derived
    /// here because Qdrant's `max_sim` yields the maximum and not which element
    /// produced it — and the display needs the element, both to quote it and to
    /// name the blocks that decided it.
    pub async fn offer(
        &self,
        scope: Option<&str>,
        bundle: &Bundle,
        session: Option<&str>,
    ) -> Result<Option<Offer>> {
        if !self.recommends() {
            return Ok(None);
        }
        let now_at = self.clock.now();
        let now = encode(now_at, scope, bundle, &self.recommend.weights);

        // Superseded and deprecated are out, by `must_not` — the same rule
        // search obeys, and for the same reason a hand-written point carrying
        // no `status` key must still be offered.
        let hits = self
            .vectors
            .context_query(&now, CANDIDATES, &Default::default())
            .await
            .unwrap_or_else(|e| {
                // A vector store that cannot answer must not take the search
                // page down with it: without an offer the page is what it was
                // yesterday, and the ladder still has three rungs below this.
                tracing::warn!(error = %e, "context query unavailable; falling through the ladder");
                Vec::new()
            });

        let ids: Vec<String> = hits.iter().map(|h| h.payload.artifact_id.clone()).collect();
        let clusters = self.store.context_clusters_of(&ids).await?;

        // The argmax, over the **full** vector — that is what reproduces the
        // store's choice, because that is what `max_sim` scored. The rung comes
        // from `context_score`, which slices the `scope` block off: the two
        // numbers answer different questions and live on different scales.
        let mut best: Option<(&crate::vector::SearchHit, &crate::store::context::StoredCluster, f32)> =
            None;
        for hit in &hits {
            let Some(mine) = clusters.get(&hit.payload.artifact_id) else {
                continue;
            };
            for c in mine {
                // A cluster written under another layout is skipped rather than
                // explained with the wrong blocks. Its centroid may still be in
                // the index — a rebuild copies any set whose width matches —
                // and the next sweep replaces it.
                if c.encoder_version != crate::core::context::ENCODER_VERSION
                    || c.centroid.len() != now.len()
                {
                    continue;
                }
                let full = cosine(&now, &c.centroid);
                if best.is_none_or(|(_, _, b)| full > b) {
                    best = Some((hit, c, full));
                }
            }
        }

        if let Some((hit, cluster, _)) = best {
            let score = context_score(&now, &cluster.centroid);
            let rung = if score >= self.recommend.strong_at {
                Some(Rung::Pattern)
            } else if score >= self.recommend.weak_at {
                Some(Rung::Similar)
            } else {
                None
            };
            if let Some(rung) = rung {
                let all = contributions(&now, &cluster.centroid, &self.recommend.weights);
                let rep: serde_json::Value =
                    serde_json::from_str(&cluster.representative).unwrap_or_default();
                return Ok(Some(Offer {
                    artifact_id: hit.payload.artifact_id.clone(),
                    title: title_of(&hit.payload),
                    rung,
                    slot: Some(cluster.slot),
                    blocks: all.iter().take(NAMED_BLOCKS).map(|(l, _)| *l).collect(),
                    at: rep.get("at").and_then(serde_json::Value::as_i64),
                    detail: serde_json::json!({
                        "score": score,
                        "bundle": bundle,
                        "representative": rep,
                        "contributions": all.iter().copied().collect::<std::collections::BTreeMap<_, _>>(),
                    })
                    .to_string(),
                }));
            }
        }

        // Nothing in `ctx`, but this sitting is open. A way back to what was
        // just being read is not a claim about a pattern, and the wording does
        // not make one.
        if let Some(sess) = session {
            let carried = self
                .sittings
                .read(sess, now_at, self.pursuit.idle_secs as i64);
            for aid in &carried.touched {
                if let Ok(a) = self.store.get_artifact(aid).await
                    && a.in_results()
                {
                    return Ok(Some(Offer {
                        artifact_id: a.id.clone(),
                        title: a.title.clone().unwrap_or_else(|| first_line(&a.text)),
                        rung: Rung::Sitting,
                        slot: None,
                        blocks: Vec::new(),
                        at: None,
                        detail: serde_json::json!({ "bundle": bundle }).to_string(),
                    }));
                }
            }
        }

        // The bottom rung, which *is* `resurface` — and says so.
        let forgotten = self.resurface(1).await.unwrap_or_default();
        Ok(forgotten.into_iter().next().map(|r| Offer {
            artifact_id: r.artifact_id.clone(),
            title: r.title.clone().unwrap_or_else(|| first_line(&r.text)),
            rung: Rung::Forgotten,
            slot: None,
            blocks: Vec::new(),
            at: None,
            detail: serde_json::json!({ "bundle": bundle }).to_string(),
        }))
    }
}

fn title_of(p: &crate::vector::VectorPayload) -> String {
    p.title.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| first_line(&p.text))
}

/// The opening of the text, for something with no title. Deliberately short:
/// the offer is one line under a search box, not a result card.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    match line.char_indices().nth(70) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}
```

Check `Core::resurface`'s return type (`src/core/search.rs:742`) and adjust the
field names in the last block to match `SearchResult` rather than guessing.

- [ ] **Step 4: Declare the module**

In `src/core/mod.rs`, add `pub mod recommend;` to the module list (after
`pub mod pdf;`).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib core::recommend 2>&1 | tail -30`
Expected: PASS, 9 tests.

If `a_resemblance_is_not_called_a_pattern` lands on `Pattern` instead, the two
bundles are closer than intended — widen the gap (change the weekday too) rather
than moving `strong_at`, which is a shipped default.

- [ ] **Step 6: Commit**

```bash
git add src/core/recommend.rs src/core/mod.rs
git commit -m "feat(recommend): four rungs, and a reason that explains the hit it is shown beside"
```

---

### Task 9: The display — one endpoint, two jobs

The bundle originates in the browser, so the server does not have it at first
render. `search_page` renders a placeholder with reserved height and htmx fills
it. One endpoint records the situation *and* answers with the fragment;
recording happens even when nothing is recommended.

**Files:**
- Modify: `src/core/recommend.rs` (one field on `Offer`)
- Modify: `src/core/mod.rs` (`Core::record_context_event`)
- Modify: `src/store/pursuits.rs` (`Store::record_recommendation`)
- Modify: `src/web/ui.rs` (the route, the handler, the view struct, `SearchTemplate::recommend`)
- Create: `src/web/templates/_context.html`
- Modify: `src/web/templates/search.html`
- Modify: `assets/app.js`, `assets/css/40-search.css`
- Test: in `src/web/ui.rs`'s existing test module

**Interfaces:**
- Consumes: `Core::offer`, `Rung::{as_str, line}`, `core::context::{parse_bundle, device_key, local_time}`.
- Produces:
  - `pub at_tz: Option<String>` on `Offer` — an amendment to Task 8, set from `rep["bundle"]["tz"]`, so the quoted stamp is printed in the zone the device was actually in.
  - `Store::record_recommendation(&self, artifact_id: &str, kind: &str, detail: &str, scope: Option<&str>, at: i64) -> Result<()>`
  - `Core::record_context_event(&self, raw: &str, bundle: &Bundle, scope: Option<&str>)` — off the request path
  - `Core::record_recommendation(&self, artifact_id: &str, kind: &str, rung: &str, slot: Option<i64>, scope: Option<&str>)` — off the request path
  - `POST /ui/context`
  - `window.engramContext()` in `app.js`

- [ ] **Step 1: Amend `Offer` with the zone**

In `src/core/recommend.rs`, add to `Offer` after `at`:

```rust
    /// The zone the representative event happened in, so "like 08.08., 15:04"
    /// is printed as the device read it. Taking the *current* bundle's zone
    /// would misdate every situation recorded on a trip.
    pub at_tz: Option<String>,
```

Set it in the `Pattern`/`Similar` branch:

```rust
                    at_tz: rep
                        .pointer("/bundle/tz")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
```

and `at_tz: None` in the two lower rungs. Re-run `cargo test --lib core::recommend`.

- [ ] **Step 2: Write the failing tests**

Append to the test module in `src/web/ui.rs`:

```rust
    #[tokio::test]
    async fn a_page_view_is_recorded_even_when_nothing_is_offered() {
        // The endpoint has two jobs and does the first unconditionally. A base
        // that has learned nothing yet is exactly the base that most needs its
        // situations written down.
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        let store = core.store.clone();
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        let res = post_form(
            &app,
            &cookie,
            "/ui/context",
            "bundle=%7B%22tz%22%3A%22Europe%2FBerlin%22%7D",
        )
        .await;
        assert_eq!(res.status(), 200);

        let rows = store.context_events_since(0).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tz.as_deref(), Some("Europe/Berlin"));
        assert!(rows[0].local_hour.is_some(), "denormalised for the sweep");
        assert_eq!(rows[0].scope.as_deref(), Some("user-1"));
    }

    #[tokio::test]
    async fn a_bundle_the_browser_could_not_build_does_not_break_the_page() {
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        let store = core.store.clone();
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        let res = post_form(&app, &cookie, "/ui/context", "bundle=%7B%7Bnope").await;
        assert_eq!(res.status(), 200, "an empty bundle is a working one");
        assert_eq!(store.context_events_since(0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_area_is_not_rendered_when_the_faculty_is_off() {
        // One gate, in one place: no placeholder, no request, nothing recorded.
        let core = crate::core::test_support::test_core().await;
        let store = core.store.clone();
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        let page = crate::web::test_support::body_of(get(&app, &cookie, "/ui/search").await).await;
        assert!(!page.contains("/ui/context"), "no placeholder");

        let res = post_form(&app, &cookie, "/ui/context", "bundle=%7B%7D").await;
        assert_eq!(res.status(), 200);
        assert!(store.context_events_since(0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_placeholder_reserves_its_height_so_the_page_does_not_jump() {
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        let page = crate::web::test_support::body_of(get(&app, &cookie, "/ui/search").await).await;
        assert!(page.contains(r#"id="context-offer""#));
        assert!(page.contains("hx-post=\"/ui/context\""));
        assert!(page.contains("engramContext()"));
    }

    #[tokio::test]
    async fn what_was_offered_is_written_down_with_its_rung() {
        // Shown against clicked, broken down by rung, is a hit rate. It is the
        // only number that can later settle whether the weights are right, and
        // a recommender with no visible hit rate becomes `[sitting] prime`:
        // a default nobody ever measured.
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        let store = core.store.clone();
        let aid = seed_forgotten_artifact(&core).await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        let body = crate::web::test_support::body_of(
            post_form(&app, &cookie, "/ui/context", "bundle=%7B%7D").await,
        )
        .await;
        assert!(body.contains("Not seen in a long time"), "{body}");
        assert!(body.contains(&aid));

        let rows = store.interactions_between(0, i64::MAX).await.unwrap();
        let shown: Vec<_> = rows.iter().filter(|r| r.kind == "recommended_shown").collect();
        assert_eq!(shown.len(), 1);
        assert!(shown[0].detail.as_deref().unwrap().contains("forgotten"));
    }

    /// One artifact old enough and unseen enough that `resurface` returns it.
    /// Mirror whatever the search tests in this module already use.
    async fn seed_forgotten_artifact(core: &crate::core::Core) -> String {
        todo!("mirror this module's existing artifact seeding")
    }
```

`post_form` and `get` are helpers this module already has under some name — read
the module's other tests and use those rather than adding new ones.

- [ ] **Step 3: Run them to verify they fail**

Run: `cargo test --lib web::ui 2>&1 | tail -20`
Expected: FAIL — no `/ui/context` route (404).

- [ ] **Step 4: Add the two store writes**

In `src/store/pursuits.rs`, beside `record_dwell`:

```rust
    /// What this base offered, and whether it was taken.
    ///
    /// `kind` is `recommended_shown` or `recommended_open`. Both live in
    /// `interaction_events` because both are things that happened after a page
    /// rendered — but neither counts as an ordinary open: the sweep reads
    /// `recommended_open` at `recommend.self_weight` and ignores
    /// `recommended_shown` entirely, and `jobs::pursuit` skips the latter too.
    ///
    /// `detail` carries the rung and the winning cluster as JSON, which is what
    /// makes the Ops hit rate a breakdown rather than one number.
    pub async fn record_recommendation(
        &self,
        artifact_id: &str,
        kind: &str,
        detail: &str,
        scope: Option<&str>,
        at: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO interaction_events (artifact_id, kind, detail, scope, at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(artifact_id)
        .bind(kind)
        .bind(detail)
        .bind(scope)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
```

- [ ] **Step 5: Add the two `Core` writes**

In `src/core/recommend.rs`, inside `impl Core`:

```rust
    /// Write down the situation this page view happened in. Off the request
    /// path: a page view must not get slower, or fail, because a bookkeeping
    /// write did — and a situation dropped at shutdown is a Friday afternoon
    /// the base never learns about, which is why it goes through `background`
    /// rather than a bare `tokio::spawn`.
    ///
    /// The raw string is stored whole, including the fields the encoder does
    /// not read today; the denormalised columns are what the sweep reads on
    /// every row.
    pub fn record_context_event(&self, raw: &str, bundle: &Bundle, scope: Option<&str>) {
        if !self.recommends() {
            return;
        }
        let at = self.clock.now();
        let t = crate::core::context::local_time(at, bundle.tz.as_deref(), bundle.tz_offset_mins);
        let row = crate::store::context::ContextEvent {
            id: 0,
            scope: scope.map(str::to_string),
            at,
            bundle: raw.to_string(),
            device_key: crate::core::context::device_key(bundle),
            local_hour: Some(t.hour as i64),
            weekday: Some(t.weekday as i64),
            tz: bundle.tz.clone(),
        };
        let store = self.store.clone();
        self.background.spawn(async move {
            if let Err(e) = store.record_context(&row).await {
                tracing::warn!(error = %e, "could not record the situation of a page view");
            }
        });
    }

    /// Write down that something was offered, or that the offer was taken.
    pub fn record_recommendation(
        &self,
        artifact_id: &str,
        kind: &str,
        rung: &str,
        slot: Option<i64>,
        scope: Option<&str>,
    ) {
        if !self.recommends() {
            return;
        }
        let detail = serde_json::json!({ "rung": rung, "slot": slot }).to_string();
        let (store, id, kind, scope) = (
            self.store.clone(),
            artifact_id.to_string(),
            kind.to_string(),
            scope.map(str::to_string),
        );
        let at = self.clock.now();
        self.background.spawn(async move {
            if let Err(e) = store
                .record_recommendation(&id, &kind, &detail, scope.as_deref(), at)
                .await
            {
                tracing::warn!(error = %e, "could not record what was offered");
            }
        });
    }
```

- [ ] **Step 6: The endpoint and the view struct**

In `src/web/ui.rs`, beside the other fragment templates:

```rust
/// One offer, flattened for the template. Every decision — which rung, which
/// blocks, how the stamp reads — is made here, so the template holds no logic
/// and a new block in the encoder changes no markup.
#[derive(Default)]
pub struct OfferView {
    pub id: String,
    pub title: String,
    /// One of four fixed strings. See `Rung::line`.
    pub rung: &'static str,
    /// The blocks that decided it, joined. Empty on the lower two rungs.
    pub blocks: String,
    /// `08.08., 15:04`, or empty.
    pub when: String,
    /// `?rec=<slot>`, or empty — what tells `artifact_detail` this open came
    /// from an offer.
    pub rec: String,
    /// The raw bundle and the contribution numbers, for the `<details>`.
    pub detail: String,
}

#[derive(Template, Default)]
#[template(path = "_context.html")]
struct ContextTemplate {
    offer: Option<OfferView>,
}

#[derive(serde::Deserialize)]
struct ContextForm {
    #[serde(default)]
    bundle: String,
}

/// One endpoint, two jobs: it writes the situation and answers with the
/// fragment. Recording happens even when nothing is recommended — a base that
/// has learned nothing yet is exactly the one that most needs its situations
/// written down.
async fn context_offer(
    State(st): State<AppState>,
    id: Identity,
    Form(f): Form<ContextForm>,
) -> Result<Response> {
    if !st.core.recommends() {
        return Ok(HtmlTemplate(ContextTemplate::default()).into_response());
    }
    let bundle = crate::core::context::parse_bundle(&f.bundle);
    st.core
        .record_context_event(&f.bundle, &bundle, Some(&id.subject));

    // A recommendation that cannot be computed is not worth a 500: the area is
    // what it was yesterday, which is empty.
    let offer = st
        .core
        .offer(Some(&id.subject), &bundle, id.session.as_deref())
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not build a recommendation");
            None
        });

    if let Some(o) = &offer {
        st.core.record_recommendation(
            &o.artifact_id,
            "recommended_shown",
            o.rung.as_str(),
            o.slot,
            Some(&id.subject),
        );
    }
    Ok(HtmlTemplate(ContextTemplate {
        offer: offer.map(offer_view),
    })
    .into_response())
}

fn offer_view(o: crate::core::recommend::Offer) -> OfferView {
    OfferView {
        rung: o.rung.line(),
        blocks: o.blocks.join(", "),
        // The device's own reading of when this happened, in the zone it
        // happened in. One date format, and the whole of the third part of the
        // line.
        when: o
            .at
            .map(|at| {
                let t = crate::core::context::local_time(at, o.at_tz.as_deref(), None);
                format!(
                    "{:02}.{:02}., {:02}:{:02}",
                    t.day,
                    t.month,
                    t.hour as u32,
                    ((t.hour % 1.0) * 60.0).round() as u32
                )
            })
            .unwrap_or_default(),
        rec: match o.slot {
            Some(s) => format!("?rec={s}"),
            None => String::new(),
        },
        id: o.artifact_id,
        title: o.title,
        detail: o.detail,
    }
}
```

Add the route beside the other search routes (`ui.rs:3670`):

```rust
        .route("/ui/context", post(context_offer))
```

- [ ] **Step 7: The fragment**

Create `src/web/templates/_context.html`:

```html
{# The area under the search box. Fetched rather than rendered because the
   bundle originates in the browser and the server does not have it at first
   render — `search_page` puts a placeholder here with reserved height, and this
   replaces it.

   Three parts, all cheap: the rung name, which is one of four fixed strings;
   the blocks that decided it, which are `&'static str` labels sorted by
   contribution; and the representative event's stamp, which is one date format.
   No sentences are generated anywhere — a new block in the encoder brings its
   own label and needs no template written for it. Generated prose per block was
   the first draft and was cut, because it coupled every new dimension to a
   sentence somebody had to keep in step with it.

   The id is kept on the answer as well as on the placeholder: app.js removes
   this area on the first keystroke, and it needs something to remove whichever
   of the two is on screen when that happens. #}
<div id="context-offer" class="offer">
{% if let Some(o) = offer %}
  <a class="offer-title" href="/ui/artifacts/{{ o.id }}{{ o.rec }}">{{ o.title }}</a>
  <p class="muted offer-why">{{ o.rung }}{% if !o.blocks.is_empty() %} · {{ o.blocks }}{% endif %}{% if !o.when.is_empty() %} · like {{ o.when }}{% endif %}
    {# `serde_json` over the raw bundle plus the contribution numbers, inside a
       `<details>`. No formatting code, and it satisfies "the parameters must be
       visible" completely: whoever wants to know exactly, expands it. It is
       also the answer to what is being collected — inspectable rather than
       promised. #}
    <details class="offer-detail">
      <summary>Details</summary>
      <pre class="mono">{{ o.detail }}</pre>
    </details>
  </p>
{% endif %}
</div>
```

- [ ] **Step 8: The placeholder**

In `src/web/templates/search.html`, immediately after the closing `</form>` of
`#filters` and before the `keyhint` paragraph:

```html
{# Reserved height, so the answer landing does not push the hint and the chips
   down a page that has already been read. The offer is for the state "no intent
   expressed yet" — app.js removes it on the first keystroke, and it does not
   come back when the box is cleared: once there is an intent the offer is
   wrong, and reappearing because someone corrected a typo is flicker. It
   returns on the next page view.

   Without JavaScript the area stays empty, which is the honest failure: the
   bundle is the browser's to send. #}
{% if recommend %}
<div id="context-offer" class="offer" hx-post="/ui/context" hx-trigger="load"
     hx-vals='js:{bundle: engramContext()}' hx-swap="outerHTML"></div>
{% endif %}
```

Add the field to `SearchTemplate` in `src/web/ui.rs`:

```rust
    /// Whether the area under the search box exists at all. See
    /// `Core::recommends`.
    recommend: bool,
```

and set it in `search_page`:

```rust
        recommend: st.core.recommends(),
```

- [ ] **Step 9: The client**

In `assets/app.js`, inside the IIFE and above the `DOMContentLoaded` handler:

```js
  // The situation this page view happened in.
  //
  // Synchronous, because htmx reads `hx-vals js:` synchronously. The two
  // asynchronous sources — the Battery API and the device list — are read once
  // at load and cached here, so the first page view of a session goes without
  // them and the server zeroes their blocks rather than inventing a value.
  //
  // Deliberately not collected: canvas, WebGL, fonts, plugins. Those identify a
  // device across a population; here the population is one authenticated person,
  // so they are constant and say nothing about which situation this is — and a
  // hardened browser randomises them per session, so every day would look like a
  // new device.
  var slow = { battery_level: null, charging: null, audio_outputs: null };

  function primeSlow() {
    if (navigator.getBattery) {
      navigator.getBattery().then(function (b) {
        slow.battery_level = b.level;
        slow.charging = b.charging;
      }).catch(function () {});
    }
    if (navigator.mediaDevices && navigator.mediaDevices.enumerateDevices) {
      navigator.mediaDevices.enumerateDevices().then(function (list) {
        slow.audio_outputs = list.filter(function (d) { return d.kind === 'audiooutput'; }).length;
      }).catch(function () {});
    }
  }

  function uaFamily() {
    var d = navigator.userAgentData;
    if (d && d.brands) {
      for (var i = 0; i < d.brands.length; i++) {
        // Chromium pads the list with a deliberately absurd brand to break
        // exactly the kind of sniffing this is not doing; skip it.
        if (!/Not.*Brand/i.test(d.brands[i].brand)) return d.brands[i].brand;
      }
    }
    var m = /(Firefox|Chrome|Safari|Edg)\/[\d.]+/.exec(navigator.userAgent || '');
    return m ? m[1] : null;
  }

  function netKind() {
    var c = navigator.connection || navigator.mozConnection;
    if (!c) return null;
    if (c.type) return c.type;
    // `effectiveType` describes speed rather than medium, and calling a slow
    // wifi "cellular" would be inventing a fact. Absent instead.
    return null;
  }

  window.engramContext = function () {
    var b = {};
    try {
      b.tz = Intl.DateTimeFormat().resolvedOptions().timeZone || null;
      b.tz_offset_mins = -new Date().getTimezoneOffset();
      b.language = navigator.language || null;
      b.languages = navigator.languages ? Array.prototype.slice.call(navigator.languages) : [];
      b.viewport_w = window.innerWidth;
      b.viewport_h = window.innerHeight;
      b.screen_w = screen.width;
      b.screen_h = screen.height;
      b.dpr = window.devicePixelRatio;
      b.color_scheme = matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
      b.platform = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || null;
      b.ua_family = uaFamily();
      b.cores = navigator.hardwareConcurrency || null;
      b.memory_gb = navigator.deviceMemory || null;
      b.touch = navigator.maxTouchPoints > 0;
      b.orientation = window.innerHeight >= window.innerWidth ? 'portrait' : 'landscape';
      b.network = netKind();
      b.battery_level = slow.battery_level;
      b.charging = slow.charging;
      b.audio_outputs = slow.audio_outputs;
    } catch (e) {
      // A partial bundle is a working one: the blocks it could not fill are
      // zeroed server-side, and the weekday and the hour still stand.
    }
    return JSON.stringify(b);
  };

  // Removed on the first keystroke, and it does not come back when the box is
  // cleared. The flag is what covers the race: the fetch fires on `load`, and
  // its answer can land *after* the first keystroke — without this, an offer
  // already dismissed would swap itself back in.
  var offerDismissed = false;

  function dropOffer() {
    var area = document.getElementById('context-offer');
    if (area) area.remove();
  }

  function contextOffer() {
    var box = document.querySelector('input[name=q]');
    if (!box) return;
    box.addEventListener('input', function () {
      offerDismissed = true;
      dropOffer();
    }, { once: true });
  }
```

In the `DOMContentLoaded` handler, beside the other initialisers:

```js
    primeSlow();
    contextOffer();
```

and in the existing `htmx:afterSwap` listener, first thing inside:

```js
      if (e.target.id === 'context-offer' && offerDismissed) dropOffer();
```

- [ ] **Step 10: The rules**

Append to `assets/css/40-search.css`:

```css
/* The area under the search box. `min-height` is what keeps the answer landing
   from pushing the hint and the chips down a page that has already been read —
   the fetch is one round trip after `load`, which is long enough to see. */
.offer { min-height: 3.25rem; margin: 0.25rem 0 0.5rem; }
.offer-title { font-size: var(--text-base); color: var(--color-fg-primary); }
.offer-why { font-size: var(--text-xs); margin: 0.125rem 0 0; }
.offer-detail { display: inline; }
.offer-detail summary { display: inline; cursor: pointer; color: var(--color-accent); }
.offer-detail pre { white-space: pre-wrap; word-break: break-all; margin: 0.375rem 0 0; font-size: var(--text-xs); }
```

- [ ] **Step 11: Run the tests to verify they pass**

Run: `cargo test --lib web::ui 2>&1 | tail -30`
Expected: PASS.

- [ ] **Step 12: See it in the real app**

Run the app against a real config with `[recommend] enabled = true`, open
`/ui/search`, and confirm: the area shows something, the page does not jump when
it lands, and the first keystroke removes it and clearing the box does not bring
it back. Use the `run` skill if the project has one.

- [ ] **Step 13: Commit**

```bash
git add src/core/recommend.rs src/core/mod.rs src/store/pursuits.rs \
        src/web/ui.rs src/web/templates/_context.html \
        src/web/templates/search.html assets/app.js assets/css/40-search.css \
        src/core/context.rs
git commit -m "feat(web): the area under the search box, and the reason under that"
```

---

### Task 10: The guard, and a hit rate somebody can see

Without this, the first lucky guess grows into a habit the system taught itself
— and nobody can tell whether the weights in §6 are right. `[sitting] prime` has
sat at `false` for months because nobody measured it; a recommender with no
visible hit rate becomes the same case.

**Files:**
- Modify: `src/web/ui.rs` (`artifact_detail`'s `rec=` branch, the Ops rollup)
- Modify: `src/jobs/pursuit.rs:520` (one filter)
- Modify: `src/store/pursuits.rs` (the rollup query)
- Modify: `src/web/templates/ops.html`
- Test: in `src/jobs/pursuit.rs` and `src/web/ui.rs`

**Interfaces:**
- Consumes: `Store::record_recommendation`, `Rung::as_str`.
- Produces:
  - `pub struct OfferRate { pub rung: String, pub shown: i64, pub opened: i64 }`
  - `Store::offer_rates(&self, since: i64) -> Result<Vec<OfferRate>>` — one row per rung, shown descending

- [ ] **Step 1: Write the failing tests**

In `src/jobs/pursuit.rs`'s test module:

```rust
    #[tokio::test]
    async fn an_offer_that_was_only_shown_is_not_engagement() {
        // `recommended_shown` is this feature's own guess, not something a
        // person did. The sweep weighs any kind that is not `dwell` or
        // `pivoted` at 1.0, so without an explicit exclusion every artifact the
        // recommender ever guessed at would count as an engaged one — and a
        // pursuit would be written around a list nobody clicked.
        let core = test_core_for_pursuits().await;
        let ids = seed_two(&core).await;
        let t0 = crate::store::now() - 10_000;
        record_search(&core, t0, &ids).await;
        core.store
            .record_recommendation(&ids[0], "recommended_shown", r#"{"rung":"pattern"}"#, Some("me"), t0 + 1)
            .await
            .unwrap();

        let n = crate::jobs::pursuit::run(&core).await.unwrap();
        let open = core.store.open_pursuits().await.unwrap();
        assert!(
            open.iter().all(|p| !p.sources.contains(&ids[0])),
            "shown is not engaged (wrote {n})"
        );
    }
```

Adapt `test_core_for_pursuits`, `seed_two` and `record_search` to whatever that
module already calls its helpers — read it first.

In `src/web/ui.rs`'s test module:

```rust
    #[tokio::test]
    async fn taking_an_offer_is_not_an_ordinary_open() {
        // §5: without this the profile reinforces itself. The row is written,
        // and it is written under its own kind so the sweep can weigh it at
        // `self_weight` — which is zero.
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        core.pursuit.enabled = true;
        core.feedback.enabled = true;
        let store = core.store.clone();
        let aid = seed_forgotten_artifact(&core).await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        let res = get(&app, &cookie, &format!("/ui/artifacts/{aid}?rec=0")).await;
        assert_eq!(res.status(), 200);
        // Background writes; drain before asserting rather than sleeping.
        drain_background(&app).await;

        let kinds: Vec<String> = store
            .interactions_between(0, i64::MAX)
            .await
            .unwrap()
            .iter()
            .map(|r| r.kind.clone())
            .collect();
        assert!(kinds.contains(&"recommended_open".to_string()));
        assert!(!kinds.contains(&"opened".to_string()), "not both: {kinds:?}");
    }

    #[tokio::test]
    async fn ops_shows_shown_against_clicked_by_rung() {
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        let store = core.store.clone();
        let aid = seed_forgotten_artifact(&core).await;
        for _ in 0..4 {
            store
                .record_recommendation(&aid, "recommended_shown", r#"{"rung":"pattern"}"#, Some("me"), 100)
                .await
                .unwrap();
        }
        store
            .record_recommendation(&aid, "recommended_open", r#"{"rung":"pattern"}"#, Some("me"), 101)
            .await
            .unwrap();

        let rates = store.offer_rates(0).await.unwrap();
        assert_eq!(rates.len(), 1);
        assert_eq!(rates[0].rung, "pattern");
        assert_eq!(rates[0].shown, 4);
        assert_eq!(rates[0].opened, 1);

        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let page = crate::web::test_support::body_of(get(&app, &cookie, "/ui/ops").await).await;
        assert!(page.contains("pattern"), "{page}");
        assert!(page.contains("4"));
    }
```

`drain_background` is `core.background.wait_idle()` — reach it however the
module's existing background-write tests do.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --lib jobs::pursuit web::ui 2>&1 | tail -20`
Expected: FAIL.

- [ ] **Step 3: Exclude the offer from engagement**

In `src/jobs/pursuit.rs:520`, at the `mine` construction:

```rust
        let mine: Vec<&crate::store::pursuits::Interaction> = attached[m]
            .iter()
            .map(|&k| &interactions[k])
            // What this base *offered* is not something a person did. Every
            // kind that is not `dwell` or `pivoted` is weighed at 1.0 below, so
            // without this every artifact the recommender ever guessed at would
            // count as engaged — and `engaged` a few lines down would call a
            // search followed by nothing a search that was followed.
            .filter(|i| i.kind != "recommended_shown")
            .collect();
```

- [ ] **Step 4: Record the click**

In `src/web/ui.rs`, extend `ArtifactViewParams`:

```rust
    /// The cluster slot this was offered under, when the link came from the
    /// area under the search box.
    #[serde(default)]
    rec: Option<i64>,
    /// And the rung it was offered on. Carried on the link because the offer
    /// was computed on a previous request and nothing server-side still holds
    /// it — without it, every click lands in one bucket on Ops.
    #[serde(default)]
    rung: Option<String>,
```

and in `artifact_detail`, replace the `record_interaction` call:

```rust
    // And the act the pursuit sweep reads: opened, or pivoted through — unless
    // this came from the area under the search box, in which case it is written
    // under its own kind and *not* as an ordinary open. §5: a
    // `recommended_open` counted as an open is the first lucky guess growing
    // into a habit the system taught itself.
    //
    // The rung rides on the link because that is the only place it still
    // exists: the offer was computed on a previous request, and Ops's
    // breakdown is a breakdown only if the click knows which rung it came from.
    match p.rec {
        Some(slot) => st.core.record_recommendation(
            &cid,
            "recommended_open",
            p.rung.as_deref().unwrap_or("unknown"),
            Some(slot),
            Some(&id.subject),
        ),
        None => st
            .core
            .record_interaction(&cid, p.via.as_deref(), Some(&id.subject)),
    }
```

And the link has to carry it. In Task 9's `offer_view`, `rec` becomes:

```rust
        rec: match o.slot {
            Some(s) => format!("?rec={s}&rung={}", o.rung.as_str()),
            None => String::new(),
        },
```

- [ ] **Step 5: The rollup**

In `src/store/pursuits.rs`:

```rust
/// Shown against clicked, for one rung of the ladder.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OfferRate {
    pub rung: String,
    pub shown: i64,
    pub opened: i64,
}

// … inside impl Store:

    /// What was offered and what was taken, by rung, since `since`.
    ///
    /// The only number that can later settle whether §6's weights are right.
    /// They are chosen, not measured, and fitting them before this data exists
    /// would be guessing with extra steps — so this is the instrument, and it
    /// goes on Ops, which is where `ROADMAP.md` puts mechanisms whose effect
    /// nobody can otherwise see.
    pub async fn offer_rates(&self, since: i64) -> Result<Vec<OfferRate>> {
        let rows = sqlx::query(
            "SELECT json_extract(detail, '$.rung') AS rung,
                    SUM(kind = 'recommended_shown') AS shown,
                    SUM(kind = 'recommended_open')  AS opened
               FROM interaction_events
              WHERE at >= ? AND kind IN ('recommended_shown', 'recommended_open')
              GROUP BY rung
              ORDER BY shown DESC, rung",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| OfferRate {
                rung: r.get::<Option<String>, _>("rung").unwrap_or_default(),
                shown: r.get("shown"),
                opened: r.get("opened"),
            })
            .collect())
    }
```

- [ ] **Step 6: Put it on Ops**

In `src/web/ui.rs`'s `ops` handler, add to the template struct and populate it
the way the neighbouring rollups are populated (`links`, `sweep_history`) —
including the same "a store that cannot answer must not take the page down"
treatment they use.

In `src/web/templates/ops.html`, after the sweep history table:

```html
{# Shown against clicked, by rung. The one number that can settle whether the
   weights the recommender rests on are right — they are chosen, not measured,
   and until this has data, fitting them would be guessing with extra steps.
   `[sitting] prime` has sat at `false` for months because nobody measured it;
   this is here so the same thing does not happen twice. #}
{% if !offer_rates.is_empty() %}
<h3>What was offered</h3>
<table class="grid">
  <thead><tr><th>Rung</th><th>Shown</th><th>Opened</th></tr></thead>
  <tbody>
  {% for r in offer_rates %}
    <tr><td>{{ r.rung }}</td><td>{{ r.shown }}</td><td>{{ r.opened }}</td></tr>
  {% endfor %}
  </tbody>
</table>
{% endif %}
```

- [ ] **Step 7: Run the tests, then everything**

Run: `cargo test --lib jobs::pursuit web::ui store::pursuits 2>&1 | tail -20`
Expected: PASS.

Run: `cargo fmt && cargo clippy --all-targets 2>&1 | tail -20 && cargo test 2>&1 | tail -10`
Expected: clean, all pass.

- [ ] **Step 8: Commit**

```bash
git add src/jobs/pursuit.rs src/web/ui.rs src/store/pursuits.rs \
        src/web/templates/ops.html
git commit -m "feat(recommend): what it offered does not teach it, and the hit rate is on the page"
```

---

### Task 11: The acceptance test, and the roadmap

The example the feature was asked for, end to end, and the entry that says what
was built.

**Files:**
- Modify: `src/core/recommend.rs` (the acceptance test)
- Modify: `ROADMAP.md`
- Test: in `src/core/recommend.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: nothing new.

- [ ] **Step 1: Write the acceptance test**

Append to `mod tests` in `src/core/recommend.rs`:

```rust
    #[tokio::test]
    async fn six_fridays_and_the_seventh_offers_it_before_it_is_asked_for() {
        // §10.5, and the whole feature in one test: seed six Fridays, set the
        // clock to the seventh at 14:52, send the phone bundle, and assert the
        // artifact comes back at rung Pattern with weekday, hour and device
        // named. Nothing here calls a model or an embedder — every step is the
        // production path with a fixed clock in it.
        let core = core_at(FRIDAY).await;
        let aid = seed_one_artifact(&core).await;
        let noise = seed_one_artifact(&core).await;

        // Six Fridays at 15:00 on the phone, opening `aid`.
        for w in 1..=6 {
            let at = FRIDAY - w * 7 * 86_400 - 52 * 60;
            let raw = serde_json::to_string(&phone()).unwrap();
            core.store
                .record_context(&crate::store::context::ContextEvent {
                    id: 0,
                    scope: Some("alice".into()),
                    at,
                    bundle: raw,
                    device_key: crate::core::context::device_key(&phone()),
                    local_hour: Some(15),
                    weekday: Some(4),
                    tz: Some("Europe/Berlin".into()),
                })
                .await
                .unwrap();
            core.store
                .record_interaction(&aid, "opened", None, Some("alice"), at + 5)
                .await
                .unwrap();
        }
        // And one Tuesday morning at a desk, opening something else — so the
        // answer is a choice rather than the only candidate.
        let mut desk = phone();
        desk.platform = Some("macOS".into());
        desk.touch = Some(false);
        desk.network = Some("wired".into());
        desk.orientation = Some("landscape".into());
        desk.viewport_w = Some(1920.0);
        desk.viewport_h = Some(1080.0);
        desk.screen_w = Some(2560.0);
        desk.screen_h = Some(1440.0);
        desk.dpr = Some(2.0);
        for w in 1..=6 {
            let at = FRIDAY - w * 7 * 86_400 - 3 * 86_400 - 7 * 3600;
            core.store
                .record_context(&crate::store::context::ContextEvent {
                    id: 0,
                    scope: Some("alice".into()),
                    at,
                    bundle: serde_json::to_string(&desk).unwrap(),
                    device_key: crate::core::context::device_key(&desk),
                    local_hour: Some(9),
                    weekday: Some(1),
                    tz: Some("Europe/Berlin".into()),
                })
                .await
                .unwrap();
            core.store
                .record_interaction(&noise, "opened", None, Some("alice"), at + 5)
                .await
                .unwrap();
        }

        let learned = crate::jobs::context::run(&core).await.unwrap();
        assert_eq!(learned.profiled, 2, "both situations were learned");

        // The seventh Friday, 14:52, on the phone.
        let offer = core.offer(Some("alice"), &phone(), None).await.unwrap().unwrap();
        assert_eq!(offer.artifact_id, aid, "the Friday thing, not the Tuesday one");
        assert_eq!(offer.rung, Rung::Pattern);
        assert!(offer.blocks.contains(&"weekday"), "{:?}", offer.blocks);
        assert!(offer.blocks.contains(&"hour"), "{:?}", offer.blocks);
        assert!(offer.blocks.contains(&"device"), "{:?}", offer.blocks);
        assert!(offer.at.is_some(), "and it quotes a Friday that happened");
    }
```

- [ ] **Step 2: Run it**

Run: `cargo test --lib core::recommend::tests::six_fridays 2>&1 | tail -30`
Expected: PASS.

If the rung lands on `Similar`, print `context_score` and the full
`contributions` list before touching `strong_at`: the likeliest cause is the
`viewport` block, whose two bundles above differ enough that the check is real.

- [ ] **Step 3: Update the roadmap**

In `ROADMAP.md`, add to the "What is built" paragraph, before
`Design records live in`:

```
a recommendation under the search box, learned from the situations an artifact
was opened in — the browser's time zone, local time, device, viewport, network
and power, clustered per artifact and stored as a `ctx` multivector scored with
`max_sim`, with the blocks that decided each offer named beneath it and shown
against clicked broken down by rung on Ops;
```

And under `[What the base says about itself]`, add:

```
- **The offer's hit rate is on Ops.** Shown against clicked, by rung. §6's
  block weights are chosen, not measured, and this is the instrument that would
  let them be fitted — fitting them before the data exists would be guessing
  with extra steps. It is here for the reason `[sitting] prime` is still
  `false`: a default nobody can see the effect of never moves.
```

And under whatever section holds deferred work:

```
- **Dropping the `scope` block.** At weight 10 it is the only thing keeping one
  person's situations from being offered to another, because every scope shares
  one collection and a payload filter cannot act on elements of a multivector
  set. It goes to 0 when each user has their own collection, and nothing else
  about the encoder changes.
- **Learned block weights.** Once the shown/clicked rate has months behind it.
- **Conjunctions across scopes.** The vector can hold them; nothing yet learns
  which ones matter.
```

- [ ] **Step 4: Everything, one last time**

Run: `cargo fmt && cargo clippy --all-targets 2>&1 | tail -20 && cargo test 2>&1 | tail -15`
Expected: clean, all pass.

- [ ] **Step 5: Commit**

```bash
git add src/core/recommend.rs ROADMAP.md
git commit -m "feat(recommend): the seventh Friday, and what the roadmap now says about it"
```

---

## What this is not allowed to break — the checklist to run at the end

Each of these has a test above. Confirm each one is green by name before calling
the branch done.

| Rule | Test |
|---|---|
| Payload survives a sweep write | Task 6: `set_context_vectors` uses `points/vectors`, never `upsert` — read the diff, not just the tests |
| Ranking is untouched | No file under `src/core/search.rs` is modified by any task above |
| Read-time cost | Task 8: `offer` makes one `context_query` and no embedder call |
| `scope` isolation | `core::context::two_people_in_the_same_situation_are_still_far_apart`, `core::recommend::one_persons_situations_are_never_offered_to_another` |
| The reason matches the hit | `core::recommend::the_reason_explains_the_artifact_that_was_offered` |
| The guard | `jobs::context::an_open_of_something_this_offered_raises_no_weight`, `web::ui::taking_an_offer_is_not_an_ordinary_open`, `jobs::pursuit::an_offer_that_was_only_shown_is_not_engagement` |
| The ladder never lies | `core::recommend::{a_resemblance_is_not_called_a_pattern, with_no_sitting_either_it_falls_to_what_has_been_forgotten}` |

**One operational note for whoever ships this:** a Qdrant collection created
before Task 6 carries no `ctx` named vector, and the sweep's first write to it
will fail with a 400 that `sweep_runs` will record as a failed run. `--reindex`
is the migration — it creates a fresh generation from the new collection body
and copies every dense vector across at no embedding cost. Run it before
switching `[recommend] enabled` on.
