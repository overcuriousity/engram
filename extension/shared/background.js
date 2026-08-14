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
