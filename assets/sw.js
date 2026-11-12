// Caches the offline page and the shell it needs; never app data.
var CACHE = 'engram-shell-v2';
var OFFLINE_URL = '/offline';

self.addEventListener('install', function (event) {
  event.waitUntil(
    caches.open(CACHE).then(function (cache) {
      return fetch(OFFLINE_URL).then(function (res) {
        return res.clone().text().then(function (html) {
          var assets = (html.match(/\/assets\/[^"'\s]+/g) || []);
          return cache.put(OFFLINE_URL, res).then(function () { return cache.addAll(assets); });
        });
      });
    }).then(function () { return self.skipWaiting(); })
  );
});

self.addEventListener('activate', function (event) {
  event.waitUntil(
    caches.keys().then(function (names) {
      return Promise.all(names.map(function (n) { return n === CACHE ? null : caches.delete(n); }));
    }).then(function () { return self.clients.claim(); })
  );
});

self.addEventListener('fetch', function (event) {
  var req = event.request;
  if (req.mode === 'navigate') {
    event.respondWith(fetch(req).catch(function () {
      return caches.match(OFFLINE_URL).then(function (cached) {
        return cached || new Response('Server unreachable.', { status: 503 });
      });
    }));
    return;
  }
  if (req.method === 'GET' && new URL(req.url).pathname.indexOf('/assets/') === 0) {
    event.respondWith(caches.match(req).then(function (hit) { return hit || fetch(req); }));
  }
});
