// Chrome's MV3 service worker needs `importScripts`; Firefox's event page has loaded the scripts already.
if (!globalThis.engramShim && typeof importScripts === 'function') {
  importScripts('shim.js');
}

const shim = globalThis.engramShim;

// Firefox cannot open the panel without a listener.
shim.action.onClicked.addListener((tab) => {
  shim.openPanel(tab.id);
});

const MENU = {
  search: 'engram-search-selection',
  capture: 'engram-capture-selection',
  page: 'engram-capture-page',
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
  // `activeTab` covers a context-menu click, so this path needs no host permission.
  shim.contextMenus.create({
    id: MENU.page,
    title: 'Capture this page',
    contexts: ['page'],
  });
});

// Work parked for a panel that has not registered its listener yet. It expires: a stale spot would run
// an old capture on whatever tab opens the panel next.
let pendingWork = null;
const PENDING_TTL_MS = 15000;

function collect() {
  const parked = pendingWork;
  pendingWork = null;
  if (!parked) return null;
  return Date.now() - parked.at > PENDING_TTL_MS ? null : parked.work;
}

async function handOver(work, tabId) {
  pendingWork = { work, at: Date.now() };
  try {
    await shim.openPanel(tabId);
  } catch (e) {
    // Chrome may refuse outside a user gesture; the panel may be open already, so the send decides.
  }

  // Claimed before the `await`: whoever empties the spot owns delivery, so work never runs twice.
  if (!pendingWork || pendingWork.work !== work) return;
  pendingWork = null;

  try {
    await shim.runtime.sendMessage(work);
  } catch (e) {
    // Re-parked only if nothing newer took the spot; the fresh timestamp is deliberate.
    if (!pendingWork) pendingWork = { work, at: Date.now() };
  }
}

shim.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg && msg.type === 'pending') {
    sendResponse(collect());
  }
  return false;
});

shim.contextMenus.onClicked.addListener(async (info, tab) => {
  // The panel is where results and errors are reported.
  if (info.menuItemId === MENU.search) {
    await handOver({ type: 'search', q: info.selectionText }, tab.id);
  } else if (info.menuItemId === MENU.capture) {
    await handOver({ type: 'capture', scope: 'selection' }, tab.id);
  } else if (info.menuItemId === MENU.page) {
    await handOver({ type: 'capture', scope: 'page' }, tab.id);
  }
});

shim.omnibox.onInputEntered.addListener(async (text) => {
  const [tab] = await shim.tabs.query({ active: true, currentWindow: true });
  await handOver({ type: 'search', q: text }, tab && tab.id);
});
