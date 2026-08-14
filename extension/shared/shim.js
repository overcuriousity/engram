// The whole of the Chrome/Firefox divergence, in one place.
//
// Firefox exposes a promise-based `browser`; Chrome exposes `chrome`, which in
// MV3 also returns promises for everything used here. The panel is the one
// real difference: Chrome opens a side panel through `chrome.sidePanel`,
// Firefox through `browser.sidebarAction`, and neither knows the other's name.
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
