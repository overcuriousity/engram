// Chrome runs this as an MV3 service worker, where dependencies are pulled in
// with `importScripts`. Firefox runs it as an MV3 event page, where the
// manifest's `background.scripts` list has already loaded them and
// `importScripts` does not exist. Guarding on the function rather than on the
// browser keeps one file serving both.
//
// If Firefox turns out to reject this, delete the call and rely on the
// manifest list alone — then keep whichever single path works in both rather
// than leaving two. See extension/README.md.
if (!globalThis.engramShim && typeof importScripts === 'function') {
  importScripts('shim.js');
}

const shim = globalThis.engramShim;

// The toolbar button opens the panel. Chrome can be told to do this without a
// listener, but Firefox cannot, so one path serves both.
shim.action.onClicked.addListener((tab) => {
  shim.openPanel(tab.id);
});

const MENU = {
  search: 'engram-search-selection',
  capture: 'engram-capture-selection',
};

shim.runtime.onInstalled.addListener(() => {
  shim.contextMenus.create({
    id: MENU.search,
    title: 'Search engram for this',
    contexts: ['selection'],
  });
  shim.contextMenus.create({
    id: MENU.capture,
    title: 'Capture selection',
    contexts: ['selection'],
  });
});

// Work the panel has not collected yet.
//
// Opening the panel and immediately messaging it is a race: a panel that was
// closed has not registered its listener when the message goes out. So the
// work is parked here first. If `sendMessage` finds a listener the panel
// already has it and the parking spot is cleared; if it does not, the panel
// asks for this on load.
let pendingWork = null;

async function handOver(work, tabId) {
  pendingWork = work;
  await shim.openPanel(tabId);
  try {
    await shim.runtime.sendMessage(work);
    // Delivered to an open panel.
    pendingWork = null;
  } catch (e) {
    // No listener yet. The panel is opening and will ask.
  }
}

shim.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg && msg.type === 'pending') {
    sendResponse(pendingWork);
    pendingWork = null;
  }
  // Nothing here is asynchronous, so the channel does not need holding open.
  return false;
});

shim.contextMenus.onClicked.addListener(async (info, tab) => {
  // Both entries open the panel and then hand it the work: the panel is where
  // results and status live, and an entry that acted silently would have
  // nowhere to report a near-duplicate or an unreachable server.
  if (info.menuItemId === MENU.search) {
    await handOver({ type: 'search', q: info.selectionText }, tab.id);
  } else if (info.menuItemId === MENU.capture) {
    await handOver({ type: 'capture', scope: 'selection' }, tab.id);
  }
});

shim.omnibox.onInputEntered.addListener(async (text) => {
  const [tab] = await shim.tabs.query({ active: true, currentWindow: true });
  await handOver({ type: 'search', q: text }, tab && tab.id);
});
