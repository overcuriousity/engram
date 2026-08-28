const $ = (id) => document.getElementById(id);

function say(message, kind) {
  const el = $('status');
  el.hidden = !message;
  el.textContent = message || '';
  el.classList.toggle('error', kind === 'error');
  el.classList.toggle('good', kind === 'good');
}

/// What the model is doing while it does it, replaced continually and gone the
/// moment the answer is complete. Separate from `say`, which reports what
/// happened and stays until something else happens.
function thinking(message) {
  const el = $('thinking');
  el.hidden = !message;
  el.textContent = message || '';
}

/// The typed address reduced to a bare origin.
///
/// It becomes a host permission match pattern, which admits a scheme, a host
/// and a port and nothing else — a path or a trailing slash makes the request
/// throw rather than fail. A bare host is read as `https`, because the one
/// deployment that is legitimately plain `http` is a local one that will have
/// been typed with its port anyway.
function cleanOrigin(raw) {
  const trimmed = raw.trim();
  if (!trimmed) return null;
  try {
    const u = new URL(/^https?:\/\//i.test(trimmed) ? trimmed : 'https://' + trimmed);
    return u.protocol + '//' + u.host;
  } catch (e) {
    return null;
  }
}

/// Where the deployment is reached, as the last `reflectPairing` read it.
///
/// Cached so that rendering a row is synchronous. Two overlapping renders that
/// each awaited storage in the middle could interleave — the citations of an
/// ask arriving into a list its `done` frame had already replaced — and the
/// cure for that race is not to have it.
let deployment = null;

/// Show the pairing face or the paired face, and answer which one it is.
///
/// Called after anything that can change the stored token — including a failed
/// call, because a 401 clears it. Without that the form would stay hidden and
/// there would be no way back to paired.
async function reflectPairing() {
  const cfg = await engramApi.config();
  deployment = cfg ? cfg.origin : null;
  $('pairing').hidden = !!cfg;
  $('forget').hidden = !cfg;
  $('recent').hidden = !cfg;
  return cfg;
}

/// The guard every action starts with. Unpaired is not an error to report and
/// move on from: it is a state with one way out, so it shows the way out.
async function requirePaired() {
  if (await reflectPairing()) return true;
  say('Not paired yet — enter the address engram is reached at.', 'error');
  $('origin').focus();
  return false;
}

/// Report a failure and re-read the pairing state, because the failure may
/// have been the one that cleared the token.
async function fail(e) {
  thinking('');
  say(e.message, 'error');
  await reflectPairing();
}

$('pair').addEventListener('click', async () => {
  const origin = cleanOrigin($('origin').value);
  if (!origin) {
    say('That is not an address — try https://engram.example.', 'error');
    return;
  }

  // The first thing awaited in this handler, and deliberately so.
  // `permissions.request` is only granted while the browser still counts this
  // click as a user gesture, and awaiting anything at all before it spends
  // that gesture: the browser answers "this function must be called during a
  // user gesture" and pairing cannot complete.
  let granted;
  try {
    granted = await engramShim.permissions.request({ origins: [origin + '/*'] });
  } catch (e) {
    say(e.message, 'error');
    return;
  }
  if (!granted) {
    say('Permission for ' + origin + ' was declined.', 'error');
    return;
  }

  say('Pairing…');
  try {
    await engramPair.pair(origin);
    say('Paired with ' + origin + '.', 'good');
  } catch (e) {
    say(e.message, 'error');
  }
  await reflectPairing();
  await refreshRecent();
});

// Forgetting hands back the host permission along with the token. Leaving the
// permission granted for a deployment this extension can no longer reach is
// access held for no reason. The token itself lives on server-side until it is
// revoked under Housekeeping, and the panel says so rather than implying it
// has done more than it has.
$('forget').addEventListener('click', async () => {
  const cfg = await engramApi.config();
  await engramApi.forget();
  if (cfg) {
    await engramShim.permissions.remove({ origins: [cfg.origin + '/*'] }).catch(() => {});
  }
  // And the permission to read pages, for the same reason: with no deployment
  // to send a capture to, being able to read every page is access held for
  // nothing. It is asked for again the next time Capture needs it.
  await engramShim.permissions.remove({ origins: ALL_HOSTS }).catch(() => {});
  hasAllHosts = false;
  clearResults();
  $('recent-list').textContent = '';
  say('Forgotten. The token is still listed under Housekeeping until revoked.');
  await reflectPairing();
});

/// Reading a page needs either `activeTab` or a host permission for it.
///
/// `activeTab` is granted for the tab the invoking gesture happened in and for
/// no other. A context menu entry therefore always has it, and the panel only
/// has it for the tab it was opened from — but a side panel stays open across
/// tab switches, so by the time Capture is pressed the operator is very often
/// somewhere else, and the injection fails with a browser error the panel had
/// no business showing raw.
///
/// The way out is a real host permission, which the manifest declares as
/// optional and which is asked for once. Both are tracked here so the click
/// handler can decide whether to ask *before* it awaits anything: like
/// pairing, `permissions.request` is only granted while the click still counts
/// as a user gesture, and awaiting first spends it.
const ALL_HOSTS = ['https://*/*', 'http://*/*'];
let hasAllHosts = false;
let openedForTabId = null;
let tabChanged = false;

engramShim.permissions
  .contains({ origins: ALL_HOSTS })
  .then((ok) => { hasAllHosts = ok; })
  .catch(() => {});

engramShim.tabs
  .query({ active: true, currentWindow: true })
  .then(([tab]) => { openedForTabId = tab ? tab.id : null; })
  .catch(() => {});

// Only the id is read, which every browser gives without a permission — the
// url would need one. Switching back to the tab the panel was opened from
// restores the grant, so this is assigned rather than latched.
engramShim.tabs.onActivated.addListener((info) => {
  tabChanged = info.tabId !== openedForTabId;
});

/// Ask the active tab for its HTML, or for the selection's.
///
/// Two injections rather than one: the scope is set on the page first, so the
/// second stays an ordinary file rather than a string this script assembles.
async function grab(scope) {
  const [tab] = await engramShim.tabs.query({ active: true, currentWindow: true });
  if (!tab) throw new Error('No page to capture.');

  try {
    await engramShim.scripting.executeScript({
      target: { tabId: tab.id },
      func: (s) => { globalThis.__engramScope = s; },
      args: [scope],
    });
    const [injected] = await engramShim.scripting.executeScript({
      target: { tabId: tab.id },
      files: ['shared/content.js'],
    });
    return injected && injected.result;
  } catch (e) {
    // No grant for this tab, or a page no extension may read at all — a
    // browser settings page, an add-on listing, a PDF viewer. "Cannot access
    // contents of the page" is true and useless; this says what to do.
    throw new Error(
      'engram cannot read this tab. Right-click the page and choose ' +
      '“Capture this page”, or allow engram to read pages when it asks.');
  }
}

/// What `ingest` decided, in the vocabulary the web UI already uses. Not a
/// second set of words for the same three cases.
function verdict(out) {
  if (out.duplicate) say('Already captured.');
  else if (out.near_duplicate) say('Captured, and parked: it looks like something you already have.');
  else say('Captured.', 'good');
}

async function capture(scope) {
  if (!(await requirePaired())) return;
  say('Capturing…');
  try {
    const page = await grab(scope);
    if (!page || !page.html) {
      say(scope === 'selection' ? 'Nothing selected.' : 'Nothing to capture.', 'error');
      return;
    }
    const out = await engramApi.call('/api/v1/corpora', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      // `scope` travels with the capture because the server cannot tell a
      // highlighted fragment from a page that rendered to almost nothing, and
      // it applies an extraction floor to the second. Three sentences picked
      // out on purpose are not a login wall.
      body: JSON.stringify({ html: page.html, url: page.url, title: page.title, scope }),
    });
    verdict(out);
    await refreshRecent();
  } catch (e) {
    await fail(e);
  }
}

$('capture-page').addEventListener('click', async () => {
  // First, and before anything is awaited. The panel is open on a tab it was
  // not opened from and holds no host permission, so the injection is about to
  // fail — asking now, while the click is still a user gesture, is the only
  // moment the browser will honour the request. A refusal is not fatal: the
  // capture goes ahead and `grab` reports what it could not read.
  if (tabChanged && !hasAllHosts) {
    try {
      hasAllHosts = await engramShim.permissions.request({ origins: ALL_HOSTS });
    } catch (e) {
      // Some browsers throw rather than answering false. Same outcome.
    }
  }
  await capture('page');
});

/// The box, and the three things that can be done with what is in it.
///
/// Typing searches. The two buttons ask and capture. Which of the three
/// happens is decided by a button or by the act of typing, and never by the
/// panel noticing how long the text is or that it has a newline in it: a box
/// that changed its mind about what it was would be a box you could not paste
/// into without reading the screen first.
const box = $('box');

/// Above this the box is no longer plausibly a query, and the live search
/// stops firing. Nothing else changes: the buttons still act on the text, the
/// box is still the same box, and a line says why the results stopped moving.
/// The alternative was embedding a pasted chapter on the deployment on every
/// pause in typing.
///
/// Deliberately generous — several paragraphs rather than one. The cost this
/// guards against is a pasted document embedded every 200ms, and stopping a
/// search that somebody meant is the worse failure of the two.
const MAX_QUERY = 2000;

/// Grow to fit the text, to a point. Past that the box scrolls rather than
/// pushing the results off the panel.
function fit() {
  box.style.height = 'auto';
  const max = Math.round(window.innerHeight * 0.4);
  const wanted = box.scrollHeight;
  box.style.height = Math.min(wanted, max) + 'px';
  box.style.overflowY = wanted > max ? 'auto' : 'hidden';
}

/// The two verbs act on text; with none there is nothing to act on.
function reflectVerbs() {
  const empty = !box.value.trim();
  $('ask').disabled = empty;
  $('capture-text').disabled = empty;
}

function hint(message) {
  const el = $('hint');
  el.hidden = !message;
  el.textContent = message || '';
}

let searchTimer;
// The settled pass, the same contract as the web UI's workspace: the
// keystroke's search answers in vector order, and once the operator stops
// typing the same question is asked once more with `rerank=true`. On a
// deployment whose scope covers search that buys the reranked order; on one
// without, the server answers in the same vector order and the identical
// list is left untouched below.
let refineTimer;

box.addEventListener('input', () => {
  fit();
  reflectVerbs();
  clearTimeout(searchTimer);
  clearTimeout(refineTimer);
  // Debounced, because this is search-as-you-type and every keystroke would
  // otherwise be an embedding call on the deployment.
  searchTimer = setTimeout(runSearch, 200);
});

/// Render hits as rows linking back to the deployment.
///
/// Built with `createElement` and `textContent` rather than assembled as an
/// HTML string: artifact text is whatever a captured page contained, and
/// putting that through `innerHTML` would run it. It is also why the ask
/// stream sends this panel values where the web UI's stream sends rendered
/// fragments.
function renderHits(hits, into) {
  for (const h of hits) {
    const el = document.createElement('div');
    el.className = 'hit';

    const title = document.createElement('h3');
    const link = document.createElement('a');
    link.href = deployment + '/ui/artifacts/' + h.artifact_id;
    link.target = '_blank';
    link.rel = 'noreferrer noopener';
    // The server names a passage by its note when it has no heading of its
    // own; what reaches here untitled has neither, and its opening is its name.
    link.textContent = h.title || label({ raw_text: h.text }) || h.artifact_id;
    title.appendChild(link);

    const body = document.createElement('p');
    body.textContent = h.text;

    el.append(title, body);
    into.appendChild(el);
  }
}

/// Which action owns the results.
///
/// Search and ask both write there and both are slow enough to overlap: the
/// keystroke that finishes a question arms a search that fires 200ms after the
/// click on ask, and two searches can be in flight at once over a deployment
/// that answers the first one second. Each action takes a turn before it goes
/// to the wire, and anything arriving for a turn that is no longer the current
/// one is dropped rather than rendered under whatever replaced it.
let turn = 0;

/// The ask currently on the wire, so that abandoning one closes its request.
///
/// Dropping its frames is not enough. The request behind an abandoned ask goes
/// on retrieving and prompting, and every one of those calls holds the
/// deployment's interactive lane — so the next ask waits behind an answer
/// nobody will read. Aborting stops the rounds that have not started yet. The
/// model call already in flight finishes regardless: the lane is held across a
/// dropped reader on purpose, so the worker cannot slip a window in against
/// hardware that is still busy.
let askAbort = null;

function abandonAsk() {
  if (askAbort) {
    askAbort.abort();
    askAbort = null;
    // The progress line goes with it. `clearResults` empties it too, but a
    // search defers that until its hits arrive, and an abandoned ask's "Also
    // looking for: …" left standing under a failure describes work that was
    // cancelled.
    thinking('');
  }
}

/// Empty the results pane. Turn-taking is the caller's: back and capture empty
/// it without putting anything on the wire, and a search empties it only once
/// its own hits have arrived, so that the pane holds the last answer until
/// there is something to put in its place rather than blinking on every pause
/// in typing.
function clearResults() {
  $('results').textContent = '';
  $('back').hidden = true;
  thinking('');
}

async function runSearch() {
  const q = box.value.trim();
  if (!q) {
    hint('');
    turn++;
    abandonAsk();
    clearResults();
    return;
  }
  if (q.length > MAX_QUERY) {
    // No turn taken and nothing cleared: what is on screen stays, and the hint
    // is there to say why it stopped moving.
    hint('Longer than a query, so searching stopped. Ask and Capture still act on it.');
    return;
  }
  hint('');
  if (!(await requirePaired())) return;
  // From here this search owns the results, and an ask still streaming into
  // them does not.
  const mine = ++turn;
  abandonAsk();
  try {
    // `door=extension` is how the judging page tells a query typed while
    // reading from one typed in the web UI. Only this value is honoured
    // server-side; a client cannot claim to be `ask` or `judge`.
    const hits = await engramApi.call(
      '/api/v1/search?door=extension&q=' + encodeURIComponent(q));
    if (mine !== turn) return;
    say('');
    clearResults();
    if (!hits.length) {
      $('results').textContent = 'Nothing.';
      return;
    }
    renderHits(hits, $('results'));
    // Long enough past the 200ms debounce to mean "settled", short enough
    // that the refinement still reads as part of the same answer — the same
    // quiet window the web UI waits.
    clearTimeout(refineTimer);
    refineTimer = setTimeout(() => refineSearch(q, mine, hits), 500);
  } catch (e) {
    if (mine !== turn) return;
    await fail(e);
  }
}

/// The settled query, asked once more with the reranker on. Unattended, so it
/// is held to less than a search: a failure keeps the vector-order list
/// already on screen and says nothing, and an answer that arrives after
/// anything else took the pane — a keystroke, an ask, back — is dropped.
async function refineSearch(q, owner, fast) {
  if (owner !== turn) return;
  let hits;
  try {
    hits = await engramApi.call(
      '/api/v1/search?door=extension&rerank=true&q=' + encodeURIComponent(q));
  } catch (e) {
    return;
  }
  if (owner !== turn) return;
  // The same rows in the same order — a deployment that does not rerank
  // search, or one whose reranker agreed with the vector order. Nothing to
  // repaint, and no reason to rebuild nodes under a reading operator.
  if (
    hits.length === fast.length &&
    hits.every((h, i) => h.artifact_id === fast[i].artifact_id)
  ) return;
  clearResults();
  // The same branch `runSearch` has, for the same reason. A rerank that comes
  // back empty over a list the operator is reading would otherwise clear the
  // node and render nothing into it, leaving a blank pane that says neither
  // "nothing" nor anything else.
  if (!hits.length) {
    $('results').textContent = 'Nothing.';
    return;
  }
  renderHits(hits, $('results'));
}

// An ask runs for as long as the model takes — `infer.ask.timeout_secs`
// defaults to 900 — and the panel stays usable throughout: the operator can
// press back, type something else, and start another.
//
// Back empties the pane itself rather than leaving it to the search it starts.
// That search returns without touching anything when the box holds more than
// `MAX_QUERY`, and back that visibly does nothing is worse than back that
// shows an empty pane.
$('back').addEventListener('click', () => {
  turn++;
  abandonAsk();
  clearResults();
  say('');
  runSearch();
});

$('ask').addEventListener('click', async () => {
  const q = box.value.trim();
  if (!q) return;

  // The keystroke that finished the question armed a search 200ms ago. Left
  // armed, it fires once this answer's nodes are on screen and empties the
  // pane out from under a stream that goes on writing into nodes no longer in
  // the document — an answer that streams into nowhere. Disarmed before the
  // await below rather than after it, or the 200ms can elapse while pairing is
  // read from storage and the search that fires takes the higher turn, whose
  // `abandonAsk` aborts the ask this click just asked for. The refine armed
  // by that search's own hits is disarmed with it — its turn-guard would
  // drop the answer anyway, but there is no reason to put it on the wire.
  clearTimeout(searchTimer);
  clearTimeout(refineTimer);

  if (!(await requirePaired())) return;

  const mine = ++turn;
  abandonAsk();
  const ask = new AbortController();
  askAbort = ask;
  clearResults();
  $('back').hidden = false;
  say('');
  thinking('Retrieving…');

  const answer = document.createElement('p');
  answer.className = 'answer';
  const sources = document.createElement('div');
  $('results').append(answer, sources);

  // Whether the stream said how it ended. A body that simply stops — a proxy
  // cutting a long answer, an intermediary closing at an idle deadline —
  // resolves the read without throwing, and without this the panel would sit on
  // "Retrieving…" over half an answer for as long as it is left open.
  let ended = false;

  try {
    await engramApi.stream('/api/v1/ask/stream', { q }, ask.signal, (name, data) => {
      if (mine !== turn) return;
      switch (name) {
        // How wide the net was and how much of it the model will see. Said out
        // loud because a missing citation is otherwise silent.
        case 'retrieved':
          thinking(data.shown + ' of ' + data.retrieved + ' excerpts kept…');
          break;
        // Round two: the subjects the model said were still missing.
        case 'needs':
          thinking('Also looking for: ' + data.queries.join(', '));
          break;
        case 'citations':
          sources.textContent = '';
          renderHits(data.hits || [], sources);
          break;
        case 'reasoning':
          thinking(data.text);
          break;
        case 'token':
          if (!$('thinking').hidden) thinking('');
          answer.textContent += data.text;
          break;
        // The whole answer, replacing the draft that streamed in. What is
        // finally on screen is what the server stands behind rather than a
        // concatenation this panel assembled.
        case 'done':
          ended = true;
          thinking('');
          answer.textContent = data.answer;
          sources.textContent = '';
          renderHits(data.citations || [], sources);
          say(closing(data));
          break;
        case 'error':
          ended = true;
          thinking('');
          say(data.error, 'error');
          break;
      }
    });
    if (mine === turn && !ended) {
      thinking('');
      say('The connection closed before the answer finished.', 'error');
    }
  } catch (e) {
    // An abandoned ask throws where it was aborted, and that is this panel's
    // own doing rather than news.
    if (mine === turn) await fail(e);
  } finally {
    if (askAbort === ask) askAbort = null;
  }
});

/// What the answer does not say about itself, said beside it.
function closing(d) {
  const notes = [];
  if (d.abstained) notes.push('The base had nothing for this.');
  if (d.dropped) notes.push(d.dropped + ' more were retrieved but did not fit.');
  if (d.truncated) notes.push('The answer stopped at its output ceiling.');
  // Literals the answer carries that no excerpt it was shown does. The one
  // place a model writes something read as fact, so what it wrote alone is
  // named rather than left to be spotted.
  if (d.unsupported && d.unsupported.length) {
    notes.push('In no excerpt: ' + d.unsupported.join(', ') + '.');
  }
  return notes.join(' ');
}

$('capture-text').addEventListener('click', async () => {
  const text = box.value.trim();
  if (!text || !(await requirePaired())) return;
  say('Capturing…');
  try {
    const out = await engramApi.call('/api/v1/corpora', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ text }),
    });
    verdict(out);
    // Cleared, because the text is now somewhere it will be found again and a
    // box still holding it invites capturing it twice.
    box.value = '';
    fit();
    reflectVerbs();
    hint('');
    turn++;
    abandonAsk();
    clearResults();
    await refreshRecent();
  } catch (e) {
    await fail(e);
  }
});

/// What this deployment has lately taken in.
///
/// The point is not browsing; it is that "Captured." stops being a claim the
/// panel makes about itself. A capture that never made it past the queue shows
/// its state here rather than looking exactly like one that did.
const RECENT = 6;
let recentOpen = false;

$('recent-toggle').addEventListener('click', async () => {
  recentOpen = !recentOpen;
  $('recent-list').hidden = !recentOpen;
  $('recent-toggle').textContent = (recentOpen ? '▾' : '▸') + ' Recent';
  if (recentOpen) await refreshRecent();
});

/// A paste has no title, so it is named by its own first line — the same thing
/// a person would call it. Empty when there is no line either, so the caller
/// picks what stands in.
function label(c) {
  if (c.title_hint) return c.title_hint;
  const first = (c.raw_text || '').split('\n').find((l) => l.trim());
  if (!first) return '';
  // The same front trim the server's `stand_in_title` does: a list dash, a
  // heading's hashes, a quote mark are structure rather than subject, and a
  // name that opens with them names the markup. Without this the panel showed
  // `- schneller Schreibzugriff` and `## 3.4.2 FESTE MFT RECORDS` where the
  // CLI, MCP and web doors all showed the words alone.
  const clipped = first.trim().replace(/^[-–—*#>·•|\s]+/, '').trim() || first.trim();
  return clipped.length > 60 ? clipped.slice(0, 60) + '…' : clipped;
}

async function refreshRecent() {
  if (!recentOpen) return;
  if (!(await engramApi.config())) return;
  const list = $('recent-list');
  try {
    const rows = await engramApi.call('/api/v1/corpora?limit=' + RECENT);
    list.textContent = '';
    if (!rows.length) {
      list.textContent = 'Nothing captured yet.';
      return;
    }
    for (const c of rows) {
      const row = document.createElement('div');
      row.className = 'recent-row';

      const link = document.createElement('a');
      link.href = deployment + '/ui/corpora/' + c.id;
      link.target = '_blank';
      link.rel = 'noreferrer noopener';
      // Same floor the hit rows carry: `label` bottoms out at '' for a
      // corpus with no `title_hint` and no usable first line — an image
      // capture, a PDF whose `raw_text` this payload does not carry — and an
      // empty anchor is an invisible, unclickable row.
      link.textContent = label(c) || c.id;

      const state = document.createElement('span');
      state.className = 'state ' + c.status;
      state.textContent = c.status;

      row.append(link, state);
      list.appendChild(row);
    }
  } catch (e) {
    // Not `fail`: the recent list is context, and a deployment that cannot
    // answer this has already said so on whatever action asked for it. A
    // second red line under it would be the same news twice.
    list.textContent = '';
  }
}

// Work handed over from the background script — a context-menu entry or an
// omnibox query.
function doWork(msg) {
  if (!msg) return;
  if (msg.type === 'capture') capture(msg.scope);
  if (msg.type === 'search') {
    box.value = msg.q;
    fit();
    reflectVerbs();
    runSearch();
  }
}

engramShim.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  doWork(msg);
  // Answered, and that is the whole point of answering. A listener that
  // returns without calling this closes the port, which the sender sees as a
  // delivery failure — so the background script would keep work it had
  // already handed over and replay it later against a different tab.
  sendResponse(true);
  return false;
});

// A panel that was closed when the menu entry fired missed the message: it
// had no listener yet. The background script parks anything undelivered, and
// this is the panel collecting it.
engramShim.runtime.sendMessage({ type: 'pending' }).then(doWork).catch(() => {});

fit();
reflectVerbs();
reflectPairing();
