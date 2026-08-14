# Capture Extension (Browser) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Chrome and Firefox extension that captures the page you are reading — or the paragraph you selected — into engram, and searches it from a side panel, without ever asking for permission to read every website.

**Architecture:** One codebase, two manifests. Everything is shared except the manifest and the single call that opens the panel — Chrome's `sidePanel` against Firefox's `sidebar_action`, behind a small shim. The browser supplies HTML it has already rendered and authenticated; the server does the extraction (`POST /api/v1/corpora` with `html`). The extension ships inside the engram binary, so a deployment always serves the build that matches it.

**Tech Stack:** Plain ES2022, no build step, no npm. WebExtensions MV3. Server side: Rust, `rust-embed`, a `build.rs` that zips the Chrome package at compile time.

**Spec:** `docs/superpowers/specs/2026-08-13-capture-surfaces-design.md`

**Depends on:** `docs/superpowers/plans/2026-08-14-capture-surfaces-server.md` — Task 5 (`html` bodies), Task 7 (`?door=extension`), Task 8 (`/ui/pair`). Do not start Task 2 here until those are merged.

## Global Constraints

- **No `<all_urls>`.** Permissions are `activeTab`, `contextMenus`, `storage`, and the panel permission per browser. Everything is user-initiated, so `activeTab` suffices and the install carries no "read your data on all websites" warning. The deployment origin is an *optional* host permission, requested at pairing time for that one origin.
- **A side panel, never a popup.** `infer.ask.timeout_secs` defaults to 900; a popup closes the instant it loses focus, so an answer could evaporate before it arrives.
- **No explanatory copy.** The panel holds a capture button, a search box, an ask box, and results. Field labels and nothing else — the register the core UI already uses.
- **Selection capture sends the selection's HTML**, not the page's.
- **No credential is ever written into a downloadable file.** The token arrives through `launchWebAuthFlow` and lives in `storage.local`.
- **Captures are not queued** when engram is unreachable. The panel says so. That was considered and cut.
- **One signed artifact serves every deployment.** The manifest is static and identical everywhere; the origin is learned at runtime from the pairing page. An XPI is signed over its contents, so a manifest rewritten at download time would invalidate the signature that makes one-click install possible.
- Rust-side changes end green: `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`.
- Commit at the end of every task. Branch: `feat/capture-extension`.

## A conflict in the spec, and what this plan does about it

The spec says both "a self-hosted XPI … auto-updates from the deployment via `update_url`" and "the manifest is static and identical for every deployment". Those cannot both hold: `update_url` is an absolute URL inside the manifest, and a self-hosted single-operator deployment has no canonical host to hardcode. Rewriting it per download is exactly what the signature forbids.

**This plan omits `update_url`.** Firefox install stays one click; updating means visiting the download page again, and the download page says so in one line. If a canonical host ever exists, adding `update_url` is a one-line manifest change and a re-sign — noted in Task 6 at the exact place it goes.

## Testing note

The spec is explicit: the extension is verified by hand, on both browsers, against a running deployment; a browser test harness is not in scope, and pretending otherwise would be worse than saying so. So the JavaScript tasks below replace "write the failing test" with a **scripted manual check** — an exact sequence and an exact expected observation, written before the code, and run before and after. Task 6 is Rust and carries real tests.

## File Structure

| File | Responsibility |
|---|---|
| `extension/shared/panel.html` | the side panel: capture button, search box, ask box, results |
| `extension/shared/panel.css` | panel styling, light and dark |
| `extension/shared/panel.js` | panel behaviour; the only file that touches the DOM of the panel |
| `extension/shared/api.js` | the engram client: base URL, bearer token, the four calls |
| `extension/shared/pair.js` | `launchWebAuthFlow`, fragment parsing, `permissions.request` |
| `extension/shared/background.js` | context menu, omnibox, panel opening, message routing |
| `extension/shared/content.js` | injected on demand; returns page HTML or selection HTML |
| `extension/shared/shim.js` | `browser` vs `chrome`, `sidePanel` vs `sidebarAction` |
| `extension/chrome/manifest.json` | MV3, `side_panel`, `key` |
| `extension/firefox/manifest.json` | MV3, `sidebar_action`, `browser_specific_settings.gecko` |
| `build.rs` | **new** — zips `extension/shared` + `extension/chrome` into `assets/extension/chrome.zip` |
| `assets/extension/firefox.xpi` | committed, AMO-signed unlisted, one per release |
| `src/web/extension.rs` | **new** — `/extension/chrome.zip`, `/extension/firefox.xpi`, `/extension/install` |
| `src/web/templates/extension.html` | **new** — the download page, carrying its own origin |

Shared files are copied into each package by `build.rs` (Chrome) and by the packaging script (Firefox), so neither manifest needs a path prefix and neither browser sees a directory the other one uses.

---

### Task 1: The extension skeleton and the panel shell

**Files:**
- Create: `extension/shared/shim.js`, `extension/shared/panel.html`, `extension/shared/panel.css`, `extension/shared/panel.js`, `extension/shared/background.js`
- Create: `extension/chrome/manifest.json`, `extension/firefox/manifest.json`
- Create: `extension/README.md`

**Interfaces:**
- Produces: `globalThis.engramShim = { runtime, tabs, storage, permissions, identity, contextMenus, openPanel(tabId) }` from `shim.js`; a panel that opens in both browsers and renders four controls.

- [ ] **Step 1: Write the manual check, before the code**

Write this into `extension/README.md` under `## Verifying`:

```markdown
### 1. The panel opens

Chrome: `chrome://extensions` → Developer mode → Load unpacked → pick
`extension/chrome` (after `./extension/pack.sh`, which copies `shared/` in).
Click the toolbar icon.
Expected: a side panel opens on the right holding a "Capture this page"
button, a search box, an ask box, and an empty results area. Nothing else.

Firefox: `about:debugging#/runtime/this-firefox` → Load Temporary Add-on →
pick `extension/firefox/manifest.json`. Click the toolbar icon.
Expected: the same four controls, in the sidebar.

Failure before Task 1 is done: no such extension loads.
```

- [ ] **Step 2: Run it and watch it fail**

Load unpacked in both browsers. Expected: "Manifest file is missing or unreadable".

- [ ] **Step 3: Write the packaging helper**

Create `extension/pack.sh`:

```sh
#!/bin/sh
# Copy the shared sources into each browser's package directory.
#
# Both browsers want a flat package with the manifest at its root, and neither
# follows a symlink out of one. Copying is what keeps a single source of truth
# for eight files that would otherwise be maintained twice.
set -eu
cd "$(dirname "$0")"
for browser in chrome firefox; do
  rm -rf "$browser/shared"
  mkdir -p "$browser/shared"
  cp shared/*.js shared/*.html shared/*.css "$browser/shared/"
done
```

`chmod +x extension/pack.sh`. Add `extension/*/shared/` to `.gitignore` — it is generated.

- [ ] **Step 4: Write the shim**

Create `extension/shared/shim.js`:

```js
// The whole of the Chrome/Firefox divergence, in one place.
//
// Firefox exposes a promise-based `browser`; Chrome exposes `chrome`, which in
// MV3 also returns promises for everything used here. The panel is the one
// real difference: Chrome opens a side panel through `chrome.sidePanel`,
// Firefox through `browser.sidebarAction`, and neither knows the other's name.
const api = globalThis.browser ?? globalThis.chrome;

globalThis.engramShim = {
  runtime: api.runtime,
  tabs: api.tabs,
  scripting: api.scripting,
  storage: api.storage,
  permissions: api.permissions,
  identity: api.identity,
  contextMenus: api.contextMenus,
  omnibox: api.omnibox,

  async openPanel(tabId) {
    if (api.sidePanel) {
      await api.sidePanel.open({ tabId });
    } else {
      await api.sidebarAction.open();
    }
  },
};
```

- [ ] **Step 5: Write the panel**

Create `extension/shared/panel.html`:

```html
<!doctype html>
<meta charset="utf-8">
<link rel="stylesheet" href="panel.css">
<main>
  <button id="capture" class="primary">Capture this page</button>
  <p id="status" hidden></p>

  <label for="q">Search</label>
  <input id="q" type="search" autocomplete="off">

  <label for="ask">Ask</label>
  <form id="ask-form"><input id="ask" type="text" autocomplete="off"></form>

  <div id="results"></div>
</main>
<script src="shim.js"></script>
<script src="api.js"></script>
<script src="pair.js"></script>
<script src="panel.js"></script>
```

Create `extension/shared/panel.css`:

```css
/* The core UI's register: no chrome around the content, one accent, and a
   dark mode that follows the browser rather than a setting nobody would find
   in a side panel. */
:root {
  --bg: #fff; --fg: #16181d; --muted: #6b7280; --line: #e5e7eb; --accent: #2f6f4f;
  color-scheme: light dark;
}
@media (prefers-color-scheme: dark) {
  :root { --bg: #14161a; --fg: #e8eaed; --muted: #9aa0a6; --line: #2a2e35; --accent: #7fb69a; }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--fg);
       font: 14px/1.5 system-ui, sans-serif; }
main { display: flex; flex-direction: column; gap: .5rem; padding: .75rem; }
label { color: var(--muted); font-size: .8rem; }
input, button { width: 100%; padding: .5rem; border: 1px solid var(--line);
                border-radius: .35rem; background: var(--bg); color: var(--fg); }
button.primary { background: var(--accent); color: #fff; border-color: transparent;
                 cursor: pointer; }
#status { margin: 0; color: var(--muted); font-size: .85rem; }
#status.error { color: #b3261e; }
.hit { border-top: 1px solid var(--line); padding: .5rem 0; }
.hit h3 { margin: 0 0 .15rem; font-size: .9rem; }
.hit p { margin: 0; color: var(--muted); }
```

Create `extension/shared/panel.js` with only the shell for now:

```js
const $ = (id) => document.getElementById(id);

function say(message, isError) {
  const el = $('status');
  el.hidden = !message;
  el.textContent = message || '';
  el.classList.toggle('error', !!isError);
}

// Filled in by Tasks 2–4. Wired now so the shell is verifiably loaded.
$('capture').addEventListener('click', () => say('not wired yet'));
```

- [ ] **Step 6: Write the two manifests**

Create `extension/chrome/manifest.json`:

```json
{
  "manifest_version": 3,
  "name": "engram",
  "version": "0.1.0",
  "description": "Capture what you are reading into engram, and search it from here.",
  "permissions": ["activeTab", "scripting", "contextMenus", "storage", "sidePanel", "identity"],
  "optional_host_permissions": ["https://*/*", "http://*/*"],
  "background": { "service_worker": "shared/background.js" },
  "side_panel": { "default_path": "shared/panel.html" },
  "action": { "default_title": "engram" },
  "omnibox": { "keyword": "eg" }
}
```

Create `extension/firefox/manifest.json`:

```json
{
  "manifest_version": 3,
  "name": "engram",
  "version": "0.1.0",
  "description": "Capture what you are reading into engram, and search it from here.",
  "browser_specific_settings": {
    "gecko": { "id": "engram@localhost", "strict_min_version": "128.0" }
  },
  "permissions": ["activeTab", "scripting", "contextMenus", "storage", "identity"],
  "optional_host_permissions": ["https://*/*", "http://*/*"],
  "background": { "scripts": ["shared/background.js"] },
  "sidebar_action": { "default_panel": "shared/panel.html", "default_title": "engram" },
  "action": { "default_title": "engram" },
  "omnibox": { "keyword": "eg" }
}
```

`optional_host_permissions` is a pattern, not a grant: nothing is readable until `permissions.request` names one origin at pairing time, and the install carries no host warning. `gecko.id` is pinned so the pairing redirect is stable across installs — Firefox derives the `extensions.allizom.org` redirect host from it, which is what `is_extension_redirect` on the server accepts.

- [ ] **Step 7: Write the background script**

Create `extension/shared/background.js`:

```js
importScripts?.('shim.js');   // Chrome service worker
// Firefox event pages load `shared/background.js` as a classic script with no
// `importScripts`; the manifest there lists only this file, so pull the shim
// in the way that works in both.
if (!globalThis.engramShim) {
  // eslint-disable-next-line no-undef
  self.importScripts ? self.importScripts('shim.js') : null;
}

const shim = globalThis.engramShim;

shim.runtime.onInstalled?.addListener(() => {
  // Everything else arrives in later tasks.
});

// The toolbar button opens the panel. Chrome can be told to do this without a
// listener, but Firefox cannot, so one path serves both.
(globalThis.browser ?? globalThis.chrome).action.onClicked.addListener((tab) => {
  shim.openPanel(tab.id);
});
```

If Firefox's MV3 event page rejects `importScripts`, list `shim.js` before `background.js` in `background.scripts` and delete the guard — check which the browser actually does, and simplify to whichever works in both rather than leaving both paths in.

- [ ] **Step 8: Run the manual check from Step 1**

Run `./extension/pack.sh`, then load unpacked in both browsers. Expected: the panel opens with four controls; clicking Capture writes "not wired yet".

- [ ] **Step 9: Commit**

```bash
git add extension .gitignore
git commit -m "feat(extension): the panel shell, in one codebase and two manifests"
```

---

### Task 2: Pairing and the API client

**Files:**
- Create: `extension/shared/api.js`, `extension/shared/pair.js`
- Modify: `extension/shared/panel.js`

**Interfaces:**
- Consumes: `GET|POST /ui/pair` (server plan, Task 8), which redirects to `<sink>#token=…&state=…&origin=…`.
- Produces:
  - `engramApi.config()` → `{origin, token} | null` from `storage.local`
  - `engramApi.call(path, init)` → parsed JSON; throws `Error` whose `message` is the server's message
  - `engramPair.pair(origin)` → stores the token, requests host permission for `origin`, resolves or throws
  - `engramPair.pairManually(origin, token)` → the fallback path

- [ ] **Step 1: Write the manual check**

Append to `extension/README.md`:

```markdown
### 2. Pairing

With engram running at http://localhost:8080 and you signed in there:
open the panel, press Capture.
Expected before pairing: "Not paired yet." and a "Pair with engram" button.

Press it, type `http://localhost:8080`, continue.
Expected: a browser window on `/ui/pair` naming that origin; press Pair; the
window closes; the panel says "Paired." A permission prompt names
`localhost:8080` and nothing else.

Then `chrome://extensions` → Details → Site access.
Expected: exactly one host, the deployment. Not "on all sites".

And in engram: Housekeeping → API tokens.
Expected: one token named "browser extension".
```

- [ ] **Step 2: Run it and watch it fail**

Press Capture. Expected: "not wired yet".

- [ ] **Step 3: Write the API client**

Create `extension/shared/api.js`:

```js
const shim = globalThis.engramShim;

globalThis.engramApi = {
  async config() {
    const { origin, token } = await shim.storage.local.get(['origin', 'token']);
    return origin && token ? { origin, token } : null;
  },

  async save(origin, token) {
    await shim.storage.local.set({ origin, token });
  },

  /// Every call goes through here so there is one place that knows about the
  /// bearer header, and one place that turns a failure into a message worth
  /// showing. The server's own text is kept: "that file is application/pdf"
  /// is useful and "request failed" is not.
  async call(path, init = {}) {
    const cfg = await this.config();
    if (!cfg) throw new Error('Not paired yet.');
    let res;
    try {
      res = await fetch(cfg.origin + path, {
        ...init,
        headers: {
          ...(init.headers || {}),
          authorization: 'Bearer ' + cfg.token,
        },
      });
    } catch (e) {
      // Unreachable is its own case, and it is not queued: the capture is
      // lost and the operator is told, rather than silently held in a queue
      // that may never drain.
      throw new Error('engram is unreachable.');
    }
    const body = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(body.error || ('engram answered ' + res.status));
    return body;
  },
};
```

Check the error key against `Error::into_response` in `src/error.rs` and use the field it actually emits.

- [ ] **Step 4: Write the pairing flow**

Create `extension/shared/pair.js`:

```js
const shim = globalThis.engramShim;

globalThis.engramPair = {
  /// Pair through the browser's own auth-flow window.
  ///
  /// The redirect target is a sink the browser intercepts and never loads, so
  /// the token comes back to this extension and to nothing else. It arrives in
  /// the fragment, which is never sent to a server and does not land in a
  /// proxy log.
  async pair(origin) {
    const redirect = shim.identity.getRedirectURL();
    const state = crypto.randomUUID();
    const url = origin + '/ui/pair'
      + '?redirect_uri=' + encodeURIComponent(redirect)
      + '&state=' + encodeURIComponent(state);

    const done = await shim.identity.launchWebAuthFlow({ url, interactive: true });
    const fragment = new URLSearchParams((done.split('#')[1] || ''));
    if (fragment.get('state') !== state) throw new Error('Pairing was tampered with.');
    const token = fragment.get('token');
    if (!token) throw new Error('engram returned no token.');

    // The origin the deployment reported for itself, not the one typed: the
    // extension asks the browser for permission to reach exactly that host.
    const learned = fragment.get('origin') || origin;
    const granted = await shim.permissions.request({ origins: [learned + '/*'] });
    if (!granted) throw new Error('Permission for ' + learned + ' was declined.');

    await globalThis.engramApi.save(learned, token);
  },

  /// The fallback for when `launchWebAuthFlow` is unavailable: a token pasted
  /// from Housekeeping → API tokens. Same end state, one more step.
  async pairManually(origin, token) {
    const granted = await shim.permissions.request({ origins: [origin + '/*'] });
    if (!granted) throw new Error('Permission for ' + origin + ' was declined.');
    await globalThis.engramApi.save(origin, token);
  },
};
```

- [ ] **Step 5: Wire the panel to it**

In `extension/shared/panel.js`, replace the placeholder listener:

```js
async function ensurePaired() {
  if (await engramApi.config()) return true;
  const origin = prompt('engram address (for example https://engram.example)');
  if (!origin) return false;
  try {
    say('Pairing…');
    await engramPair.pair(origin.replace(/\/+$/, ''));
    say('Paired.');
    return true;
  } catch (e) {
    say(e.message, true);
    return false;
  }
}

$('capture').addEventListener('click', async () => {
  if (!(await ensurePaired())) return;
  say('not wired yet');
});
```

- [ ] **Step 6: Run the manual check from Step 1**

Both browsers. Confirm the single-host site access, and the token row in engram.

- [ ] **Step 7: Commit**

```bash
git add extension
git commit -m "feat(extension): pair through the browser's auth flow"
```

---

### Task 3: Capturing the page and the selection

**Files:**
- Create: `extension/shared/content.js`
- Modify: `extension/shared/panel.js`, `extension/shared/background.js`

**Interfaces:**
- Consumes: `POST /api/v1/corpora` with `{html, url, title}` (server plan, Task 5).
- Produces: `engramCapture(scope)` in the panel, where `scope` is `'page'` or `'selection'`; a `{type: 'grab', scope}` message the content script answers with `{html, url, title}`.

- [ ] **Step 1: Write the manual check**

Append to `extension/README.md`:

```markdown
### 3. Capture

On an article page, open the panel and press "Capture this page".
Expected: "Captured." within a second or two, and the document appearing under
Recent on `/ui/capture` with `origin` = extension and a link back to the URL.

On a page behind a login you are signed into: same.
Expected: the corpus holds the article, not the login wall — which is the
whole reason the browser supplies the HTML.

On a cookie-banner-only page (or with JavaScript disabled so it renders empty):
Expected: a red line naming the extraction floor. Nothing under Recent.

Select three paragraphs, right-click → "Capture selection".
Expected: a corpus holding only those paragraphs.

Capture the same page twice.
Expected: the second says it is a duplicate, or names the near-duplicate it
matched — whatever `ingest` returned, shown as the web UI shows it.
```

- [ ] **Step 2: Run it and watch it fail**

Press Capture on an article. Expected: "not wired yet".

- [ ] **Step 3: Write the content script**

Create `extension/shared/content.js`:

```js
// Injected on demand by `scripting.executeScript`, never declared in the
// manifest: `activeTab` grants access to this one tab, for this one action,
// because the operator just asked for it. A declared content script would mean
// `<all_urls>` and a warning at install.
(() => {
  const scope = globalThis.__engramScope || 'page';

  function selectionHtml() {
    const sel = window.getSelection();
    if (!sel || sel.isCollapsed) return null;
    const box = document.createElement('div');
    for (let i = 0; i < sel.rangeCount; i++) {
      box.appendChild(sel.getRangeAt(i).cloneContents());
    }
    return box.innerHTML;
  }

  // The selection's HTML, not the page's: capturing three paragraphs must not
  // quietly store the article they sit in.
  const html = scope === 'selection'
    ? selectionHtml()
    : document.documentElement.outerHTML;

  return { html, url: location.href, title: document.title };
})();
```

- [ ] **Step 4: Wire capture in the panel**

Add to `extension/shared/panel.js`:

```js
async function grab(scope) {
  const [tab] = await engramShim.tabs.query({ active: true, currentWindow: true });
  const [{ result }] = await engramShim.scripting.executeScript({
    target: { tabId: tab.id },
    // The scope is set on the page before the file runs, so the injected file
    // stays a file rather than becoming a string this script assembles.
    func: (s) => { globalThis.__engramScope = s; },
    args: [scope],
  }).then(() => engramShim.scripting.executeScript({
    target: { tabId: tab.id },
    files: ['shared/content.js'],
  }));
  return result;
}

async function capture(scope) {
  if (!(await ensurePaired())) return;
  say('Capturing…');
  try {
    const page = await grab(scope);
    if (!page || !page.html) {
      say(scope === 'selection' ? 'Nothing selected.' : 'Nothing to capture.', true);
      return;
    }
    const out = await engramApi.call('/api/v1/corpora', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ html: page.html, url: page.url, title: page.title }),
    });
    // `ingest` already returns its verdict; the panel shows it as the web UI
    // does rather than inventing a second vocabulary for the same three cases.
    if (out.duplicate) say('Already captured.');
    else if (out.near_duplicate) say('Captured, and parked: it looks like something you already have.');
    else say('Captured.');
  } catch (e) {
    say(e.message, true);
  }
}

$('capture').addEventListener('click', () => capture('page'));

engramShim.runtime.onMessage.addListener((msg) => {
  if (msg.type === 'capture') capture(msg.scope);
  if (msg.type === 'search') { $('q').value = msg.q; runSearch(); }
});
```

`runSearch` arrives in Task 4; until then the message branch is unreachable from the panel's own controls.

- [ ] **Step 5: Run the manual check from Step 1**

All five cases, in both browsers. The login-wall case is the one that matters most — it is the reason this is an extension and not a server-side fetch.

- [ ] **Step 6: Commit**

```bash
git add extension
git commit -m "feat(extension): capture the page, and the selection"
```

---

### Task 4: Search and ask in the panel

**Files:**
- Modify: `extension/shared/panel.js`

**Interfaces:**
- Consumes: `GET /api/v1/search?q=…&door=extension` (server plan, Task 7), `POST /api/v1/ask`.
- Produces: `runSearch()`; results rendered into `#results`, each linking to its artifact on the deployment.

- [ ] **Step 1: Write the manual check**

Append to `extension/README.md`:

```markdown
### 4. Search and ask

Type into Search.
Expected: results appear as you type, each a title and a line of text; clicking
one opens that artifact on the deployment in a new tab.

With `feedback.enabled = true`, search once from the panel and once from
`/ui/search`, then open `/ui/judge`.
Expected: two events, one recorded against the `extension` door and one
against `ui`. That distinction is the point — a query typed in the panel while
reading was composed before anything came back.

Type a question into Ask and submit.
Expected: the panel keeps waiting (up to `infer.ask.timeout_secs`, default 900)
and eventually renders the answer. It must not close or clear meanwhile —
which is why this is a panel and not a popup.
```

- [ ] **Step 2: Run it and watch it fail**

Type in the search box. Expected: nothing happens.

- [ ] **Step 3: Write it**

Add to `extension/shared/panel.js`:

```js
function render(hits) {
  const box = $('results');
  box.textContent = '';
  if (!hits.length) { box.textContent = 'Nothing.'; return; }
  engramApi.config().then((cfg) => {
    for (const h of hits) {
      const el = document.createElement('div');
      el.className = 'hit';
      const title = document.createElement('h3');
      const link = document.createElement('a');
      link.href = cfg.origin + '/ui/artifacts/' + h.id;
      link.target = '_blank';
      link.rel = 'noreferrer';
      link.textContent = h.title || 'Untitled';
      title.appendChild(link);
      const body = document.createElement('p');
      body.textContent = h.text;
      el.append(title, body);
      box.appendChild(el);
    }
  });
}

let searchTimer;
async function runSearch() {
  const q = $('q').value.trim();
  if (!q) { $('results').textContent = ''; return; }
  if (!(await engramApi.config())) { say('Not paired yet.', true); return; }
  try {
    // `door=extension` is how the judging page tells a query typed while
    // reading from one typed in the web UI. Only this value is honoured
    // server-side; the client cannot claim to be `ask` or `judge`.
    const hits = await engramApi.call(
      '/api/v1/search?door=extension&q=' + encodeURIComponent(q));
    say('');
    render(hits);
  } catch (e) {
    say(e.message, true);
  }
}

// Debounced, because this is search-as-you-type and each keystroke would
// otherwise be an embedding call on the deployment.
$('q').addEventListener('input', () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(runSearch, 200);
});

$('ask-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const q = $('ask').value.trim();
  if (!q || !(await ensurePaired())) return;
  say('Thinking… this can take a while.');
  try {
    const out = await engramApi.call('/api/v1/ask', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ q }),
    });
    say('');
    const box = $('results');
    box.textContent = '';
    const answer = document.createElement('p');
    answer.textContent = out.answer;
    box.appendChild(answer);
    render(out.sources || []);
  } catch (e) {
    say(e.message, true);
  }
});
```

Check the `ask` request and response field names against `crate::core::ask::AskRequest` / `AskResponse` and use those, not these.

- [ ] **Step 4: Run the manual check from Step 1**

Both browsers. The door check needs `feedback.enabled = true` in `config.toml`.

- [ ] **Step 5: Commit**

```bash
git add extension
git commit -m "feat(extension): search and ask from the panel"
```

---

### Task 5: Context menu and omnibox

**Files:**
- Modify: `extension/shared/background.js`

**Interfaces:**
- Consumes: the panel's `runtime.onMessage` handler (Task 3).
- Produces: two context-menu entries on a selection, and the `eg` omnibox keyword.

- [ ] **Step 1: Write the manual check**

Append to `extension/README.md`:

```markdown
### 5. Outside the panel

Select text, right-click.
Expected: "Search engram for this" and "Capture selection". No entries when
nothing is selected.

Press each.
Expected: the panel opens (if closed) with the selection in the search box, or
with the selection captured.

Type `eg` in the address bar, space, then a query, and press Enter.
Expected: the panel opens with that query searched.
```

- [ ] **Step 2: Run it and watch it fail**

Right-click a selection. Expected: no engram entries.

- [ ] **Step 3: Write it**

Append to `extension/shared/background.js`:

```js
const MENU = {
  search: 'engram-search-selection',
  capture: 'engram-capture-selection',
};

shim.runtime.onInstalled.addListener(() => {
  shim.contextMenus.create({
    id: MENU.search,
    title: 'Search engram for this',
    contexts: ['selection'],
  });
  shim.contextMenus.create({
    id: MENU.capture,
    title: 'Capture selection',
    contexts: ['selection'],
  });
});

shim.contextMenus.onClicked.addListener(async (info, tab) => {
  // The panel is where results and status live, so both entries open it and
  // then hand it the work. A menu entry that acted silently would have
  // nowhere to report a near-duplicate or an unreachable server.
  await shim.openPanel(tab.id);
  if (info.menuItemId === MENU.search) {
    shim.runtime.sendMessage({ type: 'search', q: info.selectionText });
  } else if (info.menuItemId === MENU.capture) {
    shim.runtime.sendMessage({ type: 'capture', scope: 'selection' });
  }
});

shim.omnibox.onInputEntered.addListener(async (text) => {
  const [tab] = await shim.tabs.query({ active: true, currentWindow: true });
  await shim.openPanel(tab.id);
  shim.runtime.sendMessage({ type: 'search', q: text });
});
```

A panel that has just opened may not have registered its listener when `sendMessage` fires. If the manual check shows the first invocation missing, have the panel ask for pending work on load — `runtime.sendMessage({type: 'pending'})` answered from a variable in the background script — rather than sleeping.

- [ ] **Step 4: Run the manual check from Step 1**

Both browsers, including the just-opened-panel race.

- [ ] **Step 5: Commit**

```bash
git add extension
git commit -m "feat(extension): context menu and omnibox"
```

---

### Task 6: Ship it inside the binary

**Files:**
- Create: `build.rs`
- Create: `src/web/extension.rs`, `src/web/templates/extension.html`
- Modify: `Cargo.toml` (`[build-dependencies] zip`), `src/web/mod.rs`, `src/web/templates/ops.html`, `.gitignore`

**Interfaces:**
- Produces: `assets/extension/chrome.zip` (generated at compile time), `GET /extension/chrome.zip`, `GET /extension/firefox.xpi`, `GET /extension/install` — all authenticated.

- [ ] **Step 1: Write the failing tests**

Create `src/web/extension.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    #[tokio::test]
    async fn the_extension_downloads_need_authentication() {
        let (app, _token, _core) = crate::web::api::tests::app_token_and_core().await;
        for path in ["/extension/chrome.zip", "/extension/firefox.xpi", "/extension/install"] {
            let res = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_ne!(res.status(), StatusCode::OK, "{path} served unauthenticated");
        }
    }

    #[tokio::test]
    async fn the_chrome_package_is_built_into_the_binary() {
        // `build.rs` zips it at compile time, so a deployment always serves the
        // package that matches it and there is no separate artifact to publish
        // or forget.
        let zip = crate::web::assets::Assets::get("extension/chrome.zip")
            .expect("chrome.zip must be embedded");
        assert!(zip.data.len() > 512);
        assert_eq!(&zip.data[..2], b"PK");
    }

    #[tokio::test]
    async fn the_install_page_carries_its_own_origin() {
        let (app, token, _core) = crate::web::api::tests::app_token_and_core().await;
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/extension/install")
                    .header("host", "engram.example")
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("http://engram.example"), "got: {html}");
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib web::extension`
Expected: FAIL — no such module, and no embedded `extension/chrome.zip`.

- [ ] **Step 3: Write the build script**

In `Cargo.toml`:

```toml
[build-dependencies]
# Packs the Chrome extension at compile time. A build dependency, not a
# runtime one: the zip is a build artifact, not something the server assembles
# per request.
zip = { version = "6", default-features = false, features = ["deflate"] }
```

Create `build.rs`:

```rust
//! Packs the Chrome extension into `assets/extension/chrome.zip`.
//!
//! Built from source at compile time so a deployment always serves the build
//! that matches it, and so there is no second artifact to publish or forget.
//! Written into `assets/` rather than `OUT_DIR` because `rust-embed` embeds
//! that directory; the file is generated and is gitignored.
//!
//! The Firefox XPI is not built here. It must be AMO-signed, which is a
//! network round trip that does not belong in `cargo build`, so it is signed
//! once per release and committed.

use std::io::Write;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=extension/shared");
    println!("cargo:rerun-if-changed=extension/chrome/manifest.json");

    let out = Path::new("assets/extension");
    std::fs::create_dir_all(out).expect("assets/extension");
    let file = std::fs::File::create(out.join("chrome.zip")).expect("chrome.zip");
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::FileOptions<()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let manifest = std::fs::read("extension/chrome/manifest.json").expect("chrome manifest");
    zip.start_file("manifest.json", opts).unwrap();
    zip.write_all(&manifest).unwrap();

    // Flat `shared/` inside the package, matching the paths the manifest names.
    let mut names: Vec<_> = std::fs::read_dir("extension/shared")
        .expect("extension/shared")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    // Sorted, so the same sources produce the same archive twice running.
    names.sort();
    for name in names {
        let body = std::fs::read(Path::new("extension/shared").join(&name)).unwrap();
        zip.start_file(format!("shared/{name}"), opts).unwrap();
        zip.write_all(&body).unwrap();
    }
    zip.finish().unwrap();
}
```

Add to `.gitignore`:

```
assets/extension/chrome.zip
```

- [ ] **Step 4: Write the routes and the page**

Prepend to `src/web/extension.rs`:

```rust
use crate::auth::Identity;
use crate::web::assets::Assets;
use crate::web::pair::request_origin;
use crate::web::state::AppState;
use askama::Template;
use axum::Router;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

#[derive(Template)]
#[template(path = "extension.html")]
struct InstallTemplate {
    theme: String,
    origin: String,
    /// False in a checkout that has not been through a release signing. The
    /// page then says so rather than offering a link that 404s.
    have_xpi: bool,
}

/// The download page. Authenticated like everything else, and it carries this
/// deployment's origin into the pairing link — the static, signed manifest
/// cannot know it, so the page is where it is learned.
async fn install_page(_id: Identity, headers: HeaderMap) -> Response {
    crate::web::ui::HtmlTemplate(InstallTemplate {
        theme: "light".into(),
        origin: request_origin(&headers).unwrap_or_default(),
        have_xpi: Assets::get("extension/firefox.xpi").is_some(),
    })
    .into_response()
}

fn embedded(path: &str, mime: &str, filename: &str) -> Response {
    match Assets::get(path) {
        Some(f) => (
            [
                (header::CONTENT_TYPE, mime.to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{filename}\""),
                ),
                // Rebuilt with the binary, so a cached copy would be a copy of
                // the wrong build.
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
            f.data.into_owned(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not built into this binary").into_response(),
    }
}

async fn chrome_zip(_id: Identity) -> Response {
    embedded("extension/chrome.zip", "application/zip", "engram-chrome.zip")
}

/// Served with the type Firefox installs from, so the link is one click.
async fn firefox_xpi(_id: Identity) -> Response {
    embedded(
        "extension/firefox.xpi",
        "application/x-xpinstall",
        "engram.xpi",
    )
}

pub fn extension_router() -> Router<AppState> {
    Router::new()
        .route("/extension/install", get(install_page))
        .route("/extension/chrome.zip", get(chrome_zip))
        .route("/extension/firefox.xpi", get(firefox_xpi))
}
```

Create `src/web/templates/extension.html`:

```html
{% extends "layout.html" %}
{% block title %}Browser extension — engram{% endblock %}
{% block content %}
<h2>Browser extension</h2>
{# Install differs by browser, and saying so plainly is better than a single
   button that works for half of the people who press it. #}
<h3>Firefox</h3>
{% if have_xpi %}
<p><a href="/extension/firefox.xpi">Install</a> — one click.</p>
{% else %}
<p class="muted">Not in this build. The Firefox package is signed once per
release and committed; a checkout that has not been through one does not
carry it.</p>
{% endif %}
<p class="muted">Updating means coming back here after a deployment.</p>

<h3>Chrome</h3>
<p><a href="/extension/chrome.zip">Download</a>, unzip it, open
<code>chrome://extensions</code>, turn on Developer mode, and press
<em>Load unpacked</em>.</p>
<p class="muted">Chrome blocks off-store installs for ordinary users, so there
is no one-click path and no auto-update. An enterprise policy allowlist is the
alternative, and is not the default here.</p>

<h3>Pairing</h3>
<p>Open the panel and press Capture. It will ask for
<strong>{{ origin }}</strong> and for permission to reach that one host.</p>
{% endblock %}
```

- [ ] **Step 5: Mount it and link it**

In `src/web/mod.rs`: `pub mod extension;` and `.merge(extension::extension_router())`.

In `src/web/templates/ops.html`, next to the API tokens section:

```html
<p><a href="/extension/install">Browser extension</a></p>
```

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib web::extension`
Expected: PASS. `the_extension_downloads_need_authentication` will show `/extension/install` returning a redirect rather than 401 — that is `redirect_unauthenticated_browsers` doing its job, and `assert_ne!(…, OK)` is written to allow it.

- [ ] **Step 7: Sign the Firefox package**

Not `cargo build`'s job, and deliberately not automated here:

```bash
./extension/pack.sh
cd extension/firefox && zip -r ../../engram-unsigned.zip . && cd ../..
# Then: AMO → Developer Hub → Submit a New Add-on → "On your own site"
# (unlisted). Mozilla signs it; it is never listed, reviewed or searchable.
# Download the signed .xpi and commit it:
#   cp ~/Downloads/engram-*.xpi assets/extension/firefox.xpi
#   git add -f assets/extension/firefox.xpi
```

Record the exact steps in `extension/README.md` under `## Releasing`, including that `version` must be bumped in both manifests first, and that if a canonical host ever exists, `browser_specific_settings.gecko.update_url` goes in the Firefox manifest at that point and the package is re-signed.

- [ ] **Step 8: Commit**

```bash
git add build.rs Cargo.toml Cargo.lock .gitignore src/web/extension.rs \
        src/web/templates/extension.html src/web/mod.rs src/web/templates/ops.html \
        extension/README.md
git commit -m "feat(extension): ship the extension inside the binary"
```

---

### Task 7: The verification pass

**Files:**
- Modify: `extension/README.md`

No new code. The spec says the extension is verified by hand on both browsers against a running deployment, and this is that pass — run end to end, on a build installed the way an operator would install it, not a temporary one.

- [ ] **Step 1: Install as an operator would**

Deploy the branch. Visit `/extension/install`. Firefox: install the XPI from the link. Chrome: download, unzip, load unpacked.

- [ ] **Step 2: Run every check in `extension/README.md`**

Sections 1–5, both browsers. Record pass/fail per check in the commit message; a check that needed the code changed gets its fix committed under the task it belongs to, not here.

- [ ] **Step 3: Run the three failure paths deliberately**

- Stop engram, press Capture. Expected: "engram is unreachable." No queue, no retry, nothing lost silently.
- Revoke the token in Housekeeping, press Capture. Expected: the panel reports it rather than hanging.
- Capture a page that extracts to boilerplate. Expected: the floor is named and nothing is stored.

- [ ] **Step 4: Confirm the permission surface**

Chrome: Details → Site access shows one host. Firefox: `about:addons` → Permissions shows one host. Neither shows "all sites". A fresh install, before pairing, shows none.

- [ ] **Step 5: Commit the results**

```bash
git add extension/README.md
git commit -m "docs(extension): record the two-browser verification pass"
```

---

## Done when

- The panel opens in both browsers and holds four controls and no prose.
- Pairing mints a token through `launchWebAuthFlow` and grants host permission for one origin.
- A page behind a login captures the article, not the login wall.
- A selection captures the selection, not the page around it.
- A search from the panel is recorded against the `extension` door.
- `/extension/install` serves both packages, authenticated, and names this deployment's origin.
- The install carries no "read your data on all websites" warning.

## Out of scope

PDF and non-`.txt` uploads; capture-time duplicate badges; an offline capture queue; the ambient related-artifacts sidebar and the `<all_urls>` permission it would require. Each was considered and cut, and none is blocked by anything here.
