# The doors capture is reached through — Design

Date: 2026-08-27
Status: draft
Adds `POST /api/v1/capture`, `POST /ui/share`, `src/cli/` (the `-c`, `-s`, `-a`
client); touches `src/web/api.rs`, `src/web/mod.rs`, `src/main.rs`,
`src/core/ingest.rs` (two origin constants), `src/store/feedback.rs` (one
`Door`), `src/web/extension.rs`, `src/web/pair.rs`,
`assets/manifest.webmanifest`, `ROADMAP.md`.
No new store table, no migration, no model call, no change to ranking, to
segmentation or to how a corpus becomes an artifact. Every door added here ends
in `Core::ingest_capture`, `Core::ingest_url` or `ingest_file` — the three the
web capture page already ends in.

## 1. Why

Capture works and is hard to reach. Text, a link, a PDF, an image and a photo
all have a door, and every one of them is a door you have to already be
standing in: the capture page in a browser tab, a `curl` you hand-assemble, an
MCP client mid-session, or the extension on a desktop browser that has it
installed. The thing worth keeping is almost never encountered there. It is
encountered on a phone, in an app that is not a browser, or in a terminal three
directories deep, and the cost of crossing to a door is reliably higher than
the value of the paragraph — so it is not captured, and a knowledge base is
exactly as good as what reached it.

Two doors are missing and one endpoint is missing beneath them.

**The phone.** engram already offers itself as an installed app to a phone
browser once a week, and an installed app on Android can put itself in the
system share sheet. It does not. Sharing from a reader, a chat, a mail client
or the camera roll is the phone's native capture gesture, and engram is absent
from it. On iOS the same gesture has to be reached differently — Safari has no
share-target support — but it is reachable.

**The terminal.** A terminal is where the operator already is when the thing
worth keeping is a command that finally worked, a log excerpt, or a file that
was just written. Reaching the base from there today means a browser tab, which
means leaving. A client that captures, searches and asks from the shell removes
the crossing in the direction that matters most often, and — because it goes
over HTTP like every other door — costs nothing in duplicated ranking logic.

**The endpoint beneath them.** `POST /api/v1/corpora` takes JSON carrying
exactly one of `text`, `html` or `url` (`src/web/api.rs:188`); a PDF goes to
`/corpora/upload` and an image to `/corpora/image` (`src/web/api.rs:1119`,
`:1124`), each multipart, each with its own body ceiling. Three doors, and the
caller must know which one it is standing at. Every door in this document
arrives holding something it has not classified — a share sheet hands over a
blob and a maybe-URL, a shell hands over a path or a pipe — so each of them
would have to sniff the type and pick an endpoint. Written once on the server,
that dispatch is thirty lines. Written once per client, it is the reason the
clients are never written.

## 2. What is built

1. **`POST /api/v1/capture`** — one endpoint that dispatches on content type
   and ends in the ingest calls that already exist. §3.
2. **`engram -c`, `-s`, `-a`** — a client on the existing binary, with stdin
   support and a deliberately wide `-s N`. §4, and its tty rendering in §4a.
3. **A Web Share Target** in the manifest, and `POST /ui/share` behind it. §5.
4. **An iOS Shortcut and a bookmarklet**, generated pre-filled with a
   per-device token, on the page that already installs the extension. §6.
5. **Two origin values**, `cli` and `share`, so the queue and the corpus page
   can say how something arrived. §7.

Nothing in this document changes what happens to a capture after it is stored.

## 3. `POST /api/v1/capture`

Bearer-authed like the rest of `/api/v1`. Dispatch is on `Content-Type`, and
`?title=` and `?note=` apply to every branch:

| Content type | Read as |
|---|---|
| `text/plain`, body is one `http(s)` URL and nothing else | a link, fetched server-side — `Core::ingest_url` |
| `text/plain`, anything else | verbatim text — `Core::ingest_capture` |
| `application/pdf` | the PDF path, raw body rather than multipart |
| `image/*` | the image path, raw body rather than multipart |
| `multipart/form-data` | fields `text`, `url`, `title`, `note`, and one or more `file` parts |

The single-URL rule is the one guess in the endpoint, and it is made because
every share sheet on both platforms sends a bare URL as `text/plain`. It is
narrow on purpose: the body must parse as a URL with an `http` or `https`
scheme after trimming, and hold nothing else. A line of prose that begins with
a link is prose. A caller that wants the other reading has `POST /corpora`,
which is unchanged and remains the explicit door.

Each `file` part becomes its own corpus, typed by its own part header, so a
share of four photos is four captures rather than a concatenation. The response
is the existing `IngestOutcome` for a single-body request and an array of them
for a multipart request carrying more than one file — including the
`near_duplicate` field, so a parked capture is reported as parked to every door
rather than read as a success.

Body ceilings are the per-kind ones already configured — `pdf_max_bytes`,
`image_max_bytes` (`src/config.rs:60`, `:68`) — applied by branch, since one
route now carries what three carried. The `text/plain` branch re-imposes
`MAX_BODY_BYTES` on itself, the way `upload` already does for its text branch.

## 4. The client

Flags on the existing binary (`src/main.rs:8`), so there is one artifact to
build, ship and version. The verb flags are mutually exclusive:

- **`-c <PATH|URL|->`** — capture. Repeatable, so `engram -c *.pdf` is one
  invocation and several corpora. A path is uploaded with its sniffed type, a
  URL is sent as a URL, `-` reads stdin. `--title`, `--note`, `--tag`.
- **`-s [N] <QUERY>`** — search, one line per hit: rank, score, title, id, with
  the excerpt indented beneath. `--tag`, `--category`, `--json`. When the first
  value parses as a bare integer it is the number of hits wanted, and the rest
  is the query: `engram -s 40 "qdrant payload filter"`. A query that is itself
  a number is reachable as `engram -s -- 42`.

  The number is not cosmetic. `limit` sets the candidate pool at `limit *
  CANDIDATE_MULTIPLIER` (`src/core/search.rs:12`), so asking for forty searches
  wider than asking for ten rather than printing more of the same pool — and a
  wider pool is precisely what gives `per_source_cap` something left to
  redistribute when one document saturates a narrow one. The terminal is where
  a deliberately wide read belongs: it costs one embedding and one Qdrant call
  either way, and a shell can hold forty lines that a rail cannot. It is
  clamped to `MAX_LIMIT = 50` (`src/core/search.rs:9`) like every other door.
  Past fifty is one constant and no new code, but it is a change to what every
  door may ask for and to the largest pool the server will assemble, so it is
  not made here.
- **`-a <QUESTION>`** — ask, streaming `/ask/stream` to stdout as it arrives,
  citations printed after the answer.

**Stdin** is the value of whichever verb flag is present, so `… | engram -s -`
searches for what was piped. With no verb flag and a pipe on stdin, the input
is captured: `pbpaste | engram` and `git log -1 | engram` are the gesture this
whole document exists for, and requiring `-c -` there would be ceremony in
front of the one case that has to be frictionless. With no verb flag and a tty
on stdin, the binary is the server it has always been — so `engram` alone still
starts engram, and no existing invocation changes meaning.

**What the terminal must not lose.** `-s` prints the rail's honesty in a form
that survives redirection: hits past the cliff are dimmed on a tty and prefixed
with `·` when stdout is not one, and a loose match says so in words. A ranked
list that stops claiming to be an answer on the page must stop claiming it in a
pipe.

**Where it points.** `ENGRAM_URL` and `ENGRAM_TOKEN`, else
`~/.config/engram/cli.toml`. A missing or rejected token errors by naming the
page that mints one rather than by printing a 401.

**Over HTTP, never into the store.** The client opens no SQLite file and no
Qdrant connection. One set of ranking parameters, tenancy checks and feedback
recording stands behind every door, and a `-s` from the shell is a real
recorded search that `/ui/judge` can grade later — the terminal becomes a
fourth door rather than a way around the three.

Those searches are recorded under a new `Door::Cli` (`src/store/feedback.rs:20`)
rather than under `Api`, read from the `engram-cli/<version>` user agent the
client sends. The reason is the one that earned `Extension` its own value: a
query typed at a shell is composed before anything came back, about something
the operator is looking at rather than something engram showed them, and that
is the least contaminated question the base ever receives. A door label is
self-declared and therefore spoofable; on a single-operator install the worst
case is a mislabelled row in your own judge queue.

**Exit codes.** `0` results, `1` none, `2` error, so `engram -s "x" || …` is a
usable branch in a script.

## 4a. The face of it

The client is the door reached most often, from the place with the least
patience, and it is also the only door with no design at all: a REST response
rendered by `println!`. That is a waste of the one surface where engram's own
vocabulary — activation, propagation, decay, a trace that falls off a cliff —
can be drawn rather than described. So the tty rendering is deliberately
alive, and bounded by three rules that make it safe to be.

**It never survives a pipe.** With `stdout` not a terminal, or `NO_COLOR` set,
or `--plain` given, the output is the dead-plain text §4 specifies: no escape
codes, no animation, no glyphs outside ASCII. `--fancy never|auto|always`
overrides the detection in both directions. Every assertion any script makes
about this client's output is made against that form, and the fancy form is
never the one a machine sees.

**It never delays a result.** Animation runs on its own thread while a request
is in flight and stops on the first byte that arrives. Nothing is buffered to
make a frame land evenly, nothing is paced for effect, and no result is held
back so an animation can finish. The client is measured by the time from
keystroke to first hit, and that number must be indistinguishable from
`--plain`.

**It never decorates over meaning.** The cliff, the loose-match label, the
badges for synthesized and captured artifacts, and the words `held for review`
are text in both forms. Colour and motion are additive to them and never a
substitute — a rendering that says "past the cliff" only by being dim is a
rendering that says nothing to a colourblind reader, in a monochrome terminal,
or in a screenshot.

Within those rules, the vocabulary is the base's own:

- **Waiting is a propagating pulse.** A short strand carries one bright cell
  left to right with a decaying tail behind it — an impulse travelling, not a
  spinner rotating. It is what the server is doing: one query embedded, one
  pool assembled, activation read over it.
- **A score is a dendrite.** Each hit's score is a bar in gradient blocks
  drawn beside a vertical trace running down the list, and **the cliff is a
  literal break in that trace**: above it the trace is solid, at it the glyph
  snaps, below it the rows are dim and the trace is dotted. The rail's central
  idea becomes the one thing you cannot miss in the output.
- **Streaming is a readout.** Under `-a`, a single-line trace scrolls above the
  answer with its amplitude driven by the actual token arrival rate — an
  activity readout of a thing genuinely happening, not a fake one. Citations
  draw afterwards as a rooted tree.
- **Capture is sequencing.** `-c` fills a track as the body is read, and with
  `--watch` shows extraction, segmentation and embedding as three lamps lit
  from `GET /corpora/{id}`, so the operator sees the background stages the
  other doors only describe in a sentence.

**Two glyph sets, chosen by environment.** The Unicode set is used when the
locale says UTF-8; otherwise an ASCII set with the same shapes. Frames are
drawn by rewriting the current line — carriage return and erase-to-end — never
on the alternate screen, so results stay in scrollback after the process
exits and `engram -s 40 …` is still a thing you can scroll back through
tomorrow.

**Single binary, and it stays one.** The rendering needs terminal detection,
cursor movement and colour capability, which is `crossterm` — pure Rust, no C
dependency, no curses, statically linked like everything else in the tree. No
part of this section adds a runtime dependency or a file that has to ship
beside the executable.

**It is built last.** §3 and §4 are the plumbing and stand alone: a plain,
correct client is a shippable client, and every test in §9 is written against
`--plain`. This section is a layer over a finished thing, and if it is cut for
time nothing else in this document changes.

## 5. Android: the share target

The manifest (`assets/manifest.webmanifest`) gains:

```json
"share_target": {
  "action": "/ui/share",
  "method": "POST",
  "enctype": "multipart/form-data",
  "params": {
    "title": "title", "text": "text", "url": "url",
    "files": [{ "name": "file", "accept": ["image/*", "application/pdf", "text/*"] }]
  }
}
```

`POST /ui/share` is session-authed, takes the multipart parts, and hands them
to the same dispatch as §3. It then redirects to the new corpus page — chosen
over a self-closing confirmation because the corpus page is the one surface
that can say *held for review* when near-dupe parking fires
(`src/core/ingest.rs:286`). On a phone that is the only moment the operator
would ever learn that what they shared is stored but not searchable.

**One thing to verify on a device before the rest is built.** The session
cookie is `SameSite=Lax` (`src/auth/mod.rs:30`), which is the deployment's only
CSRF mitigation, and a share-target POST is dispatched by the platform rather
than by a page on the origin. If the cookie does not arrive, the redirect is to
the login page and the share is lost. The fallback, if it does: `/ui/share`
accepts the POST, stashes the parts under a one-time id, and redirects to a
`GET /ui/share/{id}` that authenticates normally and completes the capture —
one extra hop, no credential in the manifest. Which of the two is built is
decided by the device test, not by argument.

`/ui/share` creates and never destroys, which is what makes accepting a POST
the platform dispatched tolerable at all. No other route gains this property.

## 6. iOS: the Shortcut and the bookmarklet

Safari has no share-target support, so the share sheet is reached through the
Shortcuts app: a shortcut that accepts text, URLs and files from the sheet and
POSTs them to `/api/v1/capture` with a bearer token. The bookmarklet covers
pages read in Safari itself, POSTing the current URL and any selection to the
same endpoint.

Both carry a credential — a token living in a shortcut and in a bookmark — and
that is the whole of the difficulty. So neither is ever hand-assembled: the
install page generates them **already filled in with a freshly minted token,
named for the device**, the way extension pairing already mints one per install
(`src/web/pair.rs`, `auth::tokens::mint` at `src/auth/tokens.rs:33`). One token
per device, each revocable on its own from the tokens UI, and nothing typed or
pasted by a person.

`/extension/install` (`src/web/extension.rs:85`) is already the page for
getting engram onto a thing. It becomes the page for all four doors — the
extension, the Shortcut, the bookmarklet and the install prompt — rather than a
fourth place to have to find.

## 7. Provenance

Two constants beside the five in `src/core/ingest.rs:5`: `ORIGIN_CLI` (`cli`)
and `ORIGIN_SHARE` (`share`). The Shortcut and the bookmarklet report `share`
as well: they are the same gesture reached differently, and a distinction the
operator cannot act on is not worth a value. A link that arrives through any of
these doors and is fetched by the server stays `fetch`, as it does today —
origin records what read the bytes, not what asked.

This is queue-and-corpus-page provenance only. `origin` lives on the corpus row
(`src/store/corpora.rs:112`) and does not reach `ChunkPayload`
(`src/vector/mod.rs:51`), so it neither filters nor weights a search, and this
document does not change that.

## 8. What is deliberately not built

A reality check over the ranking path ran before this design and is recorded
here, because three of its findings were considered and set aside on purpose
rather than missed.

**Bulk doors — a watched folder, directory import, feeds, email-in — are not
built, and near-dupe parking is why.** Anything scoring at or above
`near_dupe_min = 0.90` against something already stored is parked: written
down, but not segmented, not embedded and not searchable until a person decides
between the two in the web UI (`src/config.rs:628`, `src/core/ingest.rs:286`).
That is a per-item human decision standing on the ingest path. It is the right
behaviour for every door in this document, all of which have the operator
present at the moment of capture; it is a queue nobody will ever drain for a
door that runs while they sleep. **A bulk door requires a bulk-safe near-dupe
policy first.** That is a separate design.

**Recency decays from capture time and stays that way.** The decay reads
`last_verified_at`, falling back to `created_at` (`src/vector/qdrant.rs:322`) —
when *engram* saw the text, not when it was written. Every door here captures
what the operator is looking at now, so the two dates agree and the flaw does
not bite. It bites an importer of old material, and belongs to the same later
design as the paragraph above.

**Nothing caps how many mediocre documents crowd out one good one.**
`per_source_cap` is per document (`src/core/ranking.rs:17`), and there is no
per-list diversity beyond it. This is left alone deliberately: the cliff already
marks where a list stopped being answers, which is the honest response to a
thin result set, and a second quality prior would be a ranking change measured
by nothing.

**Priming already holds under volume**, which is why none of this is urgent: it
is a bounded lift on what the operator has used, so newly arrived material is
neutral rather than corrosive to what is established.

## 9. Testing

- **Dispatch, per branch.** A bare URL as `text/plain` becomes a fetched link;
  a line of prose beginning with a URL becomes text; a PDF body reaches the PDF
  path; a multipart body with three files yields three corpora and three
  outcomes; a body over the branch's ceiling is refused as that kind.
- **A parked capture is reported as parked** through `/api/v1/capture` and
  through `/ui/share`, not as a plain success.
- **The client, against a test server.** `-c` with a path, a URL and a pipe;
  `-s` printing the cliff marker with stdout redirected; `-a` streaming;
  exit `1` on an empty result; the verb flags refusing each other; a tty stdin
  with no flag still starting the server.
- **`-s 40` asks for forty and `-s 500` asks for fifty**, and `-s -- 42`
  searches for `42`.
- **The plain form is the tested form.** With `stdout` redirected, with
  `NO_COLOR`, and with `--plain`, the output holds no escape byte at all — and
  the cliff, the loose label and `held for review` appear as words in that
  output, not only as colour.
- **`-s` records a search** reachable from the judge queue, under `Door::Cli`,
  and an otherwise identical request without the client's user agent still
  records as `Api`.
- **The share route refuses an unauthenticated POST**, and the device test in
  §5 settles which of the two shapes ships.
- **The install page's generated Shortcut and bookmarklet carry a token that
  works**, is named for the device, and is revocable alone.
