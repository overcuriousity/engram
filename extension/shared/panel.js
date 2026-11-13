const $ = (id) => document.getElementById(id);

function say(message, kind) {
  const el = $('status');
  el.hidden = !message;
  el.textContent = message || '';
  el.classList.toggle('error', kind === 'error');
  el.classList.toggle('good', kind === 'good');
}

/// Replaced continually and gone once the answer is complete, unlike `say`.
function thinking(message) {
  const el = $('thinking');
  el.hidden = !message;
  el.textContent = message || '';
}

/// A host-permission match pattern admits only scheme, host and port; a bare host is read as `https`.
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

/// Cached so that rendering a row is synchronous and two renders cannot interleave.
let deployment = null;

/// Called after a failed call too, since a 401 clears the token.
async function reflectPairing() {
  const cfg = await engramApi.config();
  deployment = cfg ? cfg.origin : null;
  $('pairing').hidden = !!cfg;
  $('forget').hidden = !cfg;
  $('recent').hidden = !cfg;
  return cfg;
}

async function requirePaired() {
  if (await reflectPairing()) return true;
  say('Not paired. Enter the engram address.', 'error');
  $('origin').focus();
  return false;
}

/// The failure may have cleared the token.
async function fail(e) {
  thinking('');
  say(e.message, 'error');
  await reflectPairing();
}

$('pair').addEventListener('click', async () => {
  const origin = cleanOrigin($('origin').value);
  if (!origin) {
    say('Not an address; enter the engram host name.', 'error');
    return;
  }

  // First, before any `await`: `permissions.request` needs the click's user gesture.
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

// Forgetting hands back the host permission too; the token lives on server-side until revoked.
$('forget').addEventListener('click', async () => {
  const cfg = await engramApi.config();
  await engramApi.forget();
  if (cfg) {
    await engramShim.permissions.remove({ origins: [cfg.origin + '/*'] }).catch(() => {});
  }
  // Asked for again the next time Capture needs it.
  await engramShim.permissions.remove({ origins: ALL_HOSTS }).catch(() => {});
  hasAllHosts = false;
  clearResults();
  $('recent-list').textContent = '';
  say('Forgotten; revoke the token in Settings.');
  await reflectPairing();
});

/// `activeTab` covers only the tab the panel was opened from, and a side panel survives tab switches.
/// Tracked so the click handler can ask for host permission before awaiting anything.
const ALL_HOSTS = ['https', 'http'].map((scheme) => scheme + '://*/*');
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

// Only the id, which needs no permission; returning to the original tab restores the grant.
engramShim.tabs.onActivated.addListener((info) => {
  tabChanged = info.tabId !== openedForTabId;
});

/// Two injections: the scope is set first, so the second stays an ordinary file.
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
    // No grant, or a page no extension may read; the browser's own message says nothing useful.
    throw new Error(
      'Cannot read this tab; right-click it and choose “Capture this page”.');
  }
}

/// The web UI's vocabulary for the same three cases.
function verdict(out) {
  if (out.duplicate) say('Already captured.');
  else if (out.link_unread) say('Link saved; reading again later (' + out.link_unread + ').');
  else if (out.near_duplicate) say('Captured; similar to a stored source.');
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
      // The server applies an extraction floor to pages but not to a highlighted fragment.
      body: JSON.stringify({ html: page.html, url: page.url, title: page.title, scope }),
    });
    verdict(out);
    await refreshRecent();
  } catch (e) {
    await fail(e);
  }
}

$('capture-page').addEventListener('click', async () => {
  // Asked while the click is still a user gesture; a refusal is not fatal.
  if (tabChanged && !hasAllHosts) {
    try {
      hasAllHosts = await engramShim.permissions.request({ origins: ALL_HOSTS });
    } catch (e) {
      // Some browsers throw rather than answering false.
    }
  }
  await capture('page');
});

/// Typing searches; the buttons ask and capture. Never decided by the text's length or shape.
const box = $('box');

/// Past this the live search stops, so a pasted chapter is not embedded on every pause. Generous on purpose.
const MAX_QUERY = 2000;

/// Past the cap the box scrolls rather than pushing the results off the panel.
function fit() {
  box.style.height = 'auto';
  const max = Math.round(window.innerHeight * 0.4);
  const wanted = box.scrollHeight;
  box.style.height = Math.min(wanted, max) + 'px';
  box.style.overflowY = wanted > max ? 'auto' : 'hidden';
}

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
// The settled pass, as in the web UI: the same query once more with `rerank=true`.
let refineTimer;

box.addEventListener('input', () => {
  fit();
  reflectVerbs();
  clearTimeout(searchTimer);
  clearTimeout(refineTimer);
  // Every keystroke would otherwise be an embedding call.
  searchTimer = setTimeout(runSearch, 200);
});

/// `createElement`/`textContent`, never `innerHTML`: artifact text is whatever a captured page contained.
function renderHits(hits, into) {
  for (const h of hits) {
    const el = document.createElement('div');
    el.className = 'hit';

    const title = document.createElement('h3');
    const link = document.createElement('a');
    link.href = deployment + '/ui/artifacts/' + h.artifact_id;
    link.target = '_blank';
    link.rel = 'noreferrer noopener';
    // An untitled hit has no heading and no note title; its opening is its name.
    link.textContent = h.title || label({ raw_text: h.text }) || h.artifact_id;
    title.appendChild(link);

    const body = document.createElement('p');
    body.textContent = h.text;

    el.append(title, body);
    into.appendChild(el);
  }
}

/// Search and ask overlap; a response for a turn that is no longer current is dropped.
let turn = 0;

/// Aborting frees the deployment's interactive lane for the next ask.
let askAbort = null;

function abandonAsk() {
  if (askAbort) {
    askAbort.abort();
    askAbort = null;
    // A search defers `clearResults`, so an abandoned ask's progress line would otherwise stand.
    thinking('');
  }
}

/// A search empties the pane only once its hits arrive, so it does not blink on every pause.
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
    // No turn taken: what is on screen stays, and the hint says why.
    hint('Too long to search; Ask or Capture it.');
    return;
  }
  hint('');
  if (!(await requirePaired())) return;
  const mine = ++turn;
  abandonAsk();
  try {
    // Only `door=extension` is honoured server-side; a client cannot claim `ask` or `judge`.
    const hits = (await engramApi.call(
      '/api/v1/search?door=extension&q=' + encodeURIComponent(q))).items;
    if (mine !== turn) return;
    say('');
    clearResults();
    if (!hits.length) {
      $('results').textContent = 'Nothing.';
      return;
    }
    renderHits(hits, $('results'));
    // The same settle window the web UI waits.
    clearTimeout(refineTimer);
    refineTimer = setTimeout(() => refineSearch(q, mine, hits), 500);
  } catch (e) {
    if (mine !== turn) return;
    await fail(e);
  }
}

/// Unattended: a failure keeps the list silently, and a late answer is dropped.
async function refineSearch(q, owner, fast) {
  if (owner !== turn) return;
  let hits;
  try {
    hits = (await engramApi.call(
      '/api/v1/search?door=extension&rerank=true&q=' + encodeURIComponent(q))).items;
  } catch (e) {
    return;
  }
  if (owner !== turn) return;
  // Same order: nothing to repaint under a reading operator.
  if (
    hits.length === fast.length &&
    hits.every((h, i) => h.artifact_id === fast[i].artifact_id)
  ) return;
  clearResults();
  // As in `runSearch`: an empty rerank must not blank the pane.
  if (!hits.length) {
    $('results').textContent = 'Nothing.';
    return;
  }
  renderHits(hits, $('results'));
}

// Back empties the pane itself: the search it starts does nothing past `MAX_QUERY`.
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

  // Disarmed before the await, or the armed search takes the higher turn and aborts this ask.
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

  // A body that simply stops (a proxy cut) resolves without throwing.
  let ended = false;

  try {
    await engramApi.stream('/api/v1/ask/stream', { q }, ask.signal, (name, data) => {
      if (mine !== turn) return;
      switch (name) {
        // Said aloud, because a missing citation is otherwise silent.
        case 'retrieved':
          thinking(data.shown + ' of ' + data.retrieved + ' excerpts kept…');
          break;
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
        // What the server stands behind, not the streamed draft.
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
      say('Connection closed before the answer finished.', 'error');
    }
  } catch (e) {
    // An abort is this panel's own doing, not news.
    if (mine === turn) await fail(e);
  } finally {
    if (askAbort === ask) askAbort = null;
  }
});

function closing(d) {
  const notes = [];
  if (d.abstained) notes.push('The base had nothing for this.');
  if (d.dropped) notes.push(d.dropped + ' more were retrieved but did not fit.');
  if (d.truncated) notes.push('The answer stopped at its output ceiling.');
  // Literals no excerpt carries are named, not left to be spotted.
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
    // Cleared, so the text is not captured twice.
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

/// A capture stuck in the queue shows its state here.
const RECENT = 6;
let recentOpen = false;

$('recent-toggle').addEventListener('click', async () => {
  recentOpen = !recentOpen;
  $('recent-list').hidden = !recentOpen;
  $('recent-toggle').textContent = (recentOpen ? '▾' : '▸') + ' Recent';
  if (recentOpen) await refreshRecent();
});

/// Empty when there is no line, so the caller picks the stand-in.
function label(c) {
  if (c.title_hint) return c.title_hint;
  const first = (c.raw_text || '').split('\n').find((l) => l.trim());
  if (!first) return '';
  // The same front trim as the server's `stand_in_title`: list dashes, hashes and quote marks are markup.
  const clipped = first.trim().replace(/^[-–—*#>·•|\s]+/, '').trim() || first.trim();
  return clipped.length > 60 ? clipped.slice(0, 60) + '…' : clipped;
}

async function refreshRecent() {
  if (!recentOpen) return;
  if (!(await engramApi.config())) return;
  const list = $('recent-list');
  try {
    // One page only; `next` is not followed.
    const rows = (await engramApi.call('/api/v1/corpora?limit=' + RECENT)).items;
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
      // The server's label, as every other door shows; the id keeps the anchor clickable.
      link.textContent = c.label || c.id;

      const state = document.createElement('span');
      state.className = 'state ' + c.status;
      state.textContent = c.status;

      row.append(link, state);
      list.appendChild(row);
    }
  } catch (e) {
    // Not `fail`: the action that failed has already said so.
    list.textContent = '';
  }
}

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
  // Without a response the sender sees a delivery failure and would replay the work later.
  sendResponse(true);
  return false;
});

// Collects work parked while the panel had no listener.
engramShim.runtime.sendMessage({ type: 'pending' }).then(doWork).catch(() => {});

fit();
reflectVerbs();
reflectPairing();
