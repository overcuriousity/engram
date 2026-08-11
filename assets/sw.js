var OFFLINE_URL = '/sw-offline';
var CACHE = 'engram-offline-v1';

var OFFLINE_PAGE = [
  '<!doctype html><html lang="en"><head><meta charset="utf-8">',
  '<meta name="viewport" content="width=device-width, initial-scale=1">',
  '<title>Offline — engram</title>',
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
  event.waitUntil(
    caches.keys().then(function (names) {
      return Promise.all(names.map(function (n) {
        return n === CACHE ? null : caches.delete(n);
      }));
    }).then(function () { return self.clients.claim(); })
  );
});

self.addEventListener('fetch', function (event) {
  if (event.request.mode !== 'navigate') return;

  event.respondWith(
    fetch(event.request).catch(function () {
      return caches.open(CACHE).then(function (cache) {
        return cache.match(OFFLINE_URL);
      }).then(function (cached) {
        return cached || new Response(OFFLINE_PAGE, {
          status: 503,
          headers: { 'content-type': 'text/html; charset=utf-8' }
        });
      });
    })
  );
});
