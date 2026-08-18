// The ask page's stream driver, in a real browser, against a fake engram.
//
// Driven by `tests/browser_ask.rs`, which is `#[ignore]`d because this needs
// node and a headless Chrome that `cargo test` cannot assume. Run it with:
//
//   cargo test --test browser_ask -- --ignored
//
// What it exists to catch is a deletion that no Rust test can see and no
// browser reports: an `EventSource` that is never closed. The browser
// reconnects to a stream the server has closed, and the reconnect is a fresh
// GET that asks the question again — a second model call nobody requested, and
// on a paid endpoint a second bill. So the assertion this file is built around
// is a *count*: how many times the fake server was asked to stream.
//
// The page below is the body of `src/web/templates/ask.html` with its askama
// tags resolved; `assets/app.js` and `assets/app.css` are served verbatim from
// the working tree, so the code under test is the code that ships.
//
// Usage: node ask_stream.js <repo-root> <chrome-binary> <single|double>
const http = require('http');
const fs = require('fs');
const { spawn } = require('child_process');

const ROOT = process.argv[2];
const CHROME = process.argv[3];
const SCENARIO = process.argv[4] || 'single';

// `double` submits twice while the first POST is still in flight, which is a
// double-tap on the Ask button. The delay is what makes the race reachable at
// all: without it the first stream is already open and `stop()` closes it.
const PARK_DELAY = SCENARIO === 'double' ? 150 : 0;

let parkRequests = 0;
let streamRequests = 0;
let verdictRequests = 0;
let report = null;
let child = null;

const HARNESS = `
  document.addEventListener('DOMContentLoaded', function () {
    var ask = function () {
      document.querySelector('input[name="q"]').value = 'what is alpha';
      document.getElementById('ask-form').requestSubmit();
    };
    setTimeout(ask, 50);
    ${SCENARIO === 'double' ? 'setTimeout(ask, 70);' : ''}
    // Well past the end of the stream: Chrome waits about three seconds before
    // reconnecting to a stream that closed, so a shorter wait would call an
    // unclosed EventSource clean.
    setTimeout(function () {
      var link = document.querySelector('a.cite[href="#cite-2"]');
      if (link) link.click();
      // The verdict bar arrived inside the done fragment through innerHTML,
      // which htmx does not watch. A click here is the proof the driver told
      // htmx about it: the fake server counts the POST it produces.
      var right = document.querySelector('#ask-verdict button');
      if (right) right.click();
      setTimeout(function () {
        fetch('/report', { method: 'POST', body: JSON.stringify({
          result: document.getElementById('ask-result').innerHTML,
          liveText: document.getElementById('ask-live').textContent,
          liveHidden: document.getElementById('ask-live').hidden,
          reasoningText: document.getElementById('ask-reasoning').textContent,
          reasoningHidden: document.getElementById('ask-reasoning').hidden,
          statusText: document.getElementById('ask-status').textContent,
          railIds: Array.prototype.map.call(
            document.querySelectorAll('#ask-rail [id]'), function (e) { return e.id; }),
          activeId: (document.querySelector('.rail-active') || {}).id || null,
          formAsking: document.getElementById('ask-form').classList.contains('asking'),
          errors: window.__errors || []
        })});
      }, 300);
    }, 6000);
  });
  window.addEventListener('error', function (e) {
    (window.__errors = window.__errors || []).push(String(e.message));
  });
`;

const PAGE = `<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Ask</title>
<link rel="stylesheet" href="/assets/app.css"><script src="/assets/htmx.min.js" defer></script><script src="/assets/app.js" defer></script></head><body>
<form id="ask-form" class="row"><input class="input" name="q" value="" placeholder="Ask a question…">
<button class="btn btn-accent" type="submit">Ask</button>
<span id="ask-spinner" class="spinner">thinking…</span></form>
<div id="ask-reasoning" class="reasoning" hidden></div>
<p id="ask-progress" class="ask-progress" hidden></p>
<pre id="ask-live" class="answer-live" aria-live="polite" hidden></pre>
<div id="ask-rail" class="rail" role="listbox" aria-label="Excerpts"></div>
<p id="ask-status" class="sr-only" role="status"></p>
<div id="ask-result"></div>
<img src="/hang" alt="" style="display:none">
<script>${HARNESS}</script></body></html>`;

// Shaped like `rail_fragment` and `answer_fragment` respectively. Only the
// shape matters here: what the ids and hrefs actually agree about is pinned by
// `every_citation_link_in_the_answer_points_at_an_excerpt_the_rail_carries`.
const RAIL =
  '<div class="rail-row"><a class="rail-item" id="cite-1"><span class="rail-title">alpha</span></a></div>' +
  '<div class="rail-row"><a class="rail-item" id="cite-2"><span class="rail-title">bravo</span></a></div>';
const DONE =
  '<div class="card"><div class="md"><p>alpha <a class="cite" href="#cite-1">[1]</a> and ' +
  'bravo <a class="cite" href="#cite-2">[2]</a></p></div></div>' +
  '<div id="ask-verdict"><button hx-post="/ui/ask/ev1/verdict" hx-vals=\'{"verdict":"right"}\' ' +
  'hx-target="#ask-verdict" hx-swap="outerHTML">Right</button></div>';

const server = http.createServer((req, res) => {
  if (req.url === '/ui/ask' && req.method === 'GET') {
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
    return res.end(PAGE);
  }
  // Never answered. An unfinished subresource keeps the browser from firing
  // `load` and exiting before the stream this test is about has run.
  if (req.url === '/hang') return;
  if (req.url === '/assets/app.js') {
    res.writeHead(200, { 'content-type': 'text/javascript' });
    return res.end(fs.readFileSync(ROOT + '/assets/app.js'));
  }
  if (req.url === '/assets/htmx.min.js') {
    res.writeHead(200, { 'content-type': 'text/javascript' });
    return res.end(fs.readFileSync(ROOT + '/assets/htmx.min.js'));
  }
  if (req.url === '/ui/ask/ev1/verdict' && req.method === 'POST') {
    verdictRequests++;
    res.writeHead(200, { 'content-type': 'text/html' });
    return res.end('<div id="ask-verdict">judged</div>');
  }
  if (req.url === '/assets/app.css') {
    res.writeHead(200, { 'content-type': 'text/css' });
    return res.end(fs.readFileSync(ROOT + '/assets/app.css'));
  }
  if (req.url === '/ui/ask' && req.method === 'POST') {
    const n = ++parkRequests;
    let body = '';
    req.on('data', (c) => (body += c));
    return req.on('end', () => {
      setTimeout(() => {
        res.writeHead(200, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ id: 'handoff-' + n }));
      }, PARK_DELAY);
    });
  }
  if (req.url.startsWith('/ui/ask/') && req.url.endsWith('/stream')) {
    streamRequests++;
    res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
    const send = (name, data) =>
      res.write('event: ' + name + '\ndata: ' + JSON.stringify(data) + '\n\n');
    send('retrieved', { round: 1, shown: 2, dropped: 0, cliff_at: null });
    send('citations', { rail: RAIL });
    setTimeout(() => send('reasoning', { text: 'weighing alpha\nagainst bravo' }), 60);
    setTimeout(() => send('token', { text: 'alpha ' }), 120);
    setTimeout(() => send('token', { text: '[1] and\nbravo [2]' }), 180);
    setTimeout(() => {
      send('done', { event_id: 'ev1', html: DONE });
      res.end();
    }, 240);
    return;
  }
  if (req.url === '/report') {
    let body = '';
    req.on('data', (c) => (body += c));
    return req.on('end', () => {
      report = JSON.parse(body);
      res.writeHead(204);
      res.end();
      finish();
    });
  }
  res.writeHead(404);
  res.end('no');
});

function finish() {
  if (child) child.kill();
  server.close();
  console.log(JSON.stringify({ scenario: SCENARIO, parkRequests, streamRequests, verdictRequests, report }));
  process.exit(0);
}

server.listen(0, '127.0.0.1', () => {
  const port = server.address().port;
  child = spawn(
    CHROME,
    [
      '--no-sandbox',
      '--disable-gpu',
      '--dump-dom',
      '--user-data-dir=' + fs.mkdtempSync(require('os').tmpdir() + '/engram-chrome-'),
      'http://127.0.0.1:' + port + '/ui/ask'
    ],
    { stdio: 'ignore' }
  );
  setTimeout(() => {
    console.log(JSON.stringify({ scenario: SCENARIO, error: 'the page never reported back' }));
    finish();
  }, 30000);
});
