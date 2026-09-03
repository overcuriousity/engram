// Client-side because none of it may touch the sanitized HTML on the server.
// Every function here walks text nodes only: it can wrap what is already
// rendered, and it can never introduce an element the sanitizer disallowed.
(function () {
  'use strict';

  // What the code below fires when the box's *surroundings* changed — a file
  // staged or dropped, an answer that finished, a capture that emptied the box
  // — and the verb row has to be re-read. It used to fire `input`, which was
  // true of nothing: nobody typed. `contextOffer` listens for the first
  // keystroke on `input` and takes the offer away, so dragging a file onto a
  // fresh page removed a card the operator had not touched, and if the offer
  // fetch had not landed yet, `/ui/context/seen` never went and a real
  // impression fell out of both halves of the hit rate.
  var VERB_SYNC = 'engram:verbsync';
  // Fired on the box once the base has stopped being empty and the held
  // state has been swapped in. See `syncHeld`.
  var HELD_SYNC = 'engram:held';

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

  // Chrome fires this once, early, and only hands the prompt to a listener
  // that was already there — so it is caught here, at script scope, and
  // `installNudge` decides later whether to use it.
  var installPrompt = null;
  window.addEventListener('beforeinstallprompt', function (e) {
    e.preventDefault();
    installPrompt = e;
    installNudge();
  });

  // Offered on a phone browser, once a week, until installed or declined.
  // Not in an installed window: there is nothing left to offer. Not on a
  // desktop: the browser's own address-bar affordance is enough there, and a
  // banner on a wide screen is furniture. A week rather than never, because
  // "Not now" means not now.
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
    // Safari has no prompt and no event; the route is the Share sheet. Any
    // other browser without a prompt has no route at all, so no banner.
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
        var p = installPrompt;
        installPrompt = null;
        stamp();
        if (p) p.prompt();
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
    splitHandles(root);
  }

  // ── Column boundaries the reader can move ─────────────────────────────────
  //
  // Two of them, and they are one mechanism twice: the results rail against
  // the pane, and the artifact against the source it was cut from. Each is a
  // grid whose first track is a custom property, so moving the boundary is
  // writing one number onto the container — nothing the server rendered is
  // touched, and the stylesheet's own default stands for everyone who never
  // drags anything.
  //
  // Remembered in `localStorage` and not in the base. A reading width is a
  // habit of one browser on one screen, like the theme beside it; carried
  // between a desktop and a phone it would be wrong in both places.
  function rem() {
    return parseFloat(getComputedStyle(document.documentElement).fontSize) || 16;
  }

  function clampTo(v, lo, hi) {
    return Math.min(Math.max(v, lo), Math.max(lo, hi));
  }

  // Everything that differs between a boundary between columns and a boundary
  // between rows, which is less than it sounds: which coordinate of a drag is
  // the one that means anything, which two arrow keys are the arrow keys, where
  // the boundary currently sits, and what the browser is told the thing is.
  //
  // `at` reads the boundary back off the page rather than off the property,
  // so the keyboard works in whichever unit the property happens to be written
  // in. The vertical one is positioned by that very property inside its own
  // offset parent, so `offsetLeft` is the answer; the horizontal one is in
  // flow, and where it landed is what its rect says.
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

  // Writing the boundary onto the container. `also` is for a boundary that
  // takes more than one property to state — an artifact given a height has
  // also stopped growing into whatever the lists leave — and those ride along
  // with the one number that is remembered rather than being remembered too:
  // they are not the reader's answer, they are what the answer implies.
  function pin(el, spec, v) {
    el.style.setProperty(spec.prop, v);
    for (var k in spec.also || {}) el.style.setProperty(k, spec.also[k]);
  }

  // `spec`: `axis` picks a row of the table above, `prop` is the custom
  // property the grid reads, `key` where the reader's answer is kept,
  // `value(pos, rect)` turns a position inside the container into what the
  // property should say, `nudge(rect)` is one press of an arrow key in pixels,
  // and `place` puts the handle where it belongs when appending is wrong.
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
    // A separator rather than a button as far as assistive technology is
    // concerned, because that is what it is: `aria-orientation` says which way
    // it divides, and the arrow keys below are what the role promises.
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
      // Captured, so the drag survives the pointer leaving the handle — which
      // it does immediately, the handle being as narrow as the gap it covers.
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

    // The keyboard's half of the same thing, one step of `nudge` from wherever
    // the boundary actually resolved to — see `at` on the axis for how that is
    // read back.
    h.addEventListener('keydown', function (e) {
      var step = ax.step(e.key);
      if (!step) return;
      e.preventDefault();
      var rect = el.getBoundingClientRect();
      put(ax.at(h, rect) + step * spec.nudge(rect), rect);
      remember();
    });

    // The way back to the stylesheet's own answer, on the element itself
    // rather than a control somewhere else: whoever moved it is holding it.
    h.addEventListener('dblclick', function () {
      el.style.removeProperty(spec.prop);
      for (var k in spec.also || {}) el.style.removeProperty(k);
      try {
        localStorage.removeItem(spec.key);
      } catch (e) {}
    });
  }

  // The rail is measured in pixels, because what it holds is a list of fixed
  // things — titles and snippets — and its right size is a width, not a share
  // of the window. The floors are what keep a drag from producing a column
  // that can hold nothing: neither side may be squeezed past the point where
  // it stops being readable, and the pane's floor wins when the window is too
  // narrow for both.
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
        // 30rem for the pane and 1rem for the gap between them, which is part
        // of the width the rail's own number does not describe: without it the
        // pane's floor was a rem short of what it says.
        return Math.round(clampTo(x, 15 * u, rect.width - 31 * u)) + 'px';
      },
    });
  }

  // The split is measured as a share, because both columns are prose-shaped
  // and what a reader is choosing is how to divide whatever room the pane has
  // — which is not the same number of pixels in the pane as on the standalone
  // page.
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

  // The third boundary, and the only horizontal one: the artifact against the
  // neighbour lists under it. What it divides is not the page's height — that
  // column is a scroll box of whatever height the pane leaves — but how that
  // height is spent, and the two halves want it for opposite reasons. Reading
  // the passage wants the artifact tall; casting around for what else is near
  // it wants the lists tall. The stylesheet's own answer gives the artifact
  // everything the lists do not need, which is right until it isn't.
  //
  // Pixels, like the rail and unlike the split: what is being sized is a number
  // of lines of prose, and lines do not come in percentages of a window.
  function cardHandle(col) {
    if (!col || !col.matches) return;
    var card = col.querySelector(':scope > .card');
    // Nothing under the artifact is nothing to divide it from. Both lists are
    // conditional in the template — a fresh artifact has no neighbours yet.
    if (!card || !col.querySelector(':scope > .related')) return;
    columnHandle(col, {
      axis: 'y',
      prop: '--artifact-h',
      // The stated height is only half of it. Left growing, the card would
      // take back at the next layout every pixel the drag just gave the lists.
      also: { '--artifact-fill': '0' },
      key: 'engram.artifact-h',
      label: 'Artifact height',
      // In the card's own bottom margin — see the note in 40-workspace.css —
      // so offering the boundary moves nothing on the page.
      place: function (el, h) {
        card.parentNode.insertBefore(h, card.nextSibling);
      },
      nudge: function () {
        return rem();
      },
      // `top` is where the artifact starts inside the column, which is under a
      // label and is not zero; the drag reports a position in the column, and
      // the property is a height. 7rem is the floor under the boundary: the
      // lists' own 5rem plus the label that says what they are.
      value: function (y, rect) {
        var u = rem();
        var top = card.getBoundingClientRect().top - rect.top;
        return Math.round(clampTo(y - top, 8 * u, rect.height - top - 7 * u)) + 'px';
      },
    });
  }

  // ── Ask, as it happens ────────────────────────────────────────────────────
  //
  // Two requests, because `EventSource` is GET-only and a GET that runs a model
  // call is a free-inference hole: the POST parks the question and hands back an
  // id, and the stream spends that id once. Everything the page inserts was
  // rendered and sanitized by the server — this driver moves HTML it was handed
  // and text it puts in text nodes, and never builds markup out of an answer.
  function askDriver() {
    var form = document.getElementById('box-form');
    if (!form) return;
    // The press is delegated, rather than bound to the button found here.
    // There are two states with no Ask verb in the page — an install with no
    // model, and a base with nothing held — and only the second one ends: the
    // verb arrives out-of-band on the first capture (see `syncHeld`), long
    // after this runs, and a driver that had returned at this line left it
    // dead. Nothing below fires until a press that reaches the button, so
    // arming the driver where there is no model costs nothing.
    var box = form.querySelector('textarea[name="q"]');
    if (!box) return;
    var live = document.getElementById('ask-live');
    var reasoning = document.getElementById('ask-reasoning');
    // The disclosure around it: what is shown or hidden is the box, while the
    // text still streams into the div inside it.
    var reasoningBox = document.getElementById('ask-reasoning-box');
    var progress = document.getElementById('ask-progress');
    var rail = document.getElementById('results');
    var result = document.getElementById('ask-result');
    var status = document.getElementById('ask-status');
    var spinner = document.getElementById('search-spinner');
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
      spinner.textContent = 'searching…';
      form.classList.remove('asking');
      setBusy(false);
    }

    // One act in flight. From the press of Ask until the answer lands, the box
    // is read-only, both verbs are disabled and only Stop is live.
    //
    // Read-only rather than disabled, which is what it was. `disabled` is the
    // one state that also makes text unselectable and hands focus back to the
    // body, so the question being answered could not be copied out of the box
    // holding it — while the CSS went to the trouble of keeping that text
    // legible, on the grounds that a box someone is waiting on is still a box
    // someone is reading. Read-only keeps the caret, the selection and the
    // focus, and silences search-while-type just as completely: nothing can be
    // typed or pasted into it, so no `input` is fired and the form's
    // `hx-trigger` goes quiet on its own — there is no second mechanism and no
    // flag to keep in sync with this one.
    //
    // The one thing `disabled` did that this does not is drop `q` from the
    // serialization. That was never what kept a stray request honest anyway;
    // `configRequest` below is, and it cancels every request this form makes
    // while the ask owns it.
    //
    // The re-enable lives in `stop()` and nowhere else, because every exit
    // already runs through it: the answer completing, the Stop button, and the
    // transport error that `fail()` funnels into it. That last one is the
    // reason it cannot live on the `done` handler — a dropped connection would
    // leave the box read-only forever, with no way back but a reload.
    function setBusy(busy) {
      box.readOnly = busy;
      form.setAttribute('aria-busy', busy ? 'true' : 'false');
      var vs = form.querySelectorAll('[data-verb]');
      for (var i = 0; i < vs.length; i++) vs[i].disabled = busy;
      // The chips fire the form too, and leaving them live was not a smaller
      // hole than leaving the box live: a disabled control is left out of the
      // serialization, so a chip clicked mid-answer sent `q` empty and swapped
      // a "0 results" rail over the citations the answer was being written
      // from.
      var chips = form.querySelectorAll('#kind-chips input');
      for (var j = 0; j < chips.length; j++) chips[j].disabled = busy;
      // A box someone emptied while the answer streamed must not come back
      // with live verbs over nothing to act on.
      if (!busy) box.dispatchEvent(new Event(VERB_SYNC, { bubbles: true }));
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

    // No search leaves this form while an ask owns the page. A read-only box
    // fires no `input`, but that silences neither a debounce timer that armed
    // just before Ask was pressed nor the "← results" anchor firing `submit`,
    // and either one swapped the idle rail over the citations the answer was
    // being written from. It matters more since the box stopped being
    // `disabled`: `q` now serializes while the ask runs, so a request that got
    // out would search for the question instead of sending it empty — a live
    // rail either way. `configRequest` is cancelable and is the one gate every
    // htmx request from this form passes through.
    form.addEventListener('htmx:configRequest', function (e) {
      if (form.getAttribute('aria-busy') === 'true') e.preventDefault();
    });

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
      // The rail was emptied when the ask began, and the heading over it is
      // only rewritten by a citations event that is not coming. Left alone it
      // kept claiming the last search's count over zero rows.
      var head = document.getElementById('rail-head');
      if (head) head.textContent = '';
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
        var payload = JSON.parse(e.data);
        rail.innerHTML = payload.rail;
        enhance(rail);
        // The rail holds what the current act produced. The results that were
        // here are gone, because they were produced by a different act and
        // have nothing to do with the excerpts this answer was written from.
        //
        // The query is still in the box unedited, so nothing re-triggers the
        // search on its own. This anchor is the way back — it re-fires the
        // form's own request with whatever the box holds now, rather than
        // storing a result set that would go stale the moment it was kept.
        var head = document.getElementById('rail-head');
        if (head) {
          var n = rail.querySelectorAll('.rail-item').length;
          head.innerHTML = '';
          var label = document.createElement('span');
          label.className = 'result-count';
          label.textContent = 'Written from · ' + n;
          var back = document.createElement('a');
          back.className = 'quiet-link';
          back.href = '#';
          back.setAttribute('data-rerun', '');
          back.textContent = '← results';
          head.appendChild(label);
          head.appendChild(back);
        }
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
        // The box scrolls inside the pinned workspace, so the tail is what
        // leaves the screen. Followed while the reader is at it, and left
        // alone once they have scrolled up to reread something.
        var atTail = live.scrollHeight - live.scrollTop - live.clientHeight < 40;
        live.appendChild(document.createTextNode(JSON.parse(e.data).text));
        if (atTail) live.scrollTop = live.scrollHeight;
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

    form.addEventListener('click', function (e) {
      var askBtn = e.target.closest ? e.target.closest('[data-verb="ask"]') : null;
      // `disabled` is checked as well as matched: a disabled button fires no
      // click of its own, but it is inside a form that does.
      if (!askBtn || askBtn.disabled) return;
      e.preventDefault();
      var q = box.value;
      if (!q.trim()) return;
      // The three regions move with the act, and an ask is an act. Bound to
      // the keystroke alone this never ran on the ask door: `/ui/ask?q=…`
      // renders `idle_state` true — the box arrives filled and nothing is
      // going to search — so `#rail` and `#pane` carry `hidden`, and the
      // stylesheet's `.pane[hidden]{display:none}` means the answer streamed
      // into a collapsed pane. The reader watched a spinner and then nothing.
      // The door pre-fills the box server-side, so no `input` event was ever
      // coming to reveal them.
      hideIdle();
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
      // The pane shows one act at a time: the artifact a result click left
      // here belongs to the search this ask supersedes.
      var pane = document.getElementById('pane-content');
      if (pane) pane.textContent = '';
      live.hidden = true;
      reasoningBox.hidden = true;
      reasoningBox.open = false;
      // The pane is now the act, and the layout has to be told: `has-selection`
      // was set by a result swap and nothing else, so an ask claimed no room at
      // all. Wide, the rail kept the 40rem it holds while nothing is open and
      // the answer streamed into what was left; narrow, the rail comes first in
      // the DOM and every excerpt sat above the answer.
      //
      // Its own class rather than `has-selection`, which narrow reads as
      // "hide the rail": the excerpts are what this answer was written from and
      // the `[n]` links point into them, so hiding them is the one thing that
      // must not happen here. Narrow puts the rail after the answer instead.
      var regions = document.querySelector('.regions');
      // `pane-open` goes with the artifact that was just emptied out of the
      // pane above: the class is a statement about what `#pane-content` holds,
      // and what holds the pane now is the answer.
      if (regions) {
        regions.classList.remove('pane-open');
        regions.classList.add('answering');
      }
      form.classList.add('asking');
      setBusy(true);
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
  //
  // A pursuit signal only. This used to carry the search the pane was opened
  // from, and a long enough read was written as that search having found its
  // answer — but the beacon flushes as the pane is *left*, so it landed after
  // the buttons under the result and overwrote them. The bar answers now.
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

  // ── The end of the empty base ─────────────────────────────────────────────
  //
  // The workspace is rendered once. A capture posts to `/ui/capture` and swaps
  // a receipt, and the rail refresh swaps `#results` — nothing else on the page
  // moves. So the paste that ends an empty base left the whole page still
  // arranged for one: no Ask verb, no shortcut row, a box hint introducing the
  // application and a pane saying what will happen to the first capture, all
  // of it describing a state the operator had just left, until a reload nobody
  // had a reason to perform.
  //
  // `/ui/held` returns those four regions as out-of-band swaps, written from
  // the same partials the page itself includes, so there is one author for
  // what the held state says. The placeholders are not among them and ride the
  // textarea instead — see the box's `HELD_SYNC` listener.
  //
  // Runs at most once: `data-held` is written before the request goes, so the
  // second capture of a session asks for nothing.
  function syncHeld() {
    var form = document.getElementById('box-form');
    if (!form || form.getAttribute('data-held') !== '0') return;
    form.setAttribute('data-held', '1');
    // `swap: "none"` because every region in the answer names its own target.
    // A `target` is still required, and the form is the one element here that
    // is certain to exist.
    htmx.ajax('GET', '/ui/held', { target: form, swap: 'none' })
      // A capture that stored and a page that failed to redress itself is
      // still a capture that stored: the receipt stands, and the state is
      // re-armed so the next one tries again rather than the page staying
      // wrong for the session.
      .catch(function () { form.setAttribute('data-held', '0'); })
      .then(function () {
        if (form.getAttribute('data-held') !== '1') return;
        var box = form.querySelector('textarea[name="q"]');
        if (box) box.dispatchEvent(new Event(HELD_SYNC, { bubbles: true }));
        // The shortcut row arrived hidden, as it always does. This is what
        // decides whether there is a keyboard to show it to.
        keyHint();
      });
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

  // A day link carries the browser's zone, so the day page reads the day
  // the viewer means and not the server's.
  //
  // `data-day-at` is the other half of that, and the half `?tz=` could not
  // reach: where the server wrote the date into the path itself it wrote it in
  // UTC, while the day page builds its window in the viewer's zone. East of
  // Greenwich every capture between local midnight and the offset therefore
  // linked to the previous day, which answered "Nothing on this day." Given the
  // instant, the path is rebuilt here from the local date instead.
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

  // `YYYY-MM-DD` for an instant, in the browser's own zone. Built from the
  // parts rather than sliced out of `toISOString`, which is UTC and is the bug
  // above; `en-CA` is the locale that already spells a date this way.
  function localDay(ms) {
    var d = new Date(ms);
    if (isNaN(d.getTime())) return '';
    var p = ('0');
    return d.getFullYear() + '-' +
      (p + (d.getMonth() + 1)).slice(-2) + '-' +
      (p + d.getDate()).slice(-2);
  }

  // The idle column is hidden, not removed. Removing it is how a reminder
  // armed by a capture stayed invisible until a reload: Capture empties the
  // box, the idle state is correct again, and there was no `#due` left in the
  // document for the band to swap into.
  //
  // The offer used to be removed here rather than hidden with the rest, on the
  // argument that it is a measured impression and an offer reappearing in a
  // new situation would be a second one nobody had. That argument does not
  // hold: `confirmOffer` runs on `htmx:afterSwap` and nowhere else, so the
  // impression is written exactly once, when the fragment arrives, and
  // hiding a card that is already on screen writes nothing at all. What the
  // removal did instead was destroy the card on the first keystroke — after
  // which the column came back without it for the rest of the session, and a
  // capture left a page that was not the page it started as.
  //
  // The race it was reaching for is real and is handled where it happens: an
  // offer whose fetch lands *after* the keystroke was never seen, and
  // `dropOffer` on the swap is what refuses to count it.
  //
  // The three regions move together because they are one statement about what
  // the page is doing: with an intent expressed there is a rail and a pane and
  // a chip row to narrow with; with none there is the column.
  function hideIdle() {
    document.documentElement.classList.add('typing');
    var idle = document.getElementById('idle');
    if (idle) idle.hidden = true;
    show('rail', true);
    show('pane', true);
    show('kind-row', true);
  }

  // Is the pane holding an act of its own — an ask in flight, or the answer
  // one left standing?
  //
  // `askDriver` never clears the box, on purpose: the question stays legible
  // beside the answer it produced. So emptying the box by hand — select all,
  // Delete — is not a statement that the answer is over, and treating it as
  // one hid `#pane` and `#rail` mid-stream, taking the answer and the "back to
  // results" anchor with them. `show` is the only writer of those flags, so
  // nothing brought them back until another keystroke or a reload.
  function paneIsBusy() {
    var form = document.getElementById('box-form');
    if (form && form.classList.contains('asking')) return true;
    var live = document.getElementById('ask-live');
    if (live && !live.hidden && live.textContent.trim()) return true;
    var result = document.getElementById('ask-result');
    return !!(result && result.textContent.trim());
  }

  // The box is empty again, so the column is right again. The due band is
  // re-fetched rather than left as it stands: it may have been hidden through
  // a capture that armed something, and what it holds is a minute old.
  //
  // Except where the pane is mid-answer: see `paneIsBusy`.
  function showIdle() {
    // Before the class, not after: `html.typing` says an intent is expressed,
    // and an answer standing in the pane is one whether or not the box that
    // asked for it still holds the question.
    if (paneIsBusy()) return;
    document.documentElement.classList.remove('typing');
    var idle = document.getElementById('idle');
    if (idle) idle.hidden = false;
    show('rail', false);
    show('pane', false);
    show('kind-row', false);
    var due = document.getElementById('due');
    if (due) htmx.trigger(due, 'refresh');
  }

  function show(id, on) {
    var el = document.getElementById(id);
    if (el) el.hidden = !on;
  }

  // The fourth region, at load. `workspace.html` renders `hidden` onto `#idle`,
  // `#rail`, `#pane` and `#kind-row` from `idle_state` — but `html.typing` is
  // set only by `hideIdle`, and `hideIdle` runs from a keystroke or a submit
  // that a server-rendered page never fires. A deep-linked `/ui?q=…`, or the
  // `ask_door` redirect when `[infer.ask]` is off, therefore opened with the
  // three regions correct and the class missing: on a phone the capture box
  // sat undocked, the example chips lay over the results, and `#vec-bg` stayed
  // at 0.72 opacity behind the text until the first keystroke.
  //
  // Read off `#idle` rather than the box, so the browser agrees with the
  // decision the server already made rather than making a second one.
  function syncIdle() {
    var idle = document.getElementById('idle');
    if (idle) document.documentElement.classList.toggle('typing', idle.hidden);
  }

  // The offer alone, and for exactly one case: the fetch that lands after the
  // keystroke that dismissed it. That card was never on screen, so counting it
  // as shown would put a population that structurally could not click into the
  // denominator of the hit rate. An offer that *did* arrive in time is not
  // touched here — it is hidden with the column and comes back with it.
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
    // The box is a textarea, and has been since the three pages became one.
    // Matched as `input[name=q]` this never bound, so the offer outlived the
    // first keystroke and kept logging impressions beside live queries.
    var box = document.querySelector('textarea[name="q"]');
    if (!box) return;
    // Not `{ once: true }`. The column comes back when the box is emptied, so
    // this is a transition and not a one-way dismissal: the offer is hidden
    // with the column and returns with it. What `offerDismissed` records is
    // narrower — that a keystroke has happened — so that an offer whose fetch
    // lands *after* it can be dropped by `dropOffer` instead of appearing over
    // a query nobody asked it about.
    box.addEventListener('input', function () {
      if (box.value.trim()) {
        offerDismissed = true;
        hideIdle();
      } else {
        showIdle();
      }
    });
  }

  // Say what went wrong where the answer was going to be.
  //
  // The server's own reason, verbatim, the same rule the upload path follows:
  // a generic "something went wrong" would hide what actually goes wrong here
  // — an embedder that is down, a vector store that cannot answer, a body over
  // the limit. `textContent`, because an error string is the one payload on
  // this page that went through no sanitizing renderer.
  function failedSwap(target, xhr) {
    if (!target || !target.id) return;
    var reason = 'engram is unreachable.';
    if (xhr) {
      try {
        reason = JSON.parse(xhr.responseText).error || ('engram answered ' + xhr.status + '.');
      } catch (err) {
        reason = 'engram answered ' + xhr.status + '.';
      }
    }
    var box = document.createElement('div');
    box.className = 'flag';
    box.setAttribute('role', 'status');
    // Not named `b`: `the_browser_sends_exactly_the_fields_this_struct_reads`
    // reads the context bundle's fields off this file by looking for lines
    // that start with `b.`, and a one-letter element handle here is
    // indistinguishable from a bundle assignment.
    var title = document.createElement('b');
    title.textContent = 'That did not work';
    var why = document.createElement('div');
    why.textContent = reason;
    var wrap = document.createElement('div');
    wrap.appendChild(title);
    wrap.appendChild(why);
    box.appendChild(wrap);
    target.textContent = '';
    target.appendChild(box);
    // The heading names the act that filled the rail, and nothing filled it.
    var head = document.getElementById('rail-head');
    if (head && target.id === 'results') head.textContent = '';
  }

  // The two example phrasings under the box. A chip fills the box and stops
  // there: the point is to put the phrasing in front of you, let the echo
  // answer it, and leave the press to you. The synthetic `input` is what the
  // form's own trigger and the idle column's hide both listen for, so a filled
  // box behaves exactly as a typed one does.
  //
  // Bound on the document because `_box_hint.html` is swapped out of band the
  // first time a capture ends an empty base, and a listener on the old node
  // would go with it.
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

  // The zone the echo reads dates in. Filled once, on load: it cannot be
  // rendered server-side, and `Intl` is the only thing that knows it.
  function boxZone() {
    var el = document.getElementById('box-tz');
    if (el) el.value = Intl.DateTimeFormat().resolvedOptions().timeZone || '';
  }

  // The way back to results after an Ask. Bound on the document because the
  // anchor is written into the rail by the stream driver, long after load.
  function railBack() {
    document.addEventListener('click', function (e) {
      var back = e.target.closest && e.target.closest('[data-rerun]');
      if (!back) return;
      e.preventDefault();
      var form = document.getElementById('box-form');
      if (form) htmx.trigger(form, 'submit');
    });
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
    // Read at every sync, never captured. A `querySelectorAll` here is a
    // static list, and the Ask button is not always in it: an empty base
    // renders no Ask verb at all — `_ask_verb.html` has nothing to offer until
    // something is held — and the first capture swaps a fresh `disabled`
    // button into the slot out of band, long after this ran. That button was
    // in no list, so nothing ever enabled it or lit it, and a new base's Ask
    // verb stayed dead until a full reload. This driver became the only thing
    // that decides how the verb looks when the hard-coded accent came off it,
    // so a list that cannot see it is a verb that cannot work.
    function verbs() {
      return form.querySelectorAll('[data-verb]');
    }

    // Neither verb has anything to act on while the box is empty — except
    // Capture, whose act can also be a staged file. A photo over an empty box
    // left Capture disabled and the file with no way into the base: the state
    // rendered, the control did not exist. The staged box is read directly
    // rather than mirrored into a flag, and stage()/unstage() fire `input` on
    // the box so this runs when staging changes, not only when typing does.
    function sync() {
      // An act in flight owns the verbs, and `setBusy` in askDriver is what
      // says so. Both write `disabled` on the same buttons, so without this
      // anything that fires `input` mid-answer handed them back — staging a
      // file does, and a file can be dropped on the page while an answer is
      // streaming.
      if (form.getAttribute('aria-busy') === 'true') return;
      var hasText = !!box.value.trim();
      var stagedEl = document.getElementById('staged');
      var hasFile = !!(stagedEl && !stagedEl.hidden);
      // Which verb the accent goes to. Not which verb runs — that stays a
      // press — only which one is lit. A trailing question mark or a leading
      // question word says Ask; a paste (long, multi-line, or a file) says
      // Capture; a short plain sentence lights neither, because typing
      // already searches and there is nothing to press for.
      var text = box.value.trim();
      var asksLike = /\?\s*$/.test(text) ||
        /^(who|what|when|where|why|how|which|is|are|do|does|did|can|could|should|wer|was|wann|wo|warum|wie|welche|ist|sind|kann|hat|habe)\b/i.test(text);
      var keepsLike = hasFile || text.length > 200 || text.indexOf('\n') !== -1;
      var buttons = verbs();
      for (var i = 0; i < buttons.length; i++) {
        var verb = buttons[i].getAttribute('data-verb');
        buttons[i].disabled = verb === 'capture'
          ? !(hasText || hasFile)
          // A staged file has made the box that file's note. Asking a note is
          // not a thing to do, and the answer would land beside a file the
          // question was never about.
          : (!hasText || hasFile);
        var lead = verb === 'ask' ? (asksLike && !keepsLike) : keepsLike;
        buttons[i].classList.toggle('btn-accent', lead && !buttons[i].disabled);
      }
    }

    // One line to start, growing to a ten-line cap and then scrolling inside
    // itself. Measured off `scrollHeight` rather than counting newlines,
    // because a pasted paragraph is many lines and carries none.
    //
    // This is not the box changing shape on its own in the sense the panel
    // rules out: it never becomes a different control, and it never decides
    // what the words in it are for. It only stops hiding them.
    //
    // The padding and the border have to be added back, and leaving them out
    // was worth two pixels of permanent scroll. `.box` is `border-box`, so the
    // height set here is the outside of the element, while `scrollHeight` is
    // the inside — content and padding, never the 1px border on each edge. The
    // box therefore ended two pixels shorter than the text it was measured
    // from, every time, and with `overflow-y: auto` above it that is a live
    // scrollbar on a box that has nothing to scroll. The cap is the other half
    // of the same mistake: ten lines of text plus the padding they sit in, or
    // the tenth line is the one the padding eats.
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

    // The height is a measurement of wrapped text, and text re-wraps whenever
    // the box changes width. Bound to `input` alone it was only ever correct
    // for the width the last keystroke was typed at: narrow the window, or turn
    // a phone, and a paragraph that grew to four lines is still four lines tall
    // with six lines in it — clipped, scrolling, and no key coming to fix it.
    window.addEventListener('resize', grow);
    // Inter loads with `font-display: swap`, so the first measurement is of the
    // fallback face and the swap re-wraps everything under a height nothing
    // will recompute — the box is not typed into before it is read.
    if (document.fonts && document.fonts.ready) document.fonts.ready.then(grow);

    // The two verbs, without a pointer. This is not the box inferring one from
    // a newline — the rule it is built on and keeps: Enter puts in a line
    // break here and always will. The chord is a second, deliberate gesture,
    // the same act as the button and no more ambiguous, and without it the one
    // hand that reached the box with `/` had no way back out of it.
    //
    // Routed through the button rather than the handler behind it, so that
    // everything the button already knows goes on being true: an empty box or
    // an ask in flight leaves it disabled, and a disabled verb does nothing
    // here either. Where the install has no Ask, there is no button and the
    // unshifted chord is simply not a key.
    box.addEventListener('keydown', function (e) {
      if (e.key !== 'Enter' || !(e.metaKey || e.ctrlKey) || e.altKey) return;
      var verb = form.querySelector(
        '[data-verb="' + (e.shiftKey ? 'capture' : 'ask') + '"]');
      if (!verb || verb.disabled) return;
      e.preventDefault();
      verb.click();
    });

    // The long placeholder names all three verbs, which is what the box needs
    // said and what the phone has no room for: one row at that width clips it
    // around the thirtieth character, and the hint that would have carried the
    // rest is `display: none` there. Read off the element rather than branched
    // on in the template, because app.js is one file for every installation —
    // the same reason `data-eager` exists below. Bound to the query, not read
    // once, so a rotation is not a sentence that stays cut in half.
    var narrow = window.matchMedia('(max-width: 40rem)');
    var wide = box.placeholder;
    var short = box.getAttribute('data-placeholder-narrow') || wide;
    // And a third, for when the box is not a query box at all: a staged file
    // makes it that file's note. Asked here rather than written from
    // `stage()`, because this function owns the placeholder and re-runs on
    // every rotation — set from outside, the note's prompt would survive until
    // the first turn of the phone and then be replaced by a search hint over
    // an annotation half typed. Short enough for either width.
    var NOTE_HINT = 'What is it, why keep it?';
    function fitPlaceholder() {
      var stagedEl = document.getElementById('staged');
      box.placeholder = stagedEl && !stagedEl.hidden
        ? NOTE_HINT
        : (narrow.matches ? short : wide);
    }
    narrow.addEventListener('change', fitPlaceholder);
    // And the two sentences an empty base's box will use once something is
    // held, carried on the element by the template. Read across here rather
    // than swapped in with the rest of the held state: replacing a textarea
    // takes the caret, the selection and anything typed since with it.
    box.addEventListener(HELD_SYNC, function () {
      wide = box.getAttribute('data-placeholder-held') || wide;
      short = box.getAttribute('data-placeholder-held-narrow') || short;
      fitPlaceholder();
    });
    fitPlaceholder();

    sync();
    grow();
  }

  // ── Capture, as a verb ──────────────────────────────────────────────
  // Everything the capture page used to do inline, on the one box. The
  // reasons are the page's own and travel with the code.
  function captureVerb() {
    var form = document.getElementById('box-form');
    if (!form) return;
    var box = form.querySelector('textarea[name="q"]');
    var verb = form.querySelector('[data-verb="capture"]');
    if (!box || !verb) return;

    var drop = document.getElementById('drop');
    var picker = drop && drop.querySelector('input[type=file]');
    // Same reason as EAGER: read off the picker the server rendered, which
    // names `image/*` only where `[infer.vision]` is configured.
    var VISION = !!picker && (picker.getAttribute('accept') || '').indexOf('image/*') >= 0;

    // ── What is waiting to be sent ────────────────────────────────────────
    // A file arriving is not a capture. It is held here until Capture is
    // pressed, so the note can be written into the box above it and the wrong
    // photo can be removed — on a phone the camera hands the picture back the
    // moment it is taken, and uploading on arrival meant the operator never
    // got a say.
    var staged = null, stagedUrl = null;
    // Set from the press until the upload answers. `staged` is cleared before
    // the file is sent, and a debounce armed by the last word of the note
    // fires after that — a search for the annotation, which is the one thing
    // the guard below exists to prevent. The box is not cleared early instead:
    // a capture that fails keeps its note for the second press.
    var sending = false;
    var stagedBox = document.getElementById('staged');

    // The box is that file's note while one is staged, and typing an
    // annotation into a live search box is an embedding call, an activation
    // bump and a Judge-queue row per phrase — for text nobody asked as a
    // question. Cancelling the form's own requests rather than rewriting its
    // `hx-trigger`: the trigger is the template's to say, and this holds
    // whatever it says. Capture does not go through the form, and Ask is
    // disabled in `boxVerbs` while a file waits.
    form.addEventListener('htmx:beforeRequest', function (e) {
      if (staged || sending) e.preventDefault();
    });
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
      // Capture may have been armed by this file alone. See `sync` in
      // boxVerbs, which listens for this.
      box.dispatchEvent(new Event(VERB_SYNC, { bubbles: true }));
    }

    // `restore` is the failed upload coming back: the same file, already
    // described, and no reason to take the caret off the note a second time.
    //
    // Where a capture answers. A press was once two captures — a staged file
    // and the text above it, which is now that file's note — and both wrote
    // this node whole: whichever request landed last wiped the other's line,
    // an upload's error included. A line each, cleared once per press. One
    // press writes one line now, and the appending swap below is what keeps
    // that true rather than what makes it necessary.
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
      // Said now rather than after the operator has written a note for a file
      // this server was never going to read.
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
      if (!restore && window.matchMedia('(hover: hover)').matches) box.focus();
      // A file over an empty box is still something Capture can act on. See
      // `sync` in boxVerbs, which listens for this.
      box.dispatchEvent(new Event(VERB_SYNC, { bubbles: true }));
    }

    function send(file, note) {
      if (!file) return;
      var isImage = file.type.indexOf('image/') === 0;
      // By name as well as by type: a drop from some file managers carries no
      // type at all, and the door judges those by their name too.
      var isPdf = file.type === 'application/pdf' || /\.pdf$/i.test(file.name || '');
      var result = receipt();
      if (isImage && !VISION) {
        result.textContent = 'Image capture is not configured on this server.';
        return;
      }
      // The upload does not go through htmx, so the spinner beside the button
      // never lit for it. On a phone a photo is the slowest thing this page
      // sends and pressing Capture looked like it had done nothing.
      result.textContent = 'Sending…';
      var payload = new FormData();
      if (note) payload.append('note', note);
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
          // Lifted before the branches: `refreshRail` below submits the form,
          // and a guard still standing would cancel the very refresh the
          // capture was made for. Any debounce armed while the note was being
          // typed has long since tried to fire and been cancelled — it was
          // 120ms behind the press, and this is a round trip later.
          sending = false;
          // The server's reason, verbatim. A generic "upload failed" would
          // hide what actually goes wrong here: wrong type, wrong encoding,
          // an image door that is closed.
          result.textContent = pair[0]
            ? (isImage ? 'Captured — the photo is queued to be read.'
               : isPdf ? 'Captured — the PDF is queued to be extracted.'
               : 'Captured.')
            : (pair[1].error || 'Upload failed.');
          if (pair[0]) {
            // The note went in with the file. The box is a search box again
            // and holding the words it sent is not that.
            box.value = '';
            box.dispatchEvent(new Event(VERB_SYNC, { bubbles: true }));
            refreshRail();
            // A photo or a PDF ends an empty base exactly as a paste does, and
            // this path never goes near `/ui/capture` — without this the first
            // capture of a phone-only base left the page arranged for nothing.
            syncHeld();
          } else {
            // It never left. Put it back where it was, so the fix is a second
            // press rather than a second trip to the camera.
            stage(file, true);
          }
        });
    }
    if (picker) picker.addEventListener('change', function () { stage(picker.files[0]); });

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
    // Both the staged box and the picker light up, because only one of them is
    // on screen at a time: before anything is staged the box is `display:
    // none`, so a class on it alone made the first drop — the common one —
    // a drag with no visible target anywhere on the page.
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

    // The Capture verb. A staged file is what it sends; with nothing staged it
    // posts what is in the box. Text typed above a staged file is not thrown
    // away — it goes as the capture it is, in the same press.
    //
    // The box's own form is a GET that searches, so the text cannot ride it:
    // this posts to the same `/ui/capture` the old form did, with the same
    // fields, and lands in the same fragment.
    // The rail after a capture. A synthetic `input` keeps the verbs honest
    // but matches nothing in the form's trigger list, so the rail kept
    // showing results for text that no longer existed. The form's own submit
    // is the refresh: with the box just emptied the results endpoint answers
    // with the idle rail, whose "Last captured" row is the capture landing.
    function refreshRail() {
      htmx.trigger(form, 'submit');
      // The capture emptied the box, so the idle column is correct again — and
      // this is the moment a reminder captured a second ago wants to appear.
      // The band's own polling covers the gap between here and the background
      // job that reads the intent out of the note.
      showIdle();
    }

    // Hands back its promise: the press that carries a file too runs the two
    // one after the other, and this is how the second one knows to start.
    function postText() {
      var text = box.value.trim();
      if (!text) return Promise.resolve();
      var fromAsk = document.querySelector('input[name="from_ask"]');
      // htmx settles this promise for every answer the server gives, a 500
      // among them — it rejects only for a request that never completed at
      // all — so the promise on its own says nothing about whether anything
      // was stored, and the clear below ran on the failures too. The verdict
      // is on `htmx:afterRequest`, which fires before the promise settles.
      //
      // Matched on the path because the box's own search is firing requests
      // from the same body at the same time, and either one's status would
      // otherwise be read as this one's.
      var stored = false;
      function verdict(e) {
        var path = e.detail && e.detail.pathInfo;
        if (!path || path.requestPath !== '/ui/capture') return;
        stored = !!e.detail.successful;
      }
      document.body.addEventListener('htmx:afterRequest', verdict);
      return htmx.ajax('POST', '/ui/capture', {
        target: '#capture-result',
        // Appended, not replaced: an upload sent by the same press has its own
        // line in here, and a receipt that overwrites the other one is how the
        // page came to report only whichever request was slower.
        swap: 'beforeend',
        // The zone travels with the text. Without it the box stored none and
        // the server read *tomorrow at 9* in its own default, while the echo
        // directly under the box had already told the operator 09:00 in theirs.
        values: {
          text: text,
          from_ask: fromAsk ? fromAsk.value : '',
          tz: (document.getElementById('box-tz') || {}).value || ''
        }
      // A transport failure rejects, and nothing catching it was an unhandled
      // rejection on top of a capture that visibly did nothing.
      }).catch(function () {}).then(function () {
        document.body.removeEventListener('htmx:afterRequest', verdict);
        // Cleared only on the path that stored something: a failed capture
        // that emptied the box would lose the text it failed to keep, and the
        // error fragment beside it is not where the text went.
        if (!stored) return;
        // The provenance was about the text that just went in, and the box
        // stays open. Left standing, the next thing pasted into it was stored
        // as the same model answer — `origin = "ask"`, that question, those
        // citations — for words the operator typed themselves.
        var kept = document.getElementById('kept-from');
        if (kept && kept.parentNode) kept.parentNode.removeChild(kept);
        box.value = '';
        box.dispatchEvent(new Event(VERB_SYNC, { bubbles: true }));
        refreshRail();
        // The base may have just stopped being empty. Asked after the box is
        // cleared, so the placeholder this uncovers is the one that lands.
        syncHeld();
      });
    }
    verb.addEventListener('click', function (e) {
      e.preventDefault();
      clearReceipts();
      // A staged file is one capture, annotated — never two. The box is read
      // here, at press time, which is what makes the order the file and the
      // words arrived in stop mattering: nothing moved when the file was
      // staged, and nothing moves now but this read.
      //
      // `from_ask` needs no guard: `postText` is its only sender, and a staged
      // file no longer reaches it. A whole model answer turned into a caption
      // on somebody's PDF is not what that provenance claims.
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

  // ── Vector background ───────────────────────────────────────────────────
  // A slow monochrome rotation of the store's own vectors, projected to 3-D
  // on the server (random projection, fixed seed). Decorative, and held to a
  // decoration's budget: one revalidation per load, a few bytes of it
  // whenever the store has not moved, a 12 fps ceiling, no loop at all under
  // prefers-reduced-motion, and silence on every failure — a picture of the
  // database may never take the page down with it.
  function vectorBg() {
    var CACHE_KEY = 'engram.vbg';

    // Login and any other page without chrome get no canvas. The topbar is
    // exactly the marker: login empties the nav block.
    //
    // It is also where the snapshot is dropped. Every other key this file
    // writes is a local preference; this one is server data — a projection of
    // one account's store — and `/auth/logout` clears the cookie and nothing
    // else. Without this, signing out and signing a second account in on the
    // same browser redrew the first one's cloud straight from `localStorage`,
    // with no authorization anywhere on the path.
    if (!document.querySelector('.topbar')) {
      try { localStorage.removeItem(CACHE_KEY); } catch (e) {}
      return;
    }
    var FPS = 12;
    var SPIN = 0.01 * Math.PI; // rad/s — slow enough to feel geological
    var AXIS_MIN = 6, AXIS_MAX = 14; // seconds on one rotation axis
    var BUCKETS = 8;
    // Nothing nearer to the camera than this is drawn. The projection divides
    // by `radius - z`, so a point swinging towards the eye has no bound on how
    // far off-canvas it lands — which is what turned the axes into streaks
    // running clean off every edge of the window with no endpoint in sight.
    // Points are pulled from a bounded cloud and never come close; the axes
    // are drawn out to their own ends, so they do.
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

    // The axis changes every few seconds — a fixed one reads as a screensaver
    // on rails; a wandering one reads as an object.
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

    // The one colour, re-read every draw: a theme flip mid-session is picked
    // up on the next frame with no listener at all.
    function ink() {
      var hex = getComputedStyle(document.body).getPropertyValue('--color-fg-muted').trim();
      var m = /^#([0-9a-f]{6})$/i.exec(hex);
      if (!m) return { r: 122, g: 122, b: 114 };
      var n = parseInt(m[1], 16);
      return { r: (n >> 16) & 255, g: (n >> 8) & 255, b: n & 255 };
    }

    function resize() {
      // Capped rather than native: at 3x a phone paints nine times the pixels
      // for a picture whose finest detail is a two-pixel dot.
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
      // `!(d > 0.01)` rather than `d < 0.01`, so a NaN coordinate fails here
      // instead of sailing through: every comparison against NaN is false, and
      // one `null` on the wire or one mangled cache entry used to reach
      // `buckets[Math.floor(NaN)].push` and throw twelve times a second for
      // the life of the page.
      if (!(d > 0.01)) return null;
      var unit = Math.min(W, H) * 0.52;
      var s = (radius * unit) / d;
      // Centred horizontally, but only on a window that is not much taller
      // than it is wide. On a portrait screen the page's content sits in the
      // top third and a cloud centred on the viewport hangs well below all of
      // it, reading as an object on the page rather than as the ground behind
      // one. Pulled up towards the content, and never so far that its own top
      // leaves the window.
      var cy = Math.max(unit, Math.min(H * 0.5, H * 0.5 - (H - W) * 0.22));
      var sx = x1 * s + W * 0.5, sy = -y2 * s + cy;
      if (!isFinite(sx) || !isFinite(sy)) return null;
      return { sx: sx, sy: sy, depth: z2, s: s, unit: unit };
    }

    function drawAxes(c) {
      var L = 1.2, T = 0.05, TICKS = [-1, -0.5, 0.5, 1], STEPS = 24;

      // Walked in steps rather than drawn end to end. A straight line in space
      // is still straight on the canvas, so the subdivision buys nothing for
      // shape — it buys the ability to stop the stroke exactly where the axis
      // crosses the near plane, and to resume it on the far side. Drawn as one
      // segment, an axis with one end near the eye either vanished whole or
      // stretched into a streak the length of the window.
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
      // Dimmer than the cloud it frames. These are scaffolding: they say which
      // way the thing is turning, and nothing else.
      ctx.strokeStyle = 'rgba(' + c.r + ',' + c.g + ',' + c.b + ',0.13)';
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
      // Depth shading batched by bucket: one fill per bucket rather than one
      // per point, so 2000 dots cost 8 draw calls.
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
        // Dim overall, dimmer far away: the cloud is present, never loud.
        var alpha = 0.07 + ((k + 0.5) / BUCKETS) * 0.38;
        ctx.beginPath();
        ctx.fillStyle = 'rgba(' + c.r + ',' + c.g + ',' + c.b + ',' + alpha.toFixed(3) + ')';
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
      points = pts;
      resize();
      pickAxis(0);
      window.addEventListener('resize', function () {
        resize();
        if (reduced) draw();
      });
      if (reduced) {
        // One still frame — which then has to be repainted by hand. `ink()` is
        // re-read every draw, and with the loop off there is no next draw: the
        // toggle rewrites `data-theme` in place with no reload, so a
        // reduced-motion reader who switched to dark kept the light theme's
        // ink on a dark page until they navigated.
        draw();
        new MutationObserver(draw).observe(document.documentElement, {
          attributes: true, attributeFilter: ['data-theme']
        });
        var dark = window.matchMedia('(prefers-color-scheme: dark)');
        // The same flip arriving from the system rather than from the toggle.
        if (dark.addEventListener) dark.addEventListener('change', function () { draw(); });
        return;
      }
      requestAnimationFrame(frame);
    }

    // Everything drawn is read back out of storage a page load later, where
    // anything at all could be standing in for it. One string in the array and
    // the draw loop throws on every frame, so the shape is checked once here
    // rather than trusted three hundred times a second.
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

    // The canvas comes down, the snapshot stays. Used wherever there is
    // nothing to draw *this* load without that being a verdict on what is in
    // storage: a store the server could not reach, a backdrop switched off, a
    // projection that came back empty. Whatever is stored is revalidated on
    // the next load anyway, so throwing it away here buys nothing and costs a
    // full refetch.
    function hide() {
      if (canvas.parentNode) canvas.parentNode.removeChild(canvas);
    }

    // The snapshot is never drawn on its own authority, however fresh it looks.
    // It used to stand for six hours, which meant a base emptied — or reindexed
    // away, or simply signed out of on a shared browser — went on being drawn
    // as a cloud of points that no longer existed anywhere. So every load asks,
    // carrying the tag of what it holds: the answer is either a few bytes
    // saying the store still matches, or the cloud that replaces it.
    var cached = null;
    try { cached = JSON.parse(localStorage.getItem(CACHE_KEY) || 'null'); } catch (e) {}
    // An empty point list is a snapshot like any other — of a store with
    // nothing drawable in it — and its tag is worth sending. Without that,
    // "there is nothing to draw" was the one answer the client could never
    // cache, so an instance with the backdrop off, or a store whose vectors
    // are all off the modal width, re-ran the whole question on every page
    // load and never once got to skip it.
    var stored = cached && Array.isArray(cached.points)
      && (cached.points.length === 0 || usable(cached.points));
    var have = stored && typeof cached.tag === 'string' ? cached.tag : null;

    var url = '/api/v1/vectors/sample';
    if (have) url += '?have=' + encodeURIComponent(have);
    // `cache: 'no-store'` as well as the response's own header. The header is
    // what stops a *new* answer being held; this is what stops an answer some
    // earlier build let the browser keep — a `max-age` snapshot of whoever was
    // signed in then — being replayed to whoever is signed in now.
    fetch(url, { credentials: 'same-origin', cache: 'no-store' })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (data) {
        // No answer, or an answer with no tag in it: the store could not be
        // asked, so nothing here is a statement about the cloud in storage.
        // A tag that fails to match is the statement; its absence is not.
        if (!data || typeof data.tag !== 'string') { hide(); return; }
        if (data.unchanged && have) {
          if (cached.points.length) start(cached.points); else hide();
          return;
        }
        var pts = usable(data.points) ? data.points : [];
        // Stored before the length is looked at, so that "nothing to draw"
        // is remembered under its own tag and the next load is the cheap
        // exchange rather than another full sample.
        keep(pts, data.tag);
        if (!pts.length) { hide(); return; }
        start(pts);
      })
      .catch(function () {
        // The network, not the store: nothing here says the cloud is wrong, so
        // the snapshot is left where it is for the next load to revalidate.
        hide();
      });
  }

  // ── The refining pass ─────────────────────────────────────────────────────
  //
  // Typing gets vector order at embedding speed; the reranker's opinion is
  // asked for afterwards, once the box has been quiet long enough to mean the
  // query is settled. The reranked fragment then replaces the list and the
  // rows glide to their new places, so the reordering reads as a refinement
  // happening in front of you rather than as the list twitching.
  //
  // Armed only by `data-rerank` on the form, which the server renders only
  // where a reranker actually serves search: without it a second request
  // could only buy the same order back, and the tick beside the count would
  // be claiming a confirmation that never took place.

  // Was this swap the refining pass? Read off the request rather than kept in
  // a flag, so the fast handler below and the driver cannot disagree about
  // which swap they are looking at.
  function wasRefine(e) {
    var cfg = e.detail && e.detail.requestConfig;
    // The pre-filter set: `parameters` is only what survived `hx-params`, so
    // an allowlist edit there would silently turn every refine swap back into
    // a typing swap — and the typing branch re-arms the timer, which is the
    // loop that never stops.
    var params = cfg && (cfg.unfilteredParameters || cfg.parameters);
    return !!(params && params.rerank === 'true');
  }

  // The rail's selected row, recomputed from the URL. Run after any swap that
  // repaints the list while an artifact is open: the fragment renders every
  // row `aria-selected="false"`, and the open artifact's highlight must
  // survive a repaint it had nothing to do with.
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
    // Long enough past the 120ms debounce to mean "settled", short enough
    // that the refinement still reads as part of the same answer.
    var QUIET_MS = 500;
    var timer = null;
    // Where each row stood when the refine was fired, keyed by href — taken
    // just before the request, spent by the animation, and good for exactly
    // one swap.
    var from = null;
    // The last refined answer, kept to be replayed: `key` is the query and
    // chip it answered, `hrefs` the row set it is valid over, `html` and
    // `head` the fragment and the count line it painted.
    var refined = null;
    // The key of the refine in flight, promoted into `refined` when its swap
    // lands.
    var pending = null;

    function cancel() {
      if (timer) { clearTimeout(timer); timer = null; }
    }

    // Positions are taken relative to the list itself rather than the
    // viewport: the rail is its own scroll box on a wide screen and the
    // window scrolls on a narrow one, and a refine can land seconds after it
    // fired. Viewport coordinates captured before a scroll would make every
    // unmoved row lurch by exactly the scroll distance.
    function resultsTop() {
      var el = document.getElementById('results');
      return el ? el.getBoundingClientRect().top : 0;
    }

    function fire() {
      timer = null;
      var q = box.value.trim();
      // The same two guards the endpoint applies: an empty box is the idle
      // rail, and a pasted chapter is a capture, not a query.
      if (!q || q.length > 2000) return;
      var values = { q: q, rerank: 'true' };
      var chip = form.querySelector('input[name="category"]:checked');
      if (chip && chip.value) values.category = chip.value;
      // This request is built by hand, so `hx-params` on the form does not
      // reach it: the flag has to be copied across or the refined answer
      // silently drops the explanation the fast pass painted.
      var why = form.querySelector('input[name="explain"]');
      if (why && why.value) values.explain = why.value;
      var rows = document.querySelectorAll('#results .rail-item');
      // Nothing to refine: the server skips the rerank call over an empty
      // answer, so the request would buy a guaranteed-identical fragment.
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
      // The same query, settled again over the same rows — a type-and-undo,
      // a chip toggled back. The reranker already answered this exact
      // question, so its fragment is replayed rather than bought twice.
      // Replayed only over an identical row set: a capture landing
      // mid-sitting changes what there is to rank, and a stored answer must
      // never hide a row the fast pass just showed. Both sides of the
      // comparison are fast-pass sets — this one and the one captured when
      // the stored refine fired — sorted into set order, because what is
      // being asked is "same rows", not "same order".
      if (refined && refined.key === key && refined.hrefs === hrefs.slice().sort().join('\n')) {
        var target = document.getElementById('results');
        if (target) {
          target.innerHTML = refined.html;
          var head = document.getElementById('rail-head');
          if (head && refined.head) head.innerHTML = refined.head;
          // By hand, so none of the htmx swap machinery ran: the new rows
          // need their hx- attributes wired, the same enhancements a real
          // swap gets, and the selection recomputed.
          htmx.process(target);
          enhance(target);
          markOpenRow();
          trackDwell();
          glide();
        }
        return;
      }
      // The row set the refine is answering over is the one on screen *now*:
      // the reranker reorders (and promotes into) what the fast pass matched,
      // so its fragment can show rows this list does not. Valid replay is
      // "same query over the same fast-pass rows", which only this moment
      // knows — captured here, promoted into `refined` when the swap lands.
      pending = { key: key, hrefs: hrefs.slice().sort().join('\n') };
      // `source: form` so this request stands in the form's own sync queue:
      // a keystroke's search replaces an in-flight refine exactly as it
      // replaces an older keystroke's search, and the list shown is always
      // the answer to the last thing typed.
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
          // A row the fast pass never showed: promoted from the candidate
          // pool. Nowhere to glide from, so it arrives instead.
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

    // A keystroke inside the debounce window: the pending refine belongs to a
    // query that is no longer what the box says. Composition events are
    // skipped to mirror the form's own trigger filter
    // (`input[!event.isComposing]`): a browser whose final IME event still
    // says composing fires no search off it, and a cancel with no request
    // behind it would leave the pass disarmed on a settled query. The
    // composition itself disarms once, below, when it starts.
    box.addEventListener('input', function (e) {
      if (e.isComposing) return;
      cancel();
    });
    box.addEventListener('compositionstart', cancel);
    // Any request from the form — a keystroke's search, a chip, the refine
    // itself — retires the pending timer; the fast swap landing below is what
    // arms a new one. And any mutation from anywhere — an edit saved in the
    // detail pane, a deprecate, a delete — retires the stored answer: its
    // fragment carries titles and snippets as they stood when it was bought,
    // and a pane-only edit changes those without touching the row set the
    // replay guard checks.
    document.body.addEventListener('htmx:beforeRequest', function (e) {
      if (!e.detail) return;
      if (e.detail.elt === form) {
        cancel();
        // Which of the two passes is in flight, said rather than left as one
        // word for both. `searching…` was true of everything and therefore
        // told you nothing: the fast pass is vector order at embedding speed
        // and the second one is the reranker's opinion, and how long each is
        // worth waiting for is not the same answer.
        //
        // The limit of the claim, stated because it is easy to mistake for
        // more: this names the request that is open, not the stage the server
        // is inside. `Core::search_events` reports that, and the box does not
        // read it — adopting the channel here means rebuilding what htmx does
        // around this form by hand, which is not worth two words. See
        // `docs/superpowers/specs/2026-08-28-cli-face-design.md` §1.
        var spinner = document.getElementById('search-spinner');
        if (spinner) spinner.textContent = wasRefine(e) ? 'reranking…' : 'retrieving…';
      }
      var cfg = e.detail.requestConfig;
      if (cfg && cfg.verb && String(cfg.verb).toLowerCase() !== 'get') refined = null;
    });
    document.body.addEventListener('htmx:afterSwap', function (e) {
      if (e.target.id !== 'results') return;
      if (wasRefine(e)) {
        // The fragment renders every row unselected, and this swap can land
        // over a list whose open artifact was clicked during the quiet
        // window; the highlight must not vanish under a repaint that changed
        // nothing about what is open.
        markOpenRow();
        // Kept to be replayed the next time this exact query settles over
        // this exact row set — see `fire`. The row set stored is the
        // fast-pass one `fire` captured, not the fragment's: the reranker
        // may have promoted rows the fast list never showed, and a replay
        // guard built from those would miss every time the rerank did
        // anything at all.
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
        // The same repaint hazard as the refine branch: a typing swap also
        // renders every row unselected, and the open artifact's highlight
        // must survive it.
        markOpenRow();
        // Never re-armed off a refine swap: that loop would turn one settled
        // query into a rerank call every half second forever.
        cancel();
        from = null;
        timer = setTimeout(fire, QUIET_MS);
      }
    });
  }

  // ── The countdown on the due band ─────────────────────────────────────────
  //
  // The band re-reads at most every five minutes (`due::POLL_CAP`), so a
  // gradient left to the server would step five times an hour and a countdown
  // would be wrong by up to five minutes — which is the whole of what a
  // countdown is for. Both are recomputed here from `data-due-at`, off one
  // timer, using the same rules as `due::due_words` and `due::heat`; the
  // server's render is what a reader with no JS sees, and it is correct at the
  // instant it is sent.
  var HEAT_WINDOW = 6 * 3600;

  // Whole seconds. `dueTick` works from `Date.now() / 1000`, which is
  // fractional, and every branch below the first floors its unit on the way to
  // a number — this one did not, so the last minute before a reminder counted
  // down as `in 45.372s` and re-jittered on every tick, and the minute after
  // read `3.128s overdue`. The server's `due_words` is integer arithmetic, so
  // the row was correct until the client timer took it over.
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
      // Outside the window the row shows a wall-clock time the server wrote,
      // and there is nothing to count. Left alone rather than reformatted:
      // the date words are the server's, in the viewer's zone, and rebuilding
      // them here would be a second implementation of the same sentence.
      if (ahead >= HEAT_WINDOW) return;
      var count = row.querySelector('.due-count');
      if (!count) return;
      var text = ahead <= 0 ? spanWords(-ahead) + ' overdue' : 'in ' + spanWords(ahead);
      if (count.textContent !== text) count.textContent = text;
      // It crossed while the page was open: the row is late now, and the
      // weight the stylesheet gives a late row has to follow.
      if (ahead <= 0) row.classList.add('due-overdue');
    });
  }

  // The band polls, and a swap replaces every row in it — including the
  // `later` a person has just opened and the date they are half way through
  // typing. Opening `later` and watching it shut a second later is not a
  // disclosure anyone can use, and no poll interval makes that acceptable: at
  // five minutes it is a rarer version of the same defect.
  //
  // Only the band's own requests are held off. A snooze or a `done` is a
  // button inside the band asking to be swapped, and must always land.
  //
  // The undo is the third thing a swap destroys, and the worst of the three.
  // `just` — "Done · undo" — is rendered for exactly one swap, and it shares
  // the `#due` element the poll replaces; the poll runs every two seconds
  // while anything is in flight, and a capture puts something in flight. So
  // pressing done on a moment, with a capture still settling, offered the undo
  // and took it back before it could be read — and for a deleted moment it is
  // the only way back.
  //
  // Bounded rather than absolute: an undo nobody takes must not stop the band
  // for ever. It is held for as long as a person plausibly reaches for it and
  // then let go, after which one poll clears it and the next press starts the
  // clock again.
  var UNDO_GRACE = 30000;
  var undoSince = 0;

  function dueBusy() {
    var band = document.getElementById('due');
    if (!band) return false;
    if (band.querySelector('details[open]')) return true;
    if (document.activeElement && band.contains(document.activeElement)) return true;
    if (band.querySelector('.due-done button')) {
      if (!undoSince) undoSince = Date.now();
      if (Date.now() - undoSince < UNDO_GRACE) return true;
    } else {
      undoSince = 0;
    }
    // A date typed but not yet submitted. Nothing else on the band holds text.
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
      // The poll and the `refresh` event both name the band as their source;
      // a button names itself.
      if (cfg && cfg.elt && cfg.elt !== e.target) return;
      if (dueBusy()) e.detail.shouldSwap = false;
    });
    // A second is the resolution of the last minute of a countdown, and the
    // work is a handful of rows: cheaper than the poll it replaces.
    setInterval(dueTick, 1000);
    syncIdle();
    railHandle();
    themeToggle();
    vectorBg();
    keyHint();
    installNudge();
    primeSlow();
    contextOffer();
    zoneDayLinks(document);
    restoreReading();
    boxVerbs();
    railBack();
    exampleChips();
    boxZone();
    captureVerb();
    askDriver();
    refinePass();
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
      if (!e.detail || !e.detail.xhr) return;
      // Not for the refining pass: nobody asked for that request, and the
      // vector-order list it would improve is already on screen and being
      // read. A transient failure half a second after the operator stopped
      // typing must not replace their results with an error box — and an
      // expired session must not navigate them to a login mid-read; the next
      // thing they actually do will land here and redirect with intent.
      if (wasRefine(e)) return;
      if (e.detail.xhr.status === 401) {
        var here = window.location.pathname + window.location.search;
        window.location.assign('/auth/login?go=' + encodeURIComponent(here));
        return;
      }
      failedSwap(e.detail.target, e.detail.xhr);
    });
    // The other half of the same problem. htmx swaps nothing on an error of
    // any kind, and 401 was the only one this handler knew about — so a search
    // against a base whose embedder is down did visibly nothing at all: you
    // typed, and the rail stayed exactly as empty as it was before. A door
    // that fails silently is worse than one that fails, because the only
    // reading left is that the base has nothing.
    document.body.addEventListener('htmx:sendError', function (e) {
      if (!e.detail) return;
      // Same exemption as above: a refine that never reached the server
      // leaves the list it was refining alone.
      if (wasRefine(e)) return;
      failedSwap(e.detail.target, null);
    });
    // Out-of-band content arrives on its own event, and the day link is only
    // ever delivered that way: `_idle_foot.html` is swapped `hx-swap-oob`, so
    // `htmx:afterSwap` never sees it. Unvisited, the anchor keeps the UTC date
    // the server wrote into the path and carries no `?tz=` — east of Greenwich
    // every capture after local midnight linked to the previous day, and the
    // entries typed on that page were stamped with it.
    document.body.addEventListener('htmx:oobAfterSwap', function (e) {
      zoneDayLinks(e.target);
    });
    document.body.addEventListener('htmx:afterSwap', function (e) {
      // The offer's own fetch can land after the first keystroke has already
      // dismissed it. Swapping it back in then would be exactly the flicker the
      // removal exists to prevent.
      if (e.target.id === 'context-offer') {
        if (offerDismissed) dropOffer();
        else confirmOffer(e.target);
      }
      zoneDayLinks(e.target);
      enhance(e.target);
      // A swapped-in band renders at the instant the server answered; by the
      // time it lands the numbers have moved.
      if (e.target.id === 'due') dueTick();
      trackDwell();
      // The pane now holds something, so a narrow screen can hide the rail.
      var ws = document.querySelector('.regions');
      if (ws && e.target.id === 'pane-content') {
        // An artifact is the act now; the answer that had the pane is cleared
        // just below.
        ws.classList.remove('answering');
        // Two facts, told apart: `has-selection` is "a narrow screen should
        // stop showing the list", which a fresh list undoes below, and
        // `pane-open` is "the pane holds an artifact", which a fresh list does
        // not change. See 20-layout.css.
        ws.classList.add('has-selection', 'pane-open');
      }
      // A fresh list is the answer to a new query or chip, so a narrow screen
      // shows it again rather than leaving the result you opened on screen
      // over results that have since changed underneath it.
      //
      // `#results` rather than `#rail`: the list is its own element inside the
      // rail now, so that what a search replaces is the results and not the
      // sitting beside them.
      //
      // Not for the refining pass: it lands over a list the operator may
      // already be reading, and the whole point of the glide is that the rows
      // move under a still eye. Resetting the scroll would trade that for a
      // jump.
      if (e.target.id === 'results' && !wasRefine(e)) {
        // `pane-open` is deliberately not dropped here: the pane still holds
        // what it held, and a new list is no reason to take its width away.
        // Dropping it was how a capture — which empties the box, and an empty
        // box comes back as the idle rail through this very swap — crushed the
        // open artifact into the strip left beside a 40rem rail.
        if (ws) ws.classList.remove('has-selection', 'answering');
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
      if (e.target.id === 'pane-content') {
        // An artifact filled the slot, so whatever an ask left around it —
        // the streamed text, the reasoning, the progress line, the rendered
        // answer, a capture receipt — is over. The nodes survive the swap by
        // design (they are the ask's only targets); they must not keep
        // talking over the artifact.
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

  // Focused only where a pointer says there is a hardware keyboard. On a touch
  // screen the software keyboard covers what the page was opened to show — the
  // rail, and on the start page the base introducing itself — and in an
  // installed window there is no URL bar to dismiss it from. This is why the
  // box carries no `autofocus`.
  //
  // `textarea[name="q"]` is the box. It was matched as `input[name="q"]` and
  // `textarea[name="text"]` until the three pages folded into one, which is
  // two selectors for elements the workspace no longer has: the box was not
  // focused on any pointer device, on any route. The input form is still here
  // for the assign box on Insights.
  var field = document.querySelector('textarea[name="q"], input[name="q"]');
  if (field && window.matchMedia('(hover: hover)').matches) field.focus();

  // Whether something is being typed into. Every letter shortcut below is
  // gated on this: a letter belongs to the field that has focus, and nothing
  // else.
  function typing() {
    var el = document.activeElement;
    if (!el) return false;
    var tag = el.tagName;
    return tag === 'INPUT' || tag === 'TEXTAREA' || el.isContentEditable;
  }

  // The rail is a list: arrows move through it, Enter opens what is focused.
  // j and k do the same, for hands that never left the home row.
  //
  // The arrows are gated on `typing()` too, not only the letters. They were
  // not while the box was a single-line input, where Down meant nothing to the
  // caret. The box is a textarea now — chapters get pasted into it, and
  // /ui/capture?from_ask= opens it holding a whole model answer — so Down is
  // "next line of what I am editing" long before it is "next result", and
  // stealing focus out to the rail lost the keystroke as well as the place.
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
  // The keys that are the same on every page. `/` reaches the query without a
  // pointer, Esc steps back one region — which on a narrow window is the only
  // way back to a list that has been replaced — and `s` and `r` are the two
  // things the search page can show more or less of.
  document.addEventListener('keydown', function (e) {
    if (e.metaKey || e.ctrlKey || e.altKey) return;

    if (e.key === '/' && !typing()) {
      // The box is a textarea, and has been since the three pages became one.
      // `select()` because `/` means "start a new query", not "append to the
      // last one" — the same as it always did.
      // `readOnly` while an ask is in flight, and focusing it then would put a
      // caret in a box that cannot take the query `/` promised to start.
      var q = document.querySelector('textarea[name="q"]');
      if (q && !q.readOnly) { e.preventDefault(); q.focus(); q.select(); }
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
    // Scoped to what is actually on the page, not merely to the grid: every
    // page has a `.regions`, so without this one keypress toggled something
    // the page in front of the reader does not have.
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

})();
