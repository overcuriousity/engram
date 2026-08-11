// Service worker: the one thing a browser demands before it will install a site
// as an app rather than bookmark it.
//
// It caches nothing. Every request goes to the network exactly as it would
// without a worker, and the only thing kept locally is a small page to show when
// the network is gone. That is deliberate: engram is a server — a search is a
// vector query and a capture is a write, so there is no useful offline mode to
// build, and a worker that cached the app shell would only invent a way to serve
// yesterday's HTML against today's server.

var OFFLINE_URL = '/sw-offline';
var CACHE = 'engram-offline-v1';

var OFFLINE_PAGE = [
  '<!doctype html><html lang="en"><head><meta charset="utf-8">',
  '<meta name="viewport" content="width=device-width, initial-scale=1">',
  '<title>Offline — engram</title>',
  // The app's own base colours, not a palette of this page's own. The manifest
  // paints #f8f6f1 behind a launch, so a dark offline page is the same flash of
  // the wrong colour that moving the manifest to cream was meant to remove. The
  // dark values are app.css's, for a device set that way.
  '<style>',
  'html{background:#f8f6f1;color:#2d2d2d;font:16px/1.5 system-ui,sans-serif}',
  'body{margin:0;display:grid;place-items:center;min-height:100vh;padding:2rem}',
  'div{max-width:24rem;text-align:center}',
  'p{color:#7a7a72}',
  '@media(prefers-color-scheme:dark){',
  'html{background:#0e1015;color:#e2e4ec}',
  'p{color:#8b8fa8}',
  '}',
  '</style></head><body><div>',
  '<h1>Offline</h1>',
  '<p>engram keeps its corpora on its server, so there is nothing to search',
  ' until this device can reach it again.</p>',
  '</div></body></html>'
].join('');

self.addEventListener('install', function (event) {
  event.waitUntil(
    caches.open(CACHE).then(function (cache) {
      return cache.put(
        OFFLINE_URL,
        new Response(OFFLINE_PAGE, {
          headers: { 'content-type': 'text/html; charset=utf-8' }
        })
      );
    }).then(function () { return self.skipWaiting(); })
  );
});

self.addEventListener('activate', function (event) {
  // Drop caches from any earlier version of this worker, so a rename of CACHE
  // is all it takes to retire one.
  event.waitUntil(
    caches.keys().then(function (names) {
      return Promise.all(names.map(function (n) {
        return n === CACHE ? null : caches.delete(n);
      }));
    }).then(function () { return self.clients.claim(); })
  );
});

self.addEventListener('fetch', function (event) {
  // Only navigations get the fallback. An htmx fragment or an API call that
  // fails should fail, and be handled by the page that asked for it, rather
  // than have an HTML apology swapped into a result rail.
  if (event.request.mode !== 'navigate') return;

  event.respondWith(
    fetch(event.request).catch(function () {
      return caches.open(CACHE).then(function (cache) {
        return cache.match(OFFLINE_URL);
      }).then(function (cached) {
        // A miss — install's put failed, or site data was cleared while the
        // registration survived — resolves undefined, and respondWith(undefined)
        // throws. Build the page on the spot instead of losing it.
        return cached || new Response(OFFLINE_PAGE, {
          status: 503,
          headers: { 'content-type': 'text/html; charset=utf-8' }
        });
      });
    })
  );
});
