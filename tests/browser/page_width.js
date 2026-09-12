// Every page, in a real browser — does anything stick out, and is anything
// crushed?
//
// Driven by `web::tests::no_page_runs_off_the_side_of_a_phone`, which is
// `#[ignore]`d because this needs node and a headless Chrome that `cargo test`
// cannot assume. Run it with:
//
//   cargo test --lib -- --ignored no_page_runs_off
//
// What it exists to catch is a thing only a browser can measure. A row of
// controls that does not fit does not fail to render and does not warn: it
// simply makes the document wider than the window, and the whole page slides
// sideways under the thumb. In a browser that is a scrollbar nobody asked for;
// in an installed window it is a control you cannot reach, because there is no
// wider window to open. Nothing in the Rust suite can see it — the markup is
// perfectly valid — so the measurement has to happen where the layout does.
//
// The pages come from the real router, rendered by the Rust side and handed
// over as JSON; `assets/` is served verbatim from the working tree, so the
// stylesheet and the script under test are the ones that ship.
//
// Usage: node page_width.js <repo-root> <chrome-binary> <pages.json> <width>
const http = require('http');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');

const ROOT = process.argv[2];
const CHROME = process.argv[3];
const PAGES = JSON.parse(fs.readFileSync(process.argv[4], 'utf8'));
const WIDTH = parseInt(process.argv[5], 10);

// `documentElement.scrollWidth` is the verdict and the rest is diagnosis: it
// is the one number that says the *document* is wider than the window, which
// is the only thing that actually scrolls. The element list is there to name
// the culprit, and it skips anything inside a box that scrolls on its own —
// a wide table inside `.raw` sticks out of its container by design, and that
// container is what keeps it off the document's width.
//
// On `DOMContentLoaded` and not `load`, with a subresource below that is never
// answered: `--dump-dom` prints the page and exits the moment `load` fires, so
// a probe waiting for it measured nothing and the browser was already gone.
// The page's own report is what ends the run instead.
const PROBE = `
  document.addEventListener('DOMContentLoaded', function () {
    setTimeout(function () {
      var de = document.documentElement;
      var vw = de.clientWidth;
      // Stopping at the body, not walking through it: when the viewport takes
      // its overflow from the body — which it does whenever the root is
      // visible — the browser reports the body's own overflow-x as hidden, and
      // walking past it swallowed every finding this list exists to name.
      var scrolls = function (el) {
        for (var p = el.parentElement; p && p !== document.body; p = p.parentElement) {
          var o = getComputedStyle(p).overflowX;
          if (o === 'auto' || o === 'scroll' || o === 'hidden') return true;
        }
        return false;
      };
      // The other half of the same failure, and the one that is invisible.
      // Where a row cannot wrap, a flex child that is allowed to break
      // anywhere gets squeezed to its own minimum — one character — and sets
      // its text one letter per line, a column tens of lines tall inside a box
      // a dozen pixels wide. Nothing warns, and the clipping means nothing is
      // even drawn: the card is simply four hundred pixels of nothing with a
      // chip above it.
      //
      // Counted in characters per line rather than in pixels, because the
      // pixel width at which this happens is whatever the chips beside it left
      // over — thirteen on the screen it was reported from, thirty in the
      // fixture, and a threshold tuned to one of those misses the other. Under
      // three characters a line, over more than two lines, is not a shape any
      // text is set in on purpose. A column of single words is: six lines of
      // 'Neues Einlesen von Corpora in engram' is narrow, not broken, and this
      // does not flag it.
      var own = function (el) {
        var n = 0;
        for (var c = el.firstChild; c; c = c.nextSibling) {
          if (c.nodeType === 3) n += c.data.trim().length;
        }
        return n;
      };
      var lines = function (el, r) {
        var lh = parseFloat(getComputedStyle(el).lineHeight);
        if (!lh) lh = (parseFloat(getComputedStyle(el).fontSize) || 16) * 1.2;
        return Math.round(r.height / lh);
      };
      var over = [];
      var squeezed = [];
      var name = function (el) {
        var cls = (el.getAttribute('class') || '').slice(0, 48);
        return el.tagName.toLowerCase() + (cls ? '.' + cls : '');
      };
      Array.prototype.forEach.call(document.querySelectorAll('*'), function (el) {
        var r = el.getBoundingClientRect();
        if (r.width === 0) return;
        if (r.right > vw + 0.5 || r.left < -0.5) {
          if (!scrolls(el)) {
            over.push(name(el) + ' [' + Math.round(r.left) + '\\u2192' + Math.round(r.right) + ']');
          }
        }
        // Leaves only, so the measure is of one run of text rather than of a
        // box that happens to contain an icon above a word: the tab bar's
        // links are a 20px glyph over 'Insights', which is three line-heights
        // tall and eight characters long and perfectly correct.
        var len = el.children.length === 0 ? own(el) : 0;
        if (len > 3) {
          var n = lines(el, r);
          if (n > 2 && n > len / 3) {
            squeezed.push(name(el) + ' [' + len + ' characters over ' + n + ' lines, ' +
                          Math.round(r.width) + 'px wide] ' +
                          el.textContent.trim().slice(0, 40));
          }
        }
      });
      fetch('/report', { method: 'POST', body: JSON.stringify({
        scrollWidth: de.scrollWidth, clientWidth: vw,
        over: over.slice(0, 12), squeezed: squeezed.slice(0, 12)
      })});
    }, 400);
  });
`;

let current = 0;
let pending = null;
const results = [];

const server = http.createServer((req, res) => {
  if (req.url === '/page') {
    // The probe goes in last, after everything the page loads for itself.
    // The hanging image is what keeps the browser on the page long enough to
    // hear from it; see the note on the probe.
    const html = PAGES[current].html.replace(
      /<\/body>/i,
      '<img src="/hang" alt="" style="display:none">' +
      '<script>' + PROBE + '</script></body>');
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
    return res.end(html);
  }
  // Never answered, so `load` never fires and the browser stays put.
  if (req.url === '/hang') return;
  if (req.url.startsWith('/assets/')) {
    // The stamp the templates carry is a cache key, not part of the name.
    const name = req.url.slice('/assets/'.length).split('?')[0];
    const file = ROOT + '/assets/' + name;
    if (!name.includes('..') && fs.existsSync(file)) {
      const type = name.endsWith('.css') ? 'text/css'
        : name.endsWith('.js') ? 'text/javascript'
        : name.endsWith('.woff2') ? 'font/woff2'
        : name.endsWith('.svg') ? 'image/svg+xml'
        : 'application/octet-stream';
      res.writeHead(200, { 'content-type': type });
      return res.end(fs.readFileSync(file));
    }
  }
  if (req.url === '/report' && req.method === 'POST') {
    let body = '';
    req.on('data', (c) => (body += c));
    return req.on('end', () => {
      res.writeHead(204);
      res.end();
      const r = JSON.parse(body);
      r.name = PAGES[current].name;
      results.push(r);
      next();
    });
  }
  // Anything else a page fetches for itself — a queue fragment, an offer — is
  // not what is being measured, and an error page in its place would be.
  res.writeHead(204);
  res.end();
});

let child = null;
let timer = null;
// Every profile directory handed to a browser, so `finish` can take them away
// again. One per page per width, and nothing else removes them.
const profiles = [];

function next() {
  if (child) { child.kill(); child = null; }
  if (timer) { clearTimeout(timer); timer = null; }
  current = results.length;
  if (current >= PAGES.length) return finish();
  child = spawn(
    CHROME,
    [
      '--no-sandbox',
      '--disable-gpu',
      '--dump-dom',
      '--window-size=' + WIDTH + ',900',
      // A fresh profile per browser, so nothing a page does is carried into
      // the next measurement — and remembered, because a temp directory
      // nobody removes is one this test leaves behind on every run.
      '--user-data-dir=' + profile(),
      'http://127.0.0.1:' + pending + '/page'
    ],
    { stdio: 'ignore' }
  );
  // A page that never reports is a failure of this harness, not of the page,
  // and it has to say which one so the run is debuggable.
  timer = setTimeout(() => {
    results.push({ name: PAGES[current].name, error: 'the page never reported back' });
    next();
  }, 20000);
}

function profile() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'engram-chrome-'));
  profiles.push(dir);
  return dir;
}

function finish() {
  if (child) child.kill();
  server.close();
  // The result first: `process.exit` below runs nothing on the way out, so
  // this is the last chance to print it, and a run that measured everything
  // correctly must not lose its answer to a directory that would not go.
  console.log(JSON.stringify({ width: WIDTH, pages: results }));
  // Each profile is tens of megabytes and there is one per page per width; a
  // full run left eighteen of them in the temp directory, every time.
  for (const dir of profiles) {
    try {
      fs.rmSync(dir, { recursive: true, force: true });
    } catch (e) {
      // The browser that held it is being killed as this runs, so a file that
      // is still open here is possible and is not worth failing a green run
      // over. The temp directory is swept by the system either way.
    }
  }
  process.exit(0);
}

server.listen(0, '127.0.0.1', () => {
  pending = server.address().port;
  next();
});
