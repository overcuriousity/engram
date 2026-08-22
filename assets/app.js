// Client-side because none of it may touch the sanitized HTML on the server.
// Every function here walks text nodes only: it can wrap what is already
// rendered, and it can never introduce an element the sanitizer disallowed.
(function () {
  'use strict';

  // Registering the worker is what makes the browser offer to install engram
  // rather than bookmark it. Guarded on both the API and a secure context,
  // because plain HTTP on a LAN address has neither and must still work.
  if ('serviceWorker' in navigator && window.isSecureContext) {
    window.addEventListener('load', function () {
      navigator.serviceWorker.register('/sw.js').catch(function (e) {
        console.warn('service worker registration failed', e);
      });
    });
  }

  function terms(root) {
    var host = root.closest ? (root.closest('[data-terms]') || root.querySelector('[data-terms]')) : null;
    if (!host && root.getAttribute && root.getAttribute('data-terms') !== null) host = root;
    if (!host) return [];
    var raw = (host.getAttribute('data-terms') || '').trim();
    return raw ? raw.toLowerCase().split(/\s+/).filter(function (t) { return t.length > 1; }) : [];
  }

  function highlight(root) {
    var list = terms(root);
    if (!list.length) return;
    var walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    var nodes = [];
    while (walker.nextNode()) nodes.push(walker.currentNode);

    nodes.forEach(function (node) {
      if (node.parentNode && node.parentNode.tagName === 'MARK') return;
      var text = node.nodeValue;
      var lower = text.toLowerCase();
      var hits = [];
      list.forEach(function (term) {
        var from = 0, at;
        while ((at = lower.indexOf(term, from)) !== -1) {
          hits.push([at, at + term.length]);
          from = at + term.length;
        }
      });
      if (!hits.length) return;
      hits.sort(function (a, b) { return a[0] - b[0]; });

      var frag = document.createDocumentFragment();
      var cursor = 0;
      hits.forEach(function (h) {
        if (h[0] < cursor) return;
        frag.appendChild(document.createTextNode(text.slice(cursor, h[0])));
        var mark = document.createElement('mark');
        mark.textContent = text.slice(h[0], h[1]);
        frag.appendChild(mark);
        cursor = h[1];
      });
      frag.appendChild(document.createTextNode(text.slice(cursor)));
      node.parentNode.replaceChild(frag, node);
    });
  }

  // Clamping is visual only. The text is never truncated, so a fenced command
  // is never cut in half — expanding reveals what was always there.
  function clamp(root) {
    root.querySelectorAll('.clampable:not([data-clamped])').forEach(function (el) {
      el.setAttribute('data-clamped', 'yes');
      if (el.scrollHeight <= el.clientHeight + 4) return;
      el.classList.add('is-clamped');
      var btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'btn btn-ghost btn-sm expand';
      btn.textContent = 'Expand';
      btn.addEventListener('click', function () {
        var stillClamped = el.classList.toggle('is-clamped');
        btn.textContent = stillClamped ? 'Expand' : 'Collapse';
      });
      el.parentNode.insertBefore(btn, el.nextSibling);
    });
  }

  function copyButtons(root) {
    root.querySelectorAll('[data-copyable] pre').forEach(function (pre) {
      if (pre.parentNode.classList.contains('codewrap')) return;
      var wrap = document.createElement('div');
      wrap.className = 'codewrap';
      pre.parentNode.insertBefore(wrap, pre);
      wrap.appendChild(pre);

      var btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'copy';
      btn.textContent = 'copy';
      btn.addEventListener('click', function () {
        if (!navigator.clipboard) return;
        navigator.clipboard.writeText(pre.innerText).then(function () {
          btn.textContent = 'copied';
          setTimeout(function () { btn.textContent = 'copy'; }, 1200);
        });
      });
      wrap.appendChild(btn);
    });
  }

  // A blank source line costs a full numbered row, and in a chapter of
  // exercises that was a third of the pane. Runs of three or more fold to one
  // rule carrying their count.
  //
  // Rendering only: the server still sends every line, and the numbers either
  // side of a fold are the source's own. A row inside the extraction range is
  // never folded, whatever it holds — the pane exists to show that range, and
  // hiding part of it to save space defeats the only thing the pane is for.
  function collapseBlanks(root) {
    root.querySelectorAll('.raw table:not([data-folded])').forEach(function (table) {
      table.setAttribute('data-folded', '1');
      var run = [];
      function flush() {
        if (run.length < 3) { run = []; return; }
        var hidden = run.slice();
        var mark = document.createElement('tr');
        mark.className = 'srcfold';
        var cell = document.createElement('td');
        cell.colSpan = 2;
        cell.textContent = hidden.length + ' blank lines';
        mark.appendChild(cell);
        hidden[0].parentNode.insertBefore(mark, hidden[0]);
        hidden.forEach(function (el) { el.hidden = true; });
        mark.addEventListener('click', function () {
          hidden.forEach(function (el) { el.hidden = false; });
          mark.remove();
        });
        run = [];
      }
      Array.prototype.slice.call(table.rows).forEach(function (tr) {
        var blank = tr.cells.length > 1 && tr.cells[1].textContent.trim() === '';
        if (blank && !tr.classList.contains('in')) { run.push(tr); } else { flush(); }
      });
      flush();
    });
  }

  // The artifact and the lines it came from are one thing read twice, so they
  // move together. Proportional rather than line-mapped: prose and source do
  // not share a line count, and pretending they do lands on the wrong line
  // more often than it helps.
  //
  // The card, not the column it sits in. Related and Seen together share that
  // column and are not this artifact, so binding here moved the source panel
  // whenever the neighbour list was scrolled — the one pane on the screen
  // whose whole claim is that it shows where *this* artifact came from. The
  // card is the scroll box for the same reason; see 40-search.css.
  //
  // The guard is not decoration. Setting scrollTop fires scroll, which would
  // set the other back, which would fire again.
  function lockstep(root) {
    var split = root.querySelector('.split');
    if (!split || split.dataset.lockstep) return;
    var artifact = split.children[0] && split.children[0].querySelector('.card');
    var source = split.querySelector('.raw');
    if (!artifact || !source) return;
    split.dataset.lockstep = '1';
    var busy = false;
    function sync(from, to) {
      return function () {
        if (busy) return;
        busy = true;
        var span = from.scrollHeight - from.clientHeight;
        var ratio = span > 0 ? from.scrollTop / span : 0;
        to.scrollTop = ratio * (to.scrollHeight - to.clientHeight);
        requestAnimationFrame(function () { busy = false; });
      };
    }
    artifact.addEventListener('scroll', sync(artifact, source));
    source.addEventListener('scroll', sync(source, artifact));
  }

  function enhance(root) {
    if (!root || root.nodeType !== 1) return;
    highlight(root);
    clamp(root);
    copyButtons(root);
    collapseBlanks(root);
    lockstep(root);
  }

  // ── Ask, as it happens ────────────────────────────────────────────────────
  //
  // Two requests, because `EventSource` is GET-only and a GET that runs a model
  // call is a free-inference hole: the POST parks the question and hands back an
  // id, and the stream spends that id once. Everything the page inserts was
  // rendered and sanitized by the server — this driver moves HTML it was handed
  // and text it puts in text nodes, and never builds markup out of an answer.
  function askDriver() {
    var form = document.getElementById('ask-form');
    if (!form) return;
    var live = document.getElementById('ask-live');
    var reasoning = document.getElementById('ask-reasoning');
    // The disclosure around it: what is shown or hidden is the box, while the
    // text still streams into the div inside it.
    var reasoningBox = document.getElementById('ask-reasoning-box');
    var progress = document.getElementById('ask-progress');
    var rail = document.getElementById('ask-rail');
    var result = document.getElementById('ask-result');
    var status = document.getElementById('ask-status');
    var spinner = document.getElementById('ask-spinner');
    var stopBtn = document.getElementById('ask-stop');
    var source = null;
    // The wait is fifty seconds on a fan-out and nothing on the page predicted
    // it. Counted from the submit rather than from the first token, because
    // the retrieval in front of the model is most of what is being waited on.
    var started = 0;
    var ticker = null;
    // Which ask the page belongs to. See the submit handler.
    var generation = 0;

    // The one line that matters most in this file. An `EventSource` that is
    // left open reconnects by itself when the server closes the stream, and the
    // reconnect is a fresh GET — which, on a spent id, is a 404, but on any
    // future shape of this endpoint is a second model call nobody asked for and
    // a second bill on a paid endpoint. Every exit from a stream comes through
    // here.
    function stop() {
      if (source) { source.close(); source = null; }
      if (ticker) { clearInterval(ticker); ticker = null; }
      stopBtn.hidden = true;
      spinner.textContent = 'thinking…';
      form.classList.remove('asking');
    }

    // Count up beside the spinner while the stream is open.
    function startTicking() {
      started = Date.now();
      stopBtn.hidden = false;
      if (ticker) clearInterval(ticker);
      ticker = setInterval(function () {
        var secs = Math.round((Date.now() - started) / 1000);
        spinner.textContent = 'thinking… ' + secs + 's';
      }, 1000);
    }

    stopBtn.addEventListener('click', function () {
      if (!source) return;
      var secs = Math.round((Date.now() - started) / 1000);
      stop();
      // What arrived stays. `live` holds the partial answer and the rail holds
      // the excerpts it was being written from, and both are worth more than a
      // cleared page — the reader stopped the wait, not the answer.
      status.textContent = 'stopped after ' + secs + ' seconds';
    });

    function fail(message) {
      stop();
      result.textContent = '';
      var box = document.createElement('div');
      box.className = 'flag';
      box.setAttribute('role', 'status');
      // textContent, not innerHTML: an error string is the one payload here
      // that never went through the sanitizing renderer.
      box.textContent = message;
      result.appendChild(box);
      live.hidden = true;
      reasoningBox.hidden = true;
      progress.hidden = true;
    }

    function openStream(id, mine) {
      // The id of a superseded ask is simply never spent: the stream is not
      // opened at all rather than opened and closed, which is one fewer model
      // call started and abandoned.
      if (mine !== generation) return;
      source = new EventSource('/ui/ask/' + encodeURIComponent(id) + '/stream');

      // Every handler is gated the same way. `stop()` closes the stream the
      // page is currently listening to; an event already queued from an older
      // one must not write into the answer that replaced it.
      function current() { return mine === generation; }

      source.addEventListener('citations', function (e) {
        if (!current()) return;
        // Server-rendered, ids and all: the `cite-n` anchors in here are the
        // other end of the `[n]` links the answer arrives with.
        rail.innerHTML = JSON.parse(e.data).rail;
        enhance(rail);
      });
      // The fanned-out retrieval, made visible. Without it the extra searches
      // are a silent pause in front of the answer, and the queries they ran are
      // otherwise nowhere on the page. With `plan = false` neither of these ever
      // fires and the line stays hidden.
      source.addEventListener('needs', function (e) {
        if (!current()) return;
        progress.hidden = false;
        // textContent, not innerHTML: these strings are model output that went
        // through no renderer. Same rule as the error box.
        progress.textContent = 'Looking further: ' +
          JSON.parse(e.data).queries.join(', ');
      });
      source.addEventListener('retrieved', function (e) {
        if (!current()) return;
        var round = JSON.parse(e.data);
        // Round one happens on every ask, including every ask that will never
        // fan out, and a line of retrieval statistics in front of every answer
        // is noise. The fan-out is the part nobody can otherwise see happen.
        if (round.round < 2) return;
        progress.hidden = false;
        // Appended, not assigned: the line already holds what `needs` said the
        // fan-out went looking for, and after this event nothing else on the
        // page does. Round one never writes here, so the join is only ever to
        // those queries.
        //
        // What was searched beside what is shown, rather than what was left
        // out. Every round the fan-out runs widens the net, so the number not
        // shown grows with the feature working — and "26 left out" reads as
        // twenty-six failures. The pair says the true thing: a wide search,
        // narrowed to what the window holds.
        progress.textContent = (progress.textContent ?
          progress.textContent + ' \u2014 ' : 'Round ' + round.round + ': ') +
          'searched ' + round.retrieved + ', showing ' + round.shown;
      });
      source.addEventListener('reasoning', function (e) {
        if (!current()) return;
        reasoningBox.hidden = false;
        reasoning.appendChild(document.createTextNode(JSON.parse(e.data).text));
        // Only while it is open. A closed box has no scroll position to keep,
        // and asking for one fights the page for the reader's.
        if (reasoningBox.open) reasoning.scrollTop = reasoning.scrollHeight;
      });
      source.addEventListener('token', function (e) {
        if (!current()) return;
        live.hidden = false;
        live.appendChild(document.createTextNode(JSON.parse(e.data).text));
      });
      source.addEventListener('done', function (e) {
        if (!current()) return;
        stop();
        result.innerHTML = JSON.parse(e.data).html;
        enhance(result);
        // The fragment carries `hx-post` controls — the verdict bar, "carried
        // the answer" — and htmx binds those only to markup it swapped in
        // itself or was told about. Set through `innerHTML` they are inert
        // buttons until this call; before it, a click on Right did nothing.
        if (window.htmx) window.htmx.process(result);
        // The plain stream and the model's aside have both been superseded by
        // the rendered answer. Hidden rather than emptied, so the next ask
        // reuses them.
        live.hidden = true;
        reasoningBox.hidden = true;
        // `progress` deliberately stays: what the retrieval went looking for
        // still describes the rail underneath the answer, and it is the only
        // place on the page that says a fan-out happened at all.
        // Said once, when there is something to read. The tokens streamed into
        // a polite live region as they arrived, which tells a reader that an
        // answer is coming; nothing until now said it had finished.
        status.textContent = 'The answer is ready.';
      });
      // Both failures arrive as `error`: the server's own event, which carries
      // a message, and the browser's transport error, which carries no data.
      // The second one is the dangerous one — the browser is already queuing a
      // reconnect when it fires, so `stop()` has to run on it too.
      source.addEventListener('error', function (e) {
        if (!current()) return;
        fail(e.data ? e.data : 'The connection to the answer stream was lost.');
      });
    }

    form.addEventListener('submit', function (e) {
      e.preventDefault();
      var q = form.querySelector('input[name="q"]').value;
      if (!q.trim()) return;
      // A second ask supersedes the first at every stage, which `stop()` on its
      // own does not achieve: it closes a stream that is already open, and two
      // submits made before the first POST resolves open two streams, the
      // second overwriting the only reference to the first. That first one is
      // then unclosable, reconnects on its own, and — worse — its eventual
      // error wipes the answer the second ask is in the middle of writing.
      // The generation is what the page belongs to; anything an older ask has
      // to say is dropped, including the stream it was about to open.
      var mine = ++generation;
      stop();
      status.textContent = '';
      live.textContent = '';
      reasoning.textContent = '';
      progress.textContent = '';
      progress.hidden = true;
      rail.textContent = '';
      result.textContent = '';
      live.hidden = true;
      reasoningBox.hidden = true;
      reasoningBox.open = false;
      form.classList.add('asking');
      startTicking();

      fetch('/ui/ask', {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'content-type': 'application/x-www-form-urlencoded' },
        body: new URLSearchParams({ q: q }).toString()
      }).then(function (res) {
        if (!res.ok) throw new Error('The question was refused (' + res.status + ').');
        return res.json();
      }).then(function (out) {
        openStream(out.id, mine);
      }).catch(function (err) {
        // A stale rejection has the same shape as a stale event and is dropped
        // for the same reason: it would replace a live answer with the failure
        // of an ask nobody is waiting for any more.
        if (mine !== generation) return;
        fail(err.message || 'The question could not be sent.');
      });
    });

    // A citation is a link to an excerpt on this page. Delegated, because the
    // links arrive with the answer long after this runs.
    document.addEventListener('click', function (e) {
      var link = e.target.closest ? e.target.closest('a.cite') : null;
      if (!link) return;
      var target = document.getElementById(link.getAttribute('href').slice(1));
      // Left alone when the anchor is missing: the default jump does nothing
      // visible, which is honest, where scrolling somewhere arbitrary would
      // look like provenance.
      if (!target) return;
      e.preventDefault();
      rail.querySelectorAll('.rail-active').forEach(function (el) {
        el.classList.remove('rail-active');
      });
      target.classList.add('rail-active');
      target.scrollIntoView({ block: 'center', behavior: 'smooth' });
    });
  }

  // ── Dwell ─────────────────────────────────────────────────────────────────
  //
  // How long an artifact stayed open, reported as the reader leaves it: the
  // next pane swap, the page going away, the tab going hidden. The weakest
  // pursuit signal there is, and it is sent as a beacon so leaving costs
  // nothing. Under three seconds is a glance, not a read, and is not sent.
  var dwell = { id: null, since: 0 };
  function flushDwell() {
    if (!dwell.id) return;
    var secs = Math.round((Date.now() - dwell.since) / 1000);
    var id = dwell.id;
    dwell.id = null;
    if (secs < 3) return;
    var body = new URLSearchParams({ secs: String(secs) });
    if (navigator.sendBeacon) {
      navigator.sendBeacon('/ui/artifacts/' + id + '/dwell', body);
    } else {
      fetch('/ui/artifacts/' + id + '/dwell', { method: 'POST', body: body, keepalive: true });
    }
  }
  function trackDwell() {
    var open = document.querySelector('[data-artifact]');
    var id = open ? open.getAttribute('data-artifact') : null;
    if (id === dwell.id) return;
    flushDwell();
    if (id) { dwell.id = id; dwell.since = Date.now(); }
  }

  // Shown until it is dismissed, then never again on this browser. Not shown
  // at all on a touch screen: there are no keys there and the row would be
  // furniture that costs a line of the results.
  function keyHint() {
    var hint = document.querySelector('.keyhint');
    if (!hint) return;
    var seen = true;
    try { seen = localStorage.getItem('engram.hints') === 'seen'; } catch (e) { seen = true; }
    if (seen || !window.matchMedia('(pointer: fine)').matches) return;
    hint.hidden = false;
    hint.querySelector('[data-dismiss-hint]').addEventListener('click', function () {
      hint.hidden = true;
      try { localStorage.setItem('engram.hints', 'seen'); } catch (e) {}
    });
  }

  // Restored before anything is drawn into the rail, so a remembered reading
  // mode does not flash the wide rail first.
  function restoreReading() {
    var regions = document.querySelector('.regions');
    if (!regions || !document.querySelector('.region-rail')) return;
    try {
      if (localStorage.getItem('engram.reading') === '1') regions.classList.add('reading');
    } catch (e) {}
  }

  // Follows the system until it is touched; a remembered two-state switch from
  // then on. The pre-paint script in the head is what applies a stored choice
  // before anything is drawn — this only has to keep the button honest and
  // move the status-bar colour with it.
  function themeToggle() {
    var btn = document.querySelector('[data-theme-toggle]');
    if (!btn) return;
    var label = btn.querySelector('[data-theme-label]');

    function current() {
      var set = document.documentElement.getAttribute('data-theme');
      if (set) return set;
      return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
    }

    function paint() {
      var now = current();
      // Names the destination, not the state: a button called "Dark" while the
      // page is dark reads as a label rather than as something to press.
      label.textContent = now === 'dark' ? 'Light' : 'Dark';
      // An installed app frames the page in this colour. Left alone, a light
      // page keeps a dark status bar and looks broken on a phone. The two
      // media-scoped tags in the head answer the system, not the choice, so
      // the choice needs one of its own.
      var meta = document.querySelector('meta[name="theme-color"]:not([media])');
      if (!meta) {
        meta = document.createElement('meta');
        meta.setAttribute('name', 'theme-color');
        document.head.appendChild(meta);
      }
      meta.setAttribute('content', now === 'dark' ? '#0e1015' : '#f8f6f1');
    }

    btn.addEventListener('click', function () {
      var next = current() === 'dark' ? 'light' : 'dark';
      document.documentElement.setAttribute('data-theme', next);
      try { localStorage.setItem('engram.theme', next); } catch (e) {}
      paint();
    });
    paint();
  }

  // One input that reaches everything. The prefix decides where it goes: plain
  // text searches, `>` asks, and a paste long enough to be a document offers to
  // keep it rather than to look for it.
  function commandBar() {
    var overlay = document.querySelector('.cmdk');
    if (!overlay) return;
    var input = overlay.querySelector('[data-cmdk-input]');
    // Long enough to be a document rather than a question. A sentence you are
    // searching for does not run this far; a chapter always does.
    var PASTE = 400;

    function open() {
      overlay.hidden = false;
      input.value = '';
      input.focus();
    }
    function close() { overlay.hidden = true; }

    document.addEventListener('keydown', function (e) {
      if ((e.metaKey || e.ctrlKey) && e.key === 'k') { e.preventDefault(); open(); return; }
      // Ahead of the global Escape handler by registration order, and it
      // returns, so closing the bar never also fires "back".
      if (e.key === 'Escape' && !overlay.hidden) {
        e.preventDefault();
        e.stopImmediatePropagation();
        close();
      }
    }, true);

    // The backdrop, not the box: a click inside the box is a click in the box.
    overlay.addEventListener('click', function (e) { if (e.target === overlay) close(); });

    input.addEventListener('keydown', function (e) {
      if (e.key !== 'Enter') return;
      e.preventDefault();
      var v = input.value.trim();
      if (!v) return;
      if (v.charAt(0) === '>') {
        location.href = '/ui/ask?q=' + encodeURIComponent(v.slice(1).trim());
      } else if (v.length > PASTE) {
        // Handed over in sessionStorage rather than in the URL. `/ui/capture`
        // takes a `from_ask` and nothing else, so a `?text=` would arrive at a
        // page that ignores it — and a chapter in a query string is past what
        // a URL may be anyway. Nothing on the server has to learn about this.
        try { sessionStorage.setItem('engram.paste', v); } catch (err) {}
        location.href = '/ui/capture';
      } else {
        location.href = '/ui/search?q=' + encodeURIComponent(v);
      }
    });
  }

  // The other half of the command bar's paste. Claimed once and cleared, so a
  // later visit to Capture does not refill a box you emptied on purpose —
  // which means clearing it even when the box already has something in it and
  // the paste is dropped. Clearing only on the path that used it left the text
  // in storage to be injected on some later visit, the one thing this is
  // supposed to prevent.
  function claimPaste() {
    var box = document.querySelector('textarea[name="text"]');
    if (!box) return;
    var text = null;
    try {
      text = sessionStorage.getItem('engram.paste');
      if (text) sessionStorage.removeItem('engram.paste');
    } catch (e) { return; }
    if (!text || box.value) return;
    box.value = text;
    // Assigning `value` fires nothing, and the segment-and-cost hint on the
    // capture page is bound to `input`. The command bar only routes here for a
    // paste past PASTE characters — exactly the multi-segment case the hint
    // exists to warn about — so without this it stayed hidden for every paste
    // that needed it.
    box.dispatchEvent(new Event('input', { bubbles: true }));
    box.focus();
  }

  // The situation this page view happened in.
  //
  // Synchronous, because htmx reads `hx-vals js:` synchronously. The two
  // asynchronous sources — the Battery API and the device list — are read once
  // at load and cached here, so the first page view of a session goes without
  // them and the server zeroes their blocks rather than inventing a value.
  //
  // Deliberately not collected: canvas, WebGL, fonts, plugins. Those identify a
  // device across a population; here the population is one authenticated
  // person, so they are constant and say nothing about which situation this is
  // — and a hardened browser randomises them per session, so every day would
  // look like a new device.
  var slow = { battery_level: null, charging: null, audio_outputs: null };

  function primeSlow() {
    if (navigator.getBattery) {
      navigator.getBattery().then(function (b) {
        slow.battery_level = b.level;
        slow.charging = b.charging;
      }).catch(function () {});
    }
    if (navigator.mediaDevices && navigator.mediaDevices.enumerateDevices) {
      navigator.mediaDevices.enumerateDevices().then(function (list) {
        slow.audio_outputs = list.filter(function (d) {
          return d.kind === 'audiooutput';
        }).length;
      }).catch(function () {});
    }
  }

  function uaFamily() {
    var d = navigator.userAgentData;
    if (d && d.brands) {
      for (var i = 0; i < d.brands.length; i++) {
        // Chromium pads the list with a deliberately absurd brand to break
        // exactly the kind of sniffing this is not doing; skip it.
        if (!/Not.*Brand/i.test(d.brands[i].brand)) return d.brands[i].brand;
      }
    }
    var m = /(Firefox|Edg|Chrome|Safari)\/[\d.]+/.exec(navigator.userAgent || '');
    return m ? m[1] : null;
  }

  function netKind() {
    var c = navigator.connection || navigator.mozConnection;
    if (!c) return null;
    if (c.type) return c.type;
    // `effectiveType` describes speed rather than medium, and calling a slow
    // wifi "cellular" would be inventing a fact. Absent instead.
    return null;
  }

  window.engramContext = function () {
    var b = {};
    try {
      b.tz = Intl.DateTimeFormat().resolvedOptions().timeZone || null;
      b.tz_offset_mins = -new Date().getTimezoneOffset();
      b.language = navigator.language || null;
      b.languages = navigator.languages ? Array.prototype.slice.call(navigator.languages) : [];
      b.viewport_w = window.innerWidth;
      b.viewport_h = window.innerHeight;
      b.screen_w = screen.width;
      b.screen_h = screen.height;
      b.dpr = window.devicePixelRatio;
      b.color_scheme = matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
      b.platform = (navigator.userAgentData && navigator.userAgentData.platform)
        || navigator.platform || null;
      b.ua_family = uaFamily();
      b.cores = navigator.hardwareConcurrency || null;
      b.memory_gb = navigator.deviceMemory || null;
      b.touch = navigator.maxTouchPoints > 0;
      b.orientation = window.innerHeight >= window.innerWidth ? 'portrait' : 'landscape';
      b.network = netKind();
      b.battery_level = slow.battery_level;
      b.charging = slow.charging;
      b.audio_outputs = slow.audio_outputs;
    } catch (e) {
      // A partial bundle is a working one: the blocks it could not fill are
      // zeroed server-side, and the weekday and the hour still stand.
    }
    return JSON.stringify(b);
  };

  // Removed on the first keystroke, and it does not come back when the box is
  // cleared. The flag is what covers the race: the fetch fires on `load`, and
  // its answer can land *after* the first keystroke — without this, an offer
  // already dismissed would swap itself back in.
  var offerDismissed = false;

  function dropOffer() {
    var area = document.getElementById('context-offer');
    if (area) area.remove();
  }

  // The other half of the impression. The server computes an offer without
  // knowing whether anyone saw it — this fragment races the first keystroke,
  // and the answer that loses is dropped above without ever being on screen.
  // Counting those as shown put a population that structurally cannot click
  // into the denominator of the hit rate on Ops, which is the number the block
  // weights are supposed to be fitted against one day. So the browser says so,
  // and only the browser can.
  //
  // Nothing waits on this and nothing is done with the answer: a confirmation
  // that fails to send costs one row of an instrument, not a page.
  function confirmOffer(area) {
    if (!area) return;
    var id = area.getAttribute('data-rec-id');
    var rung = area.getAttribute('data-rec-rung');
    if (!id || !rung) return;
    var body = new URLSearchParams({ artifact_id: id, rung: rung });
    var slot = area.getAttribute('data-rec-slot');
    if (slot) body.set('slot', slot);
    fetch('/ui/context/seen', { method: 'POST', body: body }).catch(function () {});
  }

  function contextOffer() {
    var box = document.querySelector('input[name=q]');
    if (!box) return;
    box.addEventListener('input', function () {
      offerDismissed = true;
      dropOffer();
    }, { once: true });
  }

  // ── The box, and what the two verbs may do with it ──────────────────
  // Typing is the third verb and needs no button: it is the `hx-trigger` on
  // the form. These two need one, because a model call and a write are both
  // deliberate acts — what decides which of the three happens is never a
  // length, a newline, or anything else the page noticed on its own.
  function boxVerbs() {
    var form = document.getElementById('box-form');
    if (!form) return;
    var box = form.querySelector('textarea[name="q"]');
    if (!box) return;
    var buttons = form.querySelectorAll('[data-verb]');

    // Neither verb has anything to act on while the box is empty.
    function sync() {
      var empty = !box.value.trim();
      for (var i = 0; i < buttons.length; i++) buttons[i].disabled = empty;
    }

    // One line to start, growing to a ten-line cap and then scrolling inside
    // itself. Measured off `scrollHeight` rather than counting newlines,
    // because a pasted paragraph is many lines and carries none.
    //
    // This is not the box changing shape on its own in the sense the panel
    // rules out: it never becomes a different control, and it never decides
    // what the words in it are for. It only stops hiding them.
    var CAP = 10;
    function grow() {
      box.style.height = 'auto';
      var line = parseFloat(getComputedStyle(box).lineHeight) || 20;
      box.style.height = Math.min(box.scrollHeight, line * CAP) + 'px';
    }

    box.addEventListener('input', function () { sync(); grow(); });
    sync();
    grow();

    // What the door asked for. Search needs no value: typing is already the
    // whole of it, and the form's `load` trigger has run by now.
    var bar = document.querySelector('[data-workspace]');
    var opens = bar && bar.getAttribute('data-open-with');
    if (opens === 'capture' && window.matchMedia('(hover: hover)').matches) {
      box.focus();
    }
  }

  // ── Capture, as a verb ──────────────────────────────────────────────
  // Everything the capture page used to do inline, on the one box. The
  // reasons are the page's own and travel with the code.
  function captureVerb() {
    var form = document.getElementById('box-form');
    if (!form) return;
    var box = form.querySelector('textarea[name="q"]');
    var hint = document.getElementById('size-hint');
    var verb = form.querySelector('[data-verb="capture"]');
    if (!box || !hint || !verb) return;
    // Rough stand-in for the tokeniser: enough to warn, never to block.
    var CHARS_PER_SEGMENT = 12000;
    // At `earned` and `off` capture makes no synthesis call, so the hint must
    // not price one. app.js is one file for every installation, so what used
    // to be a template conditional rides an attribute instead.
    var EAGER = hint.getAttribute('data-eager') === '1';
    box.addEventListener('input', function () {
      var segments = Math.ceil(box.value.length / CHARS_PER_SEGMENT);
      hint.hidden = segments < 2;
      hint.textContent = EAGER
        ? 'About ' + segments + ' segments — roughly ' + segments +
          ' model calls before this is searchable.'
        : 'About ' + segments + ' segments — searchable as written, once embedded.';
    });

    var drop = document.getElementById('drop');
    var picker = drop && drop.querySelector('input[type=file]');
    var noteBox = document.querySelector('input[name="note"]');
    // Same reason as EAGER: read off the picker the server rendered, which
    // names `image/*` only where `[infer.vision]` is configured.
    var VISION = !!picker && (picker.getAttribute('accept') || '').indexOf('image/*') >= 0;

    // ── What is waiting to be sent ────────────────────────────────────────
    // A file arriving is not a capture. It is held here until Capture is
    // pressed, so the note beside it can be written and the wrong photo can be
    // removed — on a phone the camera hands the picture back the moment it is
    // taken, and uploading on arrival meant the operator never got a say.
    var staged = null, stagedUrl = null;
    var stagedBox = document.getElementById('staged');
    var stagedName = document.getElementById('staged-name');
    var stagedThumb = document.getElementById('staged-thumb');
    var stagedClear = document.getElementById('staged-clear');

    function unstage() {
      staged = null;
      // The object URL holds the picture in memory until it is let go.
      if (stagedUrl) { URL.revokeObjectURL(stagedUrl); stagedUrl = null; }
      stagedThumb.removeAttribute('src');
      stagedThumb.hidden = true;
      stagedName.hidden = true;
      stagedClear.hidden = true;
      // The whole box goes: it is not an invitation any more, it is the thing
      // waiting to be sent. The picker in the verb row is the invitation, and
      // it costs a search nothing.
      if (stagedBox) stagedBox.hidden = true;
      if (drop) drop.hidden = false;
      // Or picking the same file twice in a row fires no `change` the second
      // time, and the drop zone looks broken.
      if (picker) picker.value = '';
    }

    // `restore` is the failed upload coming back: the same file, already
    // described, and no reason to reach for the note a second time.
    function stage(file, restore) {
      if (!file) return;
      var result = document.getElementById('capture-result');
      // Said now rather than after the operator has written a note for a file
      // this server was never going to read.
      if (file.type.indexOf('image/') === 0 && !VISION) {
        result.textContent = 'Image capture is not configured on this server.';
        unstage();
        return;
      }
      unstage();
      staged = file;
      stagedName.textContent = (file.name || 'photo.jpg') +
        ' — ready. Press Capture.';
      if (file.type.indexOf('image/') === 0) {
        stagedUrl = URL.createObjectURL(file);
        stagedThumb.src = stagedUrl;
        stagedThumb.hidden = false;
      }
      stagedName.hidden = false;
      stagedClear.hidden = false;
      // The invitation steps aside for the file that accepted it: one box,
      // saying one thing at a time.
      if (stagedBox) stagedBox.hidden = false;
      if (drop) drop.hidden = true;
      // Only where a pointer says there is a hardware keyboard — the rule the
      // paste box and the search box already follow. On a phone this would
      // throw the software keyboard over the thumbnail the operator is
      // checking, which is the picture they just took.
      if (noteBox && !restore && window.matchMedia('(hover: hover)').matches) {
        noteBox.focus();
      }
    }

    function send(file) {
      if (!file) return;
      var isImage = file.type.indexOf('image/') === 0;
      // By name as well as by type: a drop from some file managers carries no
      // type at all, and the door judges those by their name too.
      var isPdf = file.type === 'application/pdf' || /\.pdf$/i.test(file.name || '');
      var result = document.getElementById('capture-result');
      if (isImage && !VISION) {
        result.textContent = 'Image capture is not configured on this server.';
        return;
      }
      // The upload does not go through htmx, so the spinner beside the button
      // never lit for it. On a phone a photo is the slowest thing this page
      // sends and pressing Capture looked like it had done nothing.
      result.textContent = 'Sending…';
      var payload = new FormData();
      if (noteBox && noteBox.value.trim()) payload.append('note', noteBox.value.trim());
      // The fallback name matters: a PDF that arrived unnamed and went up as
      // `paste.txt` would be refused by the very door it belongs to.
      var fallbackName = isImage ? 'photo.jpg' : (isPdf ? 'capture.pdf' : 'paste.txt');
      payload.append(isImage ? 'image' : 'file', file, file.name || fallbackName);
      var url = isImage ? '/api/v1/corpora/image' : '/api/v1/corpora/upload';
      // The session cookie authenticates this: it is same-origin, so no token
      // is involved.
      fetch(url, { method: 'POST', body: payload })
        // Not every failure answers in JSON. A file over the body limit is
        // rejected before the handler sees it, and that reply is plain text —
        // parsing it threw, and with nothing catching the throw the drop zone
        // sat there having visibly done nothing.
        .then(function (r) {
          return r.json()
            .catch(function () { return { error: 'engram answered ' + r.status + '.' }; })
            .then(function (j) { return [r.ok, j]; });
        })
        .catch(function () { return [false, { error: 'engram is unreachable.' }]; })
        .then(function (pair) {
          // The server's reason, verbatim. A generic "upload failed" would
          // hide what actually goes wrong here: wrong type, wrong encoding,
          // an image door that is closed.
          result.textContent = pair[0]
            ? (isImage ? 'Captured — the photo is queued to be read.'
               : isPdf ? 'Captured — the PDF is queued to be extracted.'
               : 'Captured.')
            : (pair[1].error || 'Upload failed.');
          if (pair[0]) {
            if (noteBox) noteBox.value = '';
            htmx.trigger(document.body, 'captured');
          } else {
            // It never left. Put it back where it was, so the fix is a second
            // press rather than a second trip to the camera.
            stage(file, true);
          }
        });
    }
    if (drop) picker.addEventListener('change', function () { stage(picker.files[0]); });

    // The whole page is the drop target, because a browser that is not offered
    // the file takes it: a photo dropped an inch wide of the box replaced
    // engram with the picture, and the capture the operator was in the middle
    // of writing went with it. The box below still says where to aim; missing
    // it is no longer punished.
    //
    // Only for drags carrying files. Dragging selected text into the paste box
    // is a drop too, and cancelling that one would break a thing that works.
    function carriesFiles(e) {
      var types = e.dataTransfer && e.dataTransfer.types;
      if (!types) return false;
      for (var i = 0; i < types.length; i++) if (types[i] === 'Files') return true;
      return false;
    }
    // `dragenter` and `dragleave` fire again for every element the pointer
    // crosses on the way in, so a depth count is what tells the page's edge
    // from the boundary between two paragraphs inside it.
    var depth = 0;
    function undim() { depth = 0; if (stagedBox) stagedBox.classList.remove('dropping'); }
    document.addEventListener('dragenter', function (e) {
      if (!carriesFiles(e)) return;
      depth++;
      if (stagedBox) stagedBox.classList.add('dropping');
    });
    document.addEventListener('dragleave', function (e) {
      if (!carriesFiles(e)) return;
      if (--depth <= 0) undim();
    });
    // Without cancelling `dragover` the drop never happens — the browser reads
    // the absence as "nothing here takes this" and opens the file itself.
    document.addEventListener('dragover', function (e) {
      if (carriesFiles(e)) e.preventDefault();
    });
    document.addEventListener('drop', function (e) {
      if (!carriesFiles(e)) return;
      e.preventDefault();
      undim();
      var files = e.dataTransfer.files;
      if (files && files[0]) stage(files[0]);
    });

    if (stagedClear) stagedClear.addEventListener('click', unstage);

    // The one button. A staged file is what it sends; with nothing staged it is
    // the form's own submit and behaves exactly as it always did. Text typed
    // above a staged file is not thrown away — it goes as the capture it is,
    // in the same press.
    // The Capture verb. A staged file is what it sends; with nothing staged it
    // posts what is in the box. Text typed above a staged file is not thrown
    // away — it goes as the capture it is, in the same press.
    //
    // The box's own form is a GET that searches, so the text cannot ride it:
    // this posts to the same `/ui/capture` the old form did, with the same
    // fields, and lands in the same fragment.
    function postText() {
      var text = box.value.trim();
      if (!text) return;
      var fromAsk = document.querySelector('input[name="from_ask"]');
      htmx.ajax('POST', '/ui/capture', {
        target: '#capture-result',
        swap: 'innerHTML',
        values: { text: text, from_ask: fromAsk ? fromAsk.value : '' }
      }).then(function () {
        // Cleared only on the path that stored something: a failed capture
        // that emptied the box would lose the text it failed to keep.
        box.value = '';
        box.dispatchEvent(new Event('input', { bubbles: true }));
        htmx.trigger(document.body, 'captured');
      });
    }
    verb.addEventListener('click', function (e) {
      e.preventDefault();
      if (staged) {
        var file = staged;
        unstage();
        send(file);
      }
      postText();
    });
    // A pasted screenshot goes the same way as a dropped one — unless the
    // paste is on its way somewhere that takes text. A clipboard from Sheets,
    // Excel or Word carries the selection twice, as text and as a picture of
    // itself, so taking the picture while the caret sat in the note box
    // staged a screenshot and swallowed the paste the user asked for.
    document.addEventListener('paste', function (e) {
      var t = e.target;
      if (t && t.closest && t.closest('textarea, input, [contenteditable]')) return;
      var items = (e.clipboardData && e.clipboardData.items) || [];
      for (var i = 0; i < items.length; i++) {
        if (items[i].kind === 'file' && items[i].type.indexOf('image/') === 0) {
          e.preventDefault();
          stage(items[i].getAsFile());
          return;
        }
      }
    });
    }

  document.addEventListener('DOMContentLoaded', function () {
    enhance(document.body);
    commandBar();
    claimPaste();
    themeToggle();
    keyHint();
    primeSlow();
    contextOffer();
    restoreReading();
    boxVerbs();
    captureVerb();
    askDriver();
    trackDwell();
    window.addEventListener('pagehide', flushDwell);
    document.addEventListener('visibilitychange', function () {
      if (document.visibilityState === 'hidden') flushDwell();
      else trackDwell();
    });
    // A session that ended while the page stayed open. htmx swaps nothing on a
    // 4xx by design, so every click after that point did visibly nothing at
    // all — the one case the server cannot fix, because a fragment request
    // must not be answered with a full-page redirect into a login. Handled
    // here instead, and it names the page so signing in comes back to it.
    document.body.addEventListener('htmx:responseError', function (e) {
      if (!e.detail || !e.detail.xhr || e.detail.xhr.status !== 401) return;
      var here = window.location.pathname + window.location.search;
      window.location.assign('/auth/login?go=' + encodeURIComponent(here));
    });
    document.body.addEventListener('htmx:afterSwap', function (e) {
      // The offer's own fetch can land after the first keystroke has already
      // dismissed it. Swapping it back in then would be exactly the flicker the
      // removal exists to prevent.
      if (e.target.id === 'context-offer') {
        if (offerDismissed) dropOffer();
        else confirmOffer(e.target);
      }
      enhance(e.target);
      trackDwell();
      // The pane now holds something, so a narrow screen can hide the rail.
      var ws = document.querySelector('.regions');
      if (ws && e.target.id === 'pane') ws.classList.add('has-selection');
      // A fresh list is the answer to a new query or chip, so a narrow screen
      // shows it again rather than leaving the result you opened on screen
      // over results that have since changed underneath it.
      //
      // `#results` rather than `#rail`: the list is its own element inside the
      // rail now, so that what a search replaces is the results and not the
      // sitting beside them.
      if (e.target.id === 'results') {
        if (ws) ws.classList.remove('has-selection');
        // Back to the top of the answer. Nothing moved the scroll on a swap,
        // and the two layouts strand it in different places for the same
        // reason: whatever you had scrolled to in the last list is kept, and
        // the new list is drawn under it. On a phone the search box is the
        // fixed bar at the *bottom*, so typing leaves the window scrolled to
        // the end of the page — and the answer to what you just typed opened
        // at its last result, with the best hits somewhere above the top of
        // the screen.
        var railEl = document.getElementById('rail');
        if (railEl) {
          // Wide: the rail is its own scroll box (see 40-search.css).
          railEl.scrollTop = 0;
          // Narrow: `max-height: none`, so the window is the scroller. Only
          // ever scrolls up, and only as far as the list's own top — a page
          // that jumped on every keystroke would be worse than the bug.
          var top = railEl.getBoundingClientRect().top;
          if (top < 0) window.scrollBy(0, top);
        }
      }
      // A clicked result was never marked as the open one. `aria-selected` was
      // set only by the arrow-key handler below, so the styling for an open
      // card — the accent border, and dropping the snippet the pane beside it
      // is already showing in full — applied to keyboard navigation and to
      // nothing else: clicking left the whole list looking unselected while
      // its own pane was on screen.
      //
      // Matched on the href rather than on the click, because the pane is also
      // swapped by the Related and Seen-together links inside it, and those
      // move the selection just as truly as a click in the rail does.
      if (e.target.id === 'pane') {
        var open = window.location.pathname;
        document.querySelectorAll('.rail-item').forEach(function (el) {
          el.setAttribute('aria-selected', el.getAttribute('href') === open ? 'true' : 'false');
        });
      }
    });
  });

  // Focused only where a pointer says there is a hardware keyboard. On a touch
  // screen the software keyboard covers what the page was opened to show — the
  // results on Search, the pending decisions and recent captures on Capture,
  // which is the app's start page — and in an installed window there is no URL
  // bar to dismiss it from. This is why neither field carries `autofocus`.
  var field = document.querySelector('input[name="q"], textarea[name="text"]');
  if (field && window.matchMedia('(hover: hover)').matches) field.focus();

  // Whether something is being typed into. Every letter shortcut below is
  // gated on this: a letter belongs to the field that has focus, and nothing
  // else, which is the rule the judge shortcuts already follow.
  function typing() {
    var el = document.activeElement;
    if (!el) return false;
    var tag = el.tagName;
    return tag === 'INPUT' || tag === 'TEXTAREA' || el.isContentEditable;
  }

  // The rail is a list: arrows move through it, Enter opens what is focused.
  // j and k do the same, for hands that never left the home row.
  document.addEventListener('keydown', function (e) {
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    var down = e.key === 'ArrowDown' || e.key === 'j';
    var up = e.key === 'ArrowUp' || e.key === 'k';
    if (!down && !up) return;
    if ((e.key === 'j' || e.key === 'k') && typing()) return;
    var items = Array.prototype.slice.call(document.querySelectorAll('.rail-item'));
    if (!items.length) return;
    var i = items.indexOf(document.activeElement);
    var next = down ? Math.min(i + 1, items.length - 1) : Math.max(i - 1, 0);
    if (i === -1) next = 0;
    items.forEach(function (el) { el.setAttribute('aria-selected', 'false'); });
    items[next].setAttribute('aria-selected', 'true');
    items[next].focus();
    e.preventDefault();
  });
  // The keys that are the same on every page. `/` reaches the query without a
  // pointer, Esc steps back one region — which on a narrow window is the only
  // way back to a list that has been replaced — and `s` and `r` are the two
  // things the search page can show more or less of.
  document.addEventListener('keydown', function (e) {
    if (e.metaKey || e.ctrlKey || e.altKey) return;

    if (e.key === '/' && !typing()) {
      var q = document.querySelector('input[name="q"]');
      if (q) { e.preventDefault(); q.focus(); q.select(); }
      return;
    }
    if (e.key === 'Escape') {
      // Out of the field first. Only once nothing is focused does Escape mean
      // "back", or typing a query you thought better of would throw away the
      // results behind it.
      if (typing()) { document.activeElement.blur(); return; }
      // `.back` is in the DOM wherever an artifact is, and `display: none`
      // above 60rem — and on the standalone artifact page at every width.
      // Testing for existence alone made Escape a browser Back on a wide
      // window, where the list is already on screen and there is no region to
      // step out of. `offsetParent` is null exactly when it is not rendered.
      var back = document.querySelector('.back');
      if (back && back.offsetParent !== null) { e.preventDefault(); back.click(); }
      return;
    }
    if (typing()) return;
    var regions = document.querySelector('.regions');
    if (!regions) return;
    // Scoped to what is actually on the page, not merely to the grid. `s` is
    // also the judge's "skip", and every page has a `.regions` — without this,
    // one keypress on the judge queue fired a verdict and toggled something
    // that page does not have.
    //
    // The source is the second half of the `.split` inside the artifact, so
    // this hides that half and gives the prose the whole column. Off by
    // default: showing an artifact beside the lines it came from is the point
    // of the page, and s is for the times you have already checked.
    //
    // Gated on the source itself rather than on the split around it. A merged
    // artifact has no `.raw` — its second half is the lineage — so the split
    // was there, the key fired, and `s` hid a merge's provenance under a word
    // that says source.
    if (e.key === 's' && document.querySelector('.raw')) {
      e.preventDefault();
      regions.classList.toggle('hide-source');
      return;
    }
    if (e.key === 'r' && document.querySelector('.region-rail')) {
      e.preventDefault();
      var on = regions.classList.toggle('reading');
      // Remembered: this is a way of working rather than a choice made once
      // per visit.
      try { localStorage.setItem('engram.reading', on ? '1' : '0'); } catch (err) {}
    }
  });

  // Judging has to cost about five seconds, or it will not happen. Digits pick
  // an option, N/S/X take the three ways out. Ignored while a text field has
  // focus, so typing in the assignment search does not fire a verdict.
  document.addEventListener('keydown', function (e) {
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    var tag = document.activeElement && document.activeElement.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA') return;

    // Ahead of the card check, and not gated on one: the digit that has just
    // been regretted is the one that judged the last event, and if it emptied
    // the queue there is no card left to hang the shortcut off.
    if (e.key.toLowerCase() === 'u') {
      var undo = document.querySelector('.judge-undo');
      if (undo) { e.preventDefault(); undo.click(); }
      return;
    }
    var card = document.querySelector('.judge-card');
    if (!card) return;

    if (/^[1-9]$/.test(e.key)) {
      // Disabled options are skipped, matching the badges: a deprecated or
      // superseded candidate is shown at its place in the pool but carries no
      // digit, and the numbering runs over the choosable ones without a gap.
      var pick = card.querySelectorAll('.judge-option:not([disabled])')[Number(e.key) - 1];
      if (pick) { e.preventDefault(); pick.click(); }
      return;
    }
    // By name, never by position: the assign screen carries a judge-outs row of
    // its own whose first button is an immediate gap verdict, and N landed on
    // it whenever focus had left the search box — opening a "Read it in full"
    // is enough. Only the buttons that declare a key answer to one.
    var key = e.key.toLowerCase();
    if (/^[nsx]$/.test(key)) {
      var out = card.querySelector('.judge-outs button[data-key="' + key + '"]');
      if (out) { e.preventDefault(); out.click(); }
    }
  });
})();
