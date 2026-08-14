// Injected on demand by `scripting.executeScript`, never declared in the
// manifest: `activeTab` grants access to this one tab, for this one action,
// because the operator just asked for it. A declared content script would mean
// `<all_urls>` and a "read your data on all websites" warning at install.
//
// The last expression is the injection's result, which is how it gets back to
// the panel.
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

  // The selection's HTML, not the page's: capturing three paragraphs must not
  // quietly store the article they sit in.
  const html = scope === 'selection'
    ? selectionHtml()
    : document.documentElement.outerHTML;

  return { html, url: location.href, title: document.title };
})();
