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

  function enhance(root) {
    if (!root || root.nodeType !== 1) return;
    highlight(root);
    clamp(root);
    copyButtons(root);
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
    var progress = document.getElementById('ask-progress');
    var rail = document.getElementById('ask-rail');
    var result = document.getElementById('ask-result');
    var status = document.getElementById('ask-status');
    var source = null;
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
      form.classList.remove('asking');
    }

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
      reasoning.hidden = true;
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
        reasoning.hidden = false;
        reasoning.appendChild(document.createTextNode(JSON.parse(e.data).text));
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
        reasoning.hidden = true;
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
      reasoning.hidden = true;
      form.classList.add('asking');

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

  document.addEventListener('DOMContentLoaded', function () {
    enhance(document.body);
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
      enhance(e.target);
      trackDwell();
      // The pane now holds something, so a narrow screen can hide the rail.
      var ws = document.querySelector('.workspace');
      if (ws && e.target.id === 'pane') ws.classList.add('has-selection');
      // A fresh list is the answer to a new query or chip, so a narrow screen
      // shows it again rather than leaving the result you opened on screen
      // over results that have since changed underneath it.
      if (ws && e.target.id === 'rail') ws.classList.remove('has-selection');
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

  // The rail is a list: arrows move through it, Enter opens what is focused.
  document.addEventListener('keydown', function (e) {
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    var items = Array.prototype.slice.call(document.querySelectorAll('.rail-item'));
    if (!items.length) return;
    var i = items.indexOf(document.activeElement);
    var next = e.key === 'ArrowDown' ? Math.min(i + 1, items.length - 1) : Math.max(i - 1, 0);
    if (i === -1) next = 0;
    items.forEach(function (el) { el.setAttribute('aria-selected', 'false'); });
    items[next].setAttribute('aria-selected', 'true');
    items[next].focus();
    e.preventDefault();
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
