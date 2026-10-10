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
read on the day — and only its entries': a captured row on a day is a link,
and the document behind it may be a book.

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
| `GET /search?q=&limit=&tags=&category=&explain=&door=` | List of hits. Each may carry `weak` (a loose match) and `past_cliff` (it sits below the point where relevance falls off). Absent means false. A client that draws a result list draws both: the divider goes above the first `past_cliff` row. `door=app` says a person is typing in the phone app: the search is recorded under them like the web's, waits for its id, and answers `event` beside `items` — what an open, a verdict and a gap name. With `explain`, `reranked` and a per-row `why_ranked` sentence come too. A row whose document goes on carries `continues_to`, the next passage's id. A row the judge found disagreeing with another note carries `disagrees_with`, a list of `Disagreement` (below); absent means none. |
| `GET /search/stream`, `POST /ask/stream` | Server-sent events. `POST /ask` is the same answer in one piece. An answer written across a disagreement carries `disagreements`, a list of `Disagreement`, one per pair with both sides among the citations; absent means none. |
| `GET /resurface?limit=` | List of hits worth seeing again. |
| `GET /corpora?limit=&after=` | Paged list of corpus summaries, newest first. |
| `GET /corpora/{id}` | One corpus with its text and its artifacts. |
| `GET /corpora/{id}/file`, `…/image?original=1` | The bytes as captured; the preview by default for an image. |
| `GET /artifacts/{id}?event=` | One artifact and the document it came from. Records an open — attributed to the search named by `event` where it is the caller's own, and then `search_event` in the answer says so; the verdict bar is drawn only then. |
| `GET /artifacts/{id}/about` | `{ tag, probes, condensed, due_in }`: what the base found when it arrived, what has asked for it, the open condensation's action id where the live text is condensed, and whether a reminder on it is due. |
| `GET /artifacts/{id}/lineage` | `{ roots, also_replaced, truncated }`: what it was written from, nested by generation; what it replaced without being written from it; and whether the walk stopped early. A node's `source` is `{ corpus_id, label, start_line, end_line }` or `null` for a merge; `missing: true` is a source deleted since. |
| `GET /artifacts/{id}/versions` | List of earlier wordings, oldest first. |
| `GET /artifacts/{id}/related` | `{ related, seen_together, continues_at }`: the nearest artifacts by stored vector, what this one has been reached for alongside (empty while `[learn]` is off), and the next passage of the same document where this one stops mid-sentence. Each row is `{ id, label, named, snippet }`; a `seen_together` row also carries `why` and `corpus_title`. Does not record an open. |
| `GET /artifacts/{id}/source` | `{ corpus_id, label, lines }`: the lines the artifact was drawn from with three of context either side, each `{ number, text, in_span }`. `corpus_id` is `null` and `lines` empty for a merge, or where the document is gone. |
| `GET /days/{date}?tz=` | `{ date, tz, from, to, entries, captured, was_due, refers, sittings }` for one day read in an IANA zone. A date that is not one is a `404`. |
| `GET /moments?kind=due\|event&from=&to=` | List of reminders, or of dates that refer to the window. |
| `POST /context` | Body: the situation bundle. Answers `{ "offer": {…} \| null }`. Records the situation either way. |
| `POST /context/seen` | Body `{ artifact_id, rung, slot }`, sent when the card is actually on screen. Always `204`. |
| `GET /status`, `GET /consolidation` | The state of the base, and the pairs the judge has not settled yet. `status` also says which doors are open — `transcribe`, `asks`, `vision`, `learn`, `recommend` — and carries the idle line's facts: `held`, `last_kept`, the box hint's `examples` (in the `Accept-Language` asked for), and `teach`. |
| `GET /corpora/{id}/bands` | The corpus page as data: `image`, `pdf`, `unread`, `restored`, `note`, `coverage`, `meta`, `exif`, `promoted`, `unplaced`, `written_from`, and `bands` — each `{ from, to, gap, reread, lines, artifact_ids, echoes }`. |
| `GET /facets` | `{ categories: [{ value, count }] }`: what the box's chips narrow by. |
| `GET /echo?q=` | `{ kind, detail }`: what capture will do with that text, said before it is pressed — the line under the web's box. Empty `kind` for an empty box. No model call. |
| `GET /feedback` | What is being recorded: `{ searches: { captured, pending, judged }, asks: { asked, judged } }`, both null while `[learn]` is off. `DELETE` forgets it all and answers `{ dropped }`. |
| `GET /settings/lang`, `PUT` | `{ chosen, langs }`; `PUT { lang }` with a tag from `langs`, or empty for automatic. |
| `GET /settings/notify`, `PUT`, `POST …/test` | The channels: `{ gotify_url, gotify_token_set, up_endpoint, up_device, up_legacy }`. `PUT { gotify_url, gotify_token, up_endpoint }`; an empty field switches that channel off. `POST /settings/notify/test { channel }` answers `{ sent, error }`. |
| `GET /insights/machine`, `GET /insights/report` | What the machine is doing, and what the base did on its own — last night, the ranking, the pursuits line — in the sentences Insights says. Disclosure, not control. The report no longer carries `more_pairs`: no pair waits on anyone. |
| `POST /transcribe` | Multipart, one part named `audio`: the recording. Answers the words in it as `text/plain`. Nothing is stored — dictation is typing, not capture. `404` where no speech model is configured. |
| `POST /ask?door=app`, `POST /ask/stream?door=app` | As above, and the question is recorded under the person: the answer's `event_id` is what the three routes below name. |
| `POST /capture?from_ask=` | As the capture door, and what is stored records the question and the artifacts its answer was written from — the web's *edit first*. |
| `POST /days/{date}/entry` | Body `{ text, tz }`: an entry into that day. Answers `{ id }`. |

## Judging and undoing

The base decides everything about its own contents: duplicate pairs, captures
that resemble each other, notes nobody has confirmed, questions nothing
answered. What a person does is answer the verdict bar under a search or an
Ask, and take back anything the base did. Every action the base takes has an
undo, and the undo is on this list.

The routes that used to ask for a decision are gone and answer `404`:
`GET /pairs`, `POST /pairs/{id}/supersede` · `/synthesize` · `/discard` ·
`/dismiss`, `GET /gaps`, `POST /gaps/{kind}/{id}/dismiss`, `POST /gaps/forget`,
`POST /artifacts/{id}/verify`, `POST /artifacts/{id}/reviewed` and
`POST /corpora/{id}/resolve`.

| Route | Answers |
|---|---|
| `GET /insights` | `{ held, used, retrieval }`. Read-only. |
| `GET /insights/set-aside` | The journal: what the base merged, wrote, hid and buried on its own, each with its undo. |
| `POST /artifacts/{id}/deprecate` · `/reactivate` · `/unsupersede` | Hide by hand, and the undos a journal row admits. `204`. |
| `DELETE /artifacts/{id}` | Gone from both stores; anything written from it loses it as a source. The one decision here with no undo — `deprecate` is the one that hides and can be taken back. |
| `POST /merges/{id}/undo` | Take a merge back: its sources return, the merge is retired. `204`. |
| `POST /condensations/{id}/undo` | Put the version a condensation retired back. Answers `{ "artifact_id" }`, which the path does not carry. |
| `POST /search/{id}/verdict` | Body `{ verdict, artifact_id }` — `hit`, `no`, `skip`, or `none` to take it back. Answers `{ state, already }`; `already` is another door having judged the search first, which is a sentence and not an error. |
| `POST /search/{id}/gap` | Body `{ q }`: *nothing here has it*. Answers `{ recorded }`. |
| `POST /asks/{id}/verdict` | Body `{ verdict }` — `right`, `wrong`, `nothing_here`, or `none`. Answers `{ verdict }` as the bar words it. |
| `POST /asks/{id}/carried` | Body `{ n }`: this excerpt carried the answer. A toggle; answers `{ carried, verdict }`. |
| `POST /asks/{id}/keep` | Store the answer as a source. Answers `{ id, duplicate, parked, near_dupe_percent }`. |
| `POST /moments/{id}/date` | Body `{ at, tz }`: move a reminder, or date an undated one. `204`. |
| `POST /moments/{id}/not-a-reminder` · `POST /artifacts/{id}/is-a-reminder` | Retract the stage's reading, and put it back. The first answers `{ undo }`: the artifact the second would restore it on, or null. |
| `POST /artifacts/{id}/links/{other}/dismiss` | *Not related.* Final for that pair. `204`. |
| `POST /artifacts/{id}/dwell` | Body `{ secs }`. `204`. |
| `POST /corpora/{id}/reread` · `/entry` · `/segments/{idx}/unpromote` | Read a lost passage again (`{ from, to }`; `202` queued, `204` nothing to re-read); file a capture as the day's entry or not (`{ on }`); put a promoted window's verbatim text back. |

Three things these shapes say that are easy to miss:

**A disagreement names both sides and picks neither.** `Disagreement` is
`{ artifact_id, created_at, other_id, other_title, other_created_at, detail }`:
`artifact_id` is the hit or citation the row hangs on and `created_at` when it
was written, `other_id` the note it disagrees with, `other_title` that note's
title or `null`, `other_created_at` when it was written (unix seconds both, so
a client can say which reading is newer),
and `detail` the judge's sentence on what differs, or `null`. The base keeps
both notes in results and never settles the pair; draw both, with their dates.

**A journal row carries its `kind`, not its buttons.** `kind` is one of
`merged`, `generated`, `hidden`, `buried`, and it is the whole of what says
which undo the row admits — each of them is a route above. `subject_id` is
what those routes name, and it is always the artifact; `artifact_id` is the
same id, to open. A `kind` a client has never heard of should draw no buttons
rather than guess; that is what lets this list grow another.

One subject can appear under two kinds — an artifact the base wrote and later
hid is a `generated` row and a `hidden` one. A row's identity is `kind` and
`subject_id` together, never `subject_id` alone. Under one `kind` a subject
appears once.

**`GET /insights` is disclosure, not control.** The base tunes itself; the
page says what it did. `retrieval` is `null` where no searches are recorded —
never `0.00`, which would read as a score rather than as an absence.

## What does not cross

API token management, the browser-extension offer and instance configuration
stay on the web interface. A credential that can mint its successors is a
credential whose revocation means less than it says, and the rest is an
operator's work at a keyboard.
