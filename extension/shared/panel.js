const $ = (id) => document.getElementById(id);

function say(message, isError) {
  const el = $('status');
  el.hidden = !message;
  el.textContent = message || '';
  el.classList.toggle('error', !!isError);
}

/// Trailing slashes would double up in every URL built from this.
function cleanOrigin(raw) {
  return raw.trim().replace(/\/+$/, '');
}

async function ensurePaired() {
  if (await engramApi.config()) return true;

  const typed = prompt('engram address (for example https://engram.example)');
  if (!typed) return false;
  try {
    say('Pairing…');
    const origin = await engramPair.pair(cleanOrigin(typed));
    say('Paired with ' + origin + '.');
    return true;
  } catch (e) {
    say(e.message, true);
    return false;
  }
}

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

// Work handed over from the background script — a context-menu entry or an
// omnibox query. Wired in Task 5; the listener registers now so nothing sent
// before it exists is lost.
engramShim.runtime.onMessage.addListener((msg) => {
  if (!msg) return;
  if (msg.type === 'capture') capture(msg.scope);
  if (msg.type === 'search') { $('q').value = msg.q; runSearch(); }
});
