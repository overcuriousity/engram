# Situation Vocabulary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The context bundle the browser posts to `/ui/context` grows every prompt-free signal that describes the moment, plus place behind one permission, under a vocabulary the Android app will share — with nothing encoded and nothing about ranking touched.

**Architecture:** `Bundle` in `src/core/context.rs` is the vocabulary: every field the browser or the phone may send is an `Option` on it. `app.js` fills what a browser can and writes `null` for what only a phone can, so the existing struct-versus-JS test keeps holding. A fixture file under `android/core/src/test/resources/` lists the same names for the Kotlin side, and a Rust test keeps it equal to the struct. `device_key` and `BLOCKS` are not touched.

**Tech Stack:** Rust (serde), the existing test in `context.rs`, vanilla JS in `assets/app.js`, askama in `settings.html`.

**Spec:** `docs/superpowers/specs/2026-09-16-android-app-foundation-design.md`, section *Situation*.

## Global Constraints

- Branch: `feat/web-push`. Never rebase or touch the commits already on it.
- No change to `BLOCKS`, `CTX_DIM`, `LAYOUT_VERSION`, `encode`, `device_key`, or `[recommend.weights]`. No test on ranking changes.
- Every new `Bundle` field is `Option<T>` (or `Vec` for lists), `#[serde(default)]` inherited from the struct, and carries a one-line doc comment saying what it is and that no block reads it yet.
- Field names are `snake_case`, identical in Rust, JS and the fixture file.
- The place field is a 6-character geohash and is only ever set when the person switched it on; the switch lives in `localStorage` under `engram:place`.
- Canvas, WebGL, font and plugin enumeration are not collected.
- UI copy on the settings page is a term and a short gloss, never an explanatory sentence.
- Every commit message ends with the evidence line (which test command, how many passed) and `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`.

---

### Task 1: `Bundle` grows the vocabulary, and the fixture file pins it

**Files:**
- Modify: `src/core/context.rs` (the `Bundle` struct, after `audio_outputs`, and the tests module)
- Create: `android/core/src/test/resources/bundle-fields.txt`

**Interfaces:**
- Produces: `Bundle` with the fields listed below, `parse_bundle` unchanged, `device_key` unchanged.

- [ ] **Step 1: Write the failing tests**

In the `tests` module of `src/core/context.rs`, after `a_device_key_is_stable_and_ignores_the_situation`, add:

```rust
    /// Every name the struct reads, off the source. Shared by the two tests
    /// below and by the app.js test above, which reads it the same way.
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
        // The third sender has a compiler of its own and no way to read this
        // file: the Kotlin tests read a fixture instead, and this is what keeps
        // the fixture honest. One name per line, sorted, nothing else.
        let fixture = include_str!("../../android/core/src/test/resources/bundle-fields.txt");
        let listed: std::collections::BTreeSet<&str> =
            fixture.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        assert_eq!(
            listed,
            bundle_field_names(),
            "bundle-fields.txt and Bundle disagree about what a situation is"
        );
    }

    #[test]
    fn the_device_key_ignores_every_field_that_describes_the_moment() {
        // The key hashes six stable fields and must stay deaf to the rest —
        // a phone that walked into another room, plugged in a headset and
        // turned on do-not-disturb is the same phone.
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
```

- [ ] **Step 2: Run, expect compile failure on the unknown fields**

Run: `cargo test context::tests::the_device_key_ignores -- --nocapture`
Expected: compile error, `no field place on type Bundle`.

- [ ] **Step 3: Add the fields to `Bundle`**

In `src/core/context.rs`, directly after `pub audio_outputs: Option<u32>,` and before the closing `}` of `Bundle`, add:

```rust
    // ── Stored, not encoded. ───────────────────────────────────────────────
    // Every field below is written to `context_events.bundle` whole and read
    // by no block yet. A block that wants one is a layout change: it bumps
    // `LAYOUT_VERSION`, and the sweep rebuilds every profile from these raw
    // rows. Collecting first and encoding later is what whole-bundle storage
    // was built for. `device_key` reads none of them — they all describe the
    // moment, and none describe the machine.

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
    // Phone only: a browser writes `null` for these, so that the app.js test
    // above still sees every name.
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
```

- [ ] **Step 4: Write the fixture**

Create `android/core/src/test/resources/bundle-fields.txt` with every field name of `Bundle` — the original nineteen and the thirty-six above — one per line, sorted. Generate it rather than typing it:

```bash
sed -n '/pub struct Bundle {/,/^}/p' src/core/context.rs \
  | grep -oE '^\s+pub [a-z_0-9]+' | awk '{print $2}' | sort \
  > android/core/src/test/resources/bundle-fields.txt
wc -l android/core/src/test/resources/bundle-fields.txt   # expect 55
```

- [ ] **Step 5: Run the three new tests and the existing app.js test**

Run: `cargo test context::tests::`
Expected: the three new tests pass; `the_browser_sends_exactly_the_fields_this_struct_reads` **fails** — app.js does not send the new names yet. That failure is Task 2's starting point. Do not commit yet.

---

### Task 2: `app.js` collects the web column and writes `null` for the phone's

**Files:**
- Modify: `assets/app.js` (the `slow` object, `primeSlow`, `window.engramContext`; new helpers beside `netKind`)
- Modify: `src/web/templates/settings.html` (a row under *Notifications*, before `{% match feedback %}`)
- Test: the existing `the_browser_sends_exactly_the_fields_this_struct_reads` in `src/core/context.rs`; `cargo test web::ui::tests::a_page_view_is_recorded`

**Interfaces:**
- Consumes: the `Bundle` field names from Task 1.
- Produces: `window.engramContext()` returning JSON with all 55 keys; `localStorage['engram:place']` = `"on"` when the switch is on; `localStorage['engram:last_view']` and `localStorage['engram:views']` for the two counters.

- [ ] **Step 1: Confirm the failing test**

Run: `cargo test context::tests::the_browser_sends_exactly_the_fields_this_struct_reads`
Expected: FAIL, the diff of names naming the new fields.

- [ ] **Step 2: Extend the slow primes**

Replace `var slow = { battery_level: null, charging: null, audio_outputs: null };` and `primeSlow` with:

```js
  var slow = {
    battery_level: null, charging: null,
    audio_outputs: null, audio_inputs: null, video_inputs: null,
    keyboard_layout: null, screens: null, place: null
  };

  function primeSlow() {
    if (navigator.getBattery) {
      navigator.getBattery().then(function (b) {
        slow.battery_level = b.level;
        slow.charging = b.charging;
      }).catch(function () {});
    }
    if (navigator.mediaDevices && navigator.mediaDevices.enumerateDevices) {
      navigator.mediaDevices.enumerateDevices().then(function (list) {
        var n = { audiooutput: 0, audioinput: 0, videoinput: 0 };
        list.forEach(function (d) { if (d.kind in n) n[d.kind] += 1; });
        slow.audio_outputs = n.audiooutput;
        slow.audio_inputs = n.audioinput;
        slow.video_inputs = n.videoinput;
      }).catch(function () {});
    }
    // Chromium only. The map's own keys are layout-specific; the letter under
    // the physical `KeyQ` is enough to tell QWERTY from QWERTZ from AZERTY.
    if (navigator.keyboard && navigator.keyboard.getLayoutMap) {
      navigator.keyboard.getLayoutMap().then(function (m) {
        slow.keyboard_layout = m.get('KeyQ') + m.get('KeyY') + m.get('KeyA');
      }).catch(function () {});
    }
    if (window.getScreenDetails && screen.isExtended) {
      // Prompts for window-management on some builds; `screen.isExtended`
      // does not, and is the fact that matters. Two or more, or one.
      slow.screens = 2;
    } else {
      slow.screens = 1;
    }
    primePlace();
  }

  // Place is the one field behind a permission, and the switch is the only
  // way it is ever asked for. Low accuracy, ten minutes of cache, and the
  // position is reduced to a geohash here — the coordinates never leave.
  function primePlace() {
    var on = false;
    try { on = localStorage.getItem('engram:place') === 'on'; } catch (e) {}
    if (!on || !navigator.geolocation) return;
    navigator.geolocation.getCurrentPosition(function (pos) {
      slow.place = geohash(pos.coords.latitude, pos.coords.longitude, 6);
    }, function () {}, { enableHighAccuracy: false, maximumAge: 600000, timeout: 8000 });
  }

  // Standard geohash, base32 alphabet, `precision` characters.
  function geohash(lat, lon, precision) {
    var chars = '0123456789bcdefghjkmnpqrstuvwxyz';
    var latR = [-90, 90], lonR = [-180, 180];
    var out = '', bit = 0, ch = 0, even = true;
    while (out.length < precision) {
      if (even) {
        var midLon = (lonR[0] + lonR[1]) / 2;
        if (lon >= midLon) { ch = (ch << 1) | 1; lonR[0] = midLon; }
        else { ch = ch << 1; lonR[1] = midLon; }
      } else {
        var midLat = (latR[0] + latR[1]) / 2;
        if (lat >= midLat) { ch = (ch << 1) | 1; latR[0] = midLat; }
        else { ch = ch << 1; latR[1] = midLat; }
      }
      even = !even;
      if (++bit === 5) { out += chars.charAt(ch); bit = 0; ch = 0; }
    }
    return out;
  }
```

- [ ] **Step 3: Add the small readers beside `netKind`**

After `netKind`, add:

```js
  function media(q) {
    try { return matchMedia(q).matches; } catch (e) { return null; }
  }

  function navType() {
    var e = performance.getEntriesByType && performance.getEntriesByType('navigation')[0];
    return e && e.type ? e.type : null;
  }

  function referrerKind() {
    if (!document.referrer) return 'none';
    try {
      return new URL(document.referrer).origin === location.origin ? 'same_origin' : 'external';
    } catch (e) { return 'external'; }
  }

  // Two counters this browser keeps about itself: how long since it last
  // built a bundle, and how many it has built since its local midnight. A
  // visit after a week away and the ninth visit of a morning are different
  // situations, and nothing else in the bundle says so.
  function viewCounters() {
    var now = Date.now();
    var out = { since_last_view_s: null, views_today: null };
    try {
      var last = parseInt(localStorage.getItem('engram:last_view') || '0', 10) || 0;
      if (last) out.since_last_view_s = Math.round((now - last) / 1000);
      var day = localDay(now);
      var raw = (localStorage.getItem('engram:views') || '').split(':');
      var n = raw[0] === day ? (parseInt(raw[1], 10) || 0) : 0;
      n += 1;
      out.views_today = n;
      localStorage.setItem('engram:last_view', String(now));
      localStorage.setItem('engram:views', day + ':' + n);
    } catch (e) {}
    return out;
  }
```

`localDay` already exists further down the file; it is a function declaration, so it is hoisted and callable here.

- [ ] **Step 4: Fill the bundle**

In `window.engramContext`, after `b.audio_outputs = slow.audio_outputs;` and before the `} catch (e) {`, add:

```js
      // ── Stored, not encoded. See the Bundle struct in core/context.rs. ──
      var conn = navigator.connection || navigator.mozConnection || {};
      b.place = slow.place;
      b.net_effective = conn.effectiveType || null;
      b.net_downlink = typeof conn.downlink === 'number' ? conn.downlink : null;
      b.rtt = typeof conn.rtt === 'number' ? conn.rtt : null;
      b.save_data = typeof conn.saveData === 'boolean' ? conn.saveData : null;
      b.reduced_motion = media('(prefers-reduced-motion: reduce)');
      b.high_contrast = media('(prefers-contrast: more)');
      b.pointer = media('(pointer: coarse)') ? 'coarse' : (media('(pointer: fine)') ? 'fine' : null);
      b.hover = media('(hover: hover)');
      b.display_mode = media('(display-mode: standalone)') ? 'standalone' : 'browser';
      b.nav_type = navType();
      b.window_state = (window.outerWidth >= screen.availWidth - 8 && window.outerHeight >= screen.availHeight - 8)
        ? 'maximised' : 'windowed';
      b.focused = document.hasFocus();
      var counters = viewCounters();
      b.since_last_view_s = counters.since_last_view_s;
      b.views_today = counters.views_today;
      b.screen_x = window.screenX;
      b.screen_y = window.screenY;
      b.screens = slow.screens;
      b.avail_w = screen.availWidth;
      b.avail_h = screen.availHeight;
      b.video_inputs = slow.video_inputs;
      b.audio_inputs = slow.audio_inputs;
      b.zoom = window.visualViewport ? window.visualViewport.scale : null;
      b.fullscreen = !!document.fullscreenElement;
      b.referrer_kind = referrerKind();
      b.online = navigator.onLine;
      b.keyboard_layout = slow.keyboard_layout;
      b.hour_cycle = Intl.DateTimeFormat().resolvedOptions().hourCycle || null;
      // A browser cannot read these. The app fills them; they are listed here
      // so that the test in context.rs sees the whole vocabulary on this side.
      b.audio_route = null;
      b.dnd = null;
      b.ringer = null;
      b.power_save = null;
      b.brightness = null;
      b.lux = null;
      b.docked = null;
      b.headset = null;
```

- [ ] **Step 5: The place switch on the settings page**

In `src/web/templates/settings.html`, directly before `{% match feedback %}`, add:

```html
<h2>Place</h2>
<p class="muted">A one-kilometre cell in the situation the offer card learns from. Asked for once; never stored more finely.</p>
<label class="row" style="gap:0.5rem;align-items:center">
  <input type="checkbox" id="place-switch"> Send my place
</label>
```

And in `assets/app.js`, next to the other `DOMContentLoaded` wiring (find `contextOffer();` where it is called at load and add after it):

```js
    placeSwitch();
```

with, beside `primePlace`:

```js
  function placeSwitch() {
    var el = document.getElementById('place-switch');
    if (!el) return;
    try { el.checked = localStorage.getItem('engram:place') === 'on'; } catch (e) {}
    el.addEventListener('change', function () {
      try { localStorage.setItem('engram:place', el.checked ? 'on' : 'off'); } catch (e) {}
      // Asking now, while the person is looking at the switch they pressed,
      // is the one moment a permission prompt makes sense.
      if (el.checked) primePlace();
    });
  }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test context::tests:: && cargo test web::ui::tests::a_page_view_is_recorded && cargo test web::ui::tests::settings`
Expected: all pass, including `the_browser_sends_exactly_the_fields_this_struct_reads`.

- [ ] **Step 7: Check the page in a browser**

Run the server against a scratch config, open `/ui`, and in the console run `JSON.parse(engramContext())`. Expected: an object with 55 keys, the phone-only ones `null`, `views_today` at least 1, `place` `null` until the switch is on. Turn the switch on under Settings, allow the prompt, reload, and check `place` is six lowercase characters.

- [ ] **Step 8: Commit Tasks 1 and 2 together**

```bash
git add src/core/context.rs assets/app.js src/web/templates/settings.html android/core/src/test/resources/bundle-fields.txt
git commit -m "feat(context): the bundle takes every prompt-free signal, and place behind a switch

The struct is the vocabulary; app.js fills the web column and writes null for
what only a phone can read; bundle-fields.txt carries the same names to the
Kotlin tests. Nothing is encoded, the device key still hashes six fields, and
ranking is untouched.

Evidence: cargo test context:: — N passed; cargo test web::ui:: — M passed.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 3: A posted wide bundle is stored whole

**Files:**
- Modify: `src/web/ui.rs` (tests module, beside `a_page_view_is_recorded_even_when_nothing_is_offered`)

**Interfaces:**
- Consumes: `form(path, cookie, body)` helper already in the tests module, `store.context_events_since(0)`.

- [ ] **Step 1: Write the test**

```rust
    #[tokio::test]
    async fn a_wide_bundle_is_stored_whole_and_read_by_no_block() {
        // The new fields are stored, not encoded: the row carries them
        // verbatim, and the encoder's output is the same with or without.
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        core.learn.enabled = true;
        let store = core.store.clone();
        let background = core.background.clone();
        let weights = core.recommend.weights.clone();
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        let narrow = r#"{"tz":"Europe/Berlin","platform":"Android"}"#;
        let wide = r#"{"tz":"Europe/Berlin","platform":"Android","place":"u33dc0","audio_route":"car","dnd":true}"#;
        let res = app
            .clone()
            .oneshot(form(
                "/ui/context",
                &cookie,
                &format!("bundle={}", urlencoding::encode(wide)),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        background.wait_idle().await;

        let rows = store.context_events_since(0).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].bundle.contains("u33dc0"), "stored whole");
        assert!(rows[0].bundle.contains("\"audio_route\":\"car\""));

        let at = 1_700_000_000;
        let a = crate::core::context::encode(at, &crate::core::context::parse_bundle(narrow), &weights);
        let b = crate::core::context::encode(at, &crate::core::context::parse_bundle(wide), &weights);
        assert_eq!(a, b, "no block reads the new fields");
    }
```

If `urlencoding` is not a dependency, percent-encode by hand: replace `{`→`%7B`, `}`→`%7D`, `"`→`%22`, `:`→`%3A`, `,`→`%2C`, `/`→`%2F` with a small local `fn enc(s: &str) -> String` in the test module — check `Cargo.toml` first.

- [ ] **Step 2: Run**

Run: `cargo test web::ui::tests::a_wide_bundle_is_stored_whole`
Expected: PASS without implementation changes. If it fails on `encode` inequality, a block is reading a new field and that is a spec violation to fix in Task 1, not here.

- [ ] **Step 3: Commit**

```bash
git add src/web/ui.rs
git commit -m "test(context): a wide bundle is stored whole and encodes the same as a narrow one

Evidence: cargo test web::ui::tests::a_wide_bundle — 1 passed.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

- [ ] **Step 4: Full run, in the background, reported after**

Run: `cargo test 2>&1 | tail -5 && cargo clippy --all-targets && cargo fmt --check`
Expected: all green. Report the counts.
