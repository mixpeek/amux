const CACHE = 'amux-v0.9.1278';
const SHELL_URLS = ['/', '/state/kernel.js', '/vendor/md.js', '/state/feedback.css', '/manifest.json', '/icon.svg', '/icon.png', '/icon-192.png', '/icon-512.png'];

// Every CDN library and stylesheet index.html loads. Pre-cached one by one at
// install so the WHOLE app works offline, including features not opened yet
// (2026-10-01: an offline start with the browser HTTP cache evicted had no
// editor, charts, workspace grid, terminal, map, calendar or drag-sort, because
// only 3 of 23 CDN files were ever in this cache). Each needs
// crossorigin="anonymous" in index.html so the response is CORS, not opaque;
// tests/dashboard_assets.rs keeps the two lists in step.
const CDN_URLS = [
  'https://cdn.jsdelivr.net/npm/gridstack@7/dist/gridstack.min.css',
  'https://cdn.jsdelivr.net/npm/quill@2.0.3/dist/quill.snow.css',
  'https://cdn.jsdelivr.net/npm/quilljs-markdown@latest/dist/quilljs-markdown-common-style.css',
  'https://unpkg.com/leaflet@1.9.4/dist/leaflet.css',
  'https://unpkg.com/leaflet.markercluster@1.5.3/dist/MarkerCluster.css',
  'https://unpkg.com/leaflet.markercluster@1.5.3/dist/MarkerCluster.Default.css',
  'https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/css/xterm.min.css',
  'https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/styles/github-dark.min.css',
  'https://cdn.jsdelivr.net/npm/papaparse@5.4.1/papaparse.min.js',
  'https://cdn.jsdelivr.net/npm/sortablejs@1.15.6/Sortable.min.js',
  'https://cdn.jsdelivr.net/npm/quill@2.0.3/dist/quill.js',
  'https://cdn.jsdelivr.net/npm/quilljs-markdown@latest/dist/quilljs-markdown.js',
  'https://cdn.jsdelivr.net/npm/chart.js@4.4.4/dist/chart.umd.min.js',
  'https://cdn.jsdelivr.net/npm/xlsx@0.18.5/dist/xlsx.full.min.js',
  'https://cdn.jsdelivr.net/npm/motion@11.11.17/dist/motion.min.js',
  'https://cdn.jsdelivr.net/npm/gridstack@7/dist/gridstack-all.js',
  'https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/lib/xterm.min.js',
  'https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.10.0/lib/addon-fit.min.js',
  'https://cdn.jsdelivr.net/npm/@xterm/addon-web-links@0.11.0/lib/addon-web-links.min.js',
  'https://cdnjs.cloudflare.com/ajax/libs/highlight.js/11.9.0/highlight.min.js',
  'https://unpkg.com/leaflet@1.9.4/dist/leaflet.js',
  'https://unpkg.com/leaflet.markercluster@1.5.3/dist/leaflet.markercluster.js',
  'https://cdn.jsdelivr.net/npm/fullcalendar@6.1.15/index.global.min.js',
];

// Install: pre-cache the app shell (atomic), then the CDN libraries (best
// effort: a CDN hiccup must not block the install, so a miss is counted and
// reported instead).
self.addEventListener('install', e => {
  e.waitUntil(
    caches.open(CACHE).then(async c => {
      await c.addAll(SHELL_URLS);
      const results = await Promise.allSettled(CDN_URLS.map(u => c.add(new Request(u, { mode: 'cors' }))));
      const failed = CDN_URLS.filter((u, i) => results[i].status !== 'fulfilled');
      // Log signal: a sweep can see an install that left libraries uncached.
      try {
        await fetch('/api/client-debug', { method: 'POST', headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ kind: 'sw-precache', cache: CACHE, cdn_total: CDN_URLS.length,
            cdn_cached: CDN_URLS.length - failed.length, cdn_failed: failed.slice(0, 5),
            verdict: failed.length ? 'cdn_precache_partial' : 'cdn_precache_complete', measured: true }) });
      } catch (_) {}
    })
  );
  self.skipWaiting();
});

// Activate: clean old caches, take control immediately
self.addEventListener('activate', e => {
  e.waitUntil(
    caches.keys().then(keys =>
      Promise.all(keys.filter(k => k !== CACHE).map(k => caches.delete(k)))
    ).then(() => self.clients.claim())
  );
});

// Web Push: a push message from the server arrives here even when the PWA is
// fully closed. We MUST call showNotification (userVisibleOnly), or the browser
// penalises the subscription.
self.addEventListener('push', e => {
  let d = {};
  try { d = e.data ? e.data.json() : {}; } catch(_) { d = { title: 'amux', body: e.data ? e.data.text() : '' }; }
  const title = d.title || 'amux';
  e.waitUntil(self.registration.showNotification(title, {
    body: d.body || '',
    icon: '/icon-192.png',
    badge: '/icon-192.png',
    tag: d.tag || 'amux-push',
    renotify: true,
    requireInteraction: true,
    data: { url: d.url || '/', session: d.session || '' },
  }));
});

// Focus the app (or open it) when a notification is tapped — required for the
// click to do anything on iOS, where notifications are shown via the SW.
self.addEventListener('notificationclick', e => {
  e.notification.close();
  e.waitUntil(
    clients.matchAll({ type: 'window', includeUncontrolled: true }).then(cl => {
      for (const c of cl) { if ('focus' in c) return c.focus(); }
      if (clients.openWindow) return clients.openWindow('/');
    })
  );
});

self.addEventListener('message', e => {
  if (e.data && e.data.type === 'SKIP_WAITING') self.skipWaiting();
  // Client can push HTML into SW for localStorage-backed fallback
  if (e.data && e.data.type === 'CACHE_HTML') {
    caches.open(CACHE).then(cache => {
      const resp = new Response(e.data.html, {
        headers: { 'Content-Type': 'text/html; charset=utf-8' }
      });
      cache.put('/', resp);
    });
  }
});

self.addEventListener('fetch', e => {
  const url = new URL(e.request.url);
  if (e.request.method !== 'GET') return;
  // Only handle http/https (skip chrome-extension:// etc.)
  if (!url.protocol.startsWith('http')) return;

  // Business is a separate shell with its own hashed bundle. Never serve its
  // authentication bootstrap or connection health from the developer cache.
  if (url.pathname === '/business' || url.pathname.startsWith('/business/') ||
      url.pathname === '/health') return;

  // API requests: network only (app JS handles offline queue)
  if (url.pathname.startsWith('/api/')) return;

  // Main HTML (SPA): network-first, always cache as canonical '/' key
  // Hash fragments (#path=...) are client-side only — SW sees bare '/' regardless
  if (url.pathname === '/') {
    // A remote owner reaches the public shell with `?_token=...`, which the
    // server exchanges for an HttpOnly owner-session cookie and redirects to
    // a clean URL. Never answer that one-time exchange from the canonical `/`
    // cache: doing so drops the query before the server sees it and leaves the
    // correct owner credential looking exactly like an unauthenticated peer.
    // `fetch(e.request)` follows the server redirect on the network, applying
    // Set-Cookie before the clean owner shell is returned.
    if (url.searchParams.has('_token') || url.searchParams.has('_fresh')) {
      e.respondWith(fetch(e.request));
      return;
    }
    const canonical = new Request('/', { headers: { 'Accept': 'text/html' } });
    // STALE-WHILE-REVALIDATE, not network-first. The shell is ~1.6MB of inline
    // HTML/CSS/JS; network-first meant every single load blocked on that full
    // download before rendering a pixel, even holding a byte-identical cached
    // copy — measured transferSize 1679111 on a WARM load, which is what
    // produced a "Loading amux..." wait on a local app.
    //
    // Cached shell is served IMMEDIATELY and the update is fetched in the
    // background. Staleness is bounded: CACHE is versioned with APP_VER, so a
    // real deploy installs a new SW, whose activate wipes the old cache and
    // whose install re-fetches '/' fresh, then controllerchange reloads. The
    // worst case is being one load behind within a single version.
    e.respondWith(
      caches.open(CACHE).then(c => c.match(canonical).then(cached => {
        const net = fetch(canonical).then(response => {
          if (response.ok) c.put(canonical, response.clone());
          return response;
        }).catch(() => null);
        if (cached) {
          e.waitUntil(net);   // keep the SW alive for the background refresh
          return cached;
        }
        return net.then(r => r || new Response('Offline — please reload when connected', {
          status: 503, headers: { 'Content-Type': 'text/plain' }
        }));
      }))
    );
    return;
  }

  // Static assets (icons, manifest): cache-first, refresh in background
  e.respondWith(
    caches.open(CACHE).then(cache =>
      cache.match(e.request).then(cached => {
        const networkUpdate = fetch(e.request).then(response => {
          // `response.ok` is FALSE for an OPAQUE cross-origin response (status 0),
          // so this gate silently declined to cache every CDN asset — which is why
          // an offline PWA got no syntax highlighting despite the cache-first
          // strategy looking like it covered everything (AMUX-2460).
          //
          // Fixed at the request, not here: the hljs <script>, its theme <link>
          // and the lazily loaded grammars now carry crossorigin="anonymous", and
          // cdnjs serves access-control-allow-origin:* on all three (verified), so
          // the responses are real and this gate stores them. Deliberately NOT
          // relaxed to cache opaque responses — an opaque 404 is indistinguishable
          // from an opaque 200, so that would cache failures as confidently as
          // successes.
          if (response.ok) cache.put(e.request, response.clone());
          return response;
        }).catch(() => null);

        if (cached) return cached;
        return networkUpdate.then(r => r || new Response('Offline — please reload when connected', {
          status: 503, headers: { 'Content-Type': 'text/plain' }
        }));
      })
    )
  );
});

// NOTE: no Background Sync replay here — deliberately. The page-side flush
// (runSyncBanner, triggered by online/visibilitychange/startup) is the SINGLE
// replayer. A second SW-side replayer raced it: both fired at reconnect, the
// SW replayed from a stale IDB snapshot (duplicate-delivery risk) and its
// SYNC_COMPLETE→IDB merge resurrected ops the page had already settled
// (observed live in the AMUX-1825 e2e). iOS never supported Background Sync
// anyway, so page-side-only is also the one behavior that works everywhere.
