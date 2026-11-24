self.addEventListener('install', (e) => {
  e.waitUntil(caches.open('vs').then((c) => c.put('/sw-cached', new Response('from-cache', { headers: { 'content-type': 'text/plain' } }))).then(() => self.skipWaiting()));
});
self.addEventListener('activate', (e) => e.waitUntil(self.clients.claim()));
self.addEventListener('fetch', (e) => {
  const u = new URL(e.request.url);
  if (u.pathname === '/sw-built') e.respondWith(new Response('built', { status: 201, headers: { 'content-type': 'text/plain', 'x-sw': 'yes' } }));
  if (u.pathname === '/sw-cached') e.respondWith(caches.match('/sw-cached'));
});
