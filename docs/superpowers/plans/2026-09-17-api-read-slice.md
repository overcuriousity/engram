# The API read slice (Part B, slice one) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `/api/v1` serves everything Part E's reading screens read, under one
read contract that Parts E and F and the watch can rely on without
renegotiating.

**Architecture:** One middleware gives every JSON `GET` an `ETag` and answers
`If-None-Match`. Every list answers one envelope. The day page and lineage are
extracted from their HTML handlers into serialisable fact models in `src/core/`
that both doors render from, so the two cannot disagree. No route is added for
the phone: additions sit on the resource they describe.

**Tech Stack:** Rust, axum 0.8, sqlx/SQLite, `sha2`, `base64`, askama for the
HTML siblings. Tests through `src/web/test_support.rs` and the patterns in
`src/web/api.rs`.

**Spec:** `docs/superpowers/specs/2026-09-08-android-companion-design.md`,
Part B. The programme says B whole wants its own document; this is B sliced to
what E reads, and the section below is the part of that document that cannot
be renegotiated later.

## The JSON shapes, and why

Five rules. Every route in Part B obeys them, in this slice and the next.

**1. A list is `{ "items": […], "next": "<cursor>" | null }`.** Always an
object, never a bare array, so a route that does not page today can start
without changing shape, and a client writes one pager. `next` is opaque: the
client passes it back as `?after=` and never looks inside. Where the UI pages,
the API pages (`/corpora`); where the UI shows a bounded list (`/search`,
`/resurface`, `/moments`, versions), `next` is `null` and `limit` is the only
knob. The corpora cursor is a keyset on `(created_at, id)` and not an offset,
because the list grows at its head and a phone that captures and then opens
the list is the normal case: an offset would repeat a row. A malformed cursor
is a `400`. Optional siblings of `items` are allowed (`explanation` on
search); a query flag never changes the shape.

**2. Lists carry summaries, details carry bodies.** `GET /corpora` rows have
no `raw_text` — for a captured book that is the book, two hundred times.
`GET /corpora/{id}` has it. Search rows keep `text`: the passage is the result.

**3. Time is Unix seconds; identity is an id; a label says whether it is a
name.** No `href`, no `"14:32"`. The HTML doors build those from the same
facts. A label crosses as `label` with `named: bool` beside it, because the
opening of a text standing in for a name must not be set as a name (see
`ui::RowLabel`). The one standing exception is wording the server owns on
purpose — `due_in` — which travels beside its timestamp so every door says the
same words.

**4. Every `GET` answering JSON carries a strong `ETag` and honours
`If-None-Match`.** The tag is a hash of the body, computed in one layer over
the API router. The handler still runs; a `304` saves the transfer, which is
what costs on a train. A per-route version counter would save the compute too
and would be a second source of truth about whether the data changed; the hash
cannot be wrong. `Cache-Control: private, no-cache` — store it, always
revalidate. The two byte routes (`/corpora/{id}/file`, `/image`) tag from the
corpus `content_hash` and answer `304` before reading the blob. Streams are
not tagged.

A consequence for clients, stated here because it is not visible in any
response: `GET /artifacts/{id}` records an open, and it does so on a
revalidation too. A client fetches artifact detail when a person opens it and
never from a background refresh.

**5. Errors are `{ "error": "…" }` and the status, from `src/error.rs`.** No
error codes: the status already separates *refused* (401), *wrong* (4xx),
*broken* (5xx) and *busy* (503), and that is every distinction a client acts
on.

## Global Constraints

- The web UI's HTML and htmx do not change behaviour. Existing tests in
  `day.rs`, `lineage_view.rs`, `artifact.rs` and `ui.rs` stay green unchanged.
- No route is added for a client; additions sit on the resource they describe.
- Not crossing: API token management, the extension offer, instance
  configuration and `--grant-judge`, applying a tuning recommendation.
- No string-parsing heuristics. UI copy on engram's pages stays minimal.
- Targeted tests before each commit; one full `cargo test` in the background
  at the end, reported after. Evidence of what ran goes in each commit message.
- Commits end with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`.

## File structure

- Create `src/web/etag.rs` — the `ETag` layer and `If-None-Match` matching.
- Create `src/web/page.rs` — `Page<T>` (the envelope) and `Cursor`.
- Create `src/core/day.rs` — `Day` and `Core::day`, the day's facts.
- Create `src/core/lineage.rs` — the lineage tree as facts; the walk moves here.
- Modify `src/web/lineage_view.rs` — becomes a rendering of `core::lineage`.
- Modify `src/web/day.rs` — `page` builds `DayTemplate` from `core::day::Day`.
- Modify `src/web/api.rs` — envelopes, new routes, the layer.
- Modify `src/store/corpora.rs` — `CorpusSummary`, `list_corpus_summaries`,
  `content_hash_of`.
- Modify `extension/shared/panel.js` — three call sites read `.items`.
- Modify `docs/` API reference if one exists (`grep -rn "api/v1/corpora" docs README.md`).

---

### Task 1: The ETag layer

**Files:** Create `src/web/etag.rs`; modify `src/web/mod.rs` (`mod etag;`),
`src/web/api.rs` (`api_router` gains `.layer(axum::middleware::from_fn(crate::web::etag::layer))`).

**Interfaces:** Produces `pub async fn layer(req: Request, next: Next) -> Response`,
`pub fn matches(if_none_match: &str, tag: &str) -> bool`,
`pub fn tag_of(bytes: &[u8]) -> String` (quoted, 32 hex chars of SHA-256).

- [ ] **Step 1: failing tests** in `etag.rs`:

```rust
#[test] fn a_tag_is_quoted_and_stable() {
    assert_eq!(tag_of(b"x"), tag_of(b"x"));
    assert_ne!(tag_of(b"x"), tag_of(b"y"));
    assert!(tag_of(b"x").starts_with('"') && tag_of(b"x").ends_with('"'));
}
#[test] fn if_none_match_takes_a_list_a_weak_tag_and_a_star() {
    let t = tag_of(b"x");
    assert!(matches(&t, &t));
    assert!(matches(&format!("\"other\", W/{t}"), &t));
    assert!(matches("*", &t));
    assert!(!matches("\"other\"", &t));
}
```

and in `api.rs` tests, through the real router:

```rust
#[tokio::test] async fn a_read_answers_an_etag_and_a_304_to_its_own_tag() {
    let (app, token) = app_and_token().await;
    let res = app.clone().oneshot(bearer_get("/api/v1/status", &token)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["cache-control"], "private, no-cache");
    let tag = res.headers()["etag"].to_str().unwrap().to_string();
    let again = app.oneshot(Request::builder().uri("/api/v1/status")
        .header("authorization", format!("Bearer {token}"))
        .header("if-none-match", &tag).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(again.headers()["etag"].to_str().unwrap(), tag);
    assert!(crate::web::test_support::body_of(again).await.is_empty());
}
#[tokio::test] async fn a_capture_changes_the_tag_of_the_list_it_lands_in() { /* GET /corpora, ingest via core, GET again: tags differ */ }
#[tokio::test] async fn an_error_and_a_post_carry_no_etag() { /* 404 on /corpora/nope; POST /moments/x/done */ }
```

(`/status` may carry a clock; if its body is not stable between two calls, use
`/api/v1/corpora` on an empty base instead.)

- [ ] **Step 2:** `cargo test --lib etag` — fails, module missing.
- [ ] **Step 3: implement.** The layer passes anything that is not `GET`
  through. On a `200` whose `content-type` starts `application/json` and which
  has no `etag` already, it buffers the body with `axum::body::to_bytes(body,
  usize::MAX)`, computes `tag_of`, and either answers `304` (headers `etag`,
  `cache-control`, empty body) when `matches`, or rebuilds the response with
  the two headers set. A buffering failure is `Error::Internal`. A handler
  that set its own `etag` (Task 3's byte routes) is left alone.
- [ ] **Step 4:** `cargo test --lib etag` and the three router tests pass.
- [ ] **Step 5:** commit `feat(api): every JSON read answers an ETag and honours If-None-Match`.

### Task 2: The envelope and the cursor; `/corpora` pages by keyset without `raw_text`

**Files:** Create `src/web/page.rs`; modify `src/store/corpora.rs`,
`src/web/api.rs`, `extension/shared/panel.js:649`.

**Interfaces:** Produces

```rust
#[derive(serde::Serialize)]
pub struct Page<T> { pub items: Vec<T>, pub next: Option<String> }
impl<T> Page<T> { pub fn whole(items: Vec<T>) -> Self /* next: None */ }
pub struct Cursor { pub at: i64, pub id: String }
impl Cursor { pub fn encode(&self) -> String; pub fn decode(s: &str) -> Result<Cursor> }
```

`encode` is base64url-no-pad of `"{at}:{id}"`; `decode` answers
`Error::Validation("after: not a cursor this server issued")` for anything
else. In the store:

```rust
pub struct CorpusSummary { id, origin, label: String, named: bool, status,
    created_at, updated_at, coverage, near_dupe_of, near_dupe_score,
    source_url, restored_at, metadata }
pub async fn list_corpus_summaries(&self, before: Option<&(i64, String)>, limit: i64) -> Result<Vec<CorpusSummary>>
```

The query selects `substr(raw_text, 1, 400) AS opening` and never `raw_text`;
`label` is `ui::corpus_label`'s rule moved to a place the store can call —
move `corpus_label` to `src/core/label.rs` if `store` cannot depend on `web`,
and re-export it from `ui` so no caller changes. Order `created_at DESC, id
DESC`; the keyset predicate is `created_at < ? OR (created_at = ? AND id < ?)`.
The handler fetches `limit + 1` to learn whether there is a next page.
`ListParams` becomes `{ limit, after: Option<String> }`; `offset` is removed.

- [ ] **Step 1: failing tests:** cursor round-trips; a tampered cursor is a
  `400` through the router; `GET /corpora` answers `{items, next}`; no item has
  a `raw_text` key; three corpora read at `limit=2` then `after=next` yield
  each id exactly once **with a fourth captured between the two reads**; an
  untitled corpus has `named: false` and its opening as `label`.
- [ ] **Step 2:** run, see them fail. **Step 3:** implement. **Step 4:** pass,
  plus `cargo test --lib store::corpora`.
- [ ] **Step 5:** `panel.js:649` reads `(await engramApi.call(...)).items`.
  Lines 216 and 592 are `POST`s and do not change — confirm by reading them.
- [ ] **Step 6:** commit `feat(api)!: lists answer one envelope; /corpora pages by cursor and drops raw_text`.

### Task 3: Envelopes on search, resurface and moments; tags on the byte routes

**Files:** modify `src/web/api.rs`, `src/store/corpora.rs`
(`content_hash_of(&self, id) -> Result<Option<String>>`),
`extension/shared/panel.js:410,439`.

- `search` answers `{"items": results, "next": null}` and, with `explain`,
  the same object plus `"explanation"`. `resurface` and `list_moments` answer
  `Page::whole`. `/search/stream` is not touched.
- `get_file` and `get_image`: read `content_hash_of(id)` first (`404` when
  absent); the tag is `"<content_hash>"` for the file and
  `tag_of(format!("{content_hash}?{raw_query}"))` for the image, whose bytes
  depend on its query; on `etag::matches` answer `304` without reading the
  blob; otherwise set `etag` and `Cache-Control: private, no-cache` on the
  answer.

- [ ] **Step 1: failing tests:** search envelope keeps `weak` and
  `past_cliff` on a row that has them (extend the existing weak-hit test's
  assertions rather than building a new fixture); `explain` adds a key and
  changes nothing else; moments envelope; file route `304` on its own tag.
  Update every existing test in `api.rs` that indexes these bodies as arrays
  (`grep -n "as_array\|\[0\]" src/web/api.rs` inside the search, resurface and
  moments tests).
- [ ] **Steps 2–4:** fail, implement, pass: `cargo test --lib web::api`.
- [ ] **Step 5:** `panel.js` both search call sites read `.items`. Load the
  extension's own tests if any exist (`ls extension/test* 2>/dev/null`) and run them.
- [ ] **Step 6:** commit `feat(api)!: search, resurface and moments answer the envelope; files revalidate by content hash`.

### Task 4: The day, as facts

**Files:** Create `src/core/day.rs`; modify `src/core/mod.rs`,
`src/web/day.rs`, `src/web/api.rs`.

**Interfaces:** Produces

```rust
#[derive(serde::Serialize)]
pub struct Day { pub date: String, pub tz: String, pub from: i64, pub to: i64,
    pub entries: Vec<DayCorpus>, pub captured: Vec<DayCorpus>,
    pub was_due: Vec<DayMoment>, pub refers: Vec<DayMoment>,
    pub sittings: Vec<DaySitting> }
pub struct DayCorpus { pub id: String, pub label: String, pub named: bool, pub at: i64, pub text: String }
pub struct DayMoment { pub id: String, pub artifact_id: String, pub label: String,
    pub at: Option<i64>, pub kind: &'static str /* due|event */, pub done: bool, pub span: Option<String> }
pub struct DaySitting { pub opened_at: i64, pub closed_at: i64, pub query: String,
    pub searches: usize, pub opened: Vec<DayOpened> }
pub struct DayOpened { pub id: String, pub label: String, pub named: bool }
impl Core { pub async fn day(&self, date: &str, tz: chrono_tz::Tz) -> Result<Day> }
```

`Core::day` is `day.rs::page`'s body from the date parse to the sittings
loop, moved verbatim with its comments: the lenient-parse round trip, the
`metadata["day"]` skip, `bounds`. A date that does not parse or has no bounds
is `Error::NotFound`. `bounds`, `day_start` move with it and `web/day.rs`
imports them. `web/day.rs::page` calls `core.day`, then maps to `Line`,
`Opened`, `Sitting` exactly as today (`href` from the id, `when` through
`hm`, `detail` from `done`/`span`/`text`).

Route: `GET /days/{date}?tz=<IANA>`; `tz` goes through `core::moments::zone`
as the HTML route's does.

- [ ] **Step 1:** run `cargo test --lib web::day` and record the passing count — the oracle for the extraction.
- [ ] **Step 2: failing API tests,** one per thing the HTML sibling is tested
  for: a journal entry lands in `entries` and a capture in `captured`;
  `/days/2026-8-30` answers the same day as `/days/2026-08-30`; an entry
  naming another day is skipped on the day it was written; a done reminder is
  `done: true`; `/days/yesterday` is a `404`; the answer carries an `etag`.
- [ ] **Steps 3–4:** implement; `cargo test --lib web::day core::day web::api::tests::day` — the Step 1 count unchanged, new tests green.
- [ ] **Step 5:** commit `feat(api): the day page's facts move to core and gain a JSON door`.

### Task 5: Lineage and versions

**Files:** Create `src/core/lineage.rs`; modify `src/web/lineage_view.rs`,
`src/web/api.rs`.

**Interfaces:** Produces

```rust
#[derive(serde::Serialize, Default)]
pub struct Lineage { pub roots: Vec<Node>, pub also_replaced: Vec<Node>, pub truncated: bool }
#[derive(serde::Serialize)]
pub struct Node { pub id: String, pub label: String, pub named: bool,
    pub kind: &'static str, pub created_at: i64,
    pub source: Option<NodeSource>, pub replaced: bool, pub missing: bool,
    pub children: Vec<Node> }
pub struct NodeSource { pub corpus_id: String, pub label: String, pub line_start: Option<i64>, pub line_end: Option<i64> }
pub async fn build(store: &Store, id: &str) -> Result<Lineage>
```

The walk (`Walk`, `MAX_DEPTH`, `MAX_NODES`, `sorted`) moves to core with its
comments. Read `lineage_view.rs:150-330` before moving it: `NodeSource`'s
fields must be exactly what `source_href` is built from today — adjust the
struct to the code, not the code to this plan. `lineage_view::build` becomes
`core::lineage::build` mapped into the existing `LineageNode` (`when`
formatted, `source_href` built), so `artifact.rs` and the templates do not
change.

Routes: `GET /artifacts/{id}/lineage` (`404` for an unknown artifact; an
artifact with no history answers the empty `Lineage`, not a `404`) and
`GET /artifacts/{id}/versions` answering `Page::whole` of
`{n, title, text, caveats, created_at}`, oldest first, `404` for an unknown
artifact. Neither records an open.

- [ ] **Step 1:** `cargo test --lib web::lineage_view web::artifact` — record the count.
- [ ] **Step 2: failing API tests:** a merge of two captured artifacts answers
  two leaves under one root with `kind`s right; a deleted source is
  `missing: true`, not absent; a superseded-without-merge artifact is in
  `also_replaced`; an edited artifact lists its versions oldest first; both
  routes `404` on an unknown id. Reuse the fixtures the `lineage_view` tests build.
- [ ] **Steps 3–4:** implement; counts unchanged, new tests green.
- [ ] **Step 5:** commit `feat(api): lineage and versions sit on the artifact they describe`.

### Task 6: The offer

**Files:** modify `src/web/ui.rs` (extract), `src/web/api.rs`.

Extract from `context_offer` and `context_seen` the two halves both doors
need, as `pub(crate)` functions in `ui.rs` beside them (they are web-layer
concerns — snippet length, the unreadable-card rule — and stay there):

```rust
pub(crate) async fn compute_offer(tenant: &Tenant, raw_bundle: &str) -> Option<(Offer, String /* snippet */)>
pub(crate) async fn record_seen(tenant: &Tenant, artifact_id: &str, rung: &str, slot: Option<i64>)
```

`compute_offer` carries the `recommends()` gate, `record_context_event`, the
`offer` call with its warn-and-None, and the nothing-to-read rule. The HTML
handlers become calls to them.

Routes: `POST /context` — the body is the bundle as JSON (read as raw text and
handed to `compute_offer`, which is what the form field held); answers
`{"offer": null}` or `{"offer": {artifact_id, label, named, snippet, rung,
slot, events, blocks, at, at_tz}}`. `label`/`named`: `Offer.title` when not
empty, else the snippet with `named: false`. `rung` is `Rung::as_str`.
`POST /context/seen` takes `{artifact_id, rung, slot}` and always answers
`204`, as the HTML route does and for its stated reason.

- [ ] **Step 1:** `cargo test --lib web::ui::tests::context` (find the exact
  filter with `grep -n "fn .*context\|fn .*offer" src/web/ui.rs`) — record the count.
- [ ] **Step 2: failing API tests:** recommendations off → `{"offer": null}`;
  a base with a recorded pattern → an offer whose `artifact_id` exists (reuse
  the fixture of the HTML test that expects a filled card); `seen` with an
  unknown rung is a `204` that records nothing; `seen` with a real one writes
  `recommended_shown`.
- [ ] **Steps 3–4:** implement; counts unchanged, new green.
- [ ] **Step 5:** commit `feat(api): the offer card gains a JSON door`.

### Task 7: Say it where a reader looks, and run everything

- [ ] **Step 1:** wherever the API is documented (`grep -rln "api/v1/search" README.md docs`), update the shapes that changed and add the new routes and the five rules, briefly.
- [ ] **Step 2:** tick Part B's first slice in the programme document with one dated line: what landed and what waits for slice two.
- [ ] **Step 3:** `cargo fmt --check && cargo clippy --all-targets -- -D warnings`.
- [ ] **Step 4:** full `cargo test` in the background; report the result after; fix what it finds.
- [ ] **Step 5:** commit `docs(api): the read contract, and what slice one carries`.

## Self-review

Coverage against the brainstorm: contract rules 1–5 → Tasks 1–3; six changed
routes → Tasks 2–3 (`/corpora/{id}` and `/artifacts/{id}` change only by
gaining the layer, Task 1); four new routes → Tasks 4–6; readers → Tasks 2–3;
tests mirror each HTML sibling → Tasks 4–6. One deliberate simplification
since the brainstorm: routes the UI does not page answer `next: null` rather
than a cursor wrapping an offset — nothing to page means nothing to encode.
