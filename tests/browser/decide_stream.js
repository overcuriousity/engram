// The Decide driver, in a real browser, against a fake engram.
//
// Driven by `tests/browser_decide.rs`, `#[ignore]`d as `browser_ask.rs` is:
//
//   cargo test --test browser_decide -- --ignored
//
// As with ask, the count that matters is stream requests: an `EventSource`
// left open reconnects, and a reconnect would read every passage again.
// The fake stream is never ended by the server, so a page that forgets to
// close it leaves `openStreams` above zero.
//
// The page is `workspace.html`'s box, decide row, rail and pane with the
// askama tags resolved; `assets/` is served from the working tree.
//
// Usage: node decide_stream.js <repo-root> <chrome-binary> <scenario>
const http = require('http');
const fs = require('fs');
const os = require('os');
const { spawn } = require('child_process');

const ROOT = process.argv[2];
const CHROME = process.argv[3];
const SCENARIO = process.argv[4] || 'single';

let parkRequests = 0;
let streamRequests = 0;
let openStreams = 0;
const bodies = [];
let report = null;
let child = null;

// Each scenario is the body of a function run at `DOMContentLoaded`, with
// `q`, `press`, `later` and `send` in scope. No backticks: this is a template literal.
const SCENARIOS = {
  single: `
    q('Should I keep it?');
    press();
    var row = document.getElementById('decide-row');
    var btn = document.querySelector('[data-verb="decide"]');
    var radio = row.querySelector('input[name="decide-kind"]:checked');
    var opened = !row.hidden && btn.getAttribute('aria-expanded') === 'true' &&
      document.activeElement === radio;
    press();
    later(4500, function () { send({ rowOpenAfterFirstPress: opened }); });
  `,
};

const HARNESS = `
  window.addEventListener('error', function (e) {
    (window.__errors = window.__errors || []).push(String(e.message));
  });
  document.addEventListener('DOMContentLoaded', function () {
    var progressSeen = [];
    new MutationObserver(function () {
      var t = document.getElementById('ask-progress').textContent;
      if (progressSeen[progressSeen.length - 1] !== t) progressSeen.push(t);
    }).observe(document.getElementById('ask-progress'),
      { childList: true, characterData: true, subtree: true });
    var box = document.querySelector('textarea[name="q"]');
    function q(text) {
      box.value = text;
      box.dispatchEvent(new Event('input', { bubbles: true }));
    }
    function press() { document.querySelector('[data-verb="decide"]').click(); }
    function later(ms, f) { setTimeout(f, ms); }
    function send(extra) {
      var out = {
        resultHtml: document.getElementById('ask-result').innerHTML,
        railRows: document.querySelectorAll('#results .rail-row').length,
        railHead: document.getElementById('rail-head').textContent,
        rerun: !!document.querySelector('#rail-head [data-rerun]'),
        status: document.getElementById('ask-status').textContent,
        problemText: document.getElementById('decide-problem').textContent,
        progressSeen: progressSeen,
        formBusy: document.getElementById('box-form').getAttribute('aria-busy') === 'true',
        errors: window.__errors || []
      };
      for (var k in extra) out[k] = extra[k];
      fetch('/report', { method: 'POST', body: JSON.stringify(out) });
    }
    // After app.js's own DOMContentLoaded handler, which installs the driver.
    setTimeout(function () { ${SCENARIOS[SCENARIO] || ''} }, 50);
  });
`;

const PLUS = '<svg width="16" height="16" viewBox="0 0 24 24" aria-hidden="true"><path d="M5 12h14M12 5v14"/></svg>';
const OPTION = '<span class="decide-option">' +
  '<input class="oc-input" name="decide-option" maxlength="40" aria-label="Option">' +
  '<details><summary>describe</summary><input class="oc-input" name="decide-desc" aria-label="What this option means"></details>' +
  '</span>';
const LEVEL = '<span class="decide-level"><input class="oc-input" name="decide-level" aria-label="Level"></span>';
const ROW = '<fieldset id="decide-row" class="decide-row" hidden><legend class="muted">Answer</legend>' +
  '<label class="chip"><input type="radio" name="decide-kind" value="noul" checked><span>yes / no</span></label>' +
  '<label class="chip"><input type="radio" name="decide-kind" value="choice"><span>options</span></label>' +
  '<label class="chip"><input type="radio" name="decide-kind" value="score"><span>scale</span></label>' +
  '<div id="decide-options" class="decide-list" hidden>' + OPTION + OPTION +
  '<button class="oc-btn oc-btn--icon" type="button" data-decide-add="option" aria-label="Add an option">' + PLUS + '</button></div>' +
  '<div id="decide-levels" class="decide-list" hidden><span class="muted">lowest first</span>' + LEVEL + LEVEL +
  '<button class="oc-btn oc-btn--icon" type="button" data-decide-add="level" aria-label="Add a level">' + PLUS + '</button></div>' +
  '<p id="decide-problem" class="muted" role="status"></p>' +
  '<template id="decide-option-row">' + OPTION + '</template>' +
  '<template id="decide-level-row">' + LEVEL + '</template></fieldset>';

const PAGE = `<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"><title>Decide</title>
<link rel="stylesheet" href="/assets/app.css"><script src="/assets/vendor/htmx.min.js" defer></script>
<script type="module" src="/assets/js/decide.js"></script><script src="/assets/app.js" defer></script></head><body>
<div class="shell"><div class="regions regions-rail-focus-source">
<div class="region-bar">
<form id="box-form" hx-get="/ui/search/results" hx-target="#results" hx-params="q,category">
<div class="box-wrap"><textarea class="box" name="q" rows="1" aria-label="Search, ask, or capture"></textarea></div>
<div class="verbs">
<span id="ask-verb-slot"><button class="oc-btn" type="button" data-verb="ask" disabled>Ask</button></span>
<span id="decide-verb-slot"><button class="oc-btn" type="button" data-verb="decide" disabled aria-expanded="false" aria-controls="decide-row">Decide</button></span>
<button class="oc-btn" type="button" data-verb="capture" disabled>Capture</button>
<button id="ask-stop" class="oc-btn" type="button" hidden>Stop</button>
<span id="search-spinner" class="spinner">searching…</span><span class="verbs-spacer"></span></div>
${ROW}
</form></div>
<div id="rail" class="region-rail rail"><div id="rail-head" class="rail-act" aria-live="polite"></div>
<div id="results" role="listbox" aria-label="Results"></div></div>
<div id="pane" class="region-focus pane"><p id="ask-progress" class="muted" hidden></p>
<details id="ask-reasoning-box" class="reasoning-box" hidden><summary>Reasoning</summary><div id="ask-reasoning" class="reasoning"></div></details>
<pre id="ask-live" class="answer-live" aria-live="polite" hidden></pre>
<p id="ask-status" class="oc-sr-only" role="status"></p>
<div id="ask-result"></div><div id="pane-content"></div></div>
</div></div>
<template id="oc-notice-danger"><div class="oc-notice oc-notice--danger" role="status"><span class="oc-notice__text"></span></div></template>
<img src="/hang" alt="" style="display:none">
<script>${HARNESS}</script></body></html>`;

// Shaped like `_decide_rail_row.html`, `_decision.html` and `_decide_rail.html`.
function railRow(n, title, pick) {
  return '<div class="rail-row"><a class="rail-item" role="option" aria-selected="false" href="/ui/artifacts/a' + n + '">' +
    '<div class="rail-head"><span class="rail-rank mono">[' + n + ']</span><span class="rail-title">' + title + '</span>' +
    '<span class="rail-pick mono">' + pick + '</span></div></a></div>';
}
function bar(label, weight, prior, picked) {
  return '<div class="bar' + (picked ? ' picked' : '') + '"><span class="bar-label" aria-hidden="true">' + label + '</span>' +
    '<svg class="bar-track" viewBox="0 0 100 4" preserveAspectRatio="none" aria-hidden="true"><rect class="bar-fill" width="' + weight +
    '" height="4"/><rect class="bar-prior" x="' + prior + '" width="1" height="4"/></svg>' +
    '<span class="bar-weight mono" aria-hidden="true">' + weight + '</span>' +
    '<span class="oc-sr-only">' + label + ': ' + weight + ' of 100, ' + prior + ' without notes</span></div>';
}
function decision(q, labels) {
  const bars = labels.map((l, i) => bar(l, i === 0 ? 70 : Math.floor(30 / (labels.length - 1)), 50, i === 0)).join('');
  return '<section class="answer decision"><h2 class="oc-text-section">' + q + '</h2><div class="bars">' + bars + '</div>' +
    '<p class="muted bar-legend">without notes</p><p class="standing">The notes lean one way.</p>' +
    '<div id="decide-verdict" class="oc-row ask-verdict"><span class="muted">Was this right?</span>' +
    '<button class="oc-btn" type="button" hx-post="/ui/decide/ev1/verdict" hx-vals=\'{"verdict":"right"}\' ' +
    'hx-target="#decide-verdict" hx-swap="outerHTML">Right</button></div></section>';
}

function stream(req, res, n) {
  streamRequests++;
  openStreams++;
  req.on('close', () => openStreams--);
  res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
  const send = (name, data) =>
    res.write('event: ' + name + '\ndata: ' + JSON.stringify(data) + '\n\n');
  const at = (ms, f) => setTimeout(() => { if (!res.writableEnded) f(); }, ms);
  const labels = ['yes', 'no'];
  at(0, () => send('retrieved', { passages: 3 }));
  [true, true, false].forEach((bearing, i) =>
    at(60 * (i + 1), () => send('read', {
      rank: i + 1, of: 3, bearing, row: railRow(i + 1, 'note ' + (i + 1) + ' of ' + n, bearing ? 'yes' : 'does not bear')
    })));
  // Left open: closing it is the page's job.
  at(300, () => send('done', {
    event_id: 'ev1',
    html: decision('Should I keep it? (' + n + ')', labels),
    rail: railRow(1, 'note 1', 'yes') + railRow(2, 'note 2', 'yes') + '<p class="muted rail-rest">1 more did not bear on it</p>',
    rail_head: 'Decided from 2 of 3'
  }));
}

const server = http.createServer((req, res) => {
  const asset = { '/assets/app.js': 'text/javascript', '/assets/js/decide.js': 'text/javascript',
    '/assets/vendor/htmx.min.js': 'text/javascript', '/assets/app.css': 'text/css' }[req.url];
  if (asset) {
    res.writeHead(200, { 'content-type': asset });
    return res.end(fs.readFileSync(ROOT + req.url));
  }
  if (req.url === '/ui' && req.method === 'GET') {
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
    return res.end(PAGE);
  }
  // Never answered, so `load` never fires and `--dump-dom` does not exit early.
  if (req.url === '/hang') return;
  if (req.url === '/ui/decide' && req.method === 'POST') {
    const n = ++parkRequests;
    let body = '';
    req.on('data', (c) => (body += c));
    return req.on('end', () => {
      bodies.push({ type: req.headers['content-type'], body });
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ id: 'handoff-' + n }));
    });
  }
  const m = req.url.match(/^\/ui\/decide\/handoff-(\d+)\/stream$/);
  if (m) return stream(req, res, Number(m[1]));
  if (req.url === '/report') {
    let body = '';
    req.on('data', (c) => (body += c));
    return req.on('end', () => {
      report = JSON.parse(body);
      res.writeHead(204);
      res.end();
      // A beat for a stream the page closed to reach the server as a closed socket.
      setTimeout(finish, 200);
    });
  }
  res.writeHead(404);
  res.end('no');
});

function finish() {
  if (child) child.kill();
  server.close();
  console.log(JSON.stringify({ scenario: SCENARIO, parkRequests, streamRequests, openStreams, bodies, report }));
  process.exit(0);
}

server.listen(0, '127.0.0.1', () => {
  const port = server.address().port;
  child = spawn(CHROME, [
    '--no-sandbox', '--disable-gpu', '--dump-dom',
    '--window-size=1280,900',
    '--user-data-dir=' + fs.mkdtempSync(os.tmpdir() + '/engram-chrome-'),
    'http:' + '//127.0.0.1:' + port + '/ui'
  ], { stdio: 'ignore' });
  setTimeout(() => {
    console.log(JSON.stringify({ scenario: SCENARIO, error: 'the page never reported back' }));
    finish();
  }, 30000);
});
