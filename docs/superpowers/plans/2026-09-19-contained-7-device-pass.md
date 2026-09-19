# Contained mode, part 7 — the device pass — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to work through this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. This plan needs a phone in a person's hand; nothing in it can be done from the desktop alone.

**Goal:** Everything parts 3 to 6 and 2b compiled but could not run is run once, on the phone it was sized for, and the two defaults the design left open are set from what is measured.

**Architecture:** Two instruments. `DeviceBench`, an instrumented test in `android/core`, starts the core on the phone over whatever model files were pushed beside it and writes `bench.json`: start time, capture to searchable, search with and without each reranker, tokens a second for each ask model, dictation. The second instrument is a person with the release build and the checklist in Task 3, because a foreground download, an alarm in doze and a pulled charger are not things a test can hold.

**Tech Stack:** `adb`, Gradle's `connectedDebugAndroidTest`, the release APK built with `-Pengram.native=1`.

**Spec:** `docs/superpowers/specs/2026-09-18-android-contained-mode-design.md`, sections 2 "Models", 5 and 6.

## Global Constraints

- The phone: Pixel 8, GrapheneOS, 8 GB. Memory tagging on for the app (Settings → Apps → engram → Hardened memory allocator / memory tagging), and both dynamic code loading restrictions on (via memory and via storage). That is the only configuration tested.
- Toolchain: `export PATH=$HOME/.cargo/bin:$PATH ANDROID_NDK_HOME=$HOME/Android/Sdk/ndk/28.2.13676358`; `adb` is `$HOME/Android/Sdk/platform-tools/adb`.
- Numbers go in the commit that sets a default, as the design says. A default is not changed on an impression.
- The decision rules are the spec's and are written before measuring, so that the numbers decide: a reranker ships only if it changes the order for the better against `bge-reranker-v2-m3`'s and answers the top ten within about two seconds; the ask default is Qwen3.5-2B unless Gemma 4 E2B is clearly faster at comparable answers.
- Anything that fails here is fixed in its own commit with the failure quoted, and the pass is re-run from that point.

## What is untested until this pass

From the earlier parts, so that none is forgotten: the library loading and running under memory tagging; `Core.init` and the certificate verifier's JNI path (first exercised by sharing an https link, or by an ask endpoint); cleartext to loopback under the network security config; the foreground download worker and its notification, resume after a dropped network, and the metered question; the first-start chooser, the download screen and the switch of mode on a real activity; `Engram.switch` itself; the background pass under real charging and idle, and its ending when the charger is pulled; alarms in doze, the alarm and boot receivers; the microphone's offer and dictation end to end; every stateful screen wrapper.

---

### Task 1: Build, install, and see it start

- [ ] `cd android && ./gradlew :app:assembleRelease -Pengram.native=1` (signed, or `assembleDebug` for the pass). `adb install -r` it.
- [ ] Turn on memory tagging and both dynamic code loading restrictions for the app. Open it. **Expected:** the chooser, with "On this phone" offered. If the app dies at launch or at the choice, `adb logcat -b crash` names it: a tag-check fault (`SEGV_MTESERR`) is the risk the design named, and its answer is a version pin, not an architecture change.
- [ ] Choose "On this phone", download the embedder on Wi-Fi. Pull the network half way and restore it. **Expected:** the notification's progress resumes from where it was, the file verifies, the app opens on home.
- [ ] Capture a sentence, search for it in other words. **Expected:** found, within seconds, in aeroplane mode too.

### Task 2: Measure

- [ ] Choose the candidates and fetch them on the desktop. Rerankers of 30–300 M, multilingual, with a GGUF that llama.cpp's rerank pooling reads — search for what is current rather than recalling it; `jina-reranker-v1-tiny-en` from the test cache is the floor, not a candidate (English only). Ask: `Qwen3.5-2B-Q4_K_M.gguf` from the manifest, and Gemma 4 E2B at Q4.
- [ ] Write `captures.txt` (forty paragraphs from the live base, German and English mixed) and `questions.txt` (twenty, each with a known answer among them). On the desktop, record `bge-reranker-v2-m3`'s order for the same questions over the same captures through the server: that is what a candidate is judged against.
- [ ] Push and run:

```bash
D=/sdcard/Android/data/io.github.overcuriousity.engram.core.test/files
adb shell mkdir -p $D
adb push embed.gguf rerank-*.gguf ask-*.gguf speech.bin speech.wav captures.txt questions.txt $D/
cd android && ./gradlew :core:connectedDebugAndroidTest -Pengram.native=1 \
  -Pandroid.testInstrumentationRunnerArguments.class=io.github.overcuriousity.engram.core.contained.DeviceBench
adb pull $D/bench.json
```

  If `adb push` cannot write there before the test package has run once, run the test once first (it skips itself for want of `embed.gguf`, and its directory then exists).
- [ ] Run it a second time with another app in the foreground and a browser holding a dozen tabs. **Expected:** it completes. A kill by the low-memory killer during an ask is the finding to bring back: the answer is the 2B default staying, or the context shrinking.
- [ ] Read `bench.json` against the rules above. Note `reasoning_frames` for each ask model: a thinking model whose thinking arrives as `token` frames is the known gap from part 2, and this is where it is split off, by token id, for the model that is chosen.

### Task 3: By hand

- [ ] Share an `https://` link into the app. **Expected:** captured. A panic here is `Core.init` not having run before the core's first HTTPS call.
- [ ] Settings → Ask → Endpoint, with a real one. Ask a question. Plug in, lock the phone, leave it twenty minutes on Wi-Fi. **Expected:** Settings' background line counts down. Pull the charger mid-pass. **Expected:** `adb logcat` shows the worker stopped, and the count stops falling.
- [ ] Date a reminder two minutes out, lock the phone. **Expected:** it rings, within a few minutes; Done strikes it. Date another, reboot, do not open the app. **Expected:** it still rings.
- [ ] Hold the microphone. **Expected:** the offer. Download, hold again, speak a German sentence and an English one. **Expected:** each comes back in its own language. If a short German query comes back as English, pass the phone's language to `LocalTranscriber` — it takes one already.
- [ ] Settings → Mode → Switch to a server, pair, capture, switch back. **Expected:** each base holds its own captures and nothing else; the queue of one never appears in the other.

### Task 4: Set the defaults

- [ ] Reranker: add the winner to `ModelManifest` as `Role.rerank`, required at first start, with its URL pinned, size, SHA-256 and licence read from the publisher — or add nothing, and say in the commit which candidates were measured and why none earned its place. Either way `ModelManifestTest.theFirstStartSetIsTheEmbedderAndNothingElse` is rewritten to say what is now true.
- [ ] Ask: keep Qwen3.5-2B or make Gemma 4 E2B the default; the other stays as the explicit second choice only if it is worth offering.
- [ ] Whether a small model can read a capture: run the core on the desktop with the synthesize tier pointed at the chosen ask model behind a local OpenAI-compatible server, over `captures.txt`. If what comes back is usable, an in-process synthesizer becomes a part of its own; if not, the spec's sentence from part 6 stands and says why.
- [ ] Commit each with its numbers. Update the spec's table of models, and the memory note: the programme is finished, or what is left.
