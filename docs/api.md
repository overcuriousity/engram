# The REST API's read contract

`/api/v1`, authenticated by a bearer token or a session cookie. This page is
the part of the API that clients — the CLI, the extension, the Android app,
and later the watch — are entitled to rely on. It states the rules every route
obeys and lists the read routes. It is not a full reference; the handlers in
`src/web/api.rs` carry their own documentation.

## Five rules

**1. A list is `{ "items": […], "next": "<cursor>" | null }`.** Always an
object, never a bare array. `next` is opaque: pass it back as `?after=` and do
not look inside it. `null` means there is no further page — either this was
the last one, or the list is bounded by `limit` and does not page at all.
Optional keys may sit beside `items` (`explanation` on search); a query flag
never changes the shape. A cursor this server did not issue is a `400`.

**2. Lists carry summaries; details carry bodies.** A row of `GET /corpora`
has no `raw_text`; `GET /corpora/{id}` has it. Search rows keep `text`, because
the passage is the result. A day keeps its entries' text, because an entry is
read on the day.

**3. Time is Unix seconds, identity is an id, and a label says whether it is a
name.** No `href`, no preformatted clock time: a client builds its own links
and formats in its own locale. Wherever a row has a `label` it has `named`
beside it; `named: false` means the label is the opening of the text standing
in for a name, and must not be set as one. The one exception is wording the
server owns on purpose — `due_in` on a search row — which travels beside its
timestamp so every door says the same words.

**4. Every `GET` that answers JSON carries a strong `ETag` and honours
`If-None-Match`.** `Cache-Control: private, no-cache`: keep the body, and ask
before using it. A `304` has no body. The original file and the image of a
corpus are tagged from its content hash, keep their `max-age`, and answer
`304` without reading the bytes. Streams, writes and errors are not tagged. A
route that states its own `Cache-Control` keeps it and is not tagged
(`/vectors/sample` is `no-store`, for a reason it documents).

One consequence is not visible in any response: `GET /artifacts/{id}` records
an open, and does so on a revalidation too. Fetch artifact detail when a
person opens it, never from a background refresh.

**5. An error is `{ "error": "…" }` and the status.** `401` refused, `403`
forbidden, `404` not found, `400` the request was wrong, `502` a backend did
not answer, `503` a backend is busy and asking again will help, `500` this
server broke. There are no error codes; the status is the vocabulary.

## Read routes

| Route | Answers |
|---|---|
| `GET /search?q=&limit=&tags=&category=&explain=` | List of hits. Each may carry `weak` (a loose match) and `past_cliff` (it sits below the point where relevance falls off). Absent means false. A client that draws a result list draws both: the divider goes above the first `past_cliff` row. |
| `GET /search/stream`, `POST /ask/stream` | Server-sent events. `POST /ask` is the same answer in one piece. |
| `GET /resurface?limit=` | List of hits worth seeing again. |
| `GET /corpora?limit=&after=` | Paged list of corpus summaries, newest first. |
| `GET /corpora/{id}` | One corpus with its text and its artifacts. |
| `GET /corpora/{id}/file`, `…/image?original=1` | The bytes as captured; the preview by default for an image. |
| `GET /artifacts/{id}` | One artifact and the document it came from. Records an open. |
| `GET /artifacts/{id}/lineage` | `{ roots, also_replaced, truncated }`: what it was written from, nested by generation; what it replaced without being written from it; and whether the walk stopped early. A node's `source` is `{ corpus_id, label, start_line, end_line }` or `null` for a merge; `missing: true` is a source deleted since. |
| `GET /artifacts/{id}/versions` | List of earlier wordings, oldest first. |
| `GET /days/{date}?tz=` | `{ date, tz, from, to, entries, captured, was_due, refers, sittings }` for one day read in an IANA zone. A date that is not one is a `404`. |
| `GET /moments?kind=due\|event&from=&to=` | List of reminders, or of dates that refer to the window. |
| `POST /context` | Body: the situation bundle. Answers `{ "offer": {…} \| null }`. Records the situation either way. |
| `POST /context/seen` | Body `{ artifact_id, rung, slot }`, sent when the card is actually on screen. Always `204`. |
| `GET /status`, `GET /consolidation` | The state of the base and of the review queue. |

## Judging

Where a person decides rather than reads. Every one of these has an undo, and
the undo is on this list too.

| Route | Answers |
|---|---|
| `GET /pairs` | Open duplicate pairs, clustered: `{ members, pairs: [...] }`. One artifact against two others is one question, not two cards. |
| `POST /pairs/{id}/supersede` | Body `{"keep": "<artifact id>"}`, or none for the side the judge proposed. Keeps that one and hides the other behind it. `204`. |
| `POST /pairs/{id}/synthesize` | Ask for one artifact written from both. Queued, not written here: the writing is a model call. `204`. |
| `POST /pairs/{id}/discard` | Retire both. `204`. |
| `POST /pairs/{id}/dismiss` | Not a question worth answering. Nothing is hidden. `204`. |
| `GET /gaps` | Questions nothing covered, clustered under the name the sweep gave them, with `labelled_by` of `model` or `terms`. |
| `POST /gaps/{kind}/{id}/dismiss` | This one needs no answer. `204`. |
| `POST /gaps/forget` | Body `{"members": [{"kind", "id"}]}` — a whole cluster. `204`. |
| `GET /insights` | `{ held, used, retrieval }`. Read-only. |
| `GET /insights/set-aside` | What the base did on its own and left an undo for, and what it is waiting to be told. |
| `POST /artifacts/{id}/verify` · `/deprecate` · `/reactivate` · `/unsupersede` | The four answers a set-aside row admits. `204`. |
| `POST /merges/{id}/undo` | Take a merge back: its sources return, the merge is retired. `204`. |
| `POST /condensations/{id}/undo` | Put the version a condensation retired back. Answers `{ "artifact_id" }`, which the path does not carry. |
| `POST /corpora/{id}/resolve` | The three-way answer to a parked capture. Already existed. |

Three things these shapes say that are easy to miss:

**A pair says who has looked at it.** `unjudged` means the sweep filed it on a
cosine score and nothing has read it since, so *these two cover the same
ground* is a finding nobody made — print the measurement instead.
`via_link` means no cosine was ever computed (the pair came from repeated
co-retrieval), so `percent` is not a similarity and must not be shown as one.
`mergeable` says whether the merge path would take a synthesis at all; where it
is false, leave the button out rather than offer a press that can only come
back a validation error.

**A set-aside row carries its `kind`, not its buttons.** `kind` is one of
`merged`, `generated`, `hidden`, `buried`, `parked`, `unverified`, and it is
the whole of what says which answers the row admits — each of them is a route
above. `subject_id` is what those routes name: a corpus for `parked`, the
artifact for the rest. A `kind` a client has never heard of should draw no
buttons rather than guess; that is what lets this list grow a seventh.

**`GET /insights` has no tuning in it at all.** Applying a tuning
recommendation writes `config.toml`, and that press stays on the web where the
person who may make it is at a keyboard. `retrieval` is `null` where no
searches are recorded — never `0.00`, which would read as a score rather than
as an absence.

## What does not cross

API token management, the browser-extension offer, instance configuration and
`--grant-judge`, and applying a tuning recommendation stay on the web
interface. A credential that can mint its successors is a credential whose
revocation means less than it says, and the rest is an operator's work at a
keyboard.
