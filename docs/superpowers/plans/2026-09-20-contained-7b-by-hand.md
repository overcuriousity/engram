# Contained mode, part 7b — the pass by hand, without adb

The part of `2026-09-19-contained-7-device-pass.md` that needs no cable: the
release APK arrives through Obtainium, and a log leaves the phone through
GrapheneOS's own viewer (Settings → Apps → engram → View logs → share). The
measurements of Task 2 there — reranker choice, tokens a second — still need
`adb` and are not here.

**Phone:** Pixel 8, GrapheneOS, memory tagging on. **Build:** the release after
`699a79d` (v2026.919.1 or later).

## Already seen, 2026-09-19, on v2026.919.0

- The core loads and runs under memory tagging. No crash, no tag fault in the
  log. This was the risk the design named.
- The embedder downloaded as a foreground service and verified.
- Four faults, fixed in #109: black text on the screens before a connection;
  every shared file held with "tz only applies to a text capture" (server mode
  had it too); the download's notification updated twelve times a second
  against the five allowed; white status icons on a light app on a dark phone.

## Before starting

- [ ] Update through Obtainium. Delete the held "Files" row in Queue and share
  that file again. **Expected:** it is captured.
- [ ] Look at the status bar on a light screen. **Expected:** dark icons. This
  fix was never seen on a phone.
- [ ] Settings → Mode → On this phone, if not already there.

## 1. Capture and search

- [ ] Type five or six notes, German and English mixed, a few sentences each.
- [ ] Search for one in other words than it was written in. **Expected:**
  found, within seconds.
- [ ] Aeroplane mode on. Search again, and capture one more note and search
  for it. **Expected:** both work. Aeroplane mode off.
- [ ] Search in English for something only a German note says. Note what came
  first: on a three-note base a weak English hit once outranked the relevant
  German one, and a larger base should show whether that was the size.

## 2. A link

- [ ] From the browser, share an `https://` page to engram. **Expected:**
  captured, and readable in Library. A crash here is the certificate
  verifier's JNI path (`Core.init`), first exercised by this.

## 3. The microphone

- [ ] Hold the microphone. **Expected:** the offer to download Whisper small,
  190 MB. Take it; the notification's bar should move about once a second.
- [ ] Hold again and say one German sentence. Then one English sentence.
  **Expected:** each comes back in its own language. Write down what came back
  if not: a short German sentence returned as English is what decides whether
  the phone's language is passed to the transcriber.
- [ ] Note roughly how long a five-second recording takes to come back.

## 4. Ask

- [ ] Open Ask. **Expected:** the offer, with Qwen3.5-2B (1.3 GB) first. Take
  it on Wi-Fi.
- [ ] Ask something one of the notes answers. Note: seconds until the first
  word, whether the answer is right and cites the note, whether the phone
  gets warm, whether any `<think>` text shows in the answer.
- [ ] Ask twice more, then open another heavy app and come back.
  **Expected:** engram is still there, or restarts without losing anything.

## 5. A reminder

- [ ] Capture "remind me in two minutes to check the reminder". Lock the
  phone. **Expected, honestly unknown:** with no endpoint set, the stages that
  read a capture are held, so a reminder may never be made. Nothing ringing is
  a finding, not a failure — write down which happened.
- [ ] If Today shows the reminder: let it ring, tap Done. Make another a few
  minutes out, reboot, do not open the app. **Expected:** it still rings.

## 6. Two bases

- [ ] Settings → Mode → switch to the server. **Expected:** the old base,
  whole, and none of today's notes in it.
- [ ] Capture one note there. Switch back to On this phone. **Expected:**
  today's notes, and not that one. The Queue of one never shows in the other.

## 7. Only with an endpoint, and optional

- [ ] Settings → Mode → Ask → Endpoint, with a real one. Plug in, lock, leave
  twenty minutes on Wi-Fi. **Expected:** Settings' background line counts
  down; captures gain titles and tags. A small reasoning model behind the
  endpoint will fail unless its server has thinking off — see the handoff.

## Bring back

For anything that looked wrong: a screenshot and the log export. For sections
3 and 4 the numbers, however rough. They go in the commit that sets a default.
