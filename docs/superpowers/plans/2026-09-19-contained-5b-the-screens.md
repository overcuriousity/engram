# Contained mode, part 5b — the screens — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A person can choose where their engram lives, fetch what contained mode needs, switch between the modes, manage models, and decide how Ask is answered on the phone.

**Architecture:** Every new screen is a stateless composable that is handed what it shows and tells its caller what was pressed, as `Rail` and `ReadFrame` are, so each is tested on the JVM with the Compose rule. The stateful wrappers beside them read `Engram` and `Downloads`. Switching mode does not restart the process: `Engram.switch` shuts the old instance and builds a new one, and `MainActivity` keys the whole composition on the instance.

**Tech Stack:** Compose Material 3, Robolectric with the Compose test rule, WorkManager (through 5a's `Downloads`).

**Spec:** `docs/superpowers/specs/2026-09-18-android-contained-mode-design.md`, section 4.

## Global Constraints

- Commands run from `android/`. Baseline: 278 tests, 5 skipped, 0 failed.
- UI copy is a term and a short gloss, never an explanatory sentence. Every string a person reads is listed in this plan; none is added while implementing.
- Sizes are shown in decimal megabytes or gigabytes, one decimal from 1 GB: `334 MB`, `1.3 GB`.
- "On this phone" is not offered where `Core.available` is false.
- An install that holds a connection never sees the chooser. A pairing code arriving by scan or link goes straight to pairing.
- Judging, pairs and gaps stay where they are. Nothing new is badged, and nothing new goes on home.
- The speech model is not offered anywhere; that is part 2b. No background line in Settings; that is part 6.
- No device testing. The risk, stated once: nothing here has been touched by a finger.

## What reading the code found

1. **A process restart is not needed to switch.** Part 4 left it as the fallback. Every screen takes `engram` as a parameter and every worker calls `Engram.get`, so replacing the instance and re-keying the composition is enough, and it can be tested.
2. **The status endpoint cannot say whether Ask will work.** The contained config always has an ask role, pointed at a dead port when there is no model, so `asks` is true either way. Whether to show the offer is the app's to know: contained, ask set to `device`, no ask model installed.
3. **`App.engram` is a `lateinit var` read from receivers and activities.** It becomes a getter over `Engram.get`, so nobody holds a stale one past a switch.

## File structure

- Modify `core/.../Engram.kt` — `current`, `switch`, `shutdown`, `requiredMissing`, `askWantsAModel`, `metered`.
- Modify `core/.../contained/Contained.kt` — `stop()`.
- Create `app/.../ui/Sizes.kt` — `sizeWords(bytes)`.
- Create `app/.../ui/ModeChooser.kt` — the first-start choice.
- Create `app/.../ui/ModelRows.kt` — `ModelRow`, `MeteredDialog`, and the stateful `ModelLine` over `Downloads`.
- Create `app/.../ui/DownloadScreen.kt` — the retrieval set, before contained mode opens.
- Create `app/.../ui/AskOffer.kt` — the three-way offer, and the "off" state.
- Create `app/.../ui/ModeSettings.kt` — Settings' "Mode", "Models" and "Ask" sections.
- Modify `app/.../ui/Nav.kt`, `SettingsScreen.kt`, `AskScreen.kt`, `App.kt`, `MainActivity.kt`.
- Tests: `core/.../EngramModesTest.kt` (switching), `app/.../ui/SizesTest.kt`, `app/.../ui/ModeScreensTest.kt`.

---

### Task 1: One instance at a time, and a way to change it

**Interfaces — Produces:** `Engram.current: StateFlow<Engram?>`; `suspend fun Engram.Companion.switch(context: Context, mode: Mode): Engram`; `suspend fun Engram.shutdown()`; `Contained.stop()`; `fun Engram.requiredMissing(): List<Model>`; `val Engram.askWantsAModel: Boolean`; `val Engram.metered: Boolean`. `Engram.build` is an internal factory the companion uses, so a test can switch with its fakes.

- [ ] **Step 1: Failing tests** in `EngramModesTest`:
  - `switchingStopsTheCoreAndLeavesBothBasesWhereTheyWere` — contained instance `ready()`, enqueue a text; `shutdown()` calls the injected halt exactly once and closes the database; a server instance built after it sees an empty outbox; a contained one built after that sees the row again.
  - `whatContainedModeCannotOpenWithoutIsTheEmbedder` — `requiredMissing()` is `ModelManifest.required` until the sparse file exists, then empty; always empty in server mode.
  - `askWantsAModelOnlyWhereOneWouldBeUsed` — true in contained with `AskVia.device` and no file; false once installed, false for `endpoint` and `off`, false in server mode.
- [ ] **Step 2: Implement.**

```kotlin
    /** What contained mode cannot open without, and does not have. Empty in server mode. */
    fun requiredMissing(): List<Model> = if (contained == null) emptyList() else ModelManifest.required.filterNot(::installed)

    /** Ask is set to the phone and the phone has nothing to answer with: the moment for the offer. */
    val askWantsAModel: Boolean get() = contained != null && modes.ask == AskVia.device && state.models().ask == null

    val metered: Boolean get() = app.getSystemService(ConnectivityManager::class.java)?.isActiveNetworkMetered ?: false

    /** The end of this instance: the core stopped, the database closed. Nothing may use it afterwards. */
    suspend fun shutdown() { contained?.stop(); db.close() }
```

`Contained.stop()` takes the mutex, clears `connected`, sets `Idle`, runs `halt` on `Dispatchers.IO`. The companion:

```kotlin
        private val _current = MutableStateFlow<Engram?>(null)
        /** The instance in use. It changes when the mode does, and whatever draws from one re-draws from the next. */
        val current: StateFlow<Engram?> get() = _current

        fun get(context: Context): Engram = instance ?: synchronized(this) {
            instance ?: build(context.applicationContext).also { instance = it; _current.value = it }
        }

        /**
         * Store the mode and become an engram built for it. The two modes share
         * nothing, so there is nothing to carry over: the old instance is shut
         * and a new one reads the new mode, as a fresh process would.
         */
        suspend fun switch(context: Context, mode: Mode): Engram {
            val old = get(context)
            if (old.mode == mode && old.modes.chosen == mode) return old
            Sync.cancel(context)
            old.modes.chosen = mode
            old.shutdown()
            return synchronized(this) { build(context.applicationContext).also { instance = it; _current.value = it } }
        }
```

- [ ] **Step 3:** `--tests '*EngramModesTest*'` passes. Commit `feat(android): the mode can change without the process ending`.

---

### Task 2: The stateless screens

**Copy, complete.** Nothing else is written on these screens.

| Where | Text |
|---|---|
| Chooser, heading | `Where your engram lives` |
| Chooser, first choice | `On this phone` · gloss `private · works offline · 334 MB to fetch` (the size is `sizeWords` of `ModelManifest.required`) |
| Chooser, second choice | `With a server` · gloss `pair with an engram you run` |
| Chooser, where the core is absent | first choice not drawn |
| Download, heading | `On this phone` |
| Model row | `<name>` · gloss `<size> · <licence>`; the licence is a link where the model has `terms` |
| Model row, states | button `Download`; `Waiting for Wi-Fi`; `<done> of <size>` with `Cancel`; `Installed` with `Remove`; `failed · <error>` with `Retry` |
| Metered dialog | title `Metered network · <size>`; buttons `Download anyway`, `Wait for Wi-Fi` |
| Download, leaving | text button `With a server instead` |
| Ask offer, heading | `Ask · no model yet` |
| Ask offer, rows | the ask models as model rows; `Use an endpoint` · gloss `any OpenAI-compatible server`; `Leave Ask off` |
| Ask, off | `Ask · off` and a text button `Settings` |
| Settings, Mode | `On this phone` or the server's lines as today; button `Switch to a server` / `Switch to this phone` |
| Switch dialog | title `Switch to a server?` / `Switch to this phone?`; text `Separate bases · nothing is copied`; buttons `Switch`, `Stay` |
| Settings, Models | one model row per manifest entry whose role is `embed` or `ask` |
| Settings, Ask | chips `On this phone`, `Endpoint`, `Off`; fields `Base URL`, `Model`, `API key`; button `Save`; after saving `Saved.` |

**Interfaces — Produces:**
- `fun sizeWords(bytes: Long): String`
- `@Composable fun ModeChooser(onPhoneOffered: Boolean, fetch: String, onPhone: () -> Unit, onServer: () -> Unit)`
- `@Composable fun ModelRow(model: Model, progress: Progress, installed: Boolean, onDownload: () -> Unit, onCancel: () -> Unit, onRemove: () -> Unit)`
- `@Composable fun MeteredDialog(size: String, onAnyway: () -> Unit, onWait: () -> Unit, onDismiss: () -> Unit)`
- `@Composable fun AskOfferPane(models: @Composable () -> Unit, onEndpoint: () -> Unit, onOff: () -> Unit)`, `@Composable fun AskOffPane(onSettings: () -> Unit)`
- `@Composable fun SwitchDialog(to: Mode, onSwitch: () -> Unit, onStay: () -> Unit)`

- [ ] **Step 1: Failing tests.** `SizesTest`: `334 MB` for 333 590 944, `1.3 GB` for 1 280 835 840, `2.7 GB`, `190 MB`, `0 MB`. `ModeScreensTest`, with the Compose rule:
  - `theChooserOffersBothAndSaysWhatThePhoneCosts` — both choices exist, the gloss contains `334 MB`, each press reaches its callback.
  - `whereTheCoreIsAbsentOnlyAServerIsOffered` — `On this phone` does not exist.
  - `aModelRowSaysWhatItIsAndWhose` — name, `334 MB · Gemma Terms of Use`, `Download` press counted.
  - `aRunningDownloadShowsHowFarAndCanBeCancelled` — `167 MB of 334 MB`, `Cancel`.
  - `aWaitingDownloadSaysWhatItWaitsFor`, `anInstalledModelCanBeRemoved`, `aFailedDownloadSaysWhyAndCanBeRetried`.
  - `theMeteredDialogNamesTheSizeAndBothWaysOn`.
  - `switchingSaysOnceThatNothingIsCopied` — for both directions.
  - `theAskOfferHasThreeWaysOn`, `askOffSaysSoAndPointsAtSettings`.
- [ ] **Step 2: Implement** the composables in the files named above, in the idiom of `SettingsScreen`'s `Section` and `Line` (which move to `internal` so the new files use them rather than grow a second pair).
- [ ] **Step 3:** `./gradlew :app:testDebugUnitTest --tests '*SizesTest*' --tests '*ModeScreensTest*'` passes. Commit `feat(android): the choosing, fetching and switching screens, drawn from what they are handed`.

---

### Task 3: Wired in

- [ ] **Step 1: `App.kt`, `MainActivity.kt`.** `val engram get() = Engram.get(this)`; `unpairAsync` unchanged. `MainActivity` collects `Engram.current` and wraps `EngramApp` in `key(engram)`, so a switch discards every remembered thing with the instance it was remembered from.
- [ ] **Step 2: `ModelLine`** (stateful, in `ModelRows.kt`): collects `Downloads.progress(ctx, model)`; `Download` asks `engram.metered` and shows `MeteredDialog` or starts unmetered-only; `Remove` calls `downloader.remove` then `onChanged`; a transition to `Done` calls `onChanged`. `onChanged` is where the caller restarts the core or re-checks what is missing.
- [ ] **Step 3: `Nav.kt`.** Before the pairing gate:

```kotlin
    val unchosen = connection == null && engram.modes.chosen == null && pairText == null
    var pairing by rememberSaveable { mutableStateOf(false) }
    if (unchosen && !pairing) {
        ModeChooser(Core.available, sizeWords(ModelManifest.required.sumOf { it.bytes }), onPhone = { scope.launch { Engram.switch(ctx, Mode.contained) } }, onServer = { pairing = true })
        return
    }
    if (engram.loopback) {
        var missing by remember { mutableStateOf(engram.requiredMissing()) }
        if (missing.isNotEmpty()) {
            DownloadScreen(engram, missing, onChanged = { missing = engram.requiredMissing() }, onServer = { scope.launch { Engram.switch(ctx, Mode.server) } })
            return
        }
    }
```

`DownloadScreen` is the heading, a `ModelLine` per missing model, and `With a server instead`.
- [ ] **Step 4: `AskScreen.kt`.** At the top of the screen's column: `if (engram.loopback && engram.modes.ask == AskVia.off) AskOffPane(onSettings)`, else `if (engram.askWantsAModel) AskOfferPane(...)` whose model rows are `ModelLine`s with `onChanged = { scope.launch { engram.restartCore() }; recheck }`, `onEndpoint = onSettings`, `onOff = { engram.modes.ask = AskVia.off; recheck }`; otherwise the screen as it is. `AskScreen` gains `onSettings: () -> Unit`, passed from `Nav`.
- [ ] **Step 5: `SettingsScreen.kt`.** `Section("Server")` becomes `ModeSection(engram)`; in contained mode it is followed by `ModelsSection(engram)` and `AskSection(engram)`, and the `Reminders` and `Notifications` sections are not drawn. `Unpair` and its dialog are drawn only where `engram.store.current` holds a connection. `Switch to this phone` is drawn only where `Core.available`. Switching to a server with no stored connection lands on pairing through the gate in step 3, because `modes.chosen` is then `server` and `connection` is null — so the gate's `unchosen` must stay false there, which it does.
- [ ] **Step 6:** `./gradlew :core:testDebugUnitTest :app:testDebugUnitTest :app:lintDebug` passes. Commit `feat(android): contained mode can be chosen, fetched for, switched to and away from`.

---

### Task 4: Checked

- [ ] Full Android run in the background; the real totals and exit code.
- [ ] `-Pengram.pictures=1` on `PicturesTest`, extended with the chooser, a model row in each state and the ask offer, so there is something to look at before a device exists.
- [ ] Memory note: 5b done; what part 6 inherits.
