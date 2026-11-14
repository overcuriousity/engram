// Client-side, walking text nodes only, so it can never add an element the server's sanitizer disallowed.
(function () {
  'use strict';

  // Fired when the box's surroundings change (a staged file, a finished answer). Not `input`:
  // `contextOffer` takes `input` as the first keystroke and would drop an untouched offer.
  var VERB_SYNC = 'engram:verbsync';
  // Fired once the held state is swapped in; see `syncHeld`.
  var HELD_SYNC = 'engram:held';

  // The worker is what makes the browser offer to install. Plain HTTP on a LAN has neither API.
  if ('serviceWorker' in navigator && window.isSecureContext) {
    window.addEventListener('load', function () {
      navigator.serviceWorker.register('/sw.js').catch(function (e) {
        console.warn('service worker registration failed', e);
      });
    });
  }

  // Chrome fires this once, early, only to a listener already present.
  var installPrompt = null;
  window.addEventListener('beforeinstallprompt', function (e) {
    e.preventDefault();
    installPrompt = e;
    installNudge();
  });

  // Phone browsers only, weekly until installed or declined: "Not now" means not now.
  var INSTALL_KEY = 'engram.install-nudged';
  var INSTALL_WEEK = 7 * 24 * 60 * 60 * 1000;
  function installNudge() {
    var nudge = document.querySelector('.installnudge');
    if (!nudge || !nudge.hidden) return;
    var installed = window.matchMedia('(display-mode: standalone)').matches ||
      window.navigator.standalone === true;
    if (installed) return;
    var phone = window.matchMedia('(pointer: coarse)').matches &&
      window.matchMedia('(max-width: 40rem)').matches;
    if (!phone) return;
    var last = 0;
    try { last = parseInt(localStorage.getItem(INSTALL_KEY) || '0', 10) || 0; } catch (e) { return; }
    if (Date.now() - last < INSTALL_WEEK) return;
    // Safari's only route is the Share sheet; other browsers without a prompt get no banner.
    var ios = /iPhone|iPad|iPod/.test(navigator.userAgent) && !window.MSStream;
    if (!installPrompt && !ios) return;
    function stamp() {
      try { localStorage.setItem(INSTALL_KEY, String(Date.now())); } catch (e) {}
      nudge.hidden = true;
    }
    if (installPrompt) {
      var button = nudge.querySelector('[data-install]');
      button.hidden = false;
      button.addEventListener('click', function () {
        var installEvent = installPrompt;
        installPrompt = null;
        stamp();
        if (installEvent) installEvent.prompt.call(installEvent);
      });
    } else {
      nudge.querySelector('[data-install-how]').hidden = false;
    }
    nudge.querySelector('[data-dismiss-install]').addEventListener('click', stamp);
    window.addEventListener('appinstalled', stamp);
    nudge.hidden = false;
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

  // Visual only: the text is never truncated, so a fenced command is never cut.
  function clamp(root) {
    root.querySelectorAll('.clampable:not([data-clamped])').forEach(function (el) {
      el.setAttribute('data-clamped', 'yes');
      if (el.scrollHeight <= el.clientHeight + 4) return;
      el.classList.add('is-clamped');
      var btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'oc-btn expand';
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

  // Runs of three or more blank lines fold to a counted rule; rows inside the extraction range never fold.
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

  // Proportional, not line-mapped: prose and source do not share a line count. Bound to the card, not the
  // column (whose neighbour lists are not this artifact). The guard stops the two scroll events ping-ponging.
  function lockstep(root) {
    var split = root.querySelector('.split');
    if (!split || split.dataset.lockstep) return;
    var artifact = split.children[0] && split.children[0].querySelector('.artifact-main');
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
    splitHandles(root);
  }

  // Movable boundaries: each grid's first track is a custom property. In `localStorage`, not the base:
  // a reading width is a habit of one browser on one screen.
  function rem() {
    return parseFloat(getComputedStyle(document.documentElement).fontSize) || 16;
  }

  function clampTo(v, lo, hi) {
    return Math.min(Math.max(v, lo), Math.max(lo, hi));
  }

  // `at` reads the boundary off the page, so the keyboard works whatever unit the property is in.
  var AXES = {
    x: {
      cls: 'col-handle', orient: 'vertical', body: 'col-dragging',
      pos: function (e, rect) { return e.clientX - rect.left; },
      at: function (h) { return h.offsetLeft; },
      step: function (k) { return k === 'ArrowLeft' ? -1 : k === 'ArrowRight' ? 1 : 0; },
    },
    y: {
      cls: 'row-handle', orient: 'horizontal', body: 'row-dragging',
      pos: function (e, rect) { return e.clientY - rect.top; },
      at: function (h, rect) { return h.getBoundingClientRect().top - rect.top; },
      step: function (k) { return k === 'ArrowUp' ? -1 : k === 'ArrowDown' ? 1 : 0; },
    },
  };

  // `also` carries properties implied by the remembered value, which are not remembered themselves.
  function pin(el, spec, v) {
    el.style.setProperty(spec.prop, v);
    for (var k in spec.also || {}) el.style.setProperty(k, spec.also[k]);
  }

  // `spec`: `axis`, `prop` (the grid's property), `key` (storage), `value(pos, rect)`, `nudge(rect)`, `place`.
  function columnHandle(el, spec) {
    var ax = AXES[spec.axis || 'x'];
    if (!el || el.querySelector(':scope > .' + ax.cls)) return;
    try {
      var kept = localStorage.getItem(spec.key);
      if (kept) pin(el, spec, kept);
    } catch (e) {}

    var h = document.createElement('button');
    h.type = 'button';
    h.className = ax.cls;
    // A separator to assistive technology; the arrow keys below are what the role promises.
    h.setAttribute('role', 'separator');
    h.setAttribute('aria-orientation', ax.orient);
    h.setAttribute('aria-label', spec.label);
    h.title = spec.label + ' — drag to move, double-click to reset';
    (spec.place || function (p, n) { p.appendChild(n); })(el, h);

    function put(pos, rect) {
      pin(el, spec, spec.value(pos, rect));
    }

    function remember() {
      try {
        localStorage.setItem(spec.key, el.style.getPropertyValue(spec.prop));
      } catch (e) {}
    }

    h.addEventListener('pointerdown', function (e) {
      if (e.pointerType === 'mouse' && e.button !== 0) return;
      // Captured: the narrow handle loses the pointer immediately.
      h.setPointerCapture(e.pointerId);
      h.classList.add('dragging');
      document.body.classList.add(ax.body);
      e.preventDefault();
    });

    h.addEventListener('pointermove', function (e) {
      if (!h.classList.contains('dragging')) return;
      var rect = el.getBoundingClientRect();
      put(ax.pos(e, rect), rect);
    });

    function release(e) {
      if (!h.classList.contains('dragging')) return;
      h.classList.remove('dragging');
      document.body.classList.remove(ax.body);
      try {
        h.releasePointerCapture(e.pointerId);
      } catch (err) {}
      remember();
    }
    h.addEventListener('pointerup', release);
    h.addEventListener('pointercancel', release);

    h.addEventListener('keydown', function (e) {
      var step = ax.step(e.key);
      if (!step) return;
      e.preventDefault();
      var rect = el.getBoundingClientRect();
      put(ax.at(h, rect) + step * spec.nudge(rect), rect);
      remember();
    });

    // Double-click restores the stylesheet's own answer.
    h.addEventListener('dblclick', function () {
      el.style.removeProperty(spec.prop);
      for (var k in spec.also || {}) el.style.removeProperty(k);
      try {
        localStorage.removeItem(spec.key);
      } catch (e) {}
    });
  }

  // In pixels; the floors keep both sides readable, and the pane's wins on a narrow window.
  function railHandle() {
    columnHandle(document.querySelector('.regions-rail-focus-source'), {
      prop: '--rail-w',
      key: 'engram.rail-w',
      label: 'Results width',
      nudge: function () {
        return rem();
      },
      value: function (x, rect) {
        var u = rem();
        // 30rem for the pane plus the 1rem gap.
        return Math.round(clampTo(x, 15 * u, rect.width - 31 * u)) + 'px';
      },
    });
  }

  // A share: the pane is not the same width in the workspace as on the standalone page.
  function splitHandles(root) {
    var splits = root.matches && root.matches('.split') ? [root] : [];
    if (root.querySelectorAll) {
      splits = splits.concat(Array.prototype.slice.call(root.querySelectorAll('.split')));
    }
    splits.forEach(function (s) {
      columnHandle(s, {
        prop: '--split-l',
        key: 'engram.split-l',
        label: 'Artifact width',
        nudge: function (rect) {
          return rect.width * 0.02;
        },
        value: function (x, rect) {
          if (!rect.width) return '45.45%';
          return clampTo((x / rect.width) * 100, 22, 78).toFixed(2) + '%';
        },
      });
      cardHandle(s.firstElementChild);
    });
  }

  // The artifact against the neighbour lists under it, in pixels: it sizes lines of prose.
  function cardHandle(col) {
    if (!col || !col.matches) return;
    var card = col.querySelector(':scope > .artifact-main');
    // A fresh artifact has no neighbours yet.
    if (!card || !col.querySelector(':scope > .related')) return;
    columnHandle(col, {
      axis: 'y',
      prop: '--artifact-h',
      // Otherwise the card grows back over what the drag gave the lists.
      also: { '--artifact-fill': '0' },
      key: 'engram.artifact-h',
      label: 'Artifact height',
      // In the card's own bottom margin, so offering the boundary moves nothing.
      place: function (el, h) {
        card.parentNode.insertBefore(h, card.nextSibling);
      },
      nudge: function () {
        return rem();
      },
      // `top` is under the label, not zero. 7rem: the lists' 5rem floor plus their label.
      value: function (y, rect) {
        var u = rem();
        var top = card.getBoundingClientRect().top - rect.top;
        return Math.round(clampTo(y - top, 8 * u, rect.height - top - 7 * u)) + 'px';
      },
    });
  }

  // Ask and Decide share the box: one model call at a time, one Stop button, one ticking spinner.
  var boxRun = { started: 0, ticker: null, stops: [] };

  // Read-only, not disabled, so the question stays selectable and focused; it fires no `input`.
  // Cleared by each driver's `stop()`, which every exit runs, including a transport error.
  function boxBusy(form, busy) {
    var box = form.querySelector('textarea[name="q"]');
    var spinner = document.getElementById('search-spinner');
    if (boxRun.ticker) { clearInterval(boxRun.ticker); boxRun.ticker = null; }
    document.getElementById('ask-stop').hidden = !busy;
    spinner.textContent = 'searching…';
    form.classList.toggle('asking', busy);
    box.readOnly = busy;
    form.setAttribute('aria-busy', busy ? 'true' : 'false');
    var vs = form.querySelectorAll('[data-verb]');
    for (var i = 0; i < vs.length; i++) vs[i].disabled = busy;
    // A chip clicked mid-answer would swap a "0 results" rail over the citations.
    var chips = form.querySelectorAll('#kind-chips input');
    for (var j = 0; j < chips.length; j++) chips[j].disabled = busy;
    if (busy) {
      // From the press: retrieval before the model is most of the wait.
      boxRun.started = Date.now();
      boxRun.ticker = setInterval(function () {
        spinner.textContent = 'thinking… ' + boxSecs() + 's';
      }, 1000);
    } else {
      // A box emptied while streaming must not come back with live verbs.
      box.dispatchEvent(new Event(VERB_SYNC, { bubbles: true }));
    }
  }

  // One Stop for every verb, live from the press: a run still parking its question has no stream
  // yet, and `boxStopAll` bumps each driver's generation so that stream is never opened.
  function boxStop() {
    var form = document.getElementById('box-form');
    var btn = document.getElementById('ask-stop');
    if (!form || !btn) return;
    btn.addEventListener('click', function () {
      if (form.getAttribute('aria-busy') !== 'true') return;
      var secs = boxSecs();
      boxStopAll();
      // What arrived stays: the reader stopped the wait, not the answer.
      document.getElementById('ask-status').textContent = 'stopped after ' + secs + ' seconds';
    });
  }

  function boxSecs() {
    return Math.round((Date.now() - boxRun.started) / 1000);
  }

  // Ends every stream from the box and drops its late frames; a new run starts after it.
  function boxStopAll() {
    boxRun.stops.forEach(function (f) { f(); });
  }

  // The rail holds this act's passages; the button re-runs the box's search rather than keeping them.
  function railHeadBack(head, text) {
    head.innerHTML = '';
    var label = document.createElement('span');
    label.className = 'result-count';
    label.textContent = text;
    var back = document.createElement('button');
    back.type = 'button';
    back.className = 'oc-btn';
    back.setAttribute('data-rerun', '');
    back.textContent = 'Back to results';
    head.appendChild(label);
    head.appendChild(back);
  }

  // Ask: POST parks the question and the GET stream spends the id once, since a GET running a model call
  // would be a free-inference hole. The page only moves server-rendered HTML or sets text.
  function askDriver() {
    var form = document.getElementById('box-form');
    if (!form) return;
    // Delegated: the Ask verb can arrive out-of-band later, on the first capture (see `syncHeld`).
    var box = form.querySelector('textarea[name="q"]');
    if (!box) return;
    var live = document.getElementById('ask-live');
    var reasoning = document.getElementById('ask-reasoning');
    // The disclosure is shown or hidden; text streams into the div inside.
    var reasoningBox = document.getElementById('ask-reasoning-box');
    var progress = document.getElementById('ask-progress');
    var rail = document.getElementById('results');
    var result = document.getElementById('ask-result');
    var status = document.getElementById('ask-status');
    var source = null;
    // Which ask the page belongs to. See the submit handler.
    var generation = 0;

    // Every exit from a stream comes through here. A left-open `EventSource` reconnects by itself,
    // and a reconnect could be a second model call and a second bill.
    function stop() {
      if (source) { source.close(); source = null; }
      boxBusy(form, false);
    }
    boxRun.stops.push(function () { generation++; stop(); });

    // The one gate every htmx request from this form passes; a debounce armed before Ask, or the
    // "← results" anchor, would otherwise swap a rail over the citations.
    form.addEventListener('htmx:configRequest', function (e) {
      if (form.getAttribute('aria-busy') === 'true') e.preventDefault();
    });

    function fail(message) {
      stop();
      // No citations event is coming to rewrite the heading over the emptied rail.
      var head = document.getElementById('rail-head');
      if (head) head.textContent = '';
      result.textContent = '';
      // textContent inside: an error string never went through the sanitizer.
      result.appendChild(notice('danger', message));
      live.hidden = true;
      reasoningBox.hidden = true;
      progress.hidden = true;
    }

    function openStream(id, mine) {
      // A superseded ask's id is never spent: one fewer model call started.
      if (mine !== generation) return;
      source = new EventSource('/ui/ask/' + encodeURIComponent(id) + '/stream');

      // An event queued from an older stream must not write into the newer answer.
      function current() { return mine === generation; }

      source.addEventListener('citations', function (e) {
        if (!current()) return;
        // The `cite-n` anchors in here are the targets of the answer's `[n]` links.
        var payload = JSON.parse(e.data);
        rail.innerHTML = payload.rail;
        enhance(rail);
        // The rail now holds this act's excerpts; the anchor re-runs the box's search rather than keeping stale results.
        var head = document.getElementById('rail-head');
        if (head) railHeadBack(head, 'Written from · ' + rail.querySelectorAll('.rail-item').length);
      });
      // Shows the fan-out's queries; with `plan = false` this never fires.
      source.addEventListener('needs', function (e) {
        if (!current()) return;
        progress.hidden = false;
        // textContent: model output that went through no renderer.
        progress.textContent = 'Looking further: ' +
          JSON.parse(e.data).queries.join(', ');
      });
      source.addEventListener('retrieved', function (e) {
        if (!current()) return;
        var round = JSON.parse(e.data);
        // Round one runs on every ask; only the fan-out is worth a line.
        if (round.round < 2) return;
        progress.hidden = false;
        // Appended to what `needs` wrote. Searched beside shown: "26 left out" would read as failures.
        progress.textContent = (progress.textContent ?
          progress.textContent + ' \u2014 ' : 'Round ' + round.round + ': ') +
          'searched ' + round.retrieved + ', showing ' + round.shown;
      });
      source.addEventListener('reasoning', function (e) {
        if (!current()) return;
        reasoningBox.hidden = false;
        reasoning.appendChild(document.createTextNode(JSON.parse(e.data).text));
        // A closed box has no scroll position to keep.
        if (reasoningBox.open) reasoning.scrollTop = reasoning.scrollHeight;
      });
      source.addEventListener('token', function (e) {
        if (!current()) return;
        live.hidden = false;
        // Followed while the reader is at the tail, left alone once they scroll up.
        var atTail = live.scrollHeight - live.scrollTop - live.clientHeight < 40;
        live.appendChild(document.createTextNode(JSON.parse(e.data).text));
        if (atTail) live.scrollTop = live.scrollHeight;
      });
      source.addEventListener('done', function (e) {
        if (!current()) return;
        stop();
        result.innerHTML = JSON.parse(e.data).html;
        enhance(result);
        // htmx binds `hx-post` only on markup it swapped or was told about.
        if (window.htmx) window.htmx.process(result);
        // Hidden rather than emptied, so the next ask reuses them.
        live.hidden = true;
        reasoningBox.hidden = true;
        // `progress` stays: it still describes the rail. Announced once; the live region only said it was coming.
        status.textContent = 'The answer is ready.';
      });
      // The browser's transport error is also `error`, and it is already queuing a reconnect, so `stop()` runs.
      source.addEventListener('error', function (e) {
        if (!current()) return;
        fail(e.data ? e.data : 'The connection to the answer stream was lost.');
      });
    }

    form.addEventListener('click', function (e) {
      var askBtn = e.target.closest ? e.target.closest('[data-verb="ask"]') : null;
      // A disabled button fires no click, but its form does.
      if (!askBtn || askBtn.disabled) return;
      e.preventDefault();
      var q = box.value;
      if (!q.trim()) return;
      // The ask door renders `idle_state` with the box filled and no `input` coming, so reveal the regions here.
      hideIdle();
      // Two submits before the first POST resolves would open two streams, the first unclosable.
      // Anything from an older generation is dropped, including the stream it was about to open.
      boxStopAll();
      var mine = ++generation;
      status.textContent = '';
      live.textContent = '';
      reasoning.textContent = '';
      progress.textContent = '';
      progress.hidden = true;
      rail.textContent = '';
      result.textContent = '';
      // An artifact left in the pane belongs to the superseded search.
      var pane = document.getElementById('pane-content');
      if (pane) pane.textContent = '';
      live.hidden = true;
      reasoningBox.hidden = true;
      reasoningBox.open = false;
      // `answering`, not `has-selection`, which narrow reads as "hide the rail": the `[n]` links point into it.
      var regions = document.querySelector('.regions');
      // `pane-open` describes `#pane-content`, which the answer now replaces.
      if (regions) {
        regions.classList.remove('pane-open');
        regions.classList.add('answering');
      }
      boxBusy(form, true);

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
        // A stale rejection must not replace a live answer.
        if (mine !== generation) return;
        fail(err.message || 'The question could not be sent.');
      });
    });

    // Delegated: the links arrive with the answer.
    document.addEventListener('click', function (e) {
      var link = e.target.closest ? e.target.closest('a.cite') : null;
      if (!link) return;
      var target = document.getElementById(link.getAttribute('href').slice(1));
      // Without its anchor the jump does nothing, where scrolling elsewhere would look like provenance.
      if (!target) return;
      e.preventDefault();
      rail.querySelectorAll('.rail-active').forEach(function (el) {
        el.classList.remove('rail-active');
      });
      target.classList.add('rail-active');
      target.scrollIntoView({ block: 'center', behavior: 'smooth' });
    });
  }

  // Decide: parked and streamed as ask is, against the options in `#decide-row`; the rail fills as
  // each passage is read. Text from the stream is set as text; only server-rendered fragments are HTML.
  function decideDriver() {
    var form = document.getElementById('box-form');
    var row = document.getElementById('decide-row');
    if (!form || !row) return;
    var box = form.querySelector('textarea[name="q"]');
    var progress = document.getElementById('ask-progress');
    var rail = document.getElementById('results');
    var result = document.getElementById('ask-result');
    var status = document.getElementById('ask-status');
    var head = document.getElementById('rail-head');
    var said = document.getElementById('decide-problem');
    var lists = {
      choice: document.getElementById('decide-options'),
      score: document.getElementById('decide-levels'),
    };
    var source = null;
    // As ask's: a frame from a superseded decision must not write into this one.
    var generation = 0;

    function stop() {
      if (source) { source.close(); source = null; }
      boxBusy(form, false);
    }
    boxRun.stops.push(function () { generation++; stop(); });

    function checked() {
      return row.querySelector('input[name="decide-kind"]:checked');
    }

    function expand(on) {
      row.hidden = !on;
      var btn = form.querySelector('[data-verb="decide"]');
      if (btn) btn.setAttribute('aria-expanded', on ? 'true' : 'false');
      if (on && checked()) checked().focus();
      if (!on) box.focus();
    }

    function shape() {
      var kind = checked() ? checked().value : 'noul';
      return {
        kind: kind,
        options: Array.prototype.map.call(lists.choice.querySelectorAll('.decide-option'), function (o) {
          return {
            name: o.querySelector('[name="decide-option"]').value,
            description: o.querySelector('[name="decide-desc"]').value,
          };
        }),
        levels: Array.prototype.map.call(lists.score.querySelectorAll('[name="decide-level"]'), function (i) {
          return i.value;
        }),
      };
    }

    function line(text) {
      progress.textContent = text;
      progress.hidden = !text;
    }

    function fail(message) {
      stop();
      head.textContent = '';
      result.textContent = '';
      // textContent inside: an error string never went through the sanitizer.
      result.appendChild(notice('danger', message));
      line('');
    }

    function openStream(id, mine) {
      if (mine !== generation) return;
      var D = window.engramDecide;
      source = new EventSource('/ui/decide/' + encodeURIComponent(id) + '/stream');
      function current() { return mine === generation; }
      var read = 0;
      var bearing = 0;
      source.addEventListener('retrieved', function (e) {
        if (!current()) return;
        head.textContent = 'Reading';
        line(D.progressText(0, JSON.parse(e.data).passages, 0));
      });
      source.addEventListener('read', function (e) {
        if (!current()) return;
        var p = JSON.parse(e.data);
        rail.insertAdjacentHTML('beforeend', p.row);
        // Bound now: a row clicked mid-stream must open in the pane, not navigate the decision away.
        if (window.htmx && rail.lastElementChild) window.htmx.process(rail.lastElementChild);
        read++;
        if (p.bearing) bearing++;
        line(D.progressText(read, p.of, bearing));
      });
      source.addEventListener('done', function (e) {
        if (!current()) return;
        stop();
        var p = JSON.parse(e.data);
        result.innerHTML = p.html;
        rail.innerHTML = p.rail;
        railHeadBack(head, p.rail_head);
        [result, rail].forEach(function (el) {
          enhance(el);
          // htmx binds `hx-*` only on markup it swapped or was told about.
          if (window.htmx) window.htmx.process(el);
        });
        line('');
        status.textContent = 'The decision is ready.';
      });
      // Also the browser's transport error, which is already queuing a reconnect.
      source.addEventListener('error', function (e) {
        if (!current()) return;
        // The server's frame is `{ error }`, as on `/api/v1/decide/stream`.
        var said = null;
        try { said = e.data ? JSON.parse(e.data).error : null; } catch (_) { said = null; }
        fail(said || 'The connection to the decision was lost. Press Decide to try again.');
      });
    }

    function run() {
      var D = window.engramDecide;
      var q = box.value;
      var r = shape();
      var why = D.problem(q, r);
      said.textContent = why;
      if (why) return;
      hideIdle();
      boxStopAll();
      var mine = ++generation;
      status.textContent = '';
      line('');
      head.textContent = '';
      rail.textContent = '';
      result.textContent = '';
      // Whatever a stopped ask left streaming, or an artifact, belongs to the superseded act.
      ['ask-live', 'ask-reasoning-box'].forEach(function (id) {
        var el = document.getElementById(id);
        if (el) el.hidden = true;
      });
      var pane = document.getElementById('pane-content');
      if (pane) pane.textContent = '';
      var regions = document.querySelector('.regions');
      if (regions) {
        regions.classList.remove('pane-open');
        regions.classList.add('answering');
      }
      boxBusy(form, true);
      fetch('/ui/decide', {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'content-type': 'application/json' },
        body: D.bodyText(q, r),
      }).then(function (res) {
        if (!res.ok) throw new Error('The decision was refused (' + res.status + '). Check the options and try again.');
        return res.json();
      }).then(function (out) {
        openStream(out.id, mine);
      }).catch(function (err) {
        if (mine !== generation) return;
        fail(err.message || 'The decision could not be sent. Try again.');
      });
    }

    // Delegated: the button arrives out of band on the first capture.
    form.addEventListener('click', function (e) {
      var btn = e.target.closest ? e.target.closest('[data-verb="decide"]') : null;
      if (!btn || btn.disabled) return;
      e.preventDefault();
      if (row.hidden) expand(true);
      else run();
    });

    row.addEventListener('change', function (e) {
      if (e.target.name !== 'decide-kind') return;
      lists.choice.hidden = e.target.value !== 'choice';
      lists.score.hidden = e.target.value !== 'score';
      said.textContent = '';
    });

    row.addEventListener('click', function (e) {
      var add = e.target.closest ? e.target.closest('[data-decide-add]') : null;
      if (!add || add.disabled) return;
      var one = add.getAttribute('data-decide-add');
      var cls = one === 'option' ? '.decide-option' : '.decide-level';
      var max = window.engramDecide.LIMITS[one === 'option' ? 'options' : 'levels'][1];
      var fresh = document.getElementById('decide-' + one + '-row').content.firstElementChild.cloneNode(true);
      add.parentNode.insertBefore(fresh, add);
      fresh.querySelector('input').focus();
      add.disabled = add.parentNode.querySelectorAll(cls).length >= max;
    });

    row.addEventListener('keydown', function (e) {
      if (e.key === 'Escape') {
        // Stopped here, or the page's Escape would blur the box it just focused.
        e.preventDefault();
        e.stopPropagation();
        expand(false);
        return;
      }
      if (e.key !== 'Enter' || e.isComposing) return;
      // A summary opens its description and a button does its own act.
      if (e.target.closest('summary, button')) return;
      e.preventDefault();
      // Through the button, so whatever disables it (busy, a staged file) holds here too.
      var btn = form.querySelector('[data-verb="decide"]');
      if (btn && !btn.disabled) { btn.click(); return; }
      var staged = document.getElementById('staged');
      var busy = form.getAttribute('aria-busy') === 'true';
      // An empty box is the one reason worth saying; the button cannot say it.
      if (!busy && !box.value.trim() && !(staged && !staged.hidden)) {
        said.textContent = window.engramDecide.problem(box.value, shape());
      }
    });
  }

  // Dwell: how long an artifact stayed open, sent as a beacon on leaving; under three seconds is not sent.
  // A pursuit signal only: the beacon lands after the verdict buttons and must not overwrite them.
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
    if (id) {
      dwell.id = id;
      dwell.since = Date.now();
    }
  }

  // The first capture leaves the page arranged for an empty base. `/ui/held` returns those regions out of
  // band from the same partials; `data-held` is written first, so it runs at most once.
  function syncHeld() {
    var form = document.getElementById('box-form');
    if (!form || form.getAttribute('data-held') !== '0') return;
    form.setAttribute('data-held', '1');
    // `swap: "none"`: every region names its own target, but a `target` is still required.
    htmx.ajax('GET', '/ui/held', { target: form, swap: 'none' })
      // The capture stored regardless; re-armed so the next one tries again.
      .catch(function () { form.setAttribute('data-held', '0'); })
      .then(function () {
        if (form.getAttribute('data-held') !== '1') return;
        var box = form.querySelector('textarea[name="q"]');
        if (box) box.dispatchEvent(new Event(HELD_SYNC, { bubbles: true }));
        // Decides whether there is a keyboard to show the row to.
        keyHint();
      });
  }

  // Until dismissed; never on a touch screen, which has no keys to teach.
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

  // Before anything is drawn, so a remembered reading mode does not flash the wide rail.
  function restoreReading() {
    var regions = document.querySelector('.regions');
    if (!regions || !document.querySelector('.region-rail')) return;
    try {
      if (localStorage.getItem('engram.reading') === '1') regions.classList.add('reading');
    } catch (e) {}
  }



  // A theme stored under the old key moves to the kit's, once.
  function migrateThemeKey() {
    try {
      var old = localStorage.getItem('engram.theme');
      if (old === null) return;
      if (old === 'light' || old === 'dark') {
        localStorage.setItem('oc.theme', old);
        if (document.documentElement.dataset.prefs === 'local') {
          document.documentElement.setAttribute('data-theme', old);
        }
      }
      localStorage.removeItem('engram.theme');
    } catch (e) {}
  }

  // Synchronous, since htmx reads `hx-vals js:` synchronously; async sources are cached at load.
  // No canvas, WebGL or font probing: constant for one person, and randomised by hardened browsers.
  var slow = {
    battery_level: null, charging: null,
    audio_outputs: null, audio_inputs: null, video_inputs: null,
    keyboard_layout: null, screens: null, place: null
  };

  function primeSlow() {
    if (navigator.getBattery) {
      navigator.getBattery().then(function (b) {
        slow.battery_level = b.level;
        slow.charging = b.charging;
      }).catch(function () {});
    }
    if (navigator.mediaDevices && navigator.mediaDevices.enumerateDevices) {
      navigator.mediaDevices.enumerateDevices().then(function (list) {
        var n = { audiooutput: 0, audioinput: 0, videoinput: 0 };
        list.forEach(function (d) { if (d.kind in n) n[d.kind] += 1; });
        slow.audio_outputs = n.audiooutput;
        slow.audio_inputs = n.audioinput;
        slow.video_inputs = n.videoinput;
      }).catch(function () {});
    }
    // Chromium only; the letters under Q, Y and A tell QWERTY, QWERTZ and AZERTY apart.
    if (navigator.keyboard && navigator.keyboard.getLayoutMap) {
      navigator.keyboard.getLayoutMap().then(function (m) {
        slow.keyboard_layout = (m.get('KeyQ') || '') + (m.get('KeyY') || '') + (m.get('KeyA') || '');
      }).catch(function () {});
    }
    // `isExtended` avoids the prompt `getScreenDetails` would raise.
    slow.screens = screen.isExtended ? 2 : 1;
    primePlace();
  }

  // How long a kept geohash still describes where this browser is.
  var PLACE_GOOD_FOR = 86400000;

  // Only behind the switch; low accuracy, reduced to a geohash here. The fix lands after this load's
  // bundle, so it is kept for the next one, as the Android app's last-known position is.
  function primePlace() {
    if (!placeOn()) return;
    try {
      var kept = (localStorage.getItem('engram:place_hash') || '').split(':');
      if (kept[0] && Date.now() - (parseInt(kept[1], 10) || 0) < PLACE_GOOD_FOR) slow.place = kept[0];
    } catch (e) {}
    if (!navigator.geolocation) return;
    navigator.geolocation.getCurrentPosition(function (pos) {
      slow.place = geohash(pos.coords.latitude, pos.coords.longitude, 6);
      try { localStorage.setItem('engram:place_hash', slow.place + ':' + Date.now()); } catch (e) {}
    }, function () {}, { enableHighAccuracy: false, maximumAge: 600000, timeout: 8000 });
  }

  function placeOn() {
    try { return localStorage.getItem('engram:place') === 'on'; } catch (e) { return false; }
  }

  // Switching off withdraws what was kept.
  function forgetPlace() {
    slow.place = null;
    try { localStorage.removeItem('engram:place_hash'); } catch (e) {}
  }

  function placeSwitch() {
    var el = document.getElementById('place-switch');
    if (!el) return;
    try { el.checked = localStorage.getItem('engram:place') === 'on'; } catch (e) {}
    el.addEventListener('change', function () {
      try { localStorage.setItem('engram:place', el.checked ? 'on' : 'off'); } catch (e) {}
      // Asked while the person looks at the switch they pressed.
      if (el.checked) primePlace(); else forgetPlace();
    });
  }

  // The same function as `Geohash.encode` in the app.
  function geohash(lat, lon, precision) {
    var chars = '0123456789bcdefghjkmnpqrstuvwxyz';
    var latR = [-90, 90], lonR = [-180, 180];
    var out = '', bit = 0, ch = 0, even = true;
    while (out.length < precision) {
      if (even) {
        var midLon = (lonR[0] + lonR[1]) / 2;
        if (lon >= midLon) { ch = (ch << 1) | 1; lonR[0] = midLon; }
        else { ch = ch << 1; lonR[1] = midLon; }
      } else {
        var midLat = (latR[0] + latR[1]) / 2;
        if (lat >= midLat) { ch = (ch << 1) | 1; latR[0] = midLat; }
        else { ch = ch << 1; latR[1] = midLat; }
      }
      even = !even;
      if (++bit === 5) { out += chars.charAt(ch); bit = 0; ch = 0; }
    }
    return out;
  }

  function media(q) {
    try { return matchMedia(q).matches; } catch (e) { return null; }
  }

  function navType() {
    var e = performance.getEntriesByType && performance.getEntriesByType('navigation')[0];
    return e && e.type ? e.type : null;
  }

  function referrerKind() {
    if (!document.referrer) return 'none';
    try {
      return new URL(document.referrer).origin === location.origin ? 'same_origin' : 'external';
    } catch (e) { return 'external'; }
  }

  // A visit after a week away and the ninth of a morning are different situations.
  function viewCounters() {
    var now = Date.now();
    var out = { since_last_view_s: null, views_today: null };
    try {
      var last = parseInt(localStorage.getItem('engram:last_view') || '0', 10) || 0;
      if (last) out.since_last_view_s = Math.round((now - last) / 1000);
      var day = localDay(now);
      var raw = (localStorage.getItem('engram:views') || '').split(':');
      var n = raw[0] === day ? (parseInt(raw[1], 10) || 0) : 0;
      n += 1;
      out.views_today = n;
      localStorage.setItem('engram:last_view', String(now));
      localStorage.setItem('engram:views', day + ':' + n);
    } catch (e) {}
    return out;
  }

  function uaFamily() {
    var d = navigator.userAgentData;
    if (d && d.brands) {
      for (var i = 0; i < d.brands.length; i++) {
        // Chromium's decoy brand.
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
    // `effectiveType` is speed, not medium; absent rather than invented.
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
      // Stored, not encoded. See the Bundle struct in core/context.rs.
      var conn = navigator.connection || navigator.mozConnection || {};
      b.place = slow.place;
      b.net_effective = conn.effectiveType || null;
      b.net_downlink = typeof conn.downlink === 'number' ? conn.downlink : null;
      b.rtt = typeof conn.rtt === 'number' ? conn.rtt : null;
      b.save_data = typeof conn.saveData === 'boolean' ? conn.saveData : null;
      b.reduced_motion = media('(prefers-reduced-motion: reduce)');
      b.high_contrast = media('(prefers-contrast: more)');
      b.pointer = media('(pointer: coarse)') ? 'coarse' : (media('(pointer: fine)') ? 'fine' : null);
      b.hover = media('(hover: hover)');
      b.display_mode = media('(display-mode: standalone)') ? 'standalone' : 'browser';
      b.nav_type = navType();
      b.window_state = (window.outerWidth >= screen.availWidth - 8 && window.outerHeight >= screen.availHeight - 8)
        ? 'maximised' : 'windowed';
      b.focused = document.hasFocus();
      var counters = viewCounters();
      b.since_last_view_s = counters.since_last_view_s;
      b.views_today = counters.views_today;
      b.screen_x = window.screenX;
      b.screen_y = window.screenY;
      b.screens = slow.screens;
      b.avail_w = screen.availWidth;
      b.avail_h = screen.availHeight;
      b.video_inputs = slow.video_inputs;
      b.audio_inputs = slow.audio_inputs;
      b.zoom = window.visualViewport ? window.visualViewport.scale : null;
      b.fullscreen = !!document.fullscreenElement;
      b.referrer_kind = referrerKind();
      b.online = navigator.onLine;
      b.keyboard_layout = slow.keyboard_layout;
      b.hour_cycle = Intl.DateTimeFormat().resolvedOptions().hourCycle || null;
      // Filled by the app; listed so the test in context.rs sees the whole vocabulary.
      b.audio_route = null;
      b.dnd = null;
      b.ringer = null;
      b.power_save = null;
      b.brightness = null;
      b.lux = null;
      b.docked = null;
      b.headset = null;
    } catch (e) {
      // Unfilled blocks are zeroed server-side.
    }
    return JSON.stringify(b);
  };

  // Covers the offer fetch landing after the first keystroke.
  var offerDismissed = false;

  // The server writes the path in UTC; it is rebuilt from `data-day-at` in the viewer's zone.
  function zoneDayLinks(root) {
    var tz = (Intl.DateTimeFormat().resolvedOptions().timeZone || '');
    (root || document).querySelectorAll('a[data-day-link]').forEach(function (a) {
      var at = a.getAttribute('data-day-at');
      if (at) {
        var local = localDay(parseInt(at, 10) * 1000);
        if (local) a.href = '/ui/day/' + local;
      }
      if (a.href.indexOf('?') < 0) a.href += '?tz=' + encodeURIComponent(tz);
    });
  }

  // Not `toISOString`, which is UTC; `en-CA` already spells dates this way.
  function localDay(ms) {
    var d = new Date(ms);
    if (isNaN(d.getTime())) return '';
    var p = ('0');
    return d.getFullYear() + '-' +
      (p + (d.getMonth() + 1)).slice(-2) + '-' +
      (p + d.getDate()).slice(-2);
  }

  // Hidden, not removed: a reminder armed by a capture needs `#due` to swap into. The offer is hidden
  // too; `confirmOffer` counts it once on swap, and `dropOffer` handles the late-fetch race.
  function hideIdle() {
    document.documentElement.classList.add('typing');
    var idle = document.getElementById('idle');
    if (idle) idle.setAttribute('data-off', '');
    show('rail', true);
    show('pane', true);
    show('kind-row', true);
  }

  // `askDriver` never clears the box, so emptying it by hand must not end the answer.
  function paneIsAsking() {
    var form = document.getElementById('box-form');
    if (form && form.classList.contains('asking')) return true;
    var live = document.getElementById('ask-live');
    return !!(live && !live.hidden && live.textContent.trim());
  }

  function paneIsBusy() {
    if (paneIsAsking()) return true;
    var result = document.getElementById('ask-result');
    return !!(result && result.textContent.trim());
  }

  // `force`: a capture that just emptied the box overrules an answer left standing, never an ask in flight.
  function showIdle(force) {
    // An answer standing in the pane is an expressed intent, whatever the box holds.
    if (paneIsAsking() || (!force && paneIsBusy())) return;
    document.documentElement.classList.remove('typing');
    var idle = document.getElementById('idle');
    if (idle) idle.removeAttribute('data-off');
    show('rail', false);
    show('pane', false);
    show('kind-row', false);
    var due = document.getElementById('due');
    if (due) htmx.trigger(due, 'refresh');
    // The poll only fires while the column is visible, so catch up now.
    var foot = document.getElementById('idle-foot');
    if (foot && foot.getAttribute('hx-get')) htmx.trigger(foot, 'refresh');
  }

  function show(id, on) {
    var el = document.getElementById(id);
    if (el) el.hidden = !on;
  }

  // `hideIdle` never runs on a server-rendered `/ui?q=…`, so the class is set here, off `#idle`.
  function syncIdle() {
    var idle = document.getElementById('idle');
    if (idle) document.documentElement.classList.toggle('typing', idle.hasAttribute('data-off'));
  }

  // Only the fetch that lands after the dismissing keystroke: that card was never on screen.
  function dropOffer() {
    var area = document.getElementById('context-offer');
    if (area) area.remove();
  }

  // Only the browser knows the offer was seen; fire-and-forget.
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
    var box = document.querySelector('textarea[name="q"]');
    if (!box) return;
    // Not `{ once: true }`: the offer comes back with the column. `offerDismissed` only records a keystroke.
    box.addEventListener('input', function () {
      if (box.value.trim()) {
        offerDismissed = true;
        hideIdle();
      } else {
        showIdle();
      }
    });
  }

  // `textContent`: an error string went through no sanitizer.
  function notice(kind, text) {
    var t = document.getElementById('oc-notice-' + kind);
    var box = t.content.firstElementChild.cloneNode(true);
    box.querySelector('.oc-notice__text').textContent = text;
    return box;
  }

  function busyNote(target) {
    if (!target || target.id !== 'results') return false;
    // The next successful swap removes it.
    if (target.querySelector('[data-busy-note]')) return true;
    var box = notice('warning', 'The model endpoint is busy · the next keystroke tries again');
    box.setAttribute('data-busy-note', '');
    target.insertBefore(box, target.firstChild);
    return true;
  }

  function failedSwap(target, xhr) {
    if (!target || !target.id) return;
    var reason = 'Server unreachable.';
    if (xhr) reason = reasonOf(xhr) || ('engram answered ' + xhr.status + '.');
    target.textContent = '';
    target.appendChild(notice('danger', 'That did not work · ' + reason));
    // Nothing filled the rail.
    var head = document.getElementById('rail-head');
    if (head && target.id === 'results') head.textContent = '';
  }

  // The API answers `{"error": …}`; a `/ui` route answers a page marking its sentence `data-error-detail`.
  function reasonOf(xhr) {
    var type = xhr.getResponseHeader('content-type') || '';
    try {
      if (type.indexOf('application/json') !== -1) {
        return JSON.parse(xhr.responseText).error || null;
      }
      var page = new DOMParser().parseFromString(xhr.responseText, 'text/html');
      var said = page.querySelector('[data-error-detail]');
      return said ? said.textContent.trim() : null;
    } catch (err) {
      return null;
    }
  }

  // A chip fills the box and stops; the synthetic `input` makes it behave as typed. On the document,
  // since `_box_hint.html` is swapped out of band.
  function exampleChips() {
    document.addEventListener('click', function (e) {
      var chip = e.target.closest && e.target.closest('.chip-example');
      if (!chip) return;
      e.preventDefault();
      var box = document.querySelector('textarea[name="q"]');
      if (!box) return;
      box.value = chip.getAttribute('data-example') || '';
      box.focus();
      box.dispatchEvent(new Event('input', { bubbles: true }));
    });
  }

  // Only `Intl` knows it.
  function boxZone() {
    var el = document.getElementById('box-tz');
    if (el) el.value = Intl.DateTimeFormat().resolvedOptions().timeZone || '';
  }

  // On the document: the stream driver writes the anchor long after load.
  function railBack() {
    document.addEventListener('click', function (e) {
      var back = e.target.closest && e.target.closest('[data-rerun]');
      if (!back) return;
      e.preventDefault();
      var form = document.getElementById('box-form');
      if (form) htmx.trigger(form, 'submit');
    });
  }

  // Typing is the third verb (the form's `hx-trigger`); a model call or a write is always a press.
  function boxVerbs() {
    var form = document.getElementById('box-form');
    if (!form) return;
    var box = form.querySelector('textarea[name="q"]');
    if (!box) return;
    // Read at every sync: the Ask button can be swapped in out of band after load.
    function verbs() {
      return form.querySelectorAll('[data-verb]');
    }

    // A staged file is something for Capture to act on, even over an empty box.
    function sync() {
      // `boxBusy` owns the verbs during an ask or a decision; a mid-answer drop would otherwise re-enable them.
      if (form.getAttribute('aria-busy') === 'true') return;
      var hasText = !!box.value.trim();
      var stagedEl = document.getElementById('staged');
      var hasFile = !!(stagedEl && !stagedEl.hidden);
      // Which verb is lit, not which runs.
      var text = box.value.trim();
      var asksLike = /\?\s*$/.test(text) ||
        /^(who|what|when|where|why|how|which|is|are|do|does|did|can|could|should|wer|was|wann|wo|warum|wie|welche|ist|sind|kann|hat|habe)\b/i.test(text);
      var keepsLike = hasFile || text.length > 200 || text.indexOf('\n') !== -1;
      var buttons = verbs();
      for (var i = 0; i < buttons.length; i++) {
        var verb = buttons[i].getAttribute('data-verb');
        buttons[i].disabled = verb === 'capture'
          ? !(hasText || hasFile)
          // Asking a file's note makes no sense.
          : (!hasText || hasFile);
        // Decide is never the likelier verb: it is chosen, not guessed.
        var lead = verb === 'ask' ? (asksLike && !keepsLike) : verb === 'capture' && keepsLike;
        buttons[i].classList.toggle('oc-btn--primary', lead && !buttons[i].disabled);
      }
    }

    // Grows to a ten-line cap. `.box` is `border-box` and `scrollHeight` excludes the border, so it is
    // added back, or the box shows a scrollbar for two pixels.
    var CAP = 10;
    function grow() {
      box.style.height = 'auto';
      var css = getComputedStyle(box);
      var line = parseFloat(css.lineHeight) || 20;
      var pad = parseFloat(css.paddingTop) + parseFloat(css.paddingBottom);
      var border = box.offsetHeight - box.clientHeight;
      box.style.height =
        Math.min(box.scrollHeight - pad, line * CAP) + pad + border + 'px';
    }

    box.addEventListener('input', function () { sync(); grow(); });
    box.addEventListener(VERB_SYNC, function () { sync(); grow(); fitPlaceholder(); });

    // Text re-wraps when the width changes.
    window.addEventListener('resize', grow);
    // `font-display: swap` re-wraps after the first measurement.
    if (document.fonts && document.fonts.ready) document.fonts.ready.then(grow);

    // Enter is a line break; the chord is the button's act, routed through it so a disabled verb stays inert.
    box.addEventListener('keydown', function (e) {
      if (e.key !== 'Enter') return;
      var mod = e.metaKey || e.ctrlKey;
      var name = mod && !e.altKey ? (e.shiftKey ? 'capture' : 'ask')
        : e.altKey && !mod && !e.shiftKey ? 'decide' : null;
      if (!name) return;
      var verb = form.querySelector('[data-verb="' + name + '"]');
      if (!verb || verb.disabled) return;
      e.preventDefault();
      verb.click();
    });

    // The phone's one row clips the long placeholder; bound to the query, so rotation updates it.
    var narrow = window.matchMedia('(max-width: 40rem)');
    var wide = box.placeholder;
    var short = box.getAttribute('data-placeholder-narrow') || wide;
    // Owned here, since this re-runs on rotation and would overwrite a hint set from `stage()`.
    var NOTE_HINT = 'What is it, why keep it?';
    function fitPlaceholder() {
      var stagedEl = document.getElementById('staged');
      box.placeholder = stagedEl && !stagedEl.hidden
        ? NOTE_HINT
        : (narrow.matches ? short : wide);
    }
    narrow.addEventListener('change', fitPlaceholder);
    // Carried across rather than swapping the textarea, which would lose the caret and text.
    box.addEventListener(HELD_SYNC, function () {
      wide = box.getAttribute('data-placeholder-held') || wide;
      short = box.getAttribute('data-placeholder-held-narrow') || short;
      fitPlaceholder();
    });
    fitPlaceholder();

    sync();
    grow();
  }

  // Hold to record; the words go into the box and `VERB_SYNC` fires. Dictation never searches by itself.
  function micButton() {
    var btn = document.getElementById('mic');
    if (!btn) return;
    var form = document.getElementById('box-form');
    var box = form && form.querySelector('textarea[name="q"]');
    if (!box) return;
    var status = document.getElementById('mic-status');

    // Removed rather than left dead when the browser cannot record.
    if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia ||
        typeof window.MediaRecorder === 'undefined') {
      btn.remove();
      box.classList.remove('box-with-mic');
      return;
    }

    var recorder = null;
    var chunks = [];
    // A released press must not start a recorder that arrives late.
    var holding = false;

    function say(msg) { if (status) status.textContent = msg || ''; }

    function stopTracks(stream) {
      var tracks = stream.getTracks();
      for (var i = 0; i < tracks.length; i++) tracks[i].stop();
    }

    // Appended, never over a half-typed question.
    function fill(text) {
      if (!text) return;
      var had = box.value.replace(/\s+$/, '');
      box.value = had ? had + ' ' + text : text;
      box.dispatchEvent(new Event(VERB_SYNC, { bubbles: true }));
      // There is no Search button; the form's trigger listens for `input`.
      box.dispatchEvent(new Event('input', { bubbles: true }));
      box.focus();
      try { box.setSelectionRange(box.value.length, box.value.length); } catch (e) {}
    }

    // Always 16 kHz mono 16-bit WAV: `MediaRecorder` gives webm/mp4, and whisper.cpp without
    // `WHISPER_FFMPEG` reads only WAV. One `OfflineAudioContext` channel downmixes and resamples.
    var STT_RATE = 16000;

    function toWav(blob) {
      var AC = window.AudioContext || window.webkitAudioContext;
      var OAC = window.OfflineAudioContext || window.webkitOfflineAudioContext;
      if (!AC || !OAC || !blob.arrayBuffer) return Promise.resolve(blob);
      var ctx = new AC();
      var close = function () { if (ctx.close) { try { ctx.close(); } catch (e) {} } };
      return blob.arrayBuffer()
        .then(function (buf) {
          // Safari answers through callbacks; others return a promise.
          return new Promise(function (resolve, reject) {
            var p = ctx.decodeAudioData(buf, resolve, reject);
            if (p && p.then) p.then(resolve, reject);
          });
        })
        .then(function (audio) {
          var frames = Math.ceil(audio.duration * STT_RATE);
          if (!frames) throw new Error('nothing decoded');
          var off = new OAC(1, frames, STT_RATE);
          var src = off.createBufferSource();
          src.buffer = audio;
          src.connect(off.destination);
          src.start();
          return off.startRendering();
        })
        .then(function (rendered) { return wavOf(rendered.getChannelData(0)); })
        // A failed conversion still sends the original recording; a whisper built with ffmpeg takes it.
        .catch(function () { return blob; })
        .then(function (out) { close(); return out; }, function (e) { close(); throw e; });
    }

    // The plainest WAV (no extensible subformat, no float, sizes known up front), which `dr_wav` accepts.
    function wavOf(samples) {
      var n = samples.length;
      var buf = new ArrayBuffer(44 + n * 2);
      var v = new DataView(buf);
      var ascii = function (at, str) {
        for (var i = 0; i < str.length; i++) v.setUint8(at + i, str.charCodeAt(i));
      };
      ascii(0, 'RIFF');
      v.setUint32(4, 36 + n * 2, true);
      ascii(8, 'WAVE');
      ascii(12, 'fmt ');
      v.setUint32(16, 16, true);              // the fmt chunk's own length
      v.setUint16(20, 1, true);               // PCM, uncompressed
      v.setUint16(22, 1, true);               // mono
      v.setUint32(24, STT_RATE, true);
      v.setUint32(28, STT_RATE * 2, true);    // bytes per second
      v.setUint16(32, 2, true);               // bytes per frame
      v.setUint16(34, 16, true);              // bits per sample
      ascii(36, 'data');
      v.setUint32(40, n * 2, true);
      for (var i = 0; i < n; i++) {
        // Clamped first: values slightly outside [-1, 1] would wrap in `setInt16` and click.
        var x = Math.max(-1, Math.min(1, samples[i]));
        v.setInt16(44 + i * 2, x < 0 ? x * 0x8000 : x * 0x7fff, true);
      }
      return new Blob([buf], { type: 'audio/wav' });
    }

    function send(blob) {
      // Nothing recorded, nothing to say.
      if (!blob || !blob.size) { say(''); return; }
      btn.disabled = true;
      say('Transcribing…');
      var body = new FormData();
      // Without a filename the multipart reader sees a field, not a file.
      body.append('audio', blob, 'recording');
      fetch('/ui/transcribe', { method: 'POST', body: body, credentials: 'same-origin' })
        .then(function (r) {
          if (!r.ok) throw new Error('HTTP ' + r.status);
          return r.text();
        })
        .then(function (text) {
          fill(text.trim());
          say('');
        })
        .catch(function () {
          // The details are in the server log.
          say('Could not transcribe that.');
        })
        .then(function () { btn.disabled = false; });
    }

    function start() {
      if (holding || btn.disabled) return;
      holding = true;
      btn.classList.add('listening');
      say('Listening…');
      navigator.mediaDevices.getUserMedia({ audio: true }).then(function (stream) {
        // Released before the device opened: close it, or the microphone light stays on.
        if (!holding) { stopTracks(stream); return; }
        chunks = [];
        recorder = new MediaRecorder(stream);
        recorder.addEventListener('dataavailable', function (e) {
          if (e.data && e.data.size) chunks.push(e.data);
        });
        recorder.addEventListener('stop', function () {
          stopTracks(stream);
          var type = recorder.mimeType || (chunks[0] && chunks[0].type) || 'audio/webm';
          var blob = new Blob(chunks, { type: type });
          recorder = null;
          chunks = [];
          toWav(blob).then(send);
        });
        recorder.start();
      }).catch(function () {
        holding = false;
        btn.classList.remove('listening');
        // Pressing again cannot fix this, and the browser's prompt is gone.
        say('No microphone — the browser refused access.');
      });
    }

    function stop() {
      if (!holding) return;
      holding = false;
      btn.classList.remove('listening');
      if (!recorder) { say(''); return; }
      if (recorder.state !== 'inactive') recorder.stop();
    }

    // `pointerup` on the window: a thumb drifting off the button must still end the recording.
    btn.addEventListener('pointerdown', function (e) {
      e.preventDefault();
      start();
    });
    window.addEventListener('pointerup', stop);
    window.addEventListener('pointercancel', stop);
    // A blur mid-hold would leave a button nothing releases.
    window.addEventListener('blur', stop);

    // Held keys auto-repeat; `e.repeat` stops a second recorder.
    btn.addEventListener('keydown', function (e) {
      if (e.key !== ' ' && e.key !== 'Enter') return;
      e.preventDefault();
      if (e.repeat) return;
      start();
    });
    btn.addEventListener('keyup', function (e) {
      if (e.key !== ' ' && e.key !== 'Enter') return;
      e.preventDefault();
      stop();
    });
    // The synthesised click would start a recording nobody holds.
    btn.addEventListener('click', function (e) { e.preventDefault(); });
  }

  function captureVerb() {
    var form = document.getElementById('box-form');
    if (!form) return;
    var box = form.querySelector('textarea[name="q"]');
    var verb = form.querySelector('[data-verb="capture"]');
    if (!box || !verb) return;

    var drop = document.getElementById('drop');
    var picker = drop && drop.querySelector('input[type=file]');
    // Like EAGER: the server's picker names `image/*` only where `[infer.vision]` is configured.
    var VISION = !!picker && (picker.getAttribute('accept') || '').indexOf('image/*') >= 0;

    // A file is held until Capture is pressed, so it can be annotated or removed.
    var staged = null, stagedUrl = null;
    // Set from the press until the upload answers, so a late debounce cannot search the note.
    var sending = false;
    var stagedBox = document.getElementById('staged');

    // While a file is staged the box is its note; typing it must not search. The form's requests are
    // cancelled rather than its `hx-trigger` rewritten.
    form.addEventListener('htmx:beforeRequest', function (e) {
      if (staged || sending) e.preventDefault();
    });
    var stagedName = document.getElementById('staged-name');
    var stagedThumb = document.getElementById('staged-thumb');
    var stagedClear = document.getElementById('staged-clear');

    function unstage() {
      staged = null;
      stagedUrl = null;
      stagedThumb.removeAttribute('src');
      stagedThumb.hidden = true;
      stagedName.hidden = true;
      stagedClear.hidden = true;
      // The verb-row picker is the invitation.
      if (stagedBox) stagedBox.hidden = true;
      if (drop) drop.hidden = false;
      // Or picking the same file twice fires no `change`.
      if (picker) picker.value = '';
      // See `sync` in boxVerbs.
      box.dispatchEvent(new Event(VERB_SYNC, { bubbles: true }));
    }

    // `restore`: a failed upload coming back. One press writes one appended line.
    function receipt() {
      var host = document.getElementById('capture-result');
      var slot = document.createElement('p');
      if (host) host.appendChild(slot);
      return slot;
    }
    function clearReceipts() {
      var host = document.getElementById('capture-result');
      if (host) host.textContent = '';
    }

    function stage(file, restore) {
      if (!file) return;
      // Said before a note is written for a file the server cannot read.
      if (file.type.indexOf('image/') === 0 && !VISION) {
        clearReceipts();
        receipt().textContent = 'Image capture is not configured on this server.';
        unstage();
        return;
      }
      unstage();
      staged = file;
      stagedName.textContent = (file.name || 'photo.jpg') +
        ' — ready. Press Capture.';
      if (file.type.indexOf('image/') === 0) {
        // A data: URL: the CSP's img-src allows data:, not blob:.
        var reader = new FileReader();
        var mine = file;
        reader.onload = function () {
          if (staged !== mine) return;
          stagedUrl = reader.result;
          stagedThumb.src = stagedUrl;
          stagedThumb.hidden = false;
        };
        reader.readAsDataURL(file);
      }
      stagedName.hidden = false;
      stagedClear.hidden = false;
      if (stagedBox) stagedBox.hidden = false;
      if (drop) drop.hidden = true;
      // Only with a hardware keyboard; on a phone the keyboard would cover the thumbnail.
      if (!restore && window.matchMedia('(hover: hover)').matches) box.focus();
      // See `sync` in boxVerbs.
      box.dispatchEvent(new Event(VERB_SYNC, { bubbles: true }));
    }

    function send(file, note) {
      if (!file) return;
      var isImage = file.type.indexOf('image/') === 0;
      // Some file managers drop files with no type.
      var isPdf = file.type === 'application/pdf' || /\.pdf$/i.test(file.name || '');
      var result = receipt();
      if (isImage && !VISION) {
        result.textContent = 'Image capture is not configured on this server.';
        return;
      }
      // Not through htmx, so its spinner never lights.
      result.textContent = 'Sending…';
      var payload = new FormData();
      if (note) payload.append('note', note);
      // A PDF sent as `paste.txt` would be refused by its own door.
      var fallbackName = isImage ? 'photo.jpg' : (isPdf ? 'capture.pdf' : 'paste.txt');
      payload.append(isImage ? 'image' : 'file', file, file.name || fallbackName);
      var url = isImage ? '/api/v1/corpora/image' : '/api/v1/corpora/upload';
      // Same-origin: the session cookie authenticates it.
      fetch(url, { method: 'POST', body: payload })
        // A body over the limit is refused before the handler, as plain text.
        .then(function (r) {
          return r.json()
            .catch(function () { return { error: 'engram answered ' + r.status + '.' }; })
            .then(function (j) { return [r.ok, j]; });
        })
        .catch(function () { return [false, { error: 'Server unreachable.' }]; })
        .then(function (pair) {
          // Lifted first, or the guard cancels `refreshRail`'s own submit.
          sending = false;
          // The server's reason, verbatim.
          result.textContent = pair[0]
            ? (isImage ? 'Captured — the photo is queued to be read.'
               : isPdf ? 'Captured — the PDF is queued to be extracted.'
               : 'Captured.')
            : (pair[1].error || 'Upload failed.');
          if (pair[0]) {
            box.value = '';
            box.dispatchEvent(new Event(VERB_SYNC, { bubbles: true }));
            refreshRail();
            // This path never touches `/ui/capture`, so it must end an empty base itself.
            syncHeld();
          } else {
            // Back where it was, so the fix is a second press.
            stage(file, true);
          }
        });
    }
    if (picker) picker.addEventListener('change', function () { stage(picker.files[0]); });

    // The whole page is the drop target, or the browser opens the file over engram. Files only:
    // dropping selected text into the box must keep working.
    function carriesFiles(e) {
      var types = e.dataTransfer && e.dataTransfer.types;
      if (!types) return false;
      for (var i = 0; i < types.length; i++) if (types[i] === 'Files') return true;
      return false;
    }
    // Enter/leave fire per element crossed; a depth count finds the page's edge.
    var depth = 0;
    // Before staging the box is `display: none`, so the picker lights too.
    function dim(on) {
      if (stagedBox) stagedBox.classList.toggle('dropping', on);
      if (drop) drop.classList.toggle('dropping', on);
    }
    function undim() { depth = 0; dim(false); }
    document.addEventListener('dragenter', function (e) {
      if (!carriesFiles(e)) return;
      depth++;
      dim(true);
    });
    document.addEventListener('dragleave', function (e) {
      if (!carriesFiles(e)) return;
      if (--depth <= 0) undim();
    });
    // Without cancelling `dragover` the browser opens the file itself.
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

    // The form's own submit is the refresh: with the box empty the results endpoint answers the idle rail.
    function refreshRail() {
      htmx.trigger(form, 'submit');
      // A reminder captured a second ago should appear now; the band's poll covers the background job.
      showIdle(true);
    }

    // Returns its promise so a press with a file can chain the two.
    function postText() {
      var text = box.value.trim();
      if (!text) return Promise.resolve();
      var fromAsk = document.querySelector('input[name="from_ask"]');
      // The search this text was typed into, so its hole closes against the text that answers it.
      var fromSearch = document.querySelector('#fold-of input[name="fold"]');
      // htmx settles the promise for any answer, a 500 included; the verdict is on `htmx:afterRequest`,
      // matched on the path since the box's search runs alongside.
      var stored = false;
      function verdict(e) {
        var path = e.detail && e.detail.pathInfo;
        if (!path || path.requestPath !== '/ui/capture') return;
        stored = !!e.detail.successful;
      }
      document.body.addEventListener('htmx:afterRequest', verdict);
      return htmx.ajax('POST', '/ui/capture', {
        target: '#capture-result',
        // Appended: an upload from the same press has its own line.
        swap: 'beforeend',
        // The zone travels with the text, so *tomorrow at 9* matches the echo.
        values: {
          text: text,
          from_ask: fromAsk ? fromAsk.value : '',
          from_search: fromSearch ? fromSearch.value : '',
          tz: (document.getElementById('box-tz') || {}).value || ''
        }
      // A transport failure rejects.
      }).catch(function () {}).then(function () {
        document.body.removeEventListener('htmx:afterRequest', verdict);
        // A failed capture keeps its text.
        if (!stored) return;
        // Or the next paste is stored as the same model answer.
        var kept = document.getElementById('kept-from');
        if (kept && kept.parentNode) kept.parentNode.removeChild(kept);
        // Or the next unsearchable paste names that search and closes its gap unchecked.
        var foldOf = document.getElementById('fold-of');
        if (foldOf) foldOf.textContent = '';
        box.value = '';
        box.dispatchEvent(new Event(VERB_SYNC, { bubbles: true }));
        refreshRail();
        // After the box is cleared, so the uncovered placeholder is the one that lands.
        syncHeld();
      });
    }
    verb.addEventListener('click', function (e) {
      e.preventDefault();
      clearReceipts();
      // A staged file is one annotated capture; the box is read at press time. `from_ask` only rides `postText`.
      if (staged) {
        var file = staged;
        var note = box.value.trim();
        sending = true;
        unstage();
        send(file, note);
        return;
      }
      postText();
    });
    // A Sheets/Excel/Word clipboard carries a picture too; take it only when the paste is not headed for text.
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

  // A slow rotation of a server-side 3-D projection of the store's vectors. Held to a decoration's
  // budget: one revalidation per load, 12 fps, still under reduced motion, silent on failure.
  function vectorBg() {
    var CACHE_KEY = 'engram.vbg';

    // No topbar, no canvas. The snapshot is server data and logout clears only the cookie, so it is
    // dropped here, or the next account would see the last one's cloud.
    if (!document.querySelector('.oc-topbar')) {
      try { localStorage.removeItem(CACHE_KEY); } catch (e) {}
      return;
    }
    var FPS = 12;
    var SPIN = 0.01 * Math.PI; // rad/s — slow enough to feel geological
    var AXIS_MIN = 6, AXIS_MAX = 14; // seconds on one rotation axis
    var BUCKETS = 8;
    // The projection divides by `radius - z`, so nothing nearer than this is drawn.
    var NEAR_Z = 0.55;

    var canvas = document.createElement('canvas');
    canvas.id = 'vec-bg';
    canvas.setAttribute('aria-hidden', 'true');
    document.body.insertBefore(canvas, document.body.firstChild);
    var ctx = canvas.getContext('2d');
    if (!ctx) { canvas.parentNode.removeChild(canvas); return; }

    var points = [];
    var theta = 0.7, phi = 1.15;
    var spinT = SPIN, spinP = 0, nextAxisAt = 0;

    function pickAxis(t) {
      var r = Math.random();
      if (r < 0.35) {
        spinT = (Math.random() > 0.5 ? 1 : -1) * SPIN;
        spinP = 0;
      } else if (r < 0.65) {
        spinT = (Math.random() > 0.5 ? 1 : -1) * SPIN;
        spinP = (Math.random() - 0.5) * SPIN * 0.5;
      } else {
        spinT = (Math.random() - 0.5) * SPIN * 1.6;
        spinP = (Math.random() - 0.5) * SPIN * 0.7;
      }
      nextAxisAt = t + AXIS_MIN + Math.random() * (AXIS_MAX - AXIS_MIN);
    }

    // Re-read every draw, so a theme flip needs no listener.
    function ink() {
      return getComputedStyle(document.body).getPropertyValue('--color-fg-muted').trim() || 'gray';
    }

    function resize() {
      // Capped: at 3x a phone paints nine times the pixels for two-pixel dots.
      var dpr = Math.min(window.devicePixelRatio || 1, 1.5);
      canvas.width = Math.round(window.innerWidth * dpr);
      canvas.height = Math.round(window.innerHeight * dpr);
    }

    function project(px, py, pz) {
      var W = canvas.width, H = canvas.height;
      var x1 = px * Math.cos(theta) - pz * Math.sin(theta);
      var z1 = px * Math.sin(theta) + pz * Math.cos(theta);
      var sp = Math.sin(phi), cp = Math.cos(phi);
      var y2 = py * sp - z1 * cp;
      var z2 = py * cp + z1 * sp;
      var radius = 2.5;
      var d = radius - z2;
      // `!(d > 0.01)` rejects NaN too, which would otherwise throw every frame.
      if (!(d > 0.01)) return null;
      var unit = Math.min(W, H) * 0.52;
      var s = (radius * unit) / d;
      // On a portrait screen the cloud is pulled up towards the content.
      var cy = Math.max(unit, Math.min(H * 0.5, H * 0.5 - (H - W) * 0.22));
      var sx = x1 * s + W * 0.5, sy = -y2 * s + cy;
      if (!isFinite(sx) || !isFinite(sy)) return null;
      return { sx: sx, sy: sy, depth: z2, s: s, unit: unit };
    }

    function drawAxes(c) {
      var L = 1.2, T = 0.05, TICKS = [-1, -0.5, 0.5, 1], STEPS = 24;

      // In steps, so the stroke can stop at the near plane and resume beyond it.
      function seg(x0, y0, z0, x1, y1, z1) {
        var open = false;
        for (var n = 0; n <= STEPS; n++) {
          var f = n / STEPS;
          var q = project(x0 + (x1 - x0) * f, y0 + (y1 - y0) * f, z0 + (z1 - z0) * f);
          if (!q || q.depth > NEAR_Z) { open = false; continue; }
          if (open) ctx.lineTo(q.sx, q.sy);
          else { ctx.moveTo(q.sx, q.sy); open = true; }
        }
      }
      ctx.beginPath();
      ctx.strokeStyle = c;
      ctx.globalAlpha = 0.13;
      ctx.lineWidth = 1;
      seg(-L, 0, 0, L, 0, 0);
      for (var i = 0; i < TICKS.length; i++) seg(TICKS[i], -T, 0, TICKS[i], T, 0);
      seg(0, -L, 0, 0, L, 0);
      for (var j = 0; j < TICKS.length; j++) seg(-T, TICKS[j], 0, T, TICKS[j], 0);
      seg(0, 0, -L, 0, 0, L);
      for (var k = 0; k < TICKS.length; k++) seg(-T, 0, TICKS[k], T, 0, TICKS[k]);
      ctx.stroke();
    }

    function draw() {
      var W = canvas.width, H = canvas.height;
      ctx.clearRect(0, 0, W, H);
      var c = ink();
      drawAxes(c);
      // One fill per depth bucket: 2000 dots in 8 draw calls.
      var buckets = [];
      for (var b = 0; b < BUCKETS; b++) buckets.push([]);
      for (var i = 0; i < points.length; i++) {
        var q = project(points[i][0], points[i][1], points[i][2]);
        if (!q) continue;
        var t = Math.max(0, Math.min(0.9999, (q.depth + 1.15) / 2.3));
        q.r = Math.max(1.1, (q.s / q.unit) * 2.3);
        buckets[Math.floor(t * BUCKETS)].push(q);
      }
      for (var k = 0; k < BUCKETS; k++) {
        if (!buckets[k].length) continue;
        var alpha = 0.07 + ((k + 0.5) / BUCKETS) * 0.38;
        ctx.beginPath();
        ctx.fillStyle = c;
        ctx.globalAlpha = alpha;
        for (var j = 0; j < buckets[k].length; j++) {
          var q2 = buckets[k][j];
          ctx.moveTo(q2.sx + q2.r, q2.sy); // moveTo keeps the arcs unconnected
          ctx.arc(q2.sx, q2.sy, q2.r, 0, Math.PI * 2);
        }
        ctx.fill();
      }
    }

    var reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    var prevT = 0, lastDraw = 0;
    function frame(now) {
      requestAnimationFrame(frame);
      if (document.visibilityState !== 'visible') { prevT = now / 1000; return; }
      if (now - lastDraw < 1000 / FPS) return;
      var t = now / 1000;
      var dt = Math.min(t - prevT, 0.25);
      prevT = t;
      lastDraw = now;
      if (t > nextAxisAt) pickAxis(t);
      theta += spinT * dt;
      phi += spinP * dt;
      // Bounce off the poles so the drift can never park the cloud there.
      if (phi < 0.05) { phi = 0.05; spinP = -spinP; }
      if (phi > Math.PI - 0.05) { phi = Math.PI - 0.05; spinP = -spinP; }
      draw();
    }

    function start(pts) {
      if (!pts || !pts.length) { canvas.parentNode.removeChild(canvas); return; }
      // Centred on its centroid: a random projection's origin is anywhere.
      var cx = 0, cyy = 0, cz = 0;
      for (var i = 0; i < pts.length; i++) { cx += pts[i][0]; cyy += pts[i][1]; cz += pts[i][2]; }
      cx /= pts.length; cyy /= pts.length; cz /= pts.length;
      points = [];
      for (var j = 0; j < pts.length; j++) {
        points.push([pts[j][0] - cx, pts[j][1] - cyy, pts[j][2] - cz]);
      }
      resize();
      pickAxis(0);
      window.addEventListener('resize', function () {
        resize();
        if (reduced) draw();
      });
      if (reduced) {
        // With the loop off, a theme toggle must repaint by hand.
        draw();
        new MutationObserver(draw).observe(document.documentElement, {
          attributes: true, attributeFilter: ['data-theme']
        });
        var dark = window.matchMedia('(prefers-color-scheme: dark)');
        if (dark.addEventListener) dark.addEventListener('change', function () { draw(); });
        return;
      }
      requestAnimationFrame(frame);
    }

    // Storage can hold anything; the shape is checked once rather than every frame.
    function usable(pts) {
      if (!Array.isArray(pts) || !pts.length) return false;
      for (var i = 0; i < pts.length; i++) {
        var p = pts[i];
        if (!Array.isArray(p) || p.length < 3) return false;
        for (var j = 0; j < 3; j++) {
          if (typeof p[j] !== 'number' || !isFinite(p[j])) return false;
        }
      }
      return true;
    }

    function keep(pts, tag) {
      try {
        localStorage.setItem(CACHE_KEY, JSON.stringify({ tag: tag, points: pts }));
      } catch (e) {}
    }

    // The canvas comes down but the snapshot stays: this is no verdict on storage.
    function hide() {
      if (canvas.parentNode) canvas.parentNode.removeChild(canvas);
    }

    // Never drawn on its own authority: every load revalidates by tag.
    var cached = null;
    try { cached = JSON.parse(localStorage.getItem(CACHE_KEY) || 'null'); } catch (e) {}
    // An empty snapshot has a tag too, so "nothing to draw" is cacheable.
    var stored = cached && Array.isArray(cached.points)
      && (cached.points.length === 0 || usable(cached.points));
    var have = stored && typeof cached.tag === 'string' ? cached.tag : null;

    var url = '/api/v1/vectors/sample';
    if (have) url += '?have=' + encodeURIComponent(have);
    // `no-store` too, so a snapshot an older build let the browser keep is never replayed.
    fetch(url, { credentials: 'same-origin', cache: 'no-store' })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (data) {
        // No tag means the store could not be asked; only a mismatched tag is a verdict.
        if (!data || typeof data.tag !== 'string') { hide(); return; }
        if (data.unchanged && have) {
          if (cached.points.length) start(cached.points); else hide();
          return;
        }
        var pts = usable(data.points) ? data.points : [];
        // Stored first, so "nothing to draw" is remembered under its tag.
        keep(pts, data.tag);
        if (!pts.length) { hide(); return; }
        start(pts);
      })
      .catch(function () {
        // A network failure says nothing about the stored cloud.
        hide();
      });
  }

  // The refining pass: vector order while typing, the reranker's order once the box is quiet, gliding in.
  // Armed only by `data-rerank`, which the server renders only where a reranker serves search.

  // Read off the request rather than a flag, so the handlers cannot disagree.
  function wasRefine(e) {
    var cfg = e.detail && e.detail.requestConfig;
    // Unfiltered: `parameters` is post-`hx-params`, and a miss here re-arms the timer forever.
    var params = cfg && (cfg.unfilteredParameters || cfg.parameters);
    return !!(params && params.rerank === 'true');
  }

  // Nobody asked for this poll, so its errors must not write into the paragraph or navigate away.
  function wasIdlePoll(e) {
    var cfg = e.detail && e.detail.requestConfig;
    return String((cfg && cfg.path) || '').indexOf('/ui/idle-foot') === 0;
  }

  // A "still reading" rail was searched against an unfinished base; re-run it once the read lands.
  // Not over a box mid-request.
  function rerunAfterRead() {
    document.body.addEventListener('engram:read-landed', function () {
      if (!document.querySelector('#results .still-reading')) return;
      var form = document.getElementById('box-form');
      if (!form || form.classList.contains('htmx-request')) return;
      htmx.trigger(form, 'submit');
    });
  }

  // The fragment renders every row unselected; the open artifact's highlight must survive.
  function markOpenRow() {
    var open = window.location.pathname;
    document.querySelectorAll('.rail-item').forEach(function (el) {
      el.setAttribute('aria-selected', el.getAttribute('href') === open ? 'true' : 'false');
    });
  }

  function refinePass() {
    var form = document.getElementById('box-form');
    if (!form || form.getAttribute('data-rerank') !== 'true') return;
    var box = form.querySelector('textarea[name="q"]');
    if (!box) return;
    // Long enough past the 120ms debounce to mean "settled".
    var QUIET_MS = 500;
    var timer = null;
    // Row positions keyed by href, good for exactly one swap.
    var from = null;
    // `key`: query and chip; `hrefs`: the row set it is valid over; `html`/`head`: what it painted.
    var refined = null;
    var pending = null;

    function cancel() {
      if (timer) { clearTimeout(timer); timer = null; }
    }

    // Relative to the list: the scroller differs by width, and a refine can land after a scroll.
    function resultsTop() {
      var el = document.getElementById('results');
      return el ? el.getBoundingClientRect().top : 0;
    }

    function fire() {
      timer = null;
      var q = box.value.trim();
      // The endpoint's guards: an empty box is the idle rail, a chapter is a capture.
      if (!q || q.length > 2000) return;
      var values = { q: q, rerank: 'true' };
      var chip = form.querySelector('input[name="category"]:checked');
      if (chip && chip.value) values.category = chip.value;
      // Built by hand, so `hx-params` does not reach it.
      var why = form.querySelector('input[name="explain"]');
      if (why && why.value) values.explain = why.value;
      var rows = document.querySelectorAll('#results .rail-item');
      // The server skips the rerank over an empty answer.
      if (!rows.length) return;
      var key = q + '\u0000' + (values.category || '');
      var origin = resultsTop();
      var hrefs = [];
      from = {};
      rows.forEach(function (el) {
        var href = el.getAttribute('href');
        hrefs.push(href);
        from[href] = el.getBoundingClientRect().top - origin;
      });
      // Replayed only for the same query over the same fast-pass row set (compared as sets).
      if (refined && refined.key === key && refined.hrefs === hrefs.slice().sort().join('\n')) {
        var target = document.getElementById('results');
        if (target) {
          target.innerHTML = refined.html;
          var head = document.getElementById('rail-head');
          if (head && refined.head) head.innerHTML = refined.head;
          // Swapped by hand, so wire htmx and recompute the selection.
          htmx.process(target);
          enhance(target);
          markOpenRow();
          trackDwell();
          glide();
        }
        return;
      }
      // Captured now: only this moment knows the fast-pass rows the refine answers over.
      pending = { key: key, hrefs: hrefs.slice().sort().join('\n') };
      // `source: form` puts it in the form's sync queue, so a keystroke replaces an in-flight refine.
      htmx.ajax('GET', '/ui/search/results', {
        source: form, target: '#results', swap: 'innerHTML', values: values
      });
    }

    function glide() {
      var was = from;
      from = null;
      if (!was) return;
      if (window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches) return;
      var origin = resultsTop();
      document.querySelectorAll('#results .rail-item').forEach(function (el) {
        var top = was[el.getAttribute('href')];
        if (top === undefined) {
          // Promoted from the candidate pool: nowhere to glide from.
          el.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 300, easing: 'ease-out' });
          return;
        }
        var delta = top - (el.getBoundingClientRect().top - origin);
        if (!delta) return;
        el.animate(
          [{ transform: 'translateY(' + delta + 'px)' }, { transform: 'none' }],
          { duration: 300, easing: 'ease-in-out' }
        );
      });
    }

    // Composition events are skipped to mirror the form's `input[!event.isComposing]` filter.
    box.addEventListener('input', function (e) {
      if (e.isComposing) return;
      cancel();
    });
    box.addEventListener('compositionstart', cancel);
    // Any form request retires the timer; any mutation retires the stored answer, whose snippets may be stale.
    document.body.addEventListener('htmx:beforeRequest', function (e) {
      if (!e.detail) return;
      if (e.detail.elt === form) {
        cancel();
        // Names the open request, not the server's stage (`Core::search_events`, which the box does not read).
        var spinner = document.getElementById('search-spinner');
        if (spinner) spinner.textContent = wasRefine(e) ? 'reranking…' : 'retrieving…';
      }
      var cfg = e.detail.requestConfig;
      if (cfg && cfg.verb && String(cfg.verb).toLowerCase() !== 'get') refined = null;
    });
    document.body.addEventListener('htmx:afterSwap', function (e) {
      if (e.target.id !== 'results') return;
      if (wasRefine(e)) {
        // A row may have been opened during the quiet window.
        markOpenRow();
        // Stored with the fast-pass row set from `fire`, since the rerank may promote other rows.
        var target = document.getElementById('results');
        if (pending && target) {
          var head = document.getElementById('rail-head');
          refined = {
            key: pending.key,
            hrefs: pending.hrefs,
            html: target.innerHTML,
            head: head ? head.innerHTML : null
          };
        }
        pending = null;
        glide();
      } else {
        // A typing swap also renders every row unselected.
        markOpenRow();
        // Never re-armed off a refine swap, or it reranks every half second forever.
        cancel();
        from = null;
        timer = setTimeout(fire, QUIET_MS);
      }
    });
  }

  // The band re-reads at most every five minutes (`due::POLL_CAP`), so the countdown and heat are recomputed
  // here with the rules of `due::due_words` and `due::heat`.
  var HEAT_WINDOW = 6 * 3600;

  // Whole seconds, as the server's integer `due_words`.
  function spanWords(secs) {
    var s = Math.max(0, Math.floor(secs));
    if (s < 60) return s + 's';
    var m = Math.floor(s / 60);
    if (m < 60) return m + 'm';
    var h = Math.floor(m / 60);
    if (h < 24) return h + 'h ' + String(m % 60).padStart(2, '0') + 'm';
    var d = Math.floor(h / 24);
    if (d < 7) return d + 'd ' + (h % 24) + 'h';
    return d + 'd';
  }

  function dueTick() {
    var now = Date.now() / 1000;
    document.querySelectorAll('.due-row[data-due-at]').forEach(function (row) {
      var at = Number(row.getAttribute('data-due-at'));
      if (!at) return;
      var ahead = at - now;
      row.style.setProperty(
        '--heat',
        (ahead <= 0 ? 1 : ahead >= HEAT_WINDOW ? 0 : 1 - ahead / HEAT_WINDOW).toFixed(3)
      );
      // Beyond the window the server's wall-clock words stand.
      if (ahead >= HEAT_WINDOW) return;
      var count = row.querySelector('.due-count');
      if (!count) return;
      var text = ahead <= 0 ? spanWords(-ahead) + ' overdue' : 'in ' + spanWords(ahead);
      if (count.textContent !== text) count.textContent = text;
      if (ahead <= 0) row.classList.add('due-overdue');
    });
  }

  // Polls hold off while `later` is open, a date is being typed, or a fresh undo is showing (for a deleted
  // moment it is the only way back). Bounded; the clock is stamped in the swap handler, off the response.
  var UNDO_GRACE = 30000;
  var undoSince = 0;

  function dueBusy() {
    var band = document.getElementById('due');
    if (!band) return false;
    if (band.querySelector('details[open]')) return true;
    if (document.activeElement && band.contains(document.activeElement)) return true;
    if (undoSince && Date.now() - undoSince < UNDO_GRACE) return true;
    // A date typed but not yet submitted.
    return Array.prototype.some.call(band.querySelectorAll('input'), function (i) {
      return i.value !== '';
    });
  }

  document.addEventListener('DOMContentLoaded', function () {
    enhance(document.body);
    dueTick();
    document.body.addEventListener('htmx:beforeSwap', function (e) {
      if (!e.target || e.target.id !== 'due') return;
      var cfg = e.detail && e.detail.requestConfig;
      // The poll and `refresh` name the band as source; a button names itself.
      var own = !(cfg && cfg.elt && cfg.elt !== e.target);
      if (own && dueBusy()) {
        e.detail.shouldSwap = false;
        return;
      }
      // Read off the response: two presses in a row have no swap between them.
      undoSince = (e.detail.serverResponse || '').indexOf('due-done') >= 0 ? Date.now() : 0;
    });
    // Cheaper than the poll it replaces.
    setInterval(dueTick, 1000);
    syncIdle();
    railHandle();
    migrateThemeKey();
    vectorBg();
    keyHint();
    installNudge();
    primeSlow();
    placeSwitch();
    contextOffer();
    zoneDayLinks(document);
    restoreReading();
    boxVerbs();
    railBack();
    exampleChips();
    boxZone();
    captureVerb();
    micButton();
    askDriver();
    decideDriver();
    boxStop();
    refinePass();
    rerunAfterRead();
    trackDwell();
    window.addEventListener('pagehide', flushDwell);
    document.addEventListener('visibilitychange', function () {
      if (document.visibilityState === 'hidden') flushDwell();
      else trackDwell();
    });
    // htmx swaps nothing on a 4xx, and a fragment must not be redirected into a login; so navigate here.
    document.body.addEventListener('htmx:responseError', function (e) {
      if (!e.detail || !e.detail.xhr) return;
      // Unrequested requests must not show an error box or navigate mid-read.
      if (wasRefine(e) || wasIdlePoll(e)) return;
      if (e.detail.xhr.status === 401) {
        var here = window.location.pathname + window.location.search;
        window.location.assign('/auth/login?go=' + encodeURIComponent(here));
        return;
      }
      // 503 is `Error::InferenceBusy` alone; a busy keystroke keeps its rail.
      if (e.detail.xhr.status === 503 && busyNote(e.detail.target)) return;
      failedSwap(e.detail.target, e.detail.xhr);
    });
    // htmx swaps nothing on error, and a silent failure reads as an empty base.
    document.body.addEventListener('htmx:sendError', function (e) {
      if (!e.detail) return;
      // Same exemption as above.
      if (wasRefine(e) || wasIdlePoll(e)) return;
      failedSwap(e.detail.target, null);
    });
    // `_idle_foot.html` arrives out of band only, so `htmx:afterSwap` never sees the day link.
    document.body.addEventListener('htmx:oobAfterSwap', function (e) {
      zoneDayLinks(e.target);
    });
    document.body.addEventListener('htmx:afterSwap', function (e) {
      // An offer fetch landing after the first keystroke must not swap back in.
      if (e.target.id === 'context-offer') {
        if (offerDismissed) dropOffer();
        else confirmOffer(e.target);
      }
      zoneDayLinks(e.target);
      enhance(e.target);
      // By the time it lands the numbers have moved.
      if (e.target.id === 'due') dueTick();
      trackDwell();
      var ws = document.querySelector('.regions');
      if (ws && e.target.id === 'pane-content') {
        ws.classList.remove('answering');
        // `has-selection`: narrow hides the list (a fresh list undoes it); `pane-open`: the pane holds an artifact.
        ws.classList.add('has-selection', 'pane-open');
      }
      // A fresh list shows again on narrow screens. Not for the refine: the rows glide under a still eye.
      if (e.target.id === 'results' && !wasRefine(e)) {
        // `pane-open` stays: the pane still holds what it held.
        if (ws) ws.classList.remove('has-selection', 'answering');
        // Back to the top: on a phone the box is at the bottom, so a new list would open at its end.
        var railEl = document.getElementById('rail');
        if (railEl) {
          // Wide: the rail is its own scroll box.
          railEl.scrollTop = 0;
          // Narrow: the window scrolls; only ever up, and only to the list's top.
          var top = railEl.getBoundingClientRect().top;
          if (top < 0) window.scrollBy(0, top);
        }
      }
      // Matched on href, since Related and Seen-together links swap the pane too.
      if (e.target.id === 'pane-content') {
        // The ask's nodes survive the swap as its only targets, but must not talk over the artifact.
        ['ask-live', 'ask-reasoning-box', 'ask-progress'].forEach(function (id) {
          var el = document.getElementById(id);
          if (el) el.hidden = true;
        });
        ['ask-result', 'ask-status', 'capture-result'].forEach(function (id) {
          var el = document.getElementById(id);
          if (el) el.textContent = '';
        });
        markOpenRow();
      }
    });
  });

  // Focused only with a hardware keyboard: on touch the keyboard covers the page. `input[name="q"]` is
  // the Insights assign box.
  var field = document.querySelector('textarea[name="q"], input[name="q"]');
  if (field && window.matchMedia('(hover: hover)').matches) field.focus();

  // Every letter shortcut is gated on this.
  function typing() {
    var el = document.activeElement;
    if (!el) return false;
    var tag = el.tagName;
    return tag === 'INPUT' || tag === 'TEXTAREA' || el.isContentEditable;
  }

  // Arrows and j/k move through the rail. Gated on `typing()` too: in the textarea Down is the next line.
  document.addEventListener('keydown', function (e) {
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    var down = e.key === 'ArrowDown' || e.key === 'j';
    var up = e.key === 'ArrowUp' || e.key === 'k';
    if (!down && !up) return;
    if (typing()) return;
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
  // `/` reaches the query, Esc steps back one region, `s` and `r` show more or less.
  document.addEventListener('keydown', function (e) {
    if (e.metaKey || e.ctrlKey || e.altKey) return;

    if (e.key === '/' && !typing()) {
      // `select()`: `/` starts a new query. Not while read-only during an ask.
      var q = document.querySelector('textarea[name="q"]');
      if (q && !q.readOnly) { e.preventDefault(); q.focus(); q.select(); }
      return;
    }
    if (e.key === 'Escape') {
      // Out of the field first, so Escape does not throw away the results.
      if (typing()) { document.activeElement.blur(); return; }
      // `.back` exists but is hidden on wide windows; `offsetParent` is null exactly when it is not rendered.
      var back = document.querySelector('.back');
      if (back && back.offsetParent !== null) { e.preventDefault(); back.click(); }
      return;
    }
    if (typing()) return;
    var regions = document.querySelector('.regions');
    if (!regions) return;
    // Gated on `.raw`: a merge has no source, and its second half is the lineage.
    if (e.key === 's' && document.querySelector('.raw')) {
      e.preventDefault();
      regions.classList.toggle('hide-source');
      return;
    }
    if (e.key === 'r' && document.querySelector('.region-rail')) {
      e.preventDefault();
      var on = regions.classList.toggle('reading');
      // A way of working, not a per-visit choice.
      try { localStorage.setItem('engram.reading', on ? '1' : '0'); } catch (err) {}
    }
  });

})();

// Behaviour the templates declare in data attributes; the CSP allows no inline code.
(function () {
  'use strict';
  var VARS = ['heat', 'd'];

  function applyVars(root) {
    if (!root.querySelectorAll) return;
    VARS.forEach(function (name) {
      var attr = 'data-var-' + name;
      var els = root.querySelectorAll('[' + attr + ']');
      if (root.hasAttribute && root.hasAttribute(attr)) els = [root].concat(Array.prototype.slice.call(els));
      Array.prototype.forEach.call(els, function (el) {
        el.style.setProperty('--' + name, el.getAttribute(attr));
      });
    });
  }

  function zone() {
    try { return Intl.DateTimeFormat().resolvedOptions().timeZone || ''; } catch (e) { return ''; }
  }

  document.addEventListener('click', function (e) {
    var show = e.target.closest && e.target.closest('[data-show]');
    if (show) {
      var target = document.querySelector(show.getAttribute('data-show'));
      if (target) {
        target.hidden = false;
        show.hidden = true;
        var field = target.querySelector('textarea, input:not([type=hidden])');
        if (field) field.focus();
      }
      return;
    }
    var hide = e.target.closest && e.target.closest('[data-hide]');
    if (hide) {
      var sel = hide.getAttribute('data-hide');
      var t = document.querySelector(sel);
      if (t) t.hidden = true;
      document.querySelectorAll('[data-show="' + sel + '"]').forEach(function (b) { b.hidden = false; });
      return;
    }
    var back = e.target.closest && e.target.closest('[data-back-to-results]');
    if (back) {
      var rail = document.querySelector('.region-rail');
      if (!rail) return;
      e.preventDefault();
      rail.closest('.regions').classList.remove('has-selection');
      history.back();
    }
  });

  document.addEventListener('change', function (e) {
    var select = e.target.closest && e.target.closest('[data-shows-rule]');
    var field = select && select.form && select.form.querySelector('[data-rule-field]');
    if (field) field.hidden = select.value !== 'custom';
  });

  document.addEventListener('htmx:configRequest', function (e) {
    var elt = e.detail.elt;
    if (!elt || !elt.closest) return;
    if (elt.closest('[data-send-tz]')) e.detail.parameters.tz = zone();
    if (elt.closest('[data-send-context]') && window.engramContext) {
      e.detail.parameters.bundle = window.engramContext();
    }
  });

  document.addEventListener('htmx:beforeRequest', function (e) {
    var elt = e.detail.elt;
    var scope = elt && elt.getAttribute && elt.getAttribute('data-poll-while-visible');
    if (!scope) return;
    var region = elt.closest(scope);
    if (document.hidden || (region && region.hidden)) e.preventDefault();
  });

  document.addEventListener('htmx:afterSwap', function (e) { applyVars(e.detail.target || document); });
  document.addEventListener('htmx:oobAfterSwap', function (e) { applyVars(e.detail.target || document); });
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () { applyVars(document); });
  } else {
    applyVars(document);
  }
})();

// The due band counts down to its next row by a timer the server names, not by polling.
(function () {
  'use strict';
  var timer = null;
  function arm() {
    clearTimeout(timer);
    var due = document.getElementById('due');
    var secs = due && Number(due.getAttribute('data-refresh-in'));
    if (!secs || !window.htmx) return;
    timer = setTimeout(function () { window.htmx.trigger(due, 'refresh'); }, secs * 1000);
  }
  document.addEventListener('htmx:afterSettle', function (e) {
    if (e.detail.elt && (e.detail.elt.id === 'due' || e.detail.elt.querySelector && e.detail.elt.querySelector('#due'))) arm();
  });
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', arm); else arm();
})();
