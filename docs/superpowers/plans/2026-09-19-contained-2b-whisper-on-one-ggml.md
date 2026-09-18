# Contained mode, part 2b — speech on the device, over the one ggml — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A contained phone turns a held microphone into words without a server, and the library still holds exactly one ggml.

**Architecture:** whisper.cpp is three source files over ggml. They are vendored at the release written against the ggml nearest llama.cpp's, and compiled by this crate's `build.rs` against the ggml headers `llama-cpp-sys-2` has already installed — the path it exports as `DEP_LLAMA_GGML_CMAKE_DIR` for exactly this. Nothing builds a second ggml, so there is nothing to collide. A small C++ shim flattens whisper's by-value parameter struct into four C functions, so no bindgen and no struct layout is trusted across the boundary. `LocalTranscriber` reads WAV with `hound`, resamples with `rubato` where the rate is not 16 kHz, and loads the model for one recording.

**Tech Stack:** Rust, `cc`, `hound`, `rubato`; whisper.cpp v1.9.2 (MIT); Kotlin and Compose for the offer.

**Spec:** `docs/superpowers/specs/2026-09-18-android-contained-mode-design.md`, section 2 "Inference" and section 4. Task 1 corrects it.

## Global Constraints

- Baselines: Rust 2925 passed; Android 312 tests, 5 skipped, 0 failed; `.so` 64,688,256 bytes.
- One ggml. `llama-cpp-2` stays pinned at `=0.1.156`, and `llama-cpp-sys-2` is named at the same pin so the two cannot drift. The vendored whisper.cpp and that pin move together, and a comment in `Cargo.toml` and in `vendor/whisper.cpp/README.md` says so.
- Everything is behind the `contained` feature. The server build does not compile a line of it.
- The speech test model and sample live in `~/.cache/engram-test-models` (`speech.bin`, `speech.wav`), not in the repository. Tests that need them say they were skipped when `ENGRAM_TEST_MODELS` is unset or the files are absent.
- UI copy is a term and a short gloss; every string is listed below.
- No device testing. The risk, stated once: whisper.cpp's CPU path has not run under memory tagging, and a Tensor G3's speed on Whisper small is unmeasured.

## What reading the code found

1. **The published binding cannot share.** `whisper-rs-sys` 0.15 vendors ggml 0.9 and has no switch for an outside ggml; llama.cpp here carries ggml 0.19. Two ggmls in one library collide, which is the thing the spec warns of.
2. **`llama-cpp-sys-2` expects this.** Its build script exports `DEP_LLAMA_GGML_CMAKE_DIR` "so that a dependent crate … can use this ggml. Then there is only one ggml in the program." Cargo hands `DEP_*` only to direct dependents, so this crate names the sys crate directly.
3. **The matched release is v1.9.2.** It was written against ggml 0.18.1; v1.9.3 wants 0.20. Its three files compile cleanly against llama's 0.19 headers — checked before this plan was written.
4. **The app already records what Whisper wants:** 16 kHz, mono, 16-bit PCM in a WAV. Resampling is for anything else that reaches the route, not for the phone's own microphone.
5. **The microphone has nowhere to make its offer.** It is drawn only where the status endpoint reports transcription open, and in contained mode with no speech model it never is. In contained mode the button is drawn anyway until the offer has been answered, and the first press makes the offer.

## File structure

- Create `vendor/whisper.cpp/{src/whisper.cpp, src/whisper-arch.h, include/whisper.h, LICENSE, README.md}` and `vendor/whisper.cpp/engram_whisper.{h,cpp}` (the shim).
- Modify `build.rs`, `Cargo.toml`.
- Create `src/infer/pcm.rs` — WAV to 16 kHz mono `f32`.
- Create `src/infer/whisper.rs` — the four FFI functions, wrapped safely.
- Modify `src/infer/local.rs` (`LocalTranscriber`, `LocalModels.speech`), `src/infer/mod.rs`, `src/core/mod.rs` (`with_local_models`), `examples/contained.rs`.
- Modify `android/native/src/lib.rs`, `Core.kt` (`Setup.speech`, `Models.speech`), `Mode.kt`.
- Modify `app/.../ui/SearchScreen.kt`, `ModeSettings.kt`; create `app/.../ui/SpeechOffer.kt`.

---

### Task 1: The spec, corrected

- [ ] Section 2 "Inference": replace "Both embed ggml, and two copies in one library collide, so ggml is built once and both are pointed at it. Their pinned versions are therefore a matched pair and move together." with "Both are built over ggml, and two copies in one library collide. There is only ever one: whisper.cpp's three source files are vendored and compiled against the ggml that `llama-cpp-sys-2` builds and exports for the purpose. The vendored release is the one written against the nearest ggml at or below llama.cpp's, so the two are a matched pair and move together."
- [ ] Commit with this plan: `docs(android): the plan for contained mode, part 2b, and the spec corrected by it`.

---

### Task 2: whisper.cpp over llama.cpp's ggml

**Interfaces — Produces (`src/infer/whisper.rs`):** `pub struct Whisper` (owns the context, `Send`, not `Sync`); `Whisper::open(path: &Path) -> Result<Whisper>`; `Whisper::run(&mut self, samples: &[f32], lang: Option<&str>, threads: i32) -> Result<String>`; `Drop` frees it.

The shim, `engram_whisper.h`:

```c
#ifdef __cplusplus
extern "C" {
#endif
struct engram_whisper;
/* NULL where the file is not a model this whisper reads. No GPU, no flash attention: the CPU path. */
struct engram_whisper * engram_whisper_open(const char * path);
/* The words in `n` samples of 16 kHz mono, as a malloc'd UTF-8 string; NULL on failure.
   `lang` is an ISO-639-1 code, or NULL to let the model decide. Translation is never asked for. */
char * engram_whisper_run(struct engram_whisper * w, const float * samples, int n, const char * lang, int threads);
void engram_whisper_free_text(char * text);
void engram_whisper_close(struct engram_whisper * w);
#ifdef __cplusplus
}
#endif
```

`engram_whisper.cpp` fills `whisper_full_default_params(WHISPER_SAMPLING_GREEDY)` with `print_*` off, `no_timestamps` on, `single_segment` off, `translate` off, `n_threads`, `language` (`"auto"` with `detect_language` off where NULL), runs `whisper_full`, and joins the segment texts.

- [ ] **Step 1: Fetch** the three files and `LICENSE` from tag `v1.9.2` into `vendor/whisper.cpp/`; write `README.md`: what it is, the tag, the commit, the ggml it was written against (0.18.1), the ggml it is compiled against (0.19.0, from `llama-cpp-sys-2` 0.1.156), and that the two move together.
- [ ] **Step 2: `Cargo.toml`** — optional `llama-cpp-sys-2 = { version = "=0.1.156", default-features = false }`, `hound = "3"`, `rubato` (current), build-dependency `cc`; all in `contained`.
- [ ] **Step 3: `build.rs`** — under `CARGO_FEATURE_CONTAINED`: read `DEP_LLAMA_GGML_CMAKE_DIR`, take `../../include` from it as the ggml include directory, and fail with a sentence if either is missing; `cc::Build` in C++17 over `src/whisper.cpp` and `engram_whisper.cpp` with `WHISPER_VERSION="1.9.2"`, warnings off for the vendored file; `rerun-if-changed` on the vendor directory.
- [ ] **Step 4: Failing test** in `whisper.rs`, gated like `local.rs`'s: `speech.bin` opens, `speech.wav`'s samples come back containing `ask not what your country` (lower-cased); a path that is not a model is `Err`, not a crash.
- [ ] **Step 5: Implement** the FFI wrapper. Fetch the test files: `ggml-tiny-q5_1.bin` (pinned revision, SHA-256 `818710568da3ca15689e31a743197b520007872ff9576237bda97bd1b469c3d7`) as `speech.bin`, and whisper.cpp's `samples/jfk.wav` as `speech.wav`.
- [ ] **Step 6:** `cargo test --features contained --lib infer::whisper::` passes — which is also the proof of one ggml on the desktop: a second copy is a duplicate-symbol link error. `cargo check` without the feature still builds. Commit `feat(infer): whisper.cpp, compiled over the ggml llama.cpp already built`.

---

### Task 3: A recording, as Whisper wants it

**Interfaces — Produces (`src/infer/pcm.rs`):** `pub fn samples_16k(audio: &[u8], mime: &str) -> Result<Vec<f32>>`; `Error::Validation` for anything that is not PCM WAV.

- [ ] **Step 1: Failing tests**, WAVs written with `hound` in the test: 16 kHz mono 16-bit comes back sample for sample, scaled to ±1; stereo is averaged to mono; 48 kHz comes back a third as long (±1%) and a 440 Hz tone is still 440 Hz (zero crossings, ±2%); 8 kHz comes back twice as long; float WAV is read; a `audio/webm` mime and a body that is not RIFF are each `Validation` with a sentence naming what was expected; an empty recording is an empty vector.
- [ ] **Step 2: Implement** with `hound::WavReader` and `rubato`'s FFT fixed-ratio resampler, whole-buffer. `cargo test --features contained --lib infer::pcm::`. Commit `feat(infer): a WAV of any rate, as sixteen thousand mono samples a second`.

---

### Task 4: `LocalTranscriber`, and a core that opens the door

**Interfaces — Produces:** `LocalModels.speech: Option<PathBuf>`; `LocalTranscriber::new(path, lang: Option<String>)` implementing `Transcriber`; `Core::with_local_models` sets `transcriber` where `speech` is given. One recording at a time (a `tokio::sync::Mutex`), the model loaded for it on `spawn_blocking` and dropped when done, as the spec's memory budget asks.

- [ ] **Step 1: Failing tests**: `local::tests::a_recording_becomes_its_words` (gated); `contained::tests::a_speech_model_opens_the_microphone` — `start` with `speech` set reports `transcribe: true` in status and `POST /api/v1/transcribe` with `speech.wav` answers the words; without it, `false` and 404 (that half ungated).
- [ ] **Step 2: Implement**; `examples/contained.rs` picks up `speech.bin`. Commit `feat(contained): the microphone's door, opened by a model file`.

---

### Task 5: Across JNI, and the offer

**Copy, complete:** dialog title `Dictation · no model yet`; the speech model's row as every model row; button `Not now`.

- [ ] **Step 1:** `lib.rs` `Setup.speech`; Kotlin `Setup.speech`, `Models.speech`, `ModeState.models()` fills it from `Role.speech`; `ModeStore.speechDeclined: Boolean`; `Engram.speechWanted` — contained, no speech model installed, not declined. Tests in `ModeTest` and `EngramModesTest` (`given!!.speech` after a restart; `speechWanted` false once installed or declined, false in server mode).
- [ ] **Step 2:** `SpeechOffer.kt`: stateless `SpeechOfferDialog(model: @Composable () -> Unit, onNotNow: () -> Unit)`; test in `ModeScreensTest`.
- [ ] **Step 3:** `SearchScreen`: `micOpen = (doors?.transcribe ?: true) || engram.speechWanted`; `micDown` shows the offer instead of recording while `speechWanted`. The row is a `ModelLine` whose `onChanged` restarts the core and re-reads status; `Not now` sets `speechDeclined`, and the button goes. `ModelsSection` lists `Role.speech` too, which is the way back.
- [ ] **Step 4:** Android build with `cargo +stable ndk`; `llvm-nm` shows one `ggml_init` and no duplicate; `.so` size recorded. Full Android tests and lint. Commit `feat(android): dictation on the phone, offered at the first press`.

---

### Task 6: Checked

- [ ] Both full suites in the background; real totals and exit codes.
- [ ] Desktop runner with `speech.bin`: status says `transcribe: true`; `POST /transcribe` with `speech.wav` answers the sentence; time it.
- [ ] Memory note: 2b done, what the device pass inherits.
