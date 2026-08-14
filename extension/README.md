# engram browser extension

One codebase, two manifests. Everything is shared except the manifest and the
single call that opens the panel — Chrome's `sidePanel` against Firefox's
`sidebarAction`, behind a small shim in `shared/shim.js`.

The browser supplies HTML it has already rendered and authenticated; the server
does the extraction. A server-side GET is an anonymous client — no session, no
subscription, no JavaScript engine — and for a large share of what is worth
capturing it sees a login wall. Worse, it does not fail: it would ingest the
cookie banner and the subscribe prompt and the corpus would read as though the
capture worked.

## Loading it for development

```sh
./extension/pack.sh          # copies shared/ into each browser's package
```

- **Chrome** — `chrome://extensions` → Developer mode → Load unpacked →
  `extension/chrome`.
- **Firefox** — `about:debugging#/runtime/this-firefox` → Load Temporary
  Add-on → `extension/firefox/manifest.json`.

## Verifying

None of this is automated. A browser test harness is out of scope, and
pretending otherwise would be worse than saying so — so these are the checks,
written out, to be run by hand on both browsers against a running deployment.

**Status: not yet run.** Tick a box only after watching it pass in that
browser.

### 1. The panel opens

- [ ] Chrome: load unpacked, click the toolbar icon.
- [ ] Firefox: load the temporary add-on, click the toolbar icon.

Expected in both: a side panel holding a "Capture this page" button, a search
box, an ask box, and an empty results area. Nothing else — no explanatory copy
beyond the field labels.

### 2. Pairing

With engram running and you signed in to it in that browser:

- [ ] Open the panel while unpaired.
      Expected: an "engram address" field and a Pair button above Capture.
- [ ] Enter the address you reach engram at and press Pair.
      Expected, in this order: a permission prompt naming that one host; then
      a browser window on `/ui/pair`; press Pair there; the window closes; the
      panel says "Paired with …" and the address field is gone.
- [ ] The permission prompt names your deployment's host and nothing else.
      Chrome: Details → Site access shows exactly one host. Firefox:
      `about:addons` → Permissions shows one host. Neither says "all sites".
- [ ] In engram: Housekeeping → API tokens holds one token named
      "browser extension".
- [ ] Type a host with no scheme (`engram.example`).
      Expected: it is read as `https://` and pairs, rather than being refused
      or silently reaching for `http://`.

The order above is the whole reason this is a form and not a `prompt()`.
`permissions.request` is only granted while the browser still counts a click
as a user gesture, and a gesture does not survive the round trip through the
auth-flow window — asking for permission afterwards fails with "this function
must be called during a user gesture". So the permission is asked for first,
inside the click, and the token flow runs after it.

The address stored is the one typed, never the one the deployment reports for
itself: a deployment behind a proxy knows an internal host name, or a scheme
its proxy did not forward, and the address the browser actually reaches is the
one that has to be in the header.

### 3. Capture

- [ ] On an ordinary article, press "Capture this page".
      Expected: "Captured." and the document under Recent on `/ui/capture`,
      with origin `extension` and a link back to the URL.
- [ ] On a page behind a login you are signed into: the corpus holds the
      article, not the login wall. This is the whole reason the browser
      supplies the HTML rather than the server fetching it.
- [ ] On a page that renders to almost nothing (a cookie wall, or any page
      with JavaScript disabled): a red line naming the extraction floor, and
      nothing under Recent.
- [ ] Select three paragraphs, right-click → "Capture selection".
      Expected: a corpus holding only those paragraphs, not the article
      around them.
- [ ] Capture the same page twice.
      Expected: the second says it is already captured, or names the
      near-duplicate it matched.
- [ ] Open the panel from the toolbar on one tab, switch to another, press
      "Capture this page".
      Expected: a permission prompt asking to read pages, and then the
      capture. `activeTab` covers only the tab the opening click happened in,
      and a side panel outlives that tab — so the panel asks once for a real
      host permission. Declining is not a crash: the line reads "engram cannot
      read this tab", naming the context-menu route, not the browser's
      "Cannot access contents of the page".
- [ ] Right-click an ordinary page (nothing selected) → "Capture this page".
      Expected: the same capture, with no permission prompt ever. A
      context-menu click is a gesture on that tab, so `activeTab` covers it;
      this is the route that works without granting anything.
- [ ] Press Forget, then Capture again.
      Expected: the read-pages permission was handed back with the token, so
      the prompt appears again.

### 4. Search and ask

- [ ] Type into Search. Results appear as you type; clicking one opens that
      artifact on the deployment in a new tab.
- [ ] With `feedback.enabled = true`, search once from the panel and once
      from `/ui/search`, then open `/ui/judge`.
      Expected: two events, one against the `extension` door and one against
      `ui`. That distinction is the point — a query typed in the panel while
      reading was composed before anything came back.
- [ ] Type a question into Ask and submit.
      Expected: the panel keeps waiting (up to `infer.ask.timeout_secs`,
      default 900) and eventually renders the answer, without closing or
      clearing. This is why it is a panel and not a popup.

### 5. Outside the panel

- [ ] Select text, right-click: "Search engram for this" and "Capture
      selection" are offered. With nothing selected, neither is.
- [ ] Press each. The panel opens (if closed) with the selection searched, or
      the selection captured.
- [ ] Type `eg`, space, a query, Enter in the address bar. The panel opens
      with that query searched.
- [ ] Do the omnibox and context-menu cases with the panel **closed** first.
      The panel has to register its message listener before the background
      script sends work; `shared/background.js` holds the work and the panel
      asks for it on load, so a just-opened panel must not miss the first one.
- [ ] Same again, and count what happened: exactly one capture and one search
      event, never two. The parked work and the direct message are two routes
      to the same panel, and whichever arrives first has to take the work off
      the other — a second delivery would store the capture twice and record
      the query as two searches with two embeddings behind them.

### 6. Failure paths

- [ ] Stop engram, press Capture.
      Expected: "engram is unreachable." Nothing is queued — that was
      considered and cut. A capture is lost and said so, rather than held in
      a queue that may never drain.
- [ ] Revoke the token in Housekeeping, press Capture.
      Expected: "That token no longer works — pair again.", **and the address
      field comes back**. The stored token is cleared on the 401, because
      everything treats a stored token as "paired" and one that can never work
      would otherwise leave the panel unusable short of reinstalling.
- [ ] Pair again from that state. It works, with no clearing of extension
      storage in between.
- [ ] Press "Forget this deployment".
      Expected: the address field returns, and the host disappears from
      Chrome's Site access / Firefox's Permissions. The token is still listed
      in Housekeeping — the panel says so rather than implying it revoked it.

### 7. Icons

- [ ] The toolbar button shows the engram mark, not a puzzle piece or a grey
      letter.
- [ ] `chrome://extensions` and `about:addons` show it beside the name.

## Two things I could not check while writing this

Named here because they are the likeliest to break and the cheapest to spot:

1. **Firefox MV3 event pages and `importScripts`.** `shared/background.js`
   lists its dependencies in the manifest for Firefox and calls
   `importScripts` for Chrome's service worker. If Firefox logs an error
   about `importScripts` at load, delete the call and rely on the manifest's
   `background.scripts` list — then simplify to whichever single path works
   in both rather than leaving two.
2. **The just-opened-panel race** (check 5, last box). The handoff through
   `pendingWork` in the background script exists to close it, but only a real
   browser can say whether it does. Parked work expires after 15 seconds,
   because the ways it can fail to be collected — `sidePanel.open` refusing
   outside a user gesture, `sendMessage` reporting a delivered message as a
   failure — all end with work sitting there for the next panel to pick up and
   run against whatever tab is open by then. If the race turns out not to
   exist, delete the parking spot rather than keeping both paths.

## Releasing

Both packages are built from source by `build.rs` at compile time, so a
deployment always serves the build that matches it. `/extension/install` links
to them.

The Firefox one is unsigned, and Firefox will not install an unsigned add-on
permanently. That leaves three ways to run it, and the install page lists all
three: load it temporarily through `about:debugging` (works everywhere, gone
at the next restart), set `xpinstall.signatures.required` to `false` (Developer
Edition, Nightly and ESR only — release Firefox ignores it), or have it signed.

Signing is not publishing. An **unlisted** submission is signed and handed
back; it is never reviewed, listed or searchable, and Mozilla is the only
vendor that touches it. It does need a free AMO account, and it is a network
round trip that does not belong in `cargo build` — so it happens once per
release and the result is committed.

```sh
# 1. Bump `version` in BOTH manifests. They must match.
# 2. Build the unsigned package:
./extension/pack.sh
cd extension/firefox && zip -r ../../engram-unsigned.zip . && cd ../..
# 3. AMO → Developer Hub → Submit a New Add-on → "On your own site"
#    (unlisted). Mozilla signs it and gives it back.
# 4. Commit the signed result. `build.rs` prefers it over the unsigned
#    package and drops a marker beside it, which is what makes the install
#    page offer one click instead of the three fallbacks.
cp ~/Downloads/engram-*.xpi extension/firefox-signed.xpi
git add extension/firefox-signed.xpi
```

### Icons

`extension/shared/icon-*.png` are rasterized from `assets/icon.svg` and
committed, for the same reason the web icons are: a build must not need a
rasterizer installed. Regenerate them after editing that file:

```sh
for s in 16 32 48 128; do
  magick -background none -density 1200 assets/icon.svg \
    -resize ${s}x${s} -depth 8 -strip PNG32:extension/shared/icon-$s.png
done
```

### No auto-update, and why

The design doc asks for both a static manifest identical on every deployment
and a Firefox `update_url` pointing at the deployment. Those cannot both hold:
`update_url` is an absolute URL inside the manifest, an XPI is signed over its
contents, and a self-hosted single-operator install has no canonical host to
hardcode. Rewriting the manifest per download invalidates the signature that
makes one-click install possible.

So there is no `update_url`. Updating means visiting `/extension/install`
again after a deployment, and the download page says so. If a canonical host
ever exists, `browser_specific_settings.gecko.update_url` goes in
`firefox/manifest.json` and the package is re-signed.
