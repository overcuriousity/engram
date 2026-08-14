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
  // The whole-page twin of the panel's Capture button, and the one path to it
  // that needs no host permission: a context-menu click is a gesture on the
  // tab it happened in, so `activeTab` covers exactly that page. The panel's
  // button cannot rely on that once the operator has switched tabs — see
  // `panel.js` — so this is the route that always works.
  shim.contextMenus.create({
    id: MENU.page,
    title: 'Capture this page',
    contexts: ['page'],
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
  } catch (e) {
    // Chrome refuses to open a side panel outside a user gesture, which an
    // omnibox entry may not count as. The panel may still be open already, so
    // this is not the end of the attempt — the send below decides.
  }

  // Claim the work back before sending it. While `openPanel` was settling the
  // panel may have finished loading, asked for what was parked and got it:
  // `collect()` emptying the spot is the record that it did, and sending the
  // same work again would run it twice — a capture stored twice over, an
  // omnibox query recorded as two searches with two embeddings behind them.
  //
  // Taking the spot before the `await` rather than after is what makes this a
  // handover instead of a second race: whoever empties it owns delivery, and
  // the `pending` listener cannot run in between.
  if (!pendingWork || pendingWork.work !== work) return;
  pendingWork = null;

  try {
    await shim.runtime.sendMessage(work);
  } catch (e) {
    // No listener: the panel is not up yet and will ask on load. Park it again
    // — but only if nothing newer took the spot in the meantime, because a
    // second entry pressed since owns it now and stamping over it would drop
    // that one instead. The fresh timestamp is deliberate; the TTL is about
    // how long uncollected work may sit, and it has only just become that.
    if (!pendingWork) pendingWork = { work, at: Date.now() };
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
  } else if (info.menuItemId === MENU.page) {
    await handOver({ type: 'capture', scope: 'page' }, tab.id);
  }
});

shim.omnibox.onInputEntered.addListener(async (text) => {
  const [tab] = await shim.tabs.query({ active: true, currentWindow: true });
  await handOver({ type: 'search', q: text }, tab && tab.id);
});
