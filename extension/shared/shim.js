// The Chrome/Firefox divergence: `browser` vs `chrome`, and `sidePanel` vs `sidebarAction` for the panel.
const engramBrowser = globalThis.browser ?? globalThis.chrome;

globalThis.engramShim = {
  runtime: engramBrowser.runtime,
  tabs: engramBrowser.tabs,
  scripting: engramBrowser.scripting,
  storage: engramBrowser.storage,
  permissions: engramBrowser.permissions,
  identity: engramBrowser.identity,
  contextMenus: engramBrowser.contextMenus,
  omnibox: engramBrowser.omnibox,
  action: engramBrowser.action,

  async openPanel(tabId) {
    if (engramBrowser.sidePanel) {
      await engramBrowser.sidePanel.open({ tabId });
    } else {
      await engramBrowser.sidebarAction.open();
    }
  },
};
