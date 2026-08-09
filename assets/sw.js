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
  '<style>',
  'html{background:#0e1015;color:#e6e6e6;font:16px/1.5 system-ui,sans-serif}',
  'body{margin:0;display:grid;place-items:center;min-height:100vh;padding:2rem}',
  'div{max-width:24rem;text-align:center}',
  'p{color:#9aa0aa}',
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
      });
    })
  );
});
