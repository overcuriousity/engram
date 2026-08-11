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

  document.addEventListener('DOMContentLoaded', function () {
    enhance(document.body);
    document.body.addEventListener('htmx:afterSwap', function (e) {
      enhance(e.target);
      // The pane now holds something, so a narrow screen can hide the rail.
      var ws = document.querySelector('.workspace');
      if (ws && e.target.id === 'pane') ws.classList.add('has-selection');
      // A fresh list is the answer to a new query or chip, so a narrow screen
      // shows it again rather than leaving the result you opened on screen
      // over results that have since changed underneath it.
      if (ws && e.target.id === 'rail') ws.classList.remove('has-selection');
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
    var card = document.querySelector('.judge-card');
    if (!card || e.metaKey || e.ctrlKey || e.altKey) return;
    var tag = document.activeElement && document.activeElement.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA') return;

    if (/^[1-9]$/.test(e.key)) {
      var pick = card.querySelectorAll('.judge-option')[Number(e.key) - 1];
      if (pick) { e.preventDefault(); pick.click(); }
      return;
    }
    var outs = { n: 0, s: 1, x: 2 };
    var idx = outs[e.key.toLowerCase()];
    if (idx !== undefined) {
      var row = card.querySelectorAll('.judge-outs button');
      if (row[idx]) { e.preventDefault(); row[idx].click(); }
    }
  });
})();
