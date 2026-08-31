# A sense of time — moments, reminders, the day — Design

Date: 2026-08-30
Status: implemented (2026-08-30)

> **Implementation notes.** The classifier runs in the `Moments` stage after
> `Embed`, so the capture receipt's *kept as today's entry — undo* covers the
> cue case (checked synchronously in `ingest_capture`) and the day page's
> *not an entry* covers the classified case. `regex` (date rules) and
> `iana-time-zone` (the CLI's own zone) entered the tree. The prototype cache
> is keyed by embed model, so `--reindex` under the same model has nothing to
> clear. A forced intent (`-r`, `?intent=remind`) is recorded as `source =
> 'cue'`, not `'set'`: it is the stage's own row and a re-read replaces it. The
> search row carries `due_at` and a ready-made `due_in` ("in 2 h") so every
> door prints the same words. Under the fake embedder the test core sets
> `time.intent_at = 0.99`, since eight-dimensional hash vectors clear 0.80 by
> chance.
Adds `src/core/moments.rs`, `src/jobs/moments.rs`, `src/jobs/remind.rs`,
`src/web/day.rs`, `templates/day.html`, `templates/_due.html`; touches
`src/store/schema.sql` (one table), `src/store/control_schema.sql` (one
column), `src/store/jobs.rs` (two stages), `src/core/background.rs`,
`src/core/ingest.rs` (one origin), `src/core/search.rs` (one bounded lift),
`src/web/workspace.rs`, `src/web/ui.rs`, `src/web/api.rs`, `src/mcp/mod.rs`,
`src/cli/`, `src/config.rs`, `templates/settings.html`, `assets/app.js`.
One table, one column, two job stages, and one model call that is only ever
spent on a capture the operator flagged as a reminder.
See §10 for what it is not allowed to break.

## 1. Why

engram knows what it holds and has no idea when anything is. `created_at` is
everywhere and is read by nothing but a sort order and a "today" label. The one
place a clock was needed — the context recommendation — got `chrono`, the
client's IANA zone and `core::context::local_time`, and then nothing else in
the tree used them.

So three things a personal knowledge base is expected to do cannot be done:

**Keep its word.** A note that says *remind me Friday to send the invoice* is
stored verbatim, embedded, ranked — and on Friday the base says nothing,
because nothing in it knows Friday is a time and not a word. A base you cannot
hand a future obligation to is a base you keep a second app beside.

**Say what a note is about, in time.** *Zahnarzt 12.9.* refers to a day that
is not the day it was captured. Searching *what is coming up* finds nothing:
the embedder cannot know that a string of digits is next week, and the only
date the base holds is the capture's.

**Show a day.** Every capture, search, open and answer already carries a
timestamp, and no page reads them as a day. *What did I write down on Tuesday*
is a question over `corpora`, `search_events`, `interaction_events` and
`pursuits`, all of which exist; the page does not.

And the operator has said the last two out loud, into the base, on the day of
writing: *engram should have a sense of time, and also be able to 'act' on
what the user…* sits on the front page under LAST CAPTURED, undated.

## 2. What is built

1. **One table, `moments`** — a time attached to an artifact. A reminder and a
   date a note refers to are the same row shape, because the front page asks
   them the same question. §3.
2. **A write path that reads time out of a capture** — a cue table in ten
   languages, the embedder reused as an intent classifier, absolute dates by
   rule, and one schema-constrained model call only when a reminder was meant.
   §4.
3. **The front page says what is due**, below the recommendation, in its own
   colour, with done / snooze / set-date. §5.
4. **A day page**, and a journal origin for what was written as an entry. §6.
5. **Push**, to Gotify or a UnifiedPush endpoint, from a unit that sleeps until
   the next due moment. §7.
6. **One bounded lift** in search for a due reminder, and no other ranking
   change. §8.

### Non-goals

- **An Android app**, a CalDAV or `.ics` feed, email. The RRULE spelling in §3
  is what keeps the feed cheap to add; nothing else here anticipates it.
- **Full RRULE.** A subset, generated and accepted; the grammar arrives with the
  feed that needs it.
- **"On this day" resurfacing.** README, *decided against*: a resurfacing list
  is a different application. A reminder is the opposite — the operator asked —
  and the day page shows a day the operator navigated to. Nothing here shows
  the operator something from the past unasked.
- **Editing a moment's text.** The note is the reminder. Change the note.
- **Extracted event dates in ranking.** §8.

## 3. Moments

In `schema.sql`, per tenant:

```sql
CREATE TABLE IF NOT EXISTS moments (
  id            TEXT PRIMARY KEY,
  artifact_id   TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  -- 'due' | 'event'
  kind          TEXT NOT NULL,
  -- Unix seconds. NULL for a reminder the base could not date: kept, and
  -- shown asking for its date, rather than dropped.
  at            INTEGER,
  until         INTEGER,
  -- IANA zone the moment was read in. Recurrence and the day page need the
  -- wall-clock, and a Unix integer alone cannot give it back across DST.
  tz            TEXT NOT NULL,
  -- RRULE subset, or NULL for a single occurrence. §3.2.
  rule          TEXT,
  -- 'set' | 'cue' | 'classified' | 'extracted'. How the row came to be, so
  -- the page can say "you set this" against "read from the note".
  source        TEXT NOT NULL,
  -- The text the date was read from, verbatim. Shown beside an extracted
  -- moment so a misread is visible.
  span          TEXT,
  done_at       INTEGER,
  snoozed_until INTEGER,
  -- §7. Set once the push went out, so a restart never sends it twice.
  notified_at   INTEGER,
  created_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_moments_open ON moments(kind, done_at, at);
CREATE INDEX IF NOT EXISTS idx_moments_artifact ON moments(artifact_id);
```

`kind = 'due'` is a reminder; `kind = 'event'` is a date the note refers to.
The table lives in the tenant database and not beside the queue, because a
reminder is a fact about the operator's knowledge and the queue is instance-wide
work. The front page's read is one index walk: open rows of a kind, ordered by
`at`.

### 3.1 What can change on a row

`done_at`, `snoozed_until` and `notified_at`. Nothing else, ever. Done and
snooze each have an undo that nulls the column. A moment whose reading was
wrong is not corrected in place: **set date** on the page inserts a new row with
`source = 'set'` and marks the old one done, so the misreading stays on record.
The trace is fixed.

### 3.2 Recurrence

`rule` is an RRULE string restricted to `FREQ=DAILY|WEEKLY|MONTHLY|YEARLY`,
`INTERVAL`, `BYDAY` (weekday codes only, no ordinals), `BYMONTHDAY`, and one of
`UNTIL` or `COUNT`. `core::moments::next_after(rule, at, tz)` computes the next
occurrence in wall-clock time in `tz` — so *every Monday 09:00* stays 09:00
across a DST change — and is the only code that reads the string. Anything
outside the subset is refused at write time with the reason.

Marking a recurring moment done inserts the next occurrence as a new row with
the same `rule` and `source`, and the done row stays. The history of a
recurring reminder is its rows.

## 4. The write path

`src/core/moments.rs` holds the pure parts; `jobs/moments.rs` is the stage that
runs them. `Stage::Moments` is armed for an artifact by the `Embed` stage on
completion (`jobs/embed.rs`), background class, because the artifact's own
embedding is what step 2 compares. It runs once per artifact and is idempotent:
it deletes what it wrote with `source IN ('cue','classified','extracted')` for
that artifact before writing, so a re-embed re-reads and a `set` row survives.

In order, cheapest first:

**1. The cue table.** A `const` in `core::moments`, two intents, ten languages
(English, German, French, Spanish, Portuguese, Italian, Dutch, Polish, Turkish,
Russian — the set is a table, and a row is a one-line addition):

| intent | examples |
|---|---|
| `remind` | *remind me*, *erinnere mich*, *rappelle-moi*, *recuérdame*, *lembre-me*, *ricordami*, *herinner me*, *przypomnij mi*, *hatırlat*, *напомни* |
| `journal` | *today I*, *heute*, *aujourd'hui*, *hoy*, *hoje*, *oggi*, *vandaag*, *dzisiaj*, *bugün*, *сегодня*, *dear diary*, *liebes Tagebuch* |

Matched case-insensitively, as a whole word, in the first 200 characters. A
hit is certain and skips step 2. `journal` cues match only at the start of the
text: *heute* mid-sentence is a word, at the head of a note it is an entry.

**2. The embedder as classifier.** Each intent has ten to fifteen
prototype *sentences* — the shape a note opens with (*remind me to send the
invoice on friday*, *heute war ein langer tag*), never the bare cue word, which
embeds as a dictionary entry and sits near nothing. Embedded once per embed
model through the same `Embedder` and gate the `Embed` stage uses, and cached
under `meta` key `moments.prototypes.<model>` — keyed on the model so a switch
invalidates them, cleared by `--reindex`, held in `Core` after the first read.

The artifact's vector is read back from Qdrant by id — one point read, no
re-embedding — and compared only for the *first passage* of the corpus: a
pasted article that quotes "remind me" in paragraph nine is an article. The
score is the **maximum** cosine over an intent's prototypes, not the mean; a
note matches one phrasing, not the average of ten languages. `core::gaps::cosine`
is the function. The best intent above the line fires as `classified`.

The line is measured, not set: the prototypes are scored against the sample of
the tenant's own artifact vectors that `vector.sample` already draws, the 99th
percentile of that "ordinary note against a prototype" distribution is where
*unrelated* ends, and the line is that value rounded up to a hundredth and
clamped to `[max(time.intent_at, 0.70), 0.92]` — `gaps::link_threshold`,
applied to a different question. The configured value is a floor at every base
size, not only below the calibration threshold: it is what an operator raises
to stop ordinary notes becoming reminders, so a measurement may not undercut
it, and a floor set above the ceiling carries the ceiling up with it. Below
thirty sampled vectors `time.intent_at` (config, 0.80) stands on its own.
This is what stops a base written in German from firing `journal` on every
second note because German prose sits nearer the German prototypes than English
prose does. `bge-m3` is multilingual, so the ten languages are the training set
and the eleventh comes for free; a cue-table language is also a prototype
language. No new dependency, no new call, and no trained head: ten examples a
class is the regime where nearest-prototype beats anything fitted, and the
labels a head would want — undos on *kept as today's entry*, dismissals of a
misfired reminder — are already recorded by `source` and the undo for whoever
wants to fit one later.

**3. Absolute dates, by rule, on every artifact.** ISO (`2026-09-12`,
`2026-09-12T14:00`), day-first and month-first numerics with the separator
deciding (`12.9.`, `12.09.2026`, `12/09/2026`, `9/12/2026` when the locale in
the corpus metadata is `en-US`), month names in the ten languages (`12. Sept`,
`Sept 12`, `12 septembre`), an optional time. Each becomes an `event` moment
with `span` set to the matched text and `at` at the day's 09:00 in the
capture's zone when no time was given. A year-less date is the next occurrence
on or after the capture date. Bare numbers are not dates: `12.9` alone is a
version, and `core::infer::facts` already learned that lesson the hard way.

This is the one step that runs on a capture nobody flagged, and it makes no
model call. An `event` row is a claim about the note, shown with its `span`, and
wrong at most in the way a regex is wrong — visibly.

**4. When `remind` fired.** If a chat model is configured, one call to the
`efficient` tier through `chat(messages, Some(schema))` with:

```json
{ "when": "ISO-8601 local wall-clock or null",
  "rule": "RRULE subset or null",
  "what": "the obligation in the note's own words" }
```

The prompt carries the capture's wall-clock time and zone, and the text of the
first passage. `what` is not stored anywhere — it is asked for because a model
made to restate the obligation dates it more reliably, and discarded because
the note is the reminder. `when` becomes `at`; a `rule` outside the subset is
dropped with a `tracing::warn` and the moment is single.

If no chat model, or the call fails after the queue's attempts: the relative
table — *tomorrow*, *next monday*, *in 3 days*, *morgen*, *nächsten Freitag*,
*in 2 Wochen*, in the ten languages — plus whatever step 3 found. The nearest
future date wins.

If nothing: a `due` row with `at = NULL`, `source` as fired. It is shown on the
front page asking for a date. A reminder the base heard and could not date is
still a reminder; dropping it would be the base deciding the operator did not
mean it.

**5. When `journal` fired** on a capture whose origin is `ui`, `cli`, `share` or
`extension`: the corpus's `origin` becomes `journal`. This is the one write to
`corpora` outside ingest, and it is a channel label, not content. The captured
line on the workspace says *kept as today's entry — undo*, and undo restores the
origin. A misfire files a note under a day it already belongs to; that is the
whole cost, and why this intent may be inferred where a reminder's date may not.

`origin` gains one value, `journal`, beside `cli` and `share` from the capture
doors spec.

### 4.1 Zone

The workspace's bundle already carries `tz` (`assets/app.js:821`); capture
requests from the box send it as a field and it lands in
`metadata["tz"]`. The CLI sends the process's zone; `POST /api/v1/capture` and
MCP accept `?tz=` / a `tz` param and default to `time.default_tz` (config, the
server's zone when absent). The stage reads `metadata["tz"]` and falls back the
same way. A moment always records the zone it was read in.

## 5. The front page

Below `#context-offer` and above the keyhint row — the band the operator
pointed at — a `#due` area fetched on load like the offer is (`/ui/due`),
because the zone comes from the browser. It is rendered around something or
not at all: a base with nothing due shows nothing here, no empty box.

Order, then `at`:

1. **Overdue** — `at < now`, undone, not snoozed.
2. **Due** — within `time.horizon_hours` (default 48).
3. **Undated** — `at IS NULL`, asking *when?* with a date input.

Each row: the artifact's title as a link, *due tomorrow 09:00* / *overdue since
Tuesday* in local wall-clock, a `↻ weekly` mark when recurring, and three
buttons — **done**, **snooze** (1h · tomorrow 09:00 · next Monday 09:00), **set
date**. Done and snooze answer with the row struck through and an **undo** that
holds for the page's lifetime. Warm colour (`--due`), one token, defined in both
themes. The area is removed on the first keystroke the way the offer is: the box
is the application, and what is due is what the base says when nothing is
being asked of it.

Beneath, quieter, **Coming up**: `event` moments in the next seven days, title
and *refers to Fri 12 Sept*, with the `span` in a title attribute. No buttons —
an event is not something to do.

`LAST CAPTURED`'s *today* / *yesterday* stamps become links to the day (§6).

### 5.1 The other doors

- **CLI**: `engram` with no verb prints what is due after the status it prints
  today; `engram -r "…"` captures with `remind` forced (no classifier, straight
  to step 4); `engram -j "…"` captures with `origin = journal`; `engram -s`
  hits carrying a due moment print *due tomorrow* on the meta line, as they
  print *lifted* today.
- **MCP**: `ingest` gains `origin` (`journal` or absent) and `tz`; a new
  `due` tool returns the same three lists as `/ui/due`, markdown, so an agent
  mid-session knows what the operator is holding. `search` hits carry the due
  line the CLI does.
- **API**: `GET /api/v1/moments?from&to&kind`, `POST /moments/{id}/done`,
  `/snooze` (body: `until`), `/undo`, `POST /artifacts/{id}/moments` (body:
  `at`, `rule?`, `kind`) — the explicit door, `source = 'set'`.

## 6. The day

`/ui/day/YYYY-MM-DD` (`src/web/day.rs`, `templates/day.html`), `/ui/day/today`
redirecting. Prev / next day links; a day with nothing on it says so and still
has the box.

At the top, the box — the workspace's textarea, one verb, **Keep as entry** —
posting to `/ui/day/{date}/entry`, which is `Core::ingest_capture` with
`origin = journal` and `metadata["day"] = "YYYY-MM-DD"`. An entry written on
Wednesday about Tuesday belongs to Tuesday; `day` says which, `created_at` says
when. The day page groups by `metadata.day` when present and by `created_at` in
the viewer's zone otherwise.

Then, in order, each section absent when empty:

- **Entries** — `origin = journal`, in full, oldest first. A diary reads down.
- **Captured** — everything else captured that day, the LAST CAPTURED line
  shape.
- **Was due** — `due` moments with `at` on the day, and what happened to them.
- **Refers to this day** — `event` moments on the day, with `span`.
- **Sittings** — from `pursuits` (`opened_at`, `closed_at`, `queries`,
  `sources`) and `search_events` for the day: *14:02–14:40 — six searches
  around "qdrant payload filter", three opened*, the leading query verbatim, the
  opened artifacts linked. No model call and no prose generated: the sentence
  is a template over counts and one quoted query. A pursuit is already what a
  sitting looked like afterwards (`jobs/pursuit.rs`); this is the first page
  that shows one to the person who had it.

Everything on the page is a read over tables that exist plus `moments`. The
day page is the base's own diary, written by nobody.

## 7. Push

### 7.1 Where it goes

`users` in `control_schema.sql` gains `notify TEXT NOT NULL DEFAULT '{}'` — a
namespaced JSON like `corpora.metadata`, added through `migrate` the way
`class` was. Two keys:

```json
{ "gotify": { "url": "https://gotify.example/message", "token": "…" },
  "unifiedpush": { "endpoint": "https://nextcloud.example/…/push/…" } }
```

Both are one `POST` with a text body and no library: Gotify takes
`{title, message, priority}` with the token in a header; UnifiedPush takes the
message as the body. The Settings page gets a **Notifications** section with the
two forms and a **send a test** button per channel; the token is shown masked
after saving. Per user, because a tenant's reminders are that tenant's.

### 7.2 The unit

`Stage::Remind`, periodic in the sense of the one-system spec, but not on an
interval: it is **armed at the next due moment**.

```
run_after = MIN(at) FROM moments
            WHERE kind = 'due' AND done_at IS NULL AND notified_at IS NULL
              AND at IS NOT NULL
              AND (snoozed_until IS NULL OR snoozed_until < at)
```

Every write that can change that minimum — a new `due` row, done, snooze,
undo, set date — calls `Store::rearm_remind(subject)`, which recomputes it and
re-arms with `rearm_idle_seq` (never `enqueue`: the gate's comment on re-arming
a unit that is backing off applies here too). A user whose `notify` is `{}`
never has the unit armed; saving a channel arms it.

On wake: every open `due` row with `at <= now` and `notified_at IS NULL` is
posted, one message per moment — the artifact's title and first line, and a
link to the artifact — and `notified_at` is set in the same transaction as the
successful post. A failed post is the queue's backoff. Then it re-arms at the
new minimum, or not at all.

A snooze that ends re-notifies: snoozing nulls `notified_at`. A recurring
moment's next row is a new row and notifies on its own.

Nothing polls. A base with one reminder next month has one job row sleeping
until next month.

## 8. Ranking

One change, bounded, explained, and only for what the operator set: a search
hit whose artifact carries an open `due` moment with `at` inside
`time.horizon_hours` is lifted at most `associate.prime_lift` places by the
same second pass that lifts a primed hit (`core/search.rs`, the walk around
line 600), and never over an exact match or across the cliff. The row says *due
tomorrow*, as a primed row says *primed*. `time.lift = true` by default.

A due date the operator set is a fact about what the operator wants this week,
not a guess about relevance, which is why it ships on. Extracted `event` dates
do not lift: a misread date would move an unrelated hit, and there is no
measurement yet of how often step 3 misreads. That waits for the harness or for
a month of `span`s on the day page.

## 9. Config

```toml
[time]
horizon_hours = 48     # what the front page and the lift call "due"
coming_up_days = 7     # the event list
intent_at = 0.80       # cosine at which a prototype fires an intent
lift = true            # §8
default_tz = ""        # doors that send none; empty = the server's zone
```

No `[time].enabled`: a base with no moments renders nothing and arms nothing.
Turning a channel off is emptying it on Settings.

## 10. What this must not break

- **No model call on the query path.** The front page, the day page, `due`,
  `search` read `moments` and nothing else. The one call is in `Stage::Moments`,
  behind a fired intent, at write time.
- **Capture spends no call on paragraphs nobody flagged.** Steps 1–3 are string
  and vector operations; step 4 is reached only past a cue or a prototype.
- **The trace is fixed.** `moments` rows are never rewritten past
  `done_at`/`snoozed_until`/`notified_at`; a wrong date is a new row. The
  `origin` rewrite for a journal entry is a channel label with an undo, and
  `raw_text` is untouched.
- **No exact match is buried; the lift is bounded.** The `due` lift reuses the
  pass whose tests already pin *the hit that moved must say so* and *a hit is
  lifted once*.
- **The claim index stays covering.** `Remind` is one row per tenant armed by
  `run_after`; nothing about the walk changes.
- **Repair stays its own ticker.** `Remind` is armed by writes, not by the
  schedule, and is not repaired by it.
- **The recommendation ladder is untouched.** `#due` is a second area below
  `#context-offer`, not a rung. A reminder has a hard reason; a rung is a
  guess with its reason shown. They are not the same thing and are not styled
  as one.
- **The MCP `search` and `read` output stays what the tests pin**; the due line
  is one more meta line, appended.

## 11. Testing

- `core::moments`: cue table — one test per language per intent, and the
  mid-sentence *heute* that must not fire; date rules — a table across the
  formats in §4 step 3 with the version-number and list-marker negatives;
  relative table; `next_after` across a DST change in `Europe/Berlin` and a
  month-end `BYMONTHDAY=31`; the RRULE subset refusing an ordinal `BYDAY`.
- Classifier: `infer::fake` embedder with fixed vectors; fires at the
  threshold, not below, first passage only.
- `jobs/moments`: fires → schema call → `due` row with `at`; no chat model →
  relative table → `at`; nothing → undated row; re-run replaces `extracted`
  rows and keeps a `set` row; `journal` cue rewrites origin, undo restores it.
- `jobs/remind`: arming follows the minimum; done/snooze/undo re-arm; a user
  with `{}` never arms; `notified_at` set with the post; snooze nulls it;
  `wiremock` for the two channel shapes.
- Web: `/ui/due` order and emptiness; the day page composes the five sections
  and groups by `metadata.day` over `created_at`; the entry box writes
  `origin = journal`; undo on the workspace line.
- Search: the due lift bounded and labelled, the `primed` tests' shape.
- CLI and MCP: the due line on a hit, the `due` tool's markdown, `-r` forcing
  intent.
