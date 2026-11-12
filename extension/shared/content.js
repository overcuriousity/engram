// Injected on demand under `activeTab`; a declared content script would need `<all_urls>`.
// The last expression is the injection's result.
(() => {
  const scope = globalThis.__engramScope || 'page';

  function selectionHtml() {
    const sel = window.getSelection();
    if (!sel || sel.isCollapsed) return null;
    const box = document.createElement('div');
    for (let i = 0; i < sel.rangeCount; i++) {
      box.appendChild(sel.getRangeAt(i).cloneContents());
    }
    return box.innerHTML;
  }

  // Only the selection, not the article around it.
  const html = scope === 'selection'
    ? selectionHtml()
    : document.documentElement.outerHTML;

  return { html, url: location.href, title: document.title };
})();
