# Capture surfaces: upload, link, extension

Today there is one way in. A paste box, an API endpoint and an MCP tool all
reach `Core::ingest(text, origin, title_hint)`, and all three require that
someone already has the text on their clipboard. That is a narrow door for a
system whose premise is that you capture a long reference document once and find
one paragraph of it a year later: the moment you are most likely to meet
something worth keeping is while reading it in a browser, and that is precisely
the moment the paste box is least convenient to reach.

This adds three suppliers to the existing door, and one place they converge.

## Shape

```
upload (.txt)  ──── text ─────────┐
extension ───────── html + url ───┼──► extract() ──► ingest(text, origin,
paste-a-link ────── url ──► GET ──┘                         title_hint, source_url)
paste box ───────── text ─────────┘
```

`extract()` is the only new stage, and it exists once. The extension supplies
HTML from a rendered page; the link path supplies HTML from a server-side GET.
Both hand the same function the same kind of input, so a page captured either
way produces the same corpus.

Nothing downstream learns that HTML exists. `split → synthesize → embed →
consolidate` sees text and a provenance label, as it does today.

### Why the browser supplies the HTML

The server could fetch every URL itself and the extension could be a single
button. It would be smaller, and it would be wrong. A server-side GET is an
anonymous client: no session, no subscription, no JavaScript engine. It sees
what a logged-out stranger sees, which for a large share of what is worth
capturing is a login wall, a teaser paragraph, or `<div id="root"></div>`.

Worse, it does not fail. It ingests the cookie banner and the subscribe prompt,
synthesises artifacts out of them, and the corpus reads as though the capture
worked. Fidelity outranks convenience, and a corpus that silently contains
something other than what the operator saw is the failure this project's second
constraint exists to prevent.

So the browser, which has already rendered and authenticated the page, hands
over what it has. The link path remains — it answers the case where there is no
tab open — but it is a second supplier into one extractor rather than the only
way in, and its limits are its own rather than the feature's.

### Why extraction ends in markdown

`src/infer/split.rs` splits a corpus on headings first and a token budget
second. Extraction that flattens `<h2>` into an undistinguished line costs the
segmenter its primary boundary, and every artifact downstream is drawn from a
worse slice. So extraction produces markdown, not plain text: the structure the
page already had is structure the splitter can use.

## Server

### `src/core/extract.rs`

```rust
pub fn html_to_markdown(html: &str, base_url: Option<&Url>) -> Result<String>
```

`dom_smoothie` removes navigation, headers, footers and asides; `html2md`
converts what survives. `base_url` resolves relative links so a captured
document's references still point somewhere.

Extraction yielding less than `capture.min_extracted_chars` is an error, not a
capture. This is the guard against the silent failure described above: a page
that reduces to almost nothing is reported to the caller, and no corpus is
written.

### `src/core/fetch.rs`

```rust
pub async fn fetch_html(url: &Url) -> Result<String>
```

`http` and `https` only. Its own timeout and its own byte ceiling, because the
8 MB request-body limit governs what clients send us and says nothing about what
we go and fetch. Non-HTML content types are refused by name rather than fed to
the extractor.

### Endpoints

`POST /api/v1/corpora` accepts exactly one of `text`, `html` or `url`:

| Body | Behaviour | `origin` |
|---|---|---|
| `text` | as today | `web` |
| `html` + optional `url` | extract; `url` is provenance, not an instruction | `extension` |
| `url` alone | fetch, then extract | `fetch` |

Supplying more than one is a validation error. `origin` stops being hardcoded
`"web"` at `src/web/api.rs:132` and is derived from which field arrived.

`POST /api/v1/corpora/upload` — multipart, `text/plain` only, refused unless the
bytes are valid UTF-8, filename becomes `title_hint`, `origin` is `upload`. The
existing capture page gains a drop target; the 8 MB body limit already covers
any plausible `.txt`.

`GET /extension/firefox.xpi` and `GET /extension/chrome.zip` — authenticated. See
Distribution.

`GET /ui/pair` — authenticated. See Pairing.

### Storage

`corpora` gains a nullable `source_url`. `origin` is a channel label and a URL
is a location; overloading one with the other would lose the channel and leave
the URL unqueryable. The corpus view renders it as a link when present.

`schema.sql` states that a column change means recreating the database, which is
accepted while the project is in testing. This is a one-line addition there.

### Feedback

`Door` gains `Extension`, recorded like `Ui` and `Api`.

This matters more than a label. The judging loop exists because a query composed
while looking at an artifact borrows its vocabulary and every retrieval system
passes it; the only uncontaminated query is one asked in earnest before the
results were seen. A selection-search from the extension is the strongest
example of that there is — the operator highlights the paragraph they are
actually staring at, having seen nothing engram returned. Distinguishing those
from UI searches lets the judging page and the eval export tell the two apart.

### Config

```toml
[capture]
fetch_timeout_secs   = 30      # a server-side GET, not a local model
fetch_max_bytes      = 8388608
min_extracted_chars  = 200     # below this, extraction failed
```

## Extension

One codebase, two manifests. Shared: the panel's HTML and CSS, the API client,
the extraction-side message handling, the omnibox and context-menu wiring.
Divergent: the manifest, and the single call that opens the panel — Chrome's
`sidePanel` against Firefox's `sidebar_action`, behind a small shim.

A side panel rather than a popup, because a popup closes the instant it loses
focus and `infer.ask.timeout_secs` defaults to 900. An answer rendered into a
popup can evaporate before it arrives. The panel also has room to show results
without prose explaining them, which is the register the core UI already uses.

### Surface

The panel holds four things: a capture button, a search box, an ask box, and
results. Nothing else, and no explanatory copy beyond field labels.

Outside the panel: a context menu on a selection offering search and capture,
and the omnibox keyword `eg` routing to search. Selection capture sends the
selection's HTML rather than the page's.

### Permissions

`activeTab`, `contextMenus`, `storage`, and the panel permission for each
browser. The deployment origin is an *optional* host permission, requested at
pairing time for that one origin.

Everything is user-initiated, so `activeTab` suffices and the extension can read
nothing on a page until you act on it. No `<all_urls>` in the manifest, and
therefore no "read your data on all websites" warning at install.

### Pairing

Bearer tokens and the UI that mints them already exist (`src/auth/tokens.rs:31`,
`src/web/ui.rs:1059`). The panel opens `/ui/pair` through
`browser.identity.launchWebAuthFlow`, which both browsers support; the
already-authenticated session mints a token and returns it to the extension's
redirect. A manual paste field is the fallback when that flow is unavailable.

No credential is ever written into a downloadable file.

## Distribution

The extension ships inside the binary. `assets/` is already embedded with
`rust-embed`; the packaged extension is embedded the same way, so a deployment
always serves the build that matches it, and there is no separate artifact to
publish or forget. The Chrome zip is built from source at compile time; the
Firefox XPI is signed once per release and committed, because signing needs a
network round trip that does not belong in `cargo build`.

The manifest is static and identical for every deployment. Generating it per
host was the obvious idea and does not survive contact with Firefox: an XPI is
signed over its contents, so a manifest rewritten at download time invalidates
the signature that makes one-click install possible.

The deployment origin is learned at runtime instead. The download page carries
its own origin into the pairing link; the extension stores it, and requests host
permission for that one origin through `permissions.request()`, declared as
`optional_host_permissions`. The operator sees a prompt naming their own
deployment and nothing else, the install itself carries no host warning, and one
signed artifact serves every deployment.

The extension ID is pinned — a `key` in the Chrome manifest, `gecko.id` in the
Firefox one — so the pairing redirect is stable across installs.

Install differs by browser and the download page says so plainly:

- **Firefox** — one click. A self-hosted XPI installs from a link and
  auto-updates from the deployment via `update_url`. It must be AMO-signed as
  *unlisted*: Mozilla signs it, but it is never listed, reviewed or searchable.
  That signing step is the one place a vendor touches this, and it happens once
  per release.
- **Chrome** — download the zip, open `chrome://extensions`, enable Developer
  mode, Load unpacked. Off-store `.crx` installs are blocked for ordinary users
  and no amount of work here changes that; the alternative is an enterprise
  policy allowlist, which is documented but not the default path. No
  auto-update.

## Failure

- Extraction below the character floor: reported to the caller, nothing stored.
- Fetch failure: the status is named, not swallowed.
- Non-UTF-8 upload or wrong MIME type: refused with the reason.
- Near-duplicate: `ingest` already returns its verdict, and the panel shows it
  as the web UI does. No new machinery.
- engram unreachable from the extension: the panel says so. Captures are not
  queued — that was considered and cut.

## Testing

Server:

- `extraction_keeps_headings_the_splitter_needs` — fixture HTML through
  `html_to_markdown`, asserting `##` survives.
- `a_page_that_reduces_to_boilerplate_is_refused_not_captured` — below the
  floor, error rather than corpus.
- `capture_accepts_exactly_one_of_text_html_or_url` — each alone succeeds, any
  two together are a validation error.
- `an_html_capture_records_its_url_as_provenance_not_as_origin` — `source_url`
  set, `origin` is `extension`.
- `fetch_refuses_a_non_http_scheme` and `fetch_stops_at_the_byte_ceiling`,
  against a stub server.
- `an_upload_that_is_not_utf8_is_refused` and
  `an_uploaded_filename_becomes_the_title_hint`.
- `the_pairing_page_carries_its_own_origin` — so the extension has an origin to
  request permission for without the operator typing one.
- An extension search records `Door::Extension`.

The extension itself is verified by hand, on both browsers, against a running
deployment. A browser test harness is not in scope and pretending otherwise
would be worse than saying so.

## Out of scope

PDF and non-`.txt` uploads; capture-time duplicate badges; an offline capture
queue; the ambient related-artifacts sidebar and the `<all_urls>` permission it
would require. Each was considered and cut, and none of them is blocked by
anything here.

## Risks

`dom_smoothie` and `html2md` are new dependencies, and "lean beats clever" holds
that a dependency must earn itself. These do: hand-rolled readability heuristics
are worse code than an imported implementation of them, and both crates are
narrow. `html2md` is pre-1.0. If that is unacceptable, plain-text extraction
still works — it costs the splitter its heading boundaries, which is a real but
bounded loss.

AMO signing is an external dependency in the release path for Firefox. It is not
in the build path: an unsigned zip still installs unpacked in both browsers for
development.
