# Contained mode, part 5a — models, and what the core needs to use them — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Everything part 5's screens stand on, with no screen in it: a manifest of models, a downloader that resumes and verifies, a foreground worker around it, a core that can be given an ask endpoint and restarted with new models, and outgoing HTTPS from the core that works on Android.

**Architecture:** The manifest is Kotlin data in `android/core`. The downloader is plain OkHttp over a `.part` file with a `Range` request and a SHA-256 over the finished file, tested against MockWebServer. `contained::start_with` takes a `Setup` — the model files and, optionally, an ask endpoint rendered into the config it already writes. The JNI library gains one function, `Core.init(context)`, which hands the platform's certificate verifier its `Context`.

**Tech Stack:** Kotlin, OkHttp, WorkManager; Rust, `jni` 0.22, `rustls-platform-verifier` 0.7 (already in the lock file through reqwest 0.13).

**Spec:** `docs/superpowers/specs/2026-09-18-android-contained-mode-design.md`, sections 2 "Models", 4 and 6. Task 1 corrects it.

## Global Constraints

- Kotlin commands run from `android/`; Rust from the repository root. Android target: `export PATH=$HOME/.cargo/bin:$PATH ANDROID_NDK_HOME=$HOME/Android/Sdk/ndk/28.2.13676358`, then `cargo +stable ndk -t arm64-v8a --platform 29 …` from `android/native`.
- Baseline: Android 255 tests, 5 skipped, 0 failed. Rust 2918 passed (`cargo test --features contained --lib` with `ENGRAM_TEST_MODELS=$HOME/.cache/engram-test-models`).
- A checkout with no Rust toolchain must still build the app. Anything Gradle needs from Cargo is behind `-Pengram.native=1`, as `buildNative` is.
- Manifest values are copied from the publisher's API, not recalled: URL pinned to a revision, size in bytes, SHA-256 as published.
- No reranker entry. The spec leaves the choice to the device pass, and says the first-start download is the embedder alone if none earns its place.
- A model file is never used before its SHA-256 matches. A file that fails is deleted.
- Nothing in 5a is reachable by a person. 5b is the screens.
- Commit style as before; no push; no device testing. The risk, stated once: the certificate verifier's JNI path and the foreground worker cannot be run on this machine, only compiled and packaged.

## What reading the code found

1. **Part 5 is two plans.** The manifest, downloader, worker, endpoint and verifier are one reviewable thing with JVM and Rust tests. The chooser, the download screen, Settings "Mode" and the Ask offer are another. This is 5a.
2. **The microphone offer cannot come before part 2b.** The mic is drawn only where the status endpoint reports `POST /transcribe` open, and in contained mode nothing can open it until Whisper runs on the device. Offering a 190 MB download that nothing can use would be a lie. The speech model is in the manifest; its offer moves to 2b.
3. **"Set an endpoint" needs a core that can be told one.** `contained::start` renders a fixed config with every role pointed at a dead port. It gains a `Setup` with an optional ask endpoint.
4. **The certificate verifier is this part's.** reqwest 0.13's `rustls` feature is `rustls-platform-verifier`, which on Android panics unless initialised with a `Context`. An ask endpoint is the first thing here that makes an HTTPS call from the core, and a shared URL is the second. It needs `jni` 0.22 (the native crate has 0.21) and a small Kotlin AAR that ships inside the `rustls-platform-verifier-android` crate.
5. **New models need a new core.** `Core.start` answers the running core when called twice, so a model downloaded later is not picked up. `Contained.restart()` stops and starts it.

## File structure

- Modify `src/contained.rs` — `Endpoint`, `Setup`, `start_with`; `config_for` renders the endpoint.
- Modify `examples/contained.rs` — unchanged behaviour, through `start`.
- Modify `android/native/Cargo.toml`, `android/native/src/lib.rs` — jni 0.22, `Core.init`, `Setup` over JSON.
- Modify `android/core/build.gradle.kts`, create `android/core/consumer-rules.pro` — the verifier's AAR, only with `-Pengram.native=1`.
- Modify `android/core/.../contained/Core.kt` — `init(context)`, `Setup`.
- Create `android/core/.../contained/ModelManifest.kt` — the entries.
- Create `android/core/.../contained/Downloader.kt` — resume and verify.
- Create `android/core/.../contained/DownloadWorker.kt` — the foreground worker.
- Modify `android/core/.../contained/Contained.kt` (`restart`), `Mode.kt` (`models()` by manifest), `Engram.kt` (what 5b calls).
- Modify `android/app/src/main/AndroidManifest.xml` — foreground service permissions and type.

---

### Task 1: The spec, corrected

- [ ] **Step 1.** In section 4, replace "The first use of the microphone offers the speech model the same way." with: "The first use of the microphone offers the speech model the same way once the device can transcribe (step 2b); until then the microphone is absent in contained mode, as it is against a server with no speech model."
- [ ] **Step 2.** In section 7, replace item 5 with two: "5a. The manifest, the downloader and its worker, an ask endpoint the core can be given, the platform certificate verifier." and "5b. The first-start chooser, the download screen, Settings "Mode", the offer on the first visit to Ask."
- [ ] **Step 3.** In section 2 "Models", under the table, add: "No reranker is listed until the device pass has chosen one. Every URL is pinned to a revision, so a publisher's later upload cannot turn a good hash into a failed download."
- [ ] **Step 4.** Commit with this plan: `docs(android): the plan for contained mode, part 5a, and the spec corrected by it`.

---

### Task 2: A core that can be told where to ask

**Files:** Modify `src/contained.rs`.

**Interfaces — Produces:** `pub struct Endpoint { pub base_url: String, pub model: String, pub api_key: Option<String> }`; `pub struct Setup { pub models: LocalModels, pub ask: Option<Endpoint> }`; `pub async fn start_with(data_dir: &Path, setup: Setup) -> Result<(Started, Running)>`; `start(data_dir, models)` stays and calls it.

- [ ] **Step 1: Failing tests**, added to `contained::tests`:

```rust
    fn parsed(ask: Option<&Endpoint>) -> crate::config::Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, config_for(dir.path(), "$argon2id$x", ask)).unwrap();
        crate::config::Config::load(Some(&path)).unwrap()
    }

    #[test]
    fn with_no_endpoint_ask_points_where_nothing_listens() {
        let cfg = parsed(None);
        assert_eq!(cfg.infer.tiers["device-ask"].base_url, "http://127.0.0.1:9/v1");
    }

    #[test]
    fn an_endpoint_is_asks_and_nobody_elses() {
        let e = Endpoint {
            base_url: "https://llm.example/v1".into(),
            model: "some \"quoted\" model".into(),
            api_key: Some("sk-\\odd".into()),
        };
        let cfg = parsed(Some(&e));
        let ask = &cfg.infer.tiers["device-ask"];
        assert_eq!(ask.base_url, "https://llm.example/v1");
        assert_eq!(ask.model, "some \"quoted\" model");
        assert_eq!(ask.api_key.as_deref(), Some("sk-\\odd"));
        assert_eq!(cfg.infer.tiers["device-synthesize"].base_url, "http://127.0.0.1:9/v1");
    }
```

- [ ] **Step 2:** `cargo test --features contained --lib contained::` — fails to compile.
- [ ] **Step 3: Implement.** `config_for(data_dir, local_hash, ask: Option<&Endpoint>)`; the `[infer.tiers.device-ask]` block's `base_url` and `model` become `{ask_url}` and `{ask_model}`, followed by `{ask_key}` which is either empty or `api_key = "…"\n`, all through `quoted`. `start_with` is today's `start` body with `setup.models` and `setup.ask.as_ref()`; `start` becomes a one-line call to it.
- [ ] **Step 4:** the same command passes, the two existing tests included.
- [ ] **Step 5:** Commit `feat(contained): an ask endpoint the core can be started with`.

---

### Task 3: The certificate verifier gets its Context

**Files:** `android/native/Cargo.toml`, `android/native/src/lib.rs`, `android/core/build.gradle.kts`, `android/core/consumer-rules.pro`, `Core.kt`.

**Interfaces — Produces:** Kotlin `Core.init(context: Context)`; `@Serializable data class Endpoint(baseUrl, model, apiKey: String? = null)`; `@Serializable data class Setup(embed, rerank, ask: String? = null, askEndpoint: Endpoint? = null)` replacing `Models` on the wire (`Models` stays as the type `ModeState.models()` returns); `Core.start(dataDir, setup: Setup)`.

- [ ] **Step 1: `Cargo.toml`** — `jni = "0.22"`, and `rustls-platform-verifier = "0.7"` (the version reqwest resolved; one copy).
- [ ] **Step 2: `lib.rs`** — the three exported functions take `EnvUnowned<'l>` and do their work inside `with_env`; `Models` becomes

```rust
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Setup {
    embed: Option<std::path::PathBuf>,
    rerank: Option<std::path::PathBuf>,
    ask: Option<std::path::PathBuf>,
    ask_endpoint: Option<Endpoint>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Endpoint { base_url: String, model: String, api_key: Option<String> }
```

and the new export, in the file's existing JSON-answer style:

```rust
/// Hands the platform's certificate verifier the app's Context. Without it
/// the first HTTPS request the core makes on Android panics, because rustls
/// asks Android whether a chain is trusted and has nobody to ask through.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_overcuriousity_engram_core_contained_Core_init<'l>(
    mut env: EnvUnowned<'l>, _class: JClass<'l>, context: JObject<'l>,
) -> jstring
```

answering `{"ready":true}` or `{"error":…}`. The exact `jni` 0.22 calls are settled against the compiler; the contract above is what is fixed.
- [ ] **Step 3: Build** — `cargo +stable ndk -t arm64-v8a --platform 29 build --release` in `android/native`. Expected: builds. Then `llvm-nm -D --defined-only` on the `.so` shows three `Java_…_Core_` symbols.
- [ ] **Step 4: Gradle**, in `android/core/build.gradle.kts`, beside `buildNative`:

```kotlin
// The Kotlin half of rustls-platform-verifier ships inside its crate. Found
// through Cargo, so only where there is a Cargo: the same switch as the .so,
// without which the classes would have nothing to be called by.
if (providers.gradleProperty("engram.native").isPresent) {
    val metadata = providers.exec {
        workingDir = rootProject.file("native")
        commandLine("cargo", "+stable", "metadata", "--format-version", "1", "--filter-platform", "aarch64-linux-android")
    }.standardOutput.asText
    val maven = metadata.map { text ->
        val pkg = (groovy.json.JsonSlurper().parseText(text) as Map<*, *>)["packages"] as List<Map<*, *>>
        File(pkg.first { it["name"] == "rustls-platform-verifier-android" }["manifest_path"] as String).parentFile.resolve("maven")
    }
    repositories { maven { url = uri(maven.get()); metadataSources.artifact() } }
    dependencies { implementation("rustls:rustls-platform-verifier:0.1.1") }
}
```

If `settings.gradle.kts` sets `FAIL_ON_PROJECT_REPOS`, the repository goes there instead, under the same condition. `consumer-rules.pro`: `-keep class org.rustls.platformverifier.** { *; }`, wired with `consumerProguardFiles("consumer-rules.pro")`.
- [ ] **Step 5: `Core.kt`** — `init(context)` calls the external once (`AtomicBoolean`), throws `CoreFailed` on `error`; `start` takes `Setup`. `Contained`'s `boot` type becomes `(String, Setup) -> Started`. `ContainedTest` and `EngramModesTest` follow the rename.
- [ ] **Step 6: Verify** — `./gradlew :core:testDebugUnitTest` passes; `./gradlew :app:assembleRelease -Pengram.native=1` builds, and `unzip -l` plus `dexdump`/`apkanalyzer` (or `grep` over `classes*.dex` strings) shows `org/rustls/platformverifier/CertificateVerifier` survived minification. `./gradlew :app:assembleDebug` without the property still builds.
- [ ] **Step 7:** Commit `feat(android): the core can make an HTTPS call on a phone`.

---

### Task 4: The manifest

**Files:** Create `contained/ModelManifest.kt`; test `contained/ModelManifestTest.kt`.

- [ ] **Step 1: Failing test**

```kotlin
package io.github.overcuriousity.engram.core.contained

import org.junit.Assert.*
import org.junit.Test

class ModelManifestTest {
    @Test fun everyEntryCanBeFetchedAndChecked() = ModelManifest.all.forEach { m ->
        assertTrue(m.name, m.url.startsWith("https://"))
        assertTrue("${m.name} is not pinned to a revision", Regex("/resolve/[0-9a-f]{40}/").containsMatchIn(m.url))
        assertTrue(m.name, Regex("[0-9a-f]{64}").matches(m.sha256))
        assertTrue(m.name, m.bytes > 1_000_000)
        assertTrue(m.name, m.licence.isNotBlank())
        assertTrue(m.name, Regex("[a-z0-9.-]+").matches(m.file))
    }

    @Test fun filesAndNamesAreDistinct() {
        assertEquals(ModelManifest.all.size, ModelManifest.all.map { it.file }.toSet().size)
        assertEquals(ModelManifest.all.size, ModelManifest.all.map { it.name }.toSet().size)
    }

    @Test fun theFirstStartSetIsTheEmbedderAndNothingElse() {
        assertEquals(listOf(Role.embed), ModelManifest.required.map { it.role })
        assertNull(ModelManifest.all.firstOrNull { it.role == Role.rerank })
    }

    @Test fun eachOfferedRoleHasOneDefault() {
        assertEquals("Qwen3.5-2B", ModelManifest.defaultFor(Role.ask)!!.name)
        assertEquals(2, ModelManifest.all.count { it.role == Role.ask })
        assertNotNull(ModelManifest.defaultFor(Role.speech))
    }
}
```

- [ ] **Step 2: Implement**

```kotlin
package io.github.overcuriousity.engram.core.contained

enum class Role { embed, rerank, ask, speech }

/** One model as the app knows it: enough to fetch it, check it, and say what it is and whose. */
data class Model(
    val role: Role,
    val name: String,
    val file: String,
    val url: String,
    val sha256: String,
    val bytes: Long,
    val licence: String,
    /** Where the licence asks for a notice to be shown, the address of its terms. */
    val terms: String? = null,
    val default: Boolean = true,
)

/**
 * What can be downloaded. Changing a default is a change to this file.
 *
 * Every value was read from the publisher's API on 2026-09-19. Each URL names
 * a revision, so a later upload cannot turn a good hash into a failed download.
 * No reranker: the device pass chooses one, or chooses none.
 */
object ModelManifest {
    private fun hf(repo: String, rev: String, file: String) = "https://huggingface.co/$repo/resolve/$rev/$file"

    val all = listOf(
        Model(
            Role.embed, "EmbeddingGemma 300M", "embeddinggemma-300m-q8_0.gguf",
            hf("ggml-org/embeddinggemma-300M-GGUF", "0f741b5a6585bd53aeb15cd1372c56f2a0f65e12", "embeddinggemma-300M-Q8_0.gguf"),
            "b5ce9d77a3fc4b3b39ccb5643c36777911cc4eb46a66962eadfa3f5f60490d63", 333_590_944,
            "Gemma Terms of Use", terms = "https://ai.google.dev/gemma/terms",
        ),
        Model(
            Role.ask, "Qwen3.5-2B", "qwen3.5-2b-q4_k_m.gguf",
            hf("unsloth/Qwen3.5-2B-GGUF", "f6d5376be1edb4d416d56da11e5397a961aca8ae", "Qwen3.5-2B-Q4_K_M.gguf"),
            "aaf42c8b7c3cab2bf3d69c355048d4a0ee9973d48f16c731c0520ee914699223", 1_280_835_840, "Apache 2.0",
        ),
        Model(
            Role.ask, "Qwen3.5-4B", "qwen3.5-4b-q4_k_m.gguf",
            hf("unsloth/Qwen3.5-4B-GGUF", "e87f176479d0855a907a41277aca2f8ee7a09523", "Qwen3.5-4B-Q4_K_M.gguf"),
            "00fe7986ff5f6b463e62455821146049db6f9313603938a70800d1fb69ef11a4", 2_740_937_888, "Apache 2.0",
            default = false,
        ),
        Model(
            Role.speech, "Whisper small", "whisper-small-q5_1.bin",
            hf("ggerganov/whisper.cpp", "5359861c739e955e79d9a303bcbc70fb988958b1", "ggml-small-q5_1.bin"),
            "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb", 190_085_487, "MIT",
        ),
    )

    /** What contained mode cannot start without. */
    val required: List<Model> get() = all.filter { it.role == Role.embed }
    fun defaultFor(role: Role): Model? = all.firstOrNull { it.role == role && it.default }
}
```

- [ ] **Step 3:** `--tests '*ModelManifestTest*'` passes. Then, once, prove the pinned URL is real without downloading it: `curl -sIL <embed url> | grep -i 'content-length\|x-linked-etag'` shows 333590944 and the hash.
- [ ] **Step 4:** Commit `feat(android): the models the phone may fetch, by revision, size and hash`.

---

### Task 5: A downloader that resumes and verifies

**Files:** Create `contained/Downloader.kt`; test `contained/DownloaderTest.kt`.

**Interfaces — Produces:** `class Downloader(dir: File, client: OkHttpClient) { suspend fun fetch(model: Model, onProgress: (Long) -> Unit = {}): File; fun installed(model: Model): File?; fun remove(model: Model) }`; `class DownloadFailed(message: String) : IOException`.

- [ ] **Step 1: Failing tests** (MockWebServer; a `Model` built over `server.url("/m")` with the SHA-256 of a 200 kB body made in the test):
  - `aWholeFileArrivesVerifiedUnderItsOwnName` — 200 with the body; the returned file holds it; no `.part` remains; `installed` finds it.
  - `aCutDownloadResumesFromWhatItHas` — first response is the body with `SocketEffect` cutting it half way, so `fetch` throws `IOException` and leaves a `.part` of some length `n > 0`; the second response is `206` with the remainder and `Content-Range: bytes n-199999/200000`; the second `fetch` sends `Range: bytes=n-` and the file verifies.
  - `aServerThatIgnoresRangeIsReadFromTheStart` — a `.part` exists, the server answers `200` with everything; the result verifies and is not the two glued together.
  - `aFileThatDoesNotHashIsNotKept` — the body differs from the declared hash; `DownloadFailed`, neither file nor `.part` remains.
  - `aRangeTheServerCannotSatisfyStartsOver` — `.part` already full-length but wrong, server answers `416`; the `.part` is deleted and `DownloadFailed` thrown, so the next attempt is clean.
  - `whatIsInstalledIsNotFetchedAgain` — second `fetch` makes no request.
  - `progressIsBytesOnDiskIncludingWhatWasAlreadyThere`.

- [ ] **Step 2: Implement**

```kotlin
package io.github.overcuriousity.engram.core.contained

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.withContext
import okhttp3.OkHttpClient
import okhttp3.Request
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.security.MessageDigest

class DownloadFailed(message: String) : IOException(message)

/**
 * Fetches a model into [dir]. What has arrived is kept in `<file>.part` and
 * asked to be continued with a `Range`; the name without `.part` exists only
 * after the whole file's SHA-256 is the manifest's, so nothing that reads the
 * directory can pick up a model that was not checked.
 */
class Downloader(private val dir: File, private val client: OkHttpClient) {
    fun installed(model: Model): File? = File(dir, model.file).takeIf { it.isFile && it.length() == model.bytes }
    fun remove(model: Model) { File(dir, model.file).delete(); part(model).delete() }
    private fun part(model: Model) = File(dir, model.file + ".part")

    suspend fun fetch(model: Model, onProgress: (Long) -> Unit = {}): File = withContext(Dispatchers.IO) {
        installed(model)?.let { return@withContext it }
        dir.mkdirs()
        val part = part(model)
        val have = part.length()
        val req = Request.Builder().url(model.url).apply { if (have > 0) header("Range", "bytes=$have-") }.build()
        client.newCall(req).execute().use { res ->
            when (res.code) {
                206 -> {}
                200 -> part.delete() // the server sent everything; what was held is not a prefix of this stream
                416 -> { part.delete(); throw DownloadFailed("the server could not continue ${model.name}") }
                else -> throw DownloadFailed("${model.name}: the server answered ${res.code}")
            }
            var written = part.length()
            onProgress(written)
            FileOutputStream(part, true).use { out ->
                val src = res.body.source()
                val buf = ByteArray(1 shl 16)
                while (true) {
                    ensureActive()
                    val n = src.read(buf)
                    if (n < 0) break
                    out.write(buf, 0, n)
                    written += n
                    onProgress(written)
                }
            }
        }
        if (sha256(part) != model.sha256) {
            part.delete()
            throw DownloadFailed("${model.name} did not verify")
        }
        val done = File(dir, model.file)
        if (!part.renameTo(done)) throw DownloadFailed("${model.name} could not be put in place")
        done
    }

    private fun sha256(f: File): String {
        val md = MessageDigest.getInstance("SHA-256")
        f.inputStream().use { s -> val b = ByteArray(1 shl 16); while (true) { val n = s.read(b); if (n < 0) break; md.update(b, 0, n) } }
        return md.digest().joinToString("") { "%02x".format(it) }
    }
}
```

- [ ] **Step 3:** `--tests '*DownloaderTest*'` — all pass.
- [ ] **Step 4:** Commit `feat(android): a model download that resumes, and is not a model until it verifies`.

---

### Task 6: The worker around it, and what Engram offers 5b

**Files:** Create `contained/DownloadWorker.kt`; modify `Contained.kt`, `Mode.kt`, `Engram.kt`, `app/src/main/AndroidManifest.xml`; tests `EngramModesTest` (two added), `ModeTest` (one changed).

**Interfaces — Produces on `Engram`:** `val downloader: Downloader?` (contained mode only); `fun installed(model: Model): Boolean`; `suspend fun restartCore(): Boolean`; `var askEndpoint: Endpoint?`, kept in `ConnectionStore`'s sealed file as a third field because it carries an API key. `Downloads.start(context, model, allowMetered: Boolean)`, `Downloads.progress(context, model): Flow<Progress>`, `Downloads.cancel(context, model)`.

- [ ] **Step 1: `ModeState.models()`** returns a `Setup`-ready `Models` from the manifest: for each role, the first manifest entry of that role whose verified file exists (`Downloader.installed`), preferring the default. `ModeTest.onlyModelFilesThatExistAreNamed` is rewritten to create a file of the manifest's name and length (sparse, via `RandomAccessFile.setLength`).
- [ ] **Step 2: `Contained.restart()`** — under the same mutex: `stop()` (injected beside `boot`, default `Core::shutdown`), clear `connected`, state `Idle`, then the body of `ensure`. Test in `ContainedTest`: two boots, one stop between, and the second connection is the second `Started`.
- [ ] **Step 3: `DownloadWorker`** — a `CoroutineWorker` keyed by model name (`inputData`), `setForeground` with a progress notification on a low-importance channel `models` (`ForegroundInfo(id, notification, FOREGROUND_SERVICE_TYPE_DATA_SYNC)`), `setProgress(bytes)` at most four times a second, `Result.retry()` on `IOException`, `Result.failure()` with the message on `DownloadFailed`. `Downloads.start` enqueues unique work `model-<file>` with `NetworkType.UNMETERED`, or `CONNECTED` when `allowMetered`. Manifest: `FOREGROUND_SERVICE`, `FOREGROUND_SERVICE_DATA_SYNC`, and the `androidx.work.impl.foreground.SystemForegroundService` entry with `android:foregroundServiceType="dataSync"` and `tools:node="merge"`.
- [ ] **Step 4: `Engram`** — the properties above; `transport()` unchanged. `EngramModesTest`: `anAskEndpointReachesTheCoreOnTheNextStart` (the fake `boot` captures its `Setup`), `aDownloadedModelIsNamedAfterARestart`.
- [ ] **Step 5:** `./gradlew :core:testDebugUnitTest :app:testDebugUnitTest :app:lintDebug` passes. The worker itself has no JVM test: its logic is the downloader's, and WorkManager's foreground path needs a device.
- [ ] **Step 6:** Commit `feat(android): model downloads as foreground work, and a core restarted onto what arrived`.

---

### Task 7: Checked

- [ ] Full Android run and the Rust `--features contained --lib` suite, each once, in the background; report the real summary lines and exit codes.
- [ ] Desktop: `examples/contained` still starts; `LiveServerTest` 5 of 5; captures `ready` or `partial` through the API.
- [ ] `.so` size before and after, since `jni` 0.22 and the verifier are now linked deliberately.
- [ ] Memory note updated: 5a done, what 5b inherits.
