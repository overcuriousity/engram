# The app: reading (Part E) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Android app reads: search, ask, the day, the library and a
corpus, an artifact with its lineage and versions, the offer card and what is
worth seeing again — and says plainly when the server cannot be reached.

**Architecture:** Screens never learn where data comes from. They ask one
interface in `core`, `Reader`, and get a value, when it was fetched, and
whether the source could be reached. Today's only implementation,
`ServerReader`, keeps each response body with its `ETag` in one Room table and
revalidates with `If-None-Match`. Ask is the one screen that is not a read: it
consumes a POST that answers as a stream, through a pure reducer that the
screen draws. Writes keep going through the outbox.

**Tech Stack:** Kotlin 2.4, Compose (BOM 2026.09), Room 2.8, OkHttp 5.5,
kotlinx.serialization. No new dependency. No Google Play Services.

**Spec:** `docs/superpowers/specs/2026-09-08-android-companion-design.md`
Part E; `docs/api.md` for every shape read; the Part D spec for the frame.

## Decisions taken with the user (2026-09-17)

- **Search is home.** Bottom bar: Search · Capture · Today · Library. Ask is a
  second button beside the search box, not a tab. Queue and Settings move to
  the top bar; the Queue icon carries a count only while something waits.
- **No background saving.** Nothing is fetched that a person did not ask for.
  When the server cannot be reached the screen says *Server unreachable*, with
  a retry, above whatever was fetched before — marked with when — or above
  nothing.
- **Due rows act.** *Done* and *snooze one hour* on the home screen and on a
  day, through the outbox, as the notification already does.
- **The cue for the self-contained app.** A later version replaces the server
  with an on-device engram *by default*. `Reader` is the seam: that version is
  a second implementation of it. The interface's doc comment says so, and
  nothing above `core` may name `ServerReader`.

Out of scope, though the programme lists them under E: the home-screen widget,
the vector background. Also out: the journal entry box on a day (a write whose
route is HTML-only; B slice two).

## Global Constraints

- `Transport` stays `internal` to `core`. Screens reach the server through
  `Reader`, `Ask` and `Outbox` only. No Compose or `android.view` type in `core`.
- Artifact detail is fetched when a person opens an artifact and at no other
  time: the server records each fetch as an open.
- The offer's `seen` is posted once the card is composed on screen, not when
  the offer arrives.
- The search list keeps the *Relevance falls off here* divider (from
  `past_cliff`) and the loose badge (from `weak`). Rows below the divider keep
  their rank.
- No test on the phone asserts anything about ranking or result order. Tests
  of the rail assert where the divider goes *given* flags, never which row
  deserved a flag.
- UI copy is minimal: a term and a short gloss, never an explanatory sentence.
- The theme is the web's tokens (`Theme.kt`); no Material default colour.
- `minSdk` 29; lint `NewApi` is fatal. Evidence of what ran in each commit.

## File structure

`android/core/src/main/kotlin/…/core/`
- `Transport.kt` — gains `get(path, query, etag)` and `stream(path, query, body, onFrame)`.
- `read/Reader.kt` — `Reader`, `Read<T>`, `Reach`, `Request`. The seam.
- `read/ServerReader.kt` — cache-then-revalidate over `Transport` and `CacheDao`.
- `read/Models.kt` — the API's shapes as `@Serializable` classes; `Api` builds `Request`s.
- `ask/Ask.kt` — `AskState`, `AskFrame`, `reduce`, `annotate`, and the `Ask` runner.
- `db/Db.kt` — `CacheRow`, `CacheDao`, `AskedRow`, `AskedDao`, schema 2 and its migration.
- `Engram.kt` — exposes `reader: Reader`, `ask: Ask`.

`android/app/src/main/kotlin/…/ui/`
- `Nav.kt` — the new shell and routes.
- `Reading.kt` — `ReadState`, `rememberRead`, `Unreachable`, `FetchedAt`, `Label`, time words.
- `Rail.kt` — `railOf` (pure) and `HitRow`, `CliffRule`.
- `SearchScreen.kt`, `AskScreen.kt`, `DayScreen.kt`, `LibraryScreen.kt`,
  `CorpusScreen.kt`, `ArtifactScreen.kt`.

---

### Task 1: Transport reads and streams

**Interfaces — produces:**

```kotlin
internal data class Got(val status: Int, val body: String, val etag: String?)
internal suspend fun Transport.get(path: String, query: Map<String, String?> = emptyMap(), etag: String? = null): Got
internal suspend fun Transport.post(path: String, json: String): Answer
/** Reads `event:`/`data:` frames until the stream ends. A non-2xx answer is returned, not streamed. */
internal suspend fun Transport.stream(path: String, query: Map<String, String?>, json: String, onFrame: suspend (event: String, data: String) -> Unit): Int
```

`get` sends `If-None-Match` when `etag` is given; `304` comes back as
`Got(304, "", etag)`. `401` throws `Refused`, a pin failure `PinMismatch`, as
`send` does. `stream` uses a client derived from the shared one with
`readTimeout(0)` — an answer may think for longer than a minute — and parses
SSE by hand: lines `event: x`, `data: y` (several `data:` lines join with
`\n`), a blank line dispatches, `:` comments are ignored.

- [ ] Tests (`TransportTest`): `If-None-Match` is sent and a 304 has no body;
  the ETag header is returned; a stream of three frames with a comment and a
  two-line `data` dispatches three frames in order with the joined data; a 401
  on a stream throws `Refused`; a 502 on a stream returns 502 and dispatches nothing.
- [ ] Run `./gradlew :core:testDebugUnitTest --tests '*TransportTest*'`, fail, implement, pass, commit.

### Task 2: The cache, and the Reader seam

**Interfaces — produces:**

```kotlin
/** Where a read stands. */
enum class Reach { Fresh, Unreachable, Refused }
data class Read<out T>(val value: T?, val fetchedAt: Long?, val reach: Reach, val loading: Boolean, val error: String? = null)
data class Request(val path: String, val query: Map<String, String?> = emptyMap()) { val key: String }

/**
 * The seam. Screens ask this and never learn where an answer came from. …
 * A later, self-contained app implements this over an on-device engram and is
 * chosen by default; nothing above `core` names an implementation.
 */
interface Reader {
    /** What is held, at once; then what the source says. Completes after one round trip. */
    fun <T> read(request: Request, decode: (String) -> T): Flow<Read<T>>
    /** A POST whose answer is data and belongs to a moment: never cached. */
    suspend fun <T> ask(path: String, json: String, decode: (String) -> T): Read<T>
    /** Fire and forget; failure is silent. For `context/seen`. */
    suspend fun tell(path: String, json: String)
}
```

`Request.key` is the path plus its query sorted by name. `CacheRow(key PK,
origin, etag, body, fetchedAt)`. `ServerReader.read`:

1. Emit the held row for `(key, origin)` if any — `loading = true`.
2. `get` with its etag. `304` → touch `fetchedAt`, emit held, `Fresh`. `200` →
   upsert, emit new, `Fresh`. `IOException` → emit held (or null),
   `Unreachable`. `Refused` → `Refused`, and raise `engram.refused`.
   `PinMismatch` → raise `engram.pinMismatch`, emit `Unreachable`. Any other
   status → emit held with `error` = the server's `error` string, `Fresh`
   (the server was reached; it said no). A `404` also deletes the held row.
3. A body that does not decode is `error = "unreadable answer"`, never a crash.

A held row from another origin is never served: a re-pair elsewhere must not
show the previous server's notes. `store.clear()` on unpair also clears the cache.
Rows older than 30 days are deleted on open.

Room schema goes to version 2 with a `Migration(1, 2)` creating `cache` and
`asked`; `schemas/…/2.json` is generated and committed.

- [ ] Tests (`ServerReaderTest`, Robolectric + MockWebServer): first read emits
  loading-null then the value; second read emits held then sends
  `If-None-Match` and on 304 keeps the body and moves `fetchedAt`; a dead
  server emits held with `Unreachable`; with nothing held, null with
  `Unreachable`; 401 → `Refused` and the flag raised; 404 drops the row; a row
  held for origin A is not served for origin B; undecodable body → error, no throw.
- [ ] A migration test opens a v1 database with an outbox row and migrates it (`MigrationTestHelper` needs instrumentation; on the JVM assert instead that `Db.open` builds with the migration registered and the v1 tables survive, using Robolectric's SQLite: create v1 by raw SQL from `1.json`, then open).
- [ ] Commit.

### Task 3: The shapes

`Models.kt`: `Page<T>(items, next)`, `Hit` (every `SearchResult` key the
screens draw: `artifact_id, corpus_id, title, text, category, tags, score,
weak, past_cliff, retired, primed, in_sitting, model_written, origin_count,
due_at, due_in, borrowed_name, via, reason, status`), `CorpusRow`,
`CorpusDetail(… raw_text, chunks: List<Chunk>)`, `Chunk`, `ArtifactDetail`
(flattened chunk + `source`), `Lineage/Node/NodeSource`, `Version`, `Day` and
its rows, `DueRow(moment, title, named, opening)`, `Offer`. One
`Json { ignoreUnknownKeys = true; explicitNulls = false }`, every optional
field defaulted, so a newer server never breaks an older app.

`Api` builds requests: `search(q)`, `resurface()`, `due()`, `corpora(after)`,
`corpus(id)`, `artifact(id)`, `lineage(id)`, `versions(id)`, `day(date, tz)`.

**Drift guard, both sides.** Fixtures under
`android/core/src/test/resources/api/*.json`. Kotlin tests decode each. A Rust
test (`src/web/api.rs`, `the_android_fixtures_are_shapes_this_server_sends`)
reads the same files and, for corpus row, day, lineage, versions and offer,
asserts every key in the fixture is a key the real route answers with over a
fixture base; for a search hit, that `cli::search::fixture::hit(…, true, true)`
serialises every key the fixture's hit carries. A field renamed on the server
fails a Rust test naming the Android fixture.

- [ ] Read `Chunk`, `DueRow`/`Moment`, `CorpusStatus` serialisations from the Rust source before writing fixtures — copy, do not guess.
- [ ] Kotlin tests, Rust test, implement, both green, commit.

### Task 4: Ask, as a reducer

```kotlin
sealed interface AskFrame { Retrieved(shown, dropped), Needs(queries), Citations(hits), Reasoning(text), Token(text), Done(answer: AskAnswer), Failed(message) }
data class AskState(val question: String, val phase: Phase, val draft: String, val citations: List<Hit>, val note: String?, val answer: AskAnswer?, val error: String?)
enum class Phase { Retrieving, Writing, Done, Failed }
fun reduce(s: AskState, f: AskFrame): AskState
fun parseFrame(event: String, data: String): AskFrame?      // unknown event → null, ignored
data class Span(val text: String, val unsupported: Boolean)
fun annotate(answer: String, unsupported: List<String>): List<Span>
class Ask(transport: () -> Transport?, dao: AskedDao, clock) { fun run(question: String): Flow<AskState>; val history: Flow<List<AskedRow>> }
```

`Token` appends to `draft`. `Done` sets `answer` and `phase = Done`; the screen
then draws `annotate(answer.answer, answer.unsupported)` **instead of** the
draft — the server's whole answer replaces the concatenation, as the extension
does. `annotate` marks every occurrence of each literal, longest literal
first, never overlapping; an empty list is one plain span. `run` stores a
finished answer in `asked(question PK, body, askedAt)`; an unreachable server
yields `Failed("Server unreachable")`. A 503 yields the server's message.

- [ ] Tests: tokens accumulate; done replaces the draft; an `error` frame
  fails with its message; an unknown frame is ignored; `annotate` on two
  literals where one contains the other, on a repeated literal, on none;
  `run` against MockWebServer stores the answer and `history` lists it.
- [ ] Commit.

### Task 5: The shell

`Nav.kt`: start destination Search; bar Search · Capture · Today · Library;
top bar actions Queue (with `⇅n` while `outbox.rows` has `queued` rows) and
Settings; routes `artifact/{id}`, `corpus/{id}`, `day/{date}`, `ask?q=`.
A share or the tile still lands on Capture (`start` argument, unchanged).

`Reading.kt`: `rememberRead(engram, request, decode)` collecting `Reader.read`
with a `retry()`; `Unreachable(onRetry)` banner — *Server unreachable* ·
*Retry*; `FetchedAt(ms)` — *fetched 14:32* / *fetched 12 Sep*; `Label(text,
named)` — a name in the title style, an opening in body style and muted, never
bold; `clock(at, zone)`, `dayHeading(date)`.

- [ ] JVM tests for the pure parts: fetched-at words across today/earlier; queue count counts only `queued`.
- [ ] `:app:assembleDebug` and lint; commit.

### Task 6: Search, and the home beneath an empty box

```kotlin
sealed interface RailItem { data class Row(val hit: Hit, val rank: Int?, val loose: Boolean) ; object Cliff ; data class AllLoose(val n: Int) }
fun railOf(hits: List<Hit>): List<RailItem>
```

Read `Hit::rank` in `src/web/ui.rs` before writing it, and mirror it: rank is
the position among ranked hits and *continues* past the cliff; a weak row and
an associated row (`via != null`) have none; when every hit is weak the list
says so once (`AllLoose`) and rows drop their individual badge, as
`_results.html` does with `all_weak`. `Cliff` goes once, before the first
`past_cliff` row. Rows past it are drawn muted.

Row: rank or *loose* badge, `Label`, snippet, and the small words the web rail
uses — *primed*, *primed · seen*, *due in 2 h*, *model-written · n*, *done
reminder*. Search runs on the button and on the keyboard's search action, not
per keystroke: there is no interactive-embed deadline to honour on a phone and
every keystroke would be an embedding call over a VPN.

Empty box: offer card (`Reader.ask("/api/v1/context", situation.bundle())`,
then `tell("/api/v1/context/seen", …)` from a `LaunchedEffect` keyed on the
offer once composed), Due (done / snooze → outbox, row struck at once),
Worth seeing again. The card's line is built from `rung`: *pattern* → "like
{at in at_tz}", *similar* → "like what you opened", *tentative* → "{events}
earlier", *random* → nothing.

- [ ] Tests (`RailTest`): divider once before the first past-cliff row; none
  when no row is past it; ranks continue across it; weak and associated rows
  unranked; all-weak collapses to one notice. None of them orders anything.
- [ ] Commit.

### Task 7: Ask

The screen draws `AskState`: *retrieving…* with `shown`/`dropped` when known;
the draft as it grows; on `Done` the annotated answer — unsupported literals
in the warning colour with a dotted underline and one line beneath, *not in
any excerpt* — plus *truncated*, *from retired notes only*, *n left out* where
set; citations as hit rows that open the artifact. Below an idle box: earlier
questions from `history`, readable without the server. Leaving the screen
cancels the collection, which closes the call and frees the server's lane.

- [ ] Commit (the logic is Task 4's; this is drawing). Lint + assemble.

### Task 8: Today and any day

`DayScreen(date)`: heading, ‹ › to the neighbouring dates, sections Entries ·
Captured · Was due · Refers to this day · Sittings, each omitted when empty;
an empty day says *Nothing on this day*. Times through `clock(at, zone)` in
the device's zone, which is also the `tz` sent. Due rows act as on home.
A captured row opens its corpus; a moment and a sitting's opened rows open the artifact.

### Task 9: Library and a corpus

`LibraryScreen`: pages by `next`, loading the following page when the list's
end is reached; each page is its own cached `Request`. Row: `Label`, origin,
status when not `complete`, date. `CorpusScreen`: title, source URL, the text,
and its artifacts as rows; an image corpus shows *photo* / *document* as the
server labels it (fetching bytes is left for the device pass to justify).

### Task 10: An artifact

`ArtifactScreen(id)`: text (plain, selectable), title via `Label`, tags,
category, source line that opens the corpus; then Lineage — the tree indented
by depth, `kind`, *replaced*, *deleted since* for `missing`, *…and more* when
`truncated`, a separate *Replaced without being merged* — and Versions, oldest
first, each expandable. Lineage and versions are their own reads and their own
unreachable states: the text does not wait for them.

### Task 11: Say it, and run everything

- [ ] `android/README.md`: the screens, the Reader seam and what it is a cue for, what "Server unreachable" means.
- [ ] Programme document: one dated line under Part E.
- [ ] `./gradlew lintDebug testDebugUnitTest :app:assembleDebug` exit 0; `cargo test` for the drift guard. Commit.

## Self-review

Brief coverage: search with divider and demoted ranks → 6; ask streaming and
re-render annotated → 4, 7; day → 8; corpus list and detail → 9; artifact with
lineage and versions → 10; resurface → 6; "serves what the cache holds and
says when it was fetched" → 2, 5; the user's unreachable-is-plain and the
self-contained cue → 2 and the header. No task asserts ranking.
