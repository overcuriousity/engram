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

- [ ] Open the panel, press Capture.
      Expected: "Not paired yet." and a prompt for the engram address.
- [ ] Enter the address.
      Expected: a browser window on `/ui/pair` naming that origin; press Pair;
      the window closes; the panel says "Paired."
- [ ] A permission prompt names your deployment's host and nothing else.
- [ ] Chrome: Details → Site access shows exactly one host. Firefox:
      `about:addons` → Permissions shows one host. Neither says "all sites".
- [ ] In engram: Housekeeping → API tokens holds one token named
      "browser extension".

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

### 6. Failure paths

- [ ] Stop engram, press Capture.
      Expected: "engram is unreachable." Nothing is queued — that was
      considered and cut. A capture is lost and said so, rather than held in
      a queue that may never drain.
- [ ] Revoke the token in Housekeeping, press Capture.
      Expected: the panel reports it rather than hanging.

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
   browser can say whether it does.

## Releasing

The Chrome package is built from source by `build.rs` at compile time, so a
deployment always serves the build that matches it. The Firefox XPI must be
AMO-signed, which is a network round trip that does not belong in
`cargo build`, so it is signed once per release and committed.

```sh
# 1. Bump `version` in BOTH manifests. They must match.
# 2. Build the unsigned package:
./extension/pack.sh
cd extension/firefox && zip -r ../../engram-unsigned.zip . && cd ../..
# 3. AMO → Developer Hub → Submit a New Add-on → "On your own site"
#    (unlisted). Mozilla signs it; it is never listed, reviewed or
#    searchable. That signing step is the one place a vendor touches this.
# 4. Commit the signed result:
cp ~/Downloads/engram-*.xpi assets/extension/firefox.xpi
git add -f assets/extension/firefox.xpi
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
