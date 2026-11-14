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
// Opens and closes of streams, in the order the server saw them.
const events = [];
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
  // The second press lands while the first stream is reading. The button
  // disables itself while busy, which is the page's own guard and not the one
  // under test, so it is re-enabled here.
  double: `
    q('Should I keep it?');
    press();
    press();
    later(150, function () {
      document.querySelector('[data-verb="decide"]').disabled = false;
      press();
    });
    later(4500, function () { send({}); });
  `,
  invalid: `
    q('Which one?');
    press();
    kind('choice');
    document.querySelector('[name="decide-option"]').value = 'keep';
    press();
    later(500, function () { send({}); });
  `,
  // Enter in the row presses Decide: a staged file disables it, an empty box says why.
  enter: `
    q('Should I keep it?');
    press();
    var enter = function () {
      document.getElementById('decide-row').querySelector('input[name="decide-kind"]:checked')
        .dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }));
    };
    document.getElementById('staged').hidden = false;
    q('Should I keep it?');
    enter();
    var staged = document.getElementById('decide-problem').textContent;
    document.getElementById('staged').hidden = true;
    q('');
    enter();
    later(500, function () { send({ problemWhenStaged: staged }); });
  `,
  // 390 px: eight options of forty characters, then a result naming them with eight rail rows.
  narrow: `
    q('Which of these eight should I keep for the winter?');
    press();
    kind('choice');
    var add = document.querySelector('[data-decide-add="option"]');
    for (var i = 0; i < 6; i++) add.click();
    var names = document.querySelectorAll('[name="decide-option"]');
    for (var j = 0; j < names.length; j++) {
      names[j].value = ('option ' + (j + 1) + ' with a name forty characters long').slice(0, 40);
    }
    var row = measure();
    var addDisabled = add.disabled;
    press();
    later(4500, function () {
      send({ optionCount: names.length, addDisabled: addDisabled, rowOpen: row, result: measure(),
        decideHeight: document.querySelector('[data-verb="decide"]').getBoundingClientRect().height });
    });
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
    function kind(v) {
      var r = document.querySelector('input[name="decide-kind"][value="' + v + '"]');
      r.click();
    }
    // Whether the document scrolls sideways, and every touch target in the verbs and the row under 44 px.
    function measure() {
      var de = document.documentElement;
      var small = [];
      var targets = document.querySelectorAll('.verbs .oc-btn, #decide-row input, #decide-row button, #decide-row summary');
      for (var i = 0; i < targets.length; i++) {
        var el = targets[i];
        // A radio is drawn by its chip; the chip is what the thumb hits.
        if (el.type === 'radio') el = el.closest('label');
        var h = el.getBoundingClientRect().height;
        if (el.offsetParent !== null && h < 44) small.push((el.name || el.getAttribute('aria-label') || el.textContent.trim()) + ' ' + h);
      }
      return { scrollWidth: de.scrollWidth, clientWidth: de.clientWidth, small: small };
    }
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
<div id="staged" class="staged" hidden></div>
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

// What was asked, so the result names what the page sent.
function labelsOf(n) {
  const v = (JSON.parse(bodies[n - 1].body).questions || {}).verdict;
  if (!v) return ['yes', 'no'];
  return v.type === 'choice' ? Object.keys(v.criteria) : v.criteria;
}

function stream(req, res, n) {
  streamRequests++;
  openStreams++;
  events.push('open ' + n);
  req.on('close', () => { openStreams--; events.push('close ' + n); });
  res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
  const send = (name, data) =>
    res.write('event: ' + name + '\ndata: ' + JSON.stringify(data) + '\n\n');
  // The superseded one is slow, so its `done` would land after the second began.
  const pace = SCENARIO === 'double' && n === 1 ? 3 : 1;
  const at = (ms, f) => setTimeout(() => { if (!res.writableEnded) f(); }, ms * pace);
  const labels = labelsOf(n);
  at(0, () => send('retrieved', { passages: 3 }));
  [true, true, false].forEach((bearing, i) =>
    at(60 * (i + 1), () => send('read', {
      rank: i + 1, of: 3, bearing, row: railRow(i + 1, 'note ' + (i + 1) + ' of ' + n, bearing ? labels[0] : 'does not bear')
    })));
  const rows = SCENARIO === 'narrow' ? 8 : 2;
  let rail = '';
  for (let i = 1; i <= rows; i++) {
    const title = SCENARIO === 'narrow' ? 'a note whose title runs well past the width of any phone, ' + i : 'note ' + i;
    rail += railRow(i, title + ' of ' + n, labels[(i - 1) % labels.length]);
  }
  // Left open: closing it is the page's job.
  at(300, () => send('done', {
    event_id: 'ev' + n,
    html: decision('Should I keep it? (' + n + ')', labels),
    rail: rail + '<p class="muted rail-rest">1 more did not bear on it</p>',
    rail_head: 'Decided from ' + rows + ' of ' + (rows + 1)
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
  console.log(JSON.stringify({ scenario: SCENARIO, parkRequests, streamRequests, openStreams, events, bodies, report }));
  process.exit(0);
}

// Real key presses need the DevTools protocol: a synthetic Tab moves no focus.
const KEYS = {
  Enter: { code: 'Enter', vk: 13, text: '\r' }, Tab: { code: 'Tab', vk: 9 },
  Escape: { code: 'Escape', vk: 27 }, ArrowRight: { code: 'ArrowRight', vk: 39 }
};
const ALT = 1;
const SHIFT = 8;

async function keyboard(url) {
  const ws = await new Promise((resolve) => {
    let buf = '';
    child.stderr.on('data', (d) => {
      const m = (buf += d).match(/DevTools listening on ws:\/\/([^/]+)\//);
      if (m) resolve(m[1]);
    });
  });
  const pages = await (await fetch('http:' + '//' + ws + '/json/list')).json();
  const sock = new WebSocket(pages.find((t) => t.type === 'page').webSocketDebuggerUrl);
  await new Promise((r) => sock.addEventListener('open', r));
  let id = 0;
  const pending = new Map();
  sock.addEventListener('message', (e) => {
    const m = JSON.parse(e.data);
    if (pending.has(m.id)) { pending.get(m.id)(m.result); pending.delete(m.id); }
  });
  const cmd = (method, params) => new Promise((r) => {
    pending.set(++id, r);
    sock.send(JSON.stringify({ id, method, params: params || {} }));
  });
  const js = async (expr) => (await cmd('Runtime.evaluate', { expression: expr, returnByValue: true })).result.value;
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const key = async (name, modifiers) => {
    const k = KEYS[name];
    const base = { key: name, code: k.code, windowsVirtualKeyCode: k.vk, modifiers: modifiers || 0 };
    await cmd('Input.dispatchKeyEvent', Object.assign({ type: 'keyDown', text: k.text }, base));
    await cmd('Input.dispatchKeyEvent', Object.assign({ type: 'keyUp' }, base));
    await wait(30);
  };
  const type = (text) => cmd('Input.insertText', { text });
  // What has focus, by the name a test can read.
  const focus = () => js(`(function (el) {
    return el.getAttribute('data-decide-add') ? 'add-' + el.getAttribute('data-decide-add')
      : el.tagName === 'SUMMARY' ? 'summary' : el.name || el.tagName.toLowerCase();
  })(document.activeElement)`);
  const rowOpen = () => js(`!document.getElementById('decide-row').hidden`);

  await cmd('Page.navigate', { url });
  while (!(await js(`!!window.engramDecide && document.readyState !== 'loading'`))) await wait(50);
  await wait(100);
  const out = {};
  await js(`document.querySelector('textarea[name="q"]').focus()`);
  await type('Which one?');
  await key('Enter', ALT);
  out.openedByChord = await rowOpen();
  out.focusAfterOpen = await focus();
  await key('ArrowRight');
  out.kindAfterArrow = await js(`document.querySelector('input[name="decide-kind"]:checked').value`);
  out.optionsShown = await js(`!document.getElementById('decide-options').hidden`);
  out.tabs = [];
  const names = ['keep', 'switch'];
  for (let i = 0; i < 5; i++) {
    await key('Tab');
    const f = await focus();
    out.tabs.push(f);
    if (f === 'decide-option') await type(names.shift() || '');
  }
  // Back from the add button to the second name, and run it from there.
  await key('Tab', SHIFT);
  await key('Tab', SHIFT);
  out.focusBeforeEnter = await focus();
  await key('Enter');
  await wait(1000);
  out.statusAfterEnter = await js(`document.getElementById('ask-status').textContent`);
  await key('Escape');
  out.closedByEscape = !(await rowOpen());
  out.focusAfterEscape = await focus();
  await key('Enter', ALT);
  out.reopened = await rowOpen();
  out.optionsAfterReopen = await js(`Array.prototype.map.call(
    document.querySelectorAll('[name="decide-option"]'), function (i) { return i.value; })`);
  out.optionsShownAfterReopen = await js(`!document.getElementById('decide-options').hidden`);
  out.errors = (await js(`window.__errors || []`));
  report = out;
  sock.close();
  setTimeout(finish, 200);
}

server.listen(0, '127.0.0.1', () => {
  const port = server.address().port;
  const url = 'http:' + '//127.0.0.1:' + port + '/ui';
  const profile = '--user-data-dir=' + fs.mkdtempSync(os.tmpdir() + '/engram-chrome-');
  const width = '--window-size=' + (SCENARIO === 'narrow' ? '390,900' : '1280,900');
  if (SCENARIO === 'keyboard') {
    child = spawn(CHROME, ['--no-sandbox', '--disable-gpu', '--remote-debugging-port=0', width, profile, 'about:blank'],
      { stdio: ['ignore', 'ignore', 'pipe'] });
    keyboard(url).catch((e) => {
      console.log(JSON.stringify({ scenario: SCENARIO, error: String(e) }));
      finish();
    });
  } else {
    child = spawn(CHROME, ['--no-sandbox', '--disable-gpu', '--dump-dom', width, profile, url], { stdio: 'ignore' });
  }
  setTimeout(() => {
    console.log(JSON.stringify({ scenario: SCENARIO, error: 'the page never reported back' }));
    finish();
  }, 30000);
});
