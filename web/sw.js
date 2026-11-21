// MindCrab service worker — app shell offline + last-seen copies of notes.
// The server replaces __VERSION__ with a hash of the embedded assets.
const VERSION = "__VERSION__";
const SHELL = `mc-shell-${VERSION}`;
const DATA = "mc-data";
const ASSETS = [
  "/", "/app.js", "/app.css", "/icons.js", "/graph.js", "/i18n.js", "/icon.svg", "/manifest.webmanifest",
  "/fonts/roboto-flex-latin.woff2", "/fonts/roboto-flex-latin-ext.woff2",
];
// GET endpoints whose last response is kept for offline use.
const CACHEABLE = ["/api/session", "/api/tree", "/api/tags", "/api/note", "/api/links", "/api/history", "/api/templates", "/api/embed-status", "/api/file/"];

self.addEventListener("install", (e) => {
  e.waitUntil(caches.open(SHELL).then((c) => c.addAll(ASSETS)).then(() => self.skipWaiting()));
});

self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k.startsWith("mc-shell-") && k !== SHELL).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("message", (e) => {
  if (e.data === "clear-data") e.waitUntil(caches.delete(DATA));
});

const offline = () =>
  new Response(JSON.stringify({ error: "offline", offline: true }), { status: 503, headers: { "content-type": "application/json" } });

self.addEventListener("fetch", (e) => {
  const req = e.request;
  const url = new URL(req.url);
  if (url.origin !== location.origin || req.method !== "GET" || url.pathname.startsWith("/mcp")) return;

  if (url.pathname.startsWith("/api/")) {
    if (!CACHEABLE.some((p) => url.pathname === p || (p.endsWith("/") && url.pathname.startsWith(p)))) {
      e.respondWith(fetch(req).catch(offline));
      return;
    }
    // Network first; keep the last good answer for offline use.
    e.respondWith(
      fetch(req)
        .then((res) => {
          if (res.ok) {
            const copy = res.clone();
            caches.open(DATA).then((c) => c.put(req, copy));
          }
          return res;
        })
        .catch(() => caches.open(DATA).then((c) => c.match(req)).then((hit) => hit || offline())),
    );
    return;
  }

  // App shell: cache first, then network (and refresh the cache in the background).
  const key = req.mode === "navigate" ? "/" : req;
  e.respondWith(
    caches.open(SHELL).then((c) =>
      c.match(key).then((hit) => {
        const net = fetch(req)
          .then((res) => {
            if (res.ok && req.mode !== "navigate") c.put(req, res.clone());
            return res;
          })
          .catch(() => hit || Response.error());
        return hit || net;
      }),
    ),
  );
});
