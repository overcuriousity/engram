const $ = (id) => document.getElementById(id);

function say(message, isError) {
  const el = $('status');
  el.hidden = !message;
  el.textContent = message || '';
  el.classList.toggle('error', !!isError);
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

/// Show the pairing face or the paired face, and answer which one it is.
///
/// Called after anything that can change the stored token — including a failed
/// call, because a 401 clears it. Without that the form would stay hidden and
/// there would be no way back to paired.
async function reflectPairing() {
  const cfg = await engramApi.config();
  $('pairing').hidden = !!cfg;
  $('forget').hidden = !cfg;
  return cfg;
}

/// The guard every action starts with. Unpaired is not an error to report and
/// move on from: it is a state with one way out, so it shows the way out.
async function requirePaired() {
  if (await reflectPairing()) return true;
  say('Not paired yet — enter the address engram is reached at.', true);
  $('origin').focus();
  return false;
}

/// Report a failure and re-read the pairing state, because the failure may
/// have been the one that cleared the token.
async function fail(e) {
  say(e.message, true);
  await reflectPairing();
}

$('pair').addEventListener('click', async () => {
  const origin = cleanOrigin($('origin').value);
  if (!origin) {
    say('That is not an address — try https://engram.example.', true);
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
    say(e.message, true);
    return;
  }
  if (!granted) {
    say('Permission for ' + origin + ' was declined.', true);
    return;
  }

  say('Pairing…');
  try {
    await engramPair.pair(origin);
    say('Paired with ' + origin + '.');
  } catch (e) {
    say(e.message, true);
  }
  await reflectPairing();
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
  $('results').textContent = '';
  say('Forgotten. The token is still listed under Housekeeping until revoked.');
  await reflectPairing();
});

/// Ask the active tab for its HTML, or for the selection's.
///
/// Two injections rather than one: the scope is set on the page first, so the
/// second stays an ordinary file rather than a string this script assembles.
async function grab(scope) {
  const [tab] = await engramShim.tabs.query({ active: true, currentWindow: true });
  if (!tab) throw new Error('No page to capture.');

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
}

async function capture(scope) {
  if (!(await requirePaired())) return;
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
      // `scope` travels with the capture because the server cannot tell a
      // highlighted fragment from a page that rendered to almost nothing, and
      // it applies an extraction floor to the second. Three sentences picked
      // out on purpose are not a login wall.
      body: JSON.stringify({ html: page.html, url: page.url, title: page.title, scope }),
    });
    // `ingest` already returns its verdict; the panel shows it as the web UI
    // does rather than inventing a second vocabulary for the same three cases.
    if (out.duplicate) say('Already captured.');
    else if (out.near_duplicate) say('Captured, and parked: it looks like something you already have.');
    else say('Captured.');
  } catch (e) {
    await fail(e);
  }
}

$('capture').addEventListener('click', () => capture('page'));

/// Render hits as rows linking back to the deployment.
///
/// Built with `createElement` and `textContent` rather than assembled as an
/// HTML string: artifact text is whatever a captured page contained, and
/// putting that through `innerHTML` would run it.
async function render(hits, answer) {
  const box = $('results');
  box.textContent = '';

  if (answer) {
    const p = document.createElement('p');
    p.className = 'answer';
    p.textContent = answer;
    box.appendChild(p);
  }

  if (!hits.length) {
    if (!answer) box.textContent = 'Nothing.';
    return;
  }

  const cfg = await engramApi.config();
  for (const h of hits) {
    const el = document.createElement('div');
    el.className = 'hit';

    const title = document.createElement('h3');
    const link = document.createElement('a');
    link.href = cfg.origin + '/ui/artifacts/' + h.artifact_id;
    link.target = '_blank';
    link.rel = 'noreferrer noopener';
    link.textContent = h.title || 'Untitled';
    title.appendChild(link);

    const body = document.createElement('p');
    body.textContent = h.text;

    el.append(title, body);
    box.appendChild(el);
  }
}

let searchTimer;

async function runSearch() {
  const q = $('q').value.trim();
  if (!q) { $('results').textContent = ''; return; }
  if (!(await requirePaired())) return;
  try {
    // `door=extension` is how the judging page tells a query typed while
    // reading from one typed in the web UI. Only this value is honoured
    // server-side; a client cannot claim to be `ask` or `judge`.
    const hits = await engramApi.call(
      '/api/v1/search?door=extension&q=' + encodeURIComponent(q));
    say('');
    await render(hits);
  } catch (e) {
    await fail(e);
  }
}

// Debounced, because this is search-as-you-type and every keystroke would
// otherwise be an embedding call on the deployment.
$('q').addEventListener('input', () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(runSearch, 200);
});

$('ask-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const q = $('ask').value.trim();
  if (!q || !(await requirePaired())) return;
  // `infer.ask.timeout_secs` defaults to 900. A popup would have closed long
  // before this returns; a panel is still here.
  say('Thinking… this can take a while.');
  try {
    const out = await engramApi.call('/api/v1/ask', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ q }),
    });
    // `dropped` counts what was retrieved and left out for budget. Said out
    // loud, because a missing citation is otherwise silent.
    say(out.dropped ? out.dropped + ' more were retrieved but did not fit.' : '');
    await render(out.citations || [], out.answer);
  } catch (err) {
    await fail(err);
  }
});

// Work handed over from the background script — a context-menu entry or an
// omnibox query.
function doWork(msg) {
  if (!msg) return;
  if (msg.type === 'capture') capture(msg.scope);
  if (msg.type === 'search') { $('q').value = msg.q; runSearch(); }
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

reflectPairing();
