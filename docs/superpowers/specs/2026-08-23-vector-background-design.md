# Vector-DB Background Visualization — Design

Date: 2026-08-23
Status: approved by user (2026-08-23)

## Goal

A subtle, monochrome, slowly rotating 3D point-cloud visualization of the **actual
contents of the vector database**, rendered as a fixed background behind the engram
web UI. It must be seeded from real vector data, look "cyber-tech", and have
near-zero processing/battery cost. It must never interfere with the UI (no
interaction, no layout impact, no visible errors).

Reference: the removed ScalarForensic feature (commit `e37d349`, file
`src/scalar_forensic/web/static/viz.js` at parent `8cddbef`) — Canvas-2D
perspective-projected point cloud from real Qdrant vectors. We adopt its rendering
core, stripped down and tuned for subtlety and battery.

## User decisions (from brainstorming)

- Data binding: snapshot fetched **once per 6 hours** in the background;
  **2000 real vectors**, sample size configurable.
- Pages: **authenticated pages only** (login screen excluded). The sampling is
  scoped to the authenticated session's collection, so a future multi-user setup
  is naturally per-user.
- Aesthetic: similar to the ScalarForensic viz, but **monochrome**, more in the
  background, more battery-friendly.
- Motion: true 3D rotation via a throttled render loop (not a CSS billboard tilt).

## Architecture

Three pieces: a server-side sample+project endpoint, a client-side render module,
and a CSS placement layer.

### 1. Server: sample endpoint

- New trait method on `VectorStore` (`src/vector/mod.rs:216`):
  `sample(limit: usize) -> Vec<(String /* artifact_id */, Vec<f32> /* dense */)>`.
  - Qdrant impl (`src/vector/qdrant.rs`): paged scroll with `with_vector: true`,
    reusing the `ScrolledPoint`/`dense_of()` helpers (`qdrant.rs:327,449`) and the
    paging pattern from `all_artifact_ids` (`qdrant.rs:1395`). Scroll offset chosen
    so the sample is not always the first page (random start page or spread across
    pages).
  - Memory impl (`src/vector/memory.rs`): trivial, for tests.
- New route `GET /api/v1/vectors/sample` in `src/web/api.rs` (registered in
  `api_router()`, `api.rs:1090`). Session/token-authed like the other API routes;
  the 401→login middleware already skips `/api/` paths (`src/web/mod.rs:47-52`).
- Handler pipeline:
  1. `sample(configured sample_size)` dense vectors.
  2. Project each 768-dim (config-driven dim) vector to 3D with a **fixed
     seeded random projection** matrix (3×dim, generated from a constant seed;
     Johnson–Lindenstrauss — clusters remain clusters). Deterministic across
     requests so repeated fetches look stable.
  3. Normalize projected points to fit the unit sphere (divide by max radius).
  4. Return `{ "points": [[x,y,z], ...], "count": n }` with coordinates rounded
     to 4 decimals — ~40–60 KB JSON for 2000 points.
- Empty collection or sampling failure → `200 {"points":[],"count":0}`; the
  client renders nothing. Never a user-visible error.

### 2. Config

New sub-config following the `#[serde(default)]` pattern in `src/config.rs`:

```toml
[ui.background]
enabled = true        # master toggle
sample_size = 2000    # vectors sampled per snapshot
refresh_secs = 21600  # client-side cache TTL (6 h)
```

### 3. Client: render module in `assets/app.js`

~150 lines appended to `assets/app.js` (ES5 IIFE style, no deps, no build step,
matching existing code). Adapted from the reference `viz.js`, minus interaction,
sparks, traversers, and dual colored clouds.

- **Placement**: JS injects `<canvas id="vec-bg" aria-hidden="true">` as the first
  child of `body`, fixed-positioned behind `.shell`. No-JS clients are unaffected.
- **Fetch & cache**: on page load, read `localStorage('engram.vbg')`
  (`{ts, points}`). If younger than `refresh_secs`, render from cache with zero
  network. Otherwise fire a low-priority background `fetch` of the sample
  endpoint, render on arrival, update cache. The endpoint response includes the
  configured `refresh_secs` so the client TTL always matches server config
  (`{ "points": [...], "count": n, "refresh_secs": 21600 }`). All failures
  (offline, 401, empty) → render nothing, no console spam beyond one debug line.
- **Auth-only pages**: the module exits early unless
  `document.body.dataset.vbg === "1"`. `layout.html` emits `data-vbg="1"` on
  `<body>` only for authenticated pages (login and other public pages omit it).
- **Rendering** (from reference viz.js):
  - Perspective projection, camera orbiting origin on sphere (theta/phi), unit
    scaled to `min(W,H)`.
  - Points batched into 8 depth buckets → 8 `fill()` calls per frame; alpha and
    radius scale with depth. Single monochrome color read from the computed value
    of `--color-fg-muted`, so both light and dark themes work with no per-theme
    code. Transparent canvas (theme base color shows through; no painted
    background gradient).
  - Faint 3D axes with tick marks (reference `drawAxes`) at very low opacity —
    the "technical" signature element.
  - Slow auto-spin with randomly changing rotation axis every 6–14 s
    (reference `pickAxis`); no mouse/touch/wheel handlers at all.
- **Battery discipline**:
  - rAF loop throttled to ~12 fps (frame-skip by timestamp).
  - Loop runs only while `document.visibilityState === 'visible'` (rAF pauses on
    hide anyway; explicit check for the throttled path).
  - `prefers-reduced-motion: reduce` → render exactly one static frame, no loop.
  - No `shadowBlur`, no glow sprites, no per-point draw calls.
  - Re-render triggers outside the loop: window resize (ResizeObserver, canvas
    buffer synced to CSS size), theme toggle (existing hook at `app.js:605` /
    `themeToggle()` re-reads the color and repaints current frame).
- Cap devicePixelRatio at ~1.5 for the canvas buffer to bound fill cost on phones.

### 4. CSS layer

New `assets/css/05-background.css` (concatenated by `build.rs` in filename order;
the content-hash asset stamp auto-busts caches):

- `#vec-bg { position: fixed; inset: 0; width: 100%; height: 100%;
  pointer-events: none; z-index: 0; }` and `.shell { position: relative;
  z-index: 1; }` (or equivalent) so the canvas sits behind all content.
- Overall opacity tuned low (≈0.5) so text contrast is unaffected; final value
  verified visually in both themes.

## Data flow

```
Qdrant collection --(scroll, with_vector)--> sample() --> seeded random
projection 768→3 --> normalize --> JSON [[x,y,z]×2000] --> client fetch
(≤1 per 6h, localStorage cache) --> canvas 2D perspective render @12fps
```

## Error handling

- Server: empty/failed sample → empty `points` array, HTTP 200.
- Client: any fetch/parse failure → no canvas content, no retry storm (retry at
  next page load only).
- Config `enabled = false` → endpoint returns empty; client module exits early.

## Non-interference guarantees

- `pointer-events: none`, behind content, `aria-hidden`.
- Fixed positioning, injected after `DOMContentLoaded` — no layout shift.
- Only runs on authenticated pages.
- Respects `prefers-reduced-motion`; pauses in hidden tabs.

## Testing

- Unit tests for the projection helper (deterministic for fixed seed; output
  bounded to unit sphere).
- Trait-level test for `VectorStore::sample` on the memory impl.
- Endpoint test following existing API test patterns: authed request returns
  bounded coordinates and correct count; empty store returns empty array.
- `tests/test_static_wiring`-style check (if such a pattern exists for assets)
  that `05-background.css` is picked up by `build.rs` and the canvas id appears
  in `app.js`. Manual visual check in both themes and with
  `prefers-reduced-motion` emulated.

## Out of scope (YAGNI)

- No WebGL, no interaction (drag/zoom), no glow/spark/traverser effects, no
  per-point payload/metadata display, no KDE-wallpaper-style export, no live
  streaming of DB changes, no multi-collection color coding (single collection
  today).
