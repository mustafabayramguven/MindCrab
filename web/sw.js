// MindCrab service worker — app shell offline + last-seen copies of notes.
// The server replaces __VERSION__ with a hash of the embedded assets.
const VERSION = "__VERSION__";
const SHELL = `mc-shell-${VERSION}`;
const DATA = "mc-data";
// Offline copies kept at most (the least recently written go first); attachments are big, so fewer of them.
const DATA_MAX = 4000;
const FILE_MAX = 200;
const ASSETS = [
  "/", "/app.js", "/app.css", "/icons.js", "/graph.js", "/i18n.js", "/views.js", "/canvas.js", "/icon.svg", "/manifest.webmanifest",
  "/fonts/roboto-flex-latin.woff2", "/fonts/roboto-flex-latin-ext.woff2",
];
// GET endpoints whose last response is kept for offline use. /api/session is deliberately not
// one of them: it must come from the server, or a revoked login could be replayed offline.
const CACHEABLE = ["/api/tree", "/api/tags", "/api/note", "/api/links", "/api/history", "/api/templates", "/api/embed-status", "/api/file/", "/api/tasks", "/api/canvas", "/api/preview"];

self.addEventListener("install", (e) => {
  // Skip the HTTP cache: these copies are served without revalidation until the next version.
  const fresh = ASSETS.map((u) => new Request(u, { cache: "no-cache" }));
  e.waitUntil(caches.open(SHELL).then((c) => c.addAll(fresh)).then(() => self.skipWaiting()));
});

self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k.startsWith("mc-shell-") && k !== SHELL).map((k) => caches.delete(k))))
      // Older versions kept /api/session, which let an offline reload skip the login screen.
      .then(() => caches.open(DATA))
      .then((c) => c.delete("/api/session", { ignoreVary: true }))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("message", (e) => {
  if (e.data === "clear-data") e.waitUntil(caches.delete(DATA));
});

const offline = () =>
  new Response(JSON.stringify({ error: "offline", offline: true }), { status: 503, headers: { "content-type": "application/json" } });

// A 401 means the login is gone (token changed, cookie removed): drop the vault copies with it.
const network = (req) =>
  fetch(req).then((res) => {
    if (res.status === 401) caches.delete(DATA).catch(() => {});
    return res;
  });

// Answers from the offline copy are marked, so the page knows the server was not reached;
// when the page already holds that very copy (same ETag) it gets a 304 and re-renders nothing.
function fromCache(req, hit) {
  const tag = hit.headers.get("etag");
  const inm = req.headers.get("if-none-match");
  if (tag && inm && inm.split(",").some((t) => t.trim() === tag)) {
    return new Response(null, { status: 304, headers: { etag: tag, "x-mc-cache": "1" } });
  }
  const headers = new Headers(hit.headers);
  headers.set("x-mc-cache", "1");
  return new Response(hit.body, { status: hit.status, statusText: hit.statusText, headers });
}

// Keeps the last good answer. Copies of deleted notes, dated task counts and old attachments
// would otherwise pile up forever, so the cache is trimmed once per worker start and every 100 writes.
let writes = 0;
async function keep(req, res) {
  const c = await caches.open(DATA);
  await c.put(req, res);
  if (writes++ % 100) return;
  const keys = await c.keys(); // oldest written first
  const files = keys.filter((k) => new URL(k.url).pathname.startsWith("/api/file/"));
  const drop = new Set([...files.slice(0, Math.max(0, files.length - FILE_MAX)), ...keys.slice(0, Math.max(0, keys.length - DATA_MAX))]);
  await Promise.all([...drop].map((k) => c.delete(k)));
}

// Saving a note refreshes its offline copy, so reopening it without a connection shows what
// was just written rather than the version before the edit.
async function putNote(req) {
  let sent = null;
  try { sent = await req.clone().json(); } catch {}
  let res;
  try { res = await network(req); } catch { return offline(); }
  if (res.ok && typeof sent?.path === "string" && typeof sent.content === "string") {
    try { await refreshNote(sent.path, sent.content, (await res.clone().json()).hash); } catch {}
  }
  return res;
}
async function refreshNote(path, content, hash) {
  const c = await caches.open(DATA);
  const key = "/api/note?path=" + encodeURIComponent(path); // the URL the page reads the note from
  const hit = await c.match(key, { ignoreVary: true });
  if (!hit) return;
  if (typeof hash !== "string") return c.delete(key);
  const note = await hit.json();
  const headers = new Headers(hit.headers);
  for (const h of ["etag", "content-length", "content-encoding"]) headers.delete(h);
  await c.put(key, new Response(JSON.stringify({ ...note, content, hash, mtime: Date.now() }), { headers }));
}

// Share target: only the OS share sheet (a navigation nobody initiated) or the app itself may post
// here, so another site cannot plant content in the save dialog. Not every browser shows the worker
// the fetch-metadata headers; then a cross-site referrer still gives such a post away.
function shareAllowed(req) {
  const site = req.headers.get("sec-fetch-site");
  if (site) return site === "none" || site === "same-origin";
  const origin = req.headers.get("origin");
  if (origin && origin !== "null") return origin === location.origin;
  if (req.referrer) {
    try { return new URL(req.referrer).origin === location.origin; } catch { return false; }
  }
  return true;
}

// Keep what was shared, then open the app which shows a save dialog.
async function receiveShare(req) {
  try {
    const form = await req.formData();
    const cache = await caches.open("mc-share");
    const files = form.getAll("files").filter((f) => f && f.size);
    for (let i = 0; i < files.length; i++) {
      await cache.put(`/__share__/file/${i}`, new Response(files[i], { headers: { "content-type": files[i].type || "application/octet-stream" } }));
    }
    const meta = { title: form.get("title") || "", text: form.get("text") || "", url: form.get("url") || "", files: files.map((f) => ({ name: f.name, type: f.type })) };
    await cache.put("/__share__", new Response(JSON.stringify(meta), { headers: { "content-type": "application/json" } }));
  } catch {}
  return Response.redirect("/#/share", 303);
}

self.addEventListener("fetch", (e) => {
  const req = e.request;
  const url = new URL(req.url);
  if (url.origin !== location.origin) return;
  if (req.method === "POST" && url.pathname === "/share") {
    e.respondWith(shareAllowed(req) ? receiveShare(req) : new Response("Cross-site share rejected", { status: 403 }));
    return;
  }
  if (req.method === "PUT" && url.pathname === "/api/note") {
    e.respondWith(putNote(req));
    return;
  }
  if (req.method !== "GET" || url.pathname.startsWith("/mcp")) return;

  if (url.pathname.startsWith("/api/")) {
    if (!CACHEABLE.some((p) => url.pathname === p || (p.endsWith("/") && url.pathname.startsWith(p)))) {
      e.respondWith(network(req).catch(offline));
      return;
    }
    // Network first; keep the last good answer for offline use.
    e.respondWith(
      network(req)
        .then((res) => {
          if (res.ok) e.waitUntil(keep(req, res.clone()).catch(() => {}));
          return res;
        })
        .catch(() => caches.open(DATA).then((c) => c.match(req)).then((hit) => (hit ? fromCache(req, hit) : offline()))),
    );
    return;
  }

  // App shell: the cache is named after the asset hash, so a hit is current until the next version
  // installs and needs no revalidation round trip. Misses (lazily loaded vendor files) are filled in.
  const key = req.mode === "navigate" ? "/" : req;
  e.respondWith(
    caches.open(SHELL).then((c) =>
      c.match(key).then((hit) =>
        hit ||
        fetch(req).then((res) => {
          if (res.ok && req.mode !== "navigate") c.put(req, res.clone());
          return res;
        }),
      ),
    ),
  );
});
