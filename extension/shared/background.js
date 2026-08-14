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

// Work the panel has not collected yet, and when it was parked.
//
// Opening the panel and immediately messaging it is a race: a panel that was
// closed has not registered its listener when the message goes out. So the
// work is parked here first. If `sendMessage` finds a listener the panel
// already has it and the parking spot is cleared; if it does not, the panel
// asks for this on load.
//
// It expires, because work that is never collected is worse than work that is
// lost. `openPanel` can refuse — Chrome will not open a side panel outside a
// user gesture, which an omnibox entry may not count as — and `sendMessage`
// can report a delivered message as a failure. Either leaves a parking spot
// nothing empties, and the next panel to open, minutes later on a different
// tab, would run it: a capture aimed at a page the operator left long ago,
// stored silently under the wrong URL. The race this covers resolves in well
// under a second, so a short life costs nothing.
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
    await shim.runtime.sendMessage(work);
    // Delivered to a panel that was already listening. Cleared only if this
    // is still the work parked: a second entry pressed in between owns the
    // spot now, and clearing it would drop that one instead.
    if (pendingWork && pendingWork.work === work) pendingWork = null;
  } catch (e) {
    // The panel had no listener yet and will ask on load, or it could not be
    // opened at all. The parked copy covers the first case and expires out of
    // the second.
  }
}

shim.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg && msg.type === 'pending') {
    sendResponse(collect());
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
