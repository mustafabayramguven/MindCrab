// MindCrab web UI — vanilla ES modules, no build step.
import { ICONS } from "/icons.js";
import { t, applyI18n, LANG } from "/i18n.js";
import { createViews } from "/views.js";

const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
const stem = (p) => p.split("/").pop().replace(/\.md$/i, "");
const dirOf = (p) => (p.includes("/") ? p.slice(0, p.lastIndexOf("/")) : "");
const fold = (s) => s.toLocaleLowerCase(LANG).normalize("NFD").replace(/[̀-ͯ]/g, "").replace(/ı/g, "i");
const isMobile = () => matchMedia("(max-width: 840px)").matches;
const encPath = (p) => p.split("/").map(encodeURIComponent).join("/");

const store = {
  get(k, d) { try { const v = localStorage.getItem("mc." + k); return v === null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem("mc." + k, JSON.stringify(v)); } catch {} },
  del(k) { try { localStorage.removeItem("mc." + k); } catch {} },
};

// Unsaved edits survive reloads, crashes and offline periods.
const drafts = {
  key: (p) => "draft:" + p,
  get: (p) => store.get(drafts.key(p), null),
  put: (p, content, base) => store.set(drafts.key(p), { content, base, t: Date.now() }),
  clear: (p) => store.del(drafts.key(p)),
};

// ------------------------------------------------------------------ icons
export function icon(name) {
  const d = ICONS[name];
  return d ? `<svg viewBox="0 -960 960 960" aria-hidden="true"><path d="${d}"/></svg>` : "";
}
function hydrate(root = document) {
  root.querySelectorAll("i[data-icon]").forEach((el) => {
    if (!el.firstChild) el.innerHTML = icon(el.dataset.icon);
  });
}
const I = (name, cls = "") => `<i data-icon="${name}" class="${cls}">${icon(name)}</i>`;

// ------------------------------------------------------------------ api
class ApiError extends Error {
  constructor(status, msg) { super(msg); this.status = status; }
}
// Last ETag-tagged answer per GET url: the server replies 304 when nothing changed,
// and callers get the very same object back (so `data === previous` means "unchanged").
// Only read-only listings that are polled or reopened often; their objects are never mutated.
const etags = new Map();
const CONDITIONAL = /^\/api\/(tree|tags|graph|tasks)(\?|$)/;
async function api(method, url, body, opts = {}) {
  const init = { method, headers: {}, credentials: "same-origin" };
  const conditional = method === "GET" && CONDITIONAL.test(url);
  const memo = conditional ? etags.get(url) : undefined;
  if (memo) init.headers["if-none-match"] = memo.tag;
  if (body !== undefined) {
    if (opts.raw) init.body = body;
    else { init.body = JSON.stringify(body); init.headers["content-type"] = "application/json"; }
  }
  let res;
  try { res = await fetch(url, init); } catch (e) { setOffline(true); throw new ApiError(0, e.message); }
  if (res.status === 401 && !opts.noAuthRedirect) { showLogin(); throw new ApiError(401, "login required"); }
  if (res.status === 304 && memo) { if (S.offline) setOffline(!navigator.onLine); return memo.data; }
  const data = res.headers.get("content-type")?.includes("json") ? await res.json() : await res.text();
  if (res.status === 503 && data?.offline) { setOffline(true); throw new ApiError(0, "offline"); }
  if (!res.ok) throw new ApiError(res.status, data?.error || res.statusText);
  if (S.offline && method === "GET" && res.headers.get("date")) setOffline(!navigator.onLine);
  const tag = conditional && res.headers.get("etag");
  if (tag) {
    etags.delete(url);
    etags.set(url, { tag, data });
    if (etags.size > 64) etags.delete(etags.keys().next().value);
  }
  return data;
}

// ------------------------------------------------------------------ state
const S = {
  notes: [], folders: [], tags: [],
  path: null, note: null, content: "", dirty: false, saving: null, saveTimer: null,
  mode: store.get("mode", "edit"),
  tab: "links",
  collapsed: new Set(store.get("collapsed", [])),
  view: "empty",
  pendingAnchor: null,
  graph: null,
  semantic: false,
  offline: !navigator.onLine,
};

// ------------------------------------------------------------------ theme
const HUES = [285, 255, 215, 165, 125, 75, 35, 350];
function applyTheme() {
  const theme = store.get("theme", "system");
  const hue = store.get("hue", 285);
  const root = document.documentElement;
  root.style.setProperty("--h", hue === "mono" ? 285 : hue);
  root.style.setProperty("--c", hue === "mono" ? 0 : 1);
  const dark = theme === "dark" || (theme === "system" && matchMedia("(prefers-color-scheme: dark)").matches);
  root.classList.toggle("dark", dark);
  root.dataset.theme = theme;
  document.body.classList.toggle("wide", store.get("wide", false));
  $("#editor").spellcheck = store.get("spell", false);
  requestAnimationFrame(() => {
    const c = getComputedStyle(document.body).backgroundColor;
    $('meta[name="theme-color"]').content = c;
    S.graph?.draw();
  });
}
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", applyTheme);

function hexToHue(hex) {
  const n = parseInt(hex.slice(1), 16);
  const lin = (c) => { c /= 255; return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4; };
  const r = lin((n >> 16) & 255), g = lin((n >> 8) & 255), b = lin(n & 255);
  const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
  const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
  const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
  const A = 1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s;
  const B = 0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s;
  return Math.round(((Math.atan2(B, A) * 180) / Math.PI + 360) % 360);
}

// ------------------------------------------------------------------ ui helpers
let toastTimer;
function toast(msg, action) {
  const el = $("#snackbar");
  el.innerHTML = `<span>${esc(msg)}</span>`;
  if (action) {
    const b = document.createElement("button");
    b.className = "btn text";
    b.textContent = action.label;
    b.onclick = () => { el.hidden = true; action.run(); };
    el.append(b);
  }
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.hidden = true), action ? 6000 : 3000);
}
const fail = (e) => { if (e?.status !== 401 && e?.status !== 0) toast(t("toast.error", { msg: e.message || e })); };

function setOffline(v) {
  if (S.offline === v) return;
  S.offline = v;
  $("#offline-banner").hidden = !v;
  if (!v && S.dirty) save();
}
addEventListener("online", () => { setOffline(false); poll(); });
addEventListener("offline", () => setOffline(true));

function closeMenu() { $("#menu").hidden = true; }
function openMenu(anchor, items) {
  const m = $("#menu");
  m.innerHTML = "";
  for (const it of items) {
    if (it === "-") { m.append(document.createElement("hr")); continue; }
    const b = document.createElement("button");
    b.type = "button";
    if (it.danger) b.className = "danger";
    b.innerHTML = `${I(it.icon)}<span>${esc(it.label)}</span>`;
    b.onclick = () => { closeMenu(); it.run(); };
    m.append(b);
  }
  m.hidden = false;
  const r = anchor.getBoundingClientRect ? anchor.getBoundingClientRect() : { left: anchor.x, right: anchor.x, top: anchor.y, bottom: anchor.y };
  const mw = m.offsetWidth, mh = m.offsetHeight;
  let x = Math.min(r.right - mw, innerWidth - mw - 8);
  if (x < 8) x = Math.min(r.left, innerWidth - mw - 8);
  let y = r.bottom + 4;
  if (y + mh > innerHeight - 8) y = Math.max(8, r.top - mh - 4);
  m.style.left = Math.max(8, x) + "px";
  m.style.top = y + "px";
  m.querySelector("button")?.focus();
}
document.addEventListener("pointerdown", (e) => {
  if (!e.target.closest("#menu")) closeMenu();
  if (!e.target.closest("#suggest") && e.target !== $("#editor")) hideSuggest();
});

function dialogPrompt({ title, text = "", label = "", value = "", ok = t("action.create"), input = true, danger = false, select = null }) {
  const d = $("#prompt-dialog");
  $("#prompt-title").textContent = title;
  $("#prompt-text").textContent = text;
  $("#prompt-label").textContent = label;
  $("#prompt-field").hidden = !input;
  const inp = $("#prompt-input");
  inp.value = value;
  inp.required = input;
  const okBtn = $("#prompt-ok");
  okBtn.textContent = ok;
  okBtn.className = "btn " + (danger ? "danger" : "filled");
  const sw = $("#prompt-select-wrap");
  sw.hidden = !select;
  if (select) {
    $("#prompt-select-label").textContent = select.label;
    $("#prompt-select").innerHTML = select.options.map((o) => `<option value="${esc(o.value)}">${esc(o.label)}</option>`).join("");
    $("#prompt-select").value = select.value ?? "";
  }
  return new Promise((resolve) => {
    d.onclose = () => {
      const v = d.returnValue === "ok" ? (input ? inp.value.trim() : true) : null;
      if (select) resolve(v ? { value: v, choice: $("#prompt-select").value } : null);
      else resolve(v || null);
    };
    d.returnValue = "";
    d.showModal();
    if (input) {
      inp.focus();
      const base = value.lastIndexOf("/") + 1;
      const end = value.toLowerCase().endsWith(".md") ? value.length - 3 : value.length;
      inp.setSelectionRange(base, end);
    }
  });
}
$$("dialog [data-close]").forEach((b) => b.addEventListener("click", () => b.closest("dialog").close()));
$$("dialog").forEach((d) => d.addEventListener("click", (e) => { if (e.target === d) d.close(); }));

function fmtTime(ms) {
  if (!ms) return "";
  const d = new Date(ms);
  const diff = (Date.now() - ms) / 1000;
  const rtf = new Intl.RelativeTimeFormat(LANG, { numeric: "auto" });
  if (diff < 45) return t("time.justNow");
  if (diff < 3600) return rtf.format(-Math.round(diff / 60), "minute");
  if (diff < 86400) return rtf.format(-Math.round(diff / 3600), "hour");
  if (diff < 86400 * 7) return rtf.format(-Math.round(diff / 86400), "day");
  return d.toLocaleDateString(LANG, { year: "numeric", month: "short", day: "numeric" });
}
const fmtFull = (ms) => new Date(ms).toLocaleString(LANG, { dateStyle: "medium", timeStyle: "short" });
const fmtSize = (n) => (n < 1024 ? `${n} B` : n < 1048576 ? `${(n / 1024).toFixed(1)} KB` : `${(n / 1048576).toFixed(1)} MB`);

// ------------------------------------------------------------------ layout
function setDrawer(open) {
  $("#app").classList.toggle("drawer-closed", !open);
  if (!isMobile()) store.set("drawer", open);
}
function setPanel(open) {
  $("#app").classList.toggle("panel-closed", !open);
  $("#app").classList.toggle("panel-open-mobile", open && isMobile());
  $("#toggle-panel").innerHTML = I(open ? "right_panel_close" : "right_panel_open");
  if (!isMobile()) store.set("panel", open);
  if (open) loadPanel();
}
const panelOpen = () => !$("#app").classList.contains("panel-closed");

function setMode(mode) {
  if (mode === "split" && isMobile()) mode = "edit";
  S.mode = mode;
  store.set("mode", mode);
  const v = $("#note-view");
  v.classList.remove("mode-edit", "mode-split", "mode-preview");
  v.classList.add("mode-" + mode);
  $$("#mode-switch button").forEach((b) => b.classList.toggle("on", b.dataset.mode === mode));
  if (mode !== "edit") renderPreview(true);
}

function showView(name) {
  S.view = name;
  $("#app").dataset.view = name;
  if (name !== "canvas" && S.canvas) { S.canvas.destroy(); S.canvas = null; S.canvasPath = null; }
  if (name !== "page") S.page = null;
  for (const v of ["note", "list", "graph", "empty", "page", "canvas"]) $(`#${v}-view`).hidden = v !== name;
  $$(".nav-list .nav-item").forEach((a) => a.classList.toggle("on", name === "page" && a.dataset.nav === S.page));
  hideHover();
  const noteOnly = name === "note";
  $("#mode-switch").style.visibility = noteOnly ? "" : "hidden";
  $("#note-menu").style.visibility = noteOnly ? "" : "hidden";
  $("#save-state").hidden = !noteOnly;
  $("#nav-graph").classList.toggle("on", name === "graph");
  if (!noteOnly) { $("#crumbs").textContent = ""; }
}

function showPage(name, title) {
  if (S.dirty) save();
  S.page = name;
  showView("page");
  S.page = name;
  $$(".nav-list .nav-item").forEach((a) => a.classList.toggle("on", a.dataset.nav === name));
  $("#note-title").textContent = title;
  document.title = `${title} · MindCrab`;
  S.path = null;
  highlightTree();
  if (panelOpen()) loadPanel();
  if (isMobile()) setDrawer(false);
}

// ------------------------------------------------------------------ tree
async function loadTree() {
  const data = await api("GET", "/api/tree");
  if (data === S.treeData) return data; // 304: nothing changed
  S.treeData = data;
  S.notes = data.notes;
  S.folders = data.folders;
  S.canvases = data.canvases || [];
  S.hidden = new Set(data.hidden || []);
  const pb = $("#pending-badge");
  pb.hidden = !data.pending;
  pb.textContent = data.pending || "";
  renderTree();
  return data;
}

function buildTree() {
  const root = { name: "", path: "", folders: new Map(), notes: [], canvases: [] };
  const ensure = (path) => {
    let node = root;
    if (!path) return node;
    let acc = "";
    for (const seg of path.split("/")) {
      acc = acc ? `${acc}/${seg}` : seg;
      if (!node.folders.has(seg)) node.folders.set(seg, { name: seg, path: acc, folders: new Map(), notes: [], canvases: [] });
      node = node.folders.get(seg);
    }
    return node;
  };
  S.folders.forEach(ensure);
  for (const n of S.notes) ensure(dirOf(n.path)).notes.push(n);
  for (const c of S.canvases || []) ensure(dirOf(c)).canvases.push(c);
  return root;
}

const coll = new Intl.Collator(LANG, { numeric: true, sensitivity: "base" });
function renderTree() {
  const root = buildTree();
  const ul = $("#tree");
  ul.innerHTML = "";
  const walk = (node, parent) => {
    const folders = [...node.folders.values()].sort((a, b) => coll.compare(a.name, b.name));
    for (const f of folders) {
      const li = document.createElement("li");
      li.className = "tree-folder" + (S.collapsed.has(f.path) ? " collapsed" : "");
      li.innerHTML = `<div class="tree-row" data-folder="${esc(f.path)}" tabindex="0">${I("keyboard_arrow_down", "chev")}${I(S.collapsed.has(f.path) ? "folder" : "folder_open")}<span class="name">${esc(f.name)}</span><button class="icon-btn small more" type="button">${I("more_vert")}</button></div><ul role="group"></ul>`;
      parent.append(li);
      walk(f, li.querySelector("ul"));
    }
    for (const n of node.notes.sort((a, b) => coll.compare(stem(a.path), stem(b.path)))) {
      const li = document.createElement("li");
      const lock = S.hidden?.has(n.path) ? `<span title="${esc(t("hidden.badge"))}">${I("lock", "lock")}</span>` : "";
      li.innerHTML = `<div class="tree-row${n.path === S.path ? " on" : ""}" data-note="${esc(n.path)}" draggable="true" tabindex="0" title="${esc(n.title)}">${I("description")}<span class="name">${esc(stem(n.path))}</span>${lock}<button class="icon-btn small more" type="button">${I("more_vert")}</button></div>`;
      parent.append(li);
    }
    for (const c of (node.canvases || []).sort((a, b) => coll.compare(a, b))) {
      const li = document.createElement("li");
      li.innerHTML = `<div class="tree-row${c === S.canvasPath ? " on" : ""}" data-canvas="${esc(c)}" tabindex="0">${I("dashboard")}<span class="name">${esc(c.split("/").pop().replace(/\.canvas$/i, ""))}</span></div>`;
      parent.append(li);
    }
  };
  walk(root, ul);
}

function highlightTree() {
  $$("#tree .tree-row[data-note]").forEach((r) => r.classList.toggle("on", r.dataset.note === S.path));
  // expand parents of the current note
  if (S.path) {
    let d = dirOf(S.path), changed = false;
    while (d) { if (S.collapsed.delete(d)) changed = true; d = dirOf(d); }
    if (changed) { store.set("collapsed", [...S.collapsed]); renderTree(); }
    $(`#tree .tree-row.on`)?.scrollIntoView({ block: "nearest" });
  }
}

$("#tree").addEventListener("click", (e) => {
  const row = e.target.closest(".tree-row");
  if (!row) return;
  if (row.dataset.canvas) { location.hash = "#/c/" + encPath(row.dataset.canvas); return; }
  if (e.target.closest(".more")) {
    e.stopPropagation();
    return row.dataset.note ? noteMenu(e.target.closest(".more"), row.dataset.note) : folderMenu(e.target.closest(".more"), row.dataset.folder);
  }
  if (row.dataset.folder !== undefined) {
    const f = row.dataset.folder;
    S.collapsed.has(f) ? S.collapsed.delete(f) : S.collapsed.add(f);
    store.set("collapsed", [...S.collapsed]);
    renderTree();
  } else {
    go(row.dataset.note);
  }
});
$("#tree").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && e.target.classList.contains("tree-row")) e.target.click();
});
$("#tree").addEventListener("contextmenu", (e) => {
  const row = e.target.closest(".tree-row");
  if (!row || row.dataset.canvas) return;
  e.preventDefault();
  const at = { x: e.clientX, y: e.clientY };
  row.dataset.note ? noteMenu(at, row.dataset.note) : folderMenu(at, row.dataset.folder);
});
// Drag notes onto folders to move them.
$("#tree").addEventListener("dragstart", (e) => {
  const row = e.target.closest(".tree-row[data-note]");
  if (row) { e.dataTransfer.setData("text/x-mindcrab-note", row.dataset.note); e.dataTransfer.effectAllowed = "move"; }
});
const dropTarget = (e) => e.target.closest(".tree-row[data-folder]") || (e.target.closest(".section-label") ? { dataset: { folder: "" } } : null);
for (const el of [$("#tree"), $(".drawer-scroll .section-label")]) {
  el.addEventListener("dragover", (e) => {
    if (!e.dataTransfer.types.includes("text/x-mindcrab-note")) return;
    const tgt = dropTarget(e);
    $$(".tree-row.drop").forEach((r) => r.classList.remove("drop"));
    if (tgt) { e.preventDefault(); tgt.classList?.add("drop"); }
  });
  el.addEventListener("drop", async (e) => {
    $$(".tree-row.drop").forEach((r) => r.classList.remove("drop"));
    const from = e.dataTransfer.getData("text/x-mindcrab-note");
    const tgt = dropTarget(e);
    if (!from || !tgt) return;
    e.preventDefault();
    const folder = tgt.dataset.folder;
    const to = (folder ? folder + "/" : "") + from.split("/").pop();
    if (to !== from) await renameNote(from, to);
  });
}

function noteMenu(anchor, path) {
  openMenu(anchor, [
    { icon: "drive_file_rename", label: t("action.rename"), run: () => promptRename(path) },
    { icon: "content_copy", label: t("action.copyLink"), run: () => copy(`[[${linkName(path)}]]`) },
    "-",
    { icon: "delete", label: t("action.delete"), danger: true, run: () => deleteNote(path) },
  ]);
}
function folderMenu(anchor, folder) {
  openMenu(anchor, [
    { icon: "note_add", label: t("action.newHere"), run: () => newNote(folder) },
    { icon: "create_new_folder", label: t("action.folderHere"), run: () => newFolder(folder) },
    { icon: "dashboard", label: t("action.newCanvas"), run: () => newCanvas(folder) },
    { icon: "drive_file_move", label: t("action.renameFolder"), run: () => renameFolder(folder) },
    "-",
    { icon: "delete", label: t("action.delete"), danger: true, run: () => deleteFolder(folder) },
  ]);
}

async function loadTags() {
  const { tags } = await api("GET", "/api/tags");
  if (tags === S.tags) return;
  S.tags = tags;
  $("#tag-list").innerHTML = tags.length
    ? tags.map((x) => `<a class="chip" href="#/tag/${encodeURIComponent(x.tag)}">#${esc(x.tag)} <span class="count">${x.count}</span></a>`).join("")
    : `<span class="panel-empty">${esc(t("links.none"))}</span>`;
}

// ------------------------------------------------------------------ notes
function linkName(path) {
  const s = stem(path);
  const same = S.notes.filter((n) => stem(n.path).toLowerCase() === s.toLowerCase());
  return same.length > 1 ? path.replace(/\.md$/i, "") : s;
}

async function openNote(path, { keepScroll = false } = {}) {
  if (S.dirty && S.path && S.path !== path) await save();
  let note;
  try {
    note = await api("GET", "/api/note?path=" + encodeURIComponent(path));
  } catch (e) {
    if (e.status === 404) { toast(e.message); showView("empty"); S.path = null; highlightTree(); return; }
    throw e;
  }
  const ed = $("#editor");
  const scroll = keepScroll ? [ed.scrollTop, ed.selectionStart] : null;
  S.path = note.path;
  S.note = note;
  S.content = note.content;
  S.dirty = false;
  ed.value = note.content;
  if (scroll) { ed.scrollTop = scroll[0]; ed.selectionStart = ed.selectionEnd = Math.min(scroll[1], ed.value.length); }
  else { ed.scrollTop = 0; ed.setSelectionRange(0, 0); }
  showView("note");
  setTitle();
  setSaveState("saved");
  renderProps();
  const draft = drafts.get(note.path);
  if (draft && draft.content !== note.content) {
    let use = draft.base === note.hash;
    if (!use) use = !!(await dialogPrompt({ title: t("draft.title"), text: t("draft.body"), input: false, ok: t("draft.use") }));
    if (use && S.path === note.path) {
      ed.value = draft.content;
      onEdit();
      if (draft.base === note.hash) toast(t("draft.restored"));
    } else if (!use) drafts.clear(note.path);
  } else if (draft) drafts.clear(note.path);
  setMode(S.mode);
  highlightTree();
  store.set("last", note.path);
  if (panelOpen()) loadPanel();
  if (isMobile()) setDrawer(false);
}

function setTitle() {
  const n = S.note;
  $("#note-title").textContent = n ? stem(n.path) : "";
  $("#crumbs").textContent = n ? dirOf(n.path).split("/").filter(Boolean).join(" › ") : "";
  document.title = n ? `${stem(n.path)} · MindCrab` : "MindCrab";
}

function setSaveState(s) {
  const el = $("#save-state");
  const map = { saved: ["cloud_done", "save.saved"], saving: ["sync", "save.saving"], dirty: ["edit", "save.dirty"], error: ["error", "save.error"], offline: ["sync", "save.offline"] };
  const [ic, key] = map[s];
  el.className = "save-state label-medium " + (s === "dirty" ? "dirty" : s === "error" || s === "offline" ? "error" : "");
  el.innerHTML = `${I(ic)}<span class="txt">${esc(t(key))}</span>`;
  el.title = t(key);
}

let draftTimer;
function onEdit() {
  S.dirty = $("#editor").value !== S.content;
  clearTimeout(draftTimer);
  if (S.path && S.note) {
    const path = S.path, base = S.note.hash;
    draftTimer = setTimeout(() => (S.dirty ? drafts.put(path, $("#editor").value, base) : drafts.clear(path)), 400);
  }
  setSaveState(S.dirty ? "dirty" : "saved");
  clearTimeout(S.saveTimer);
  if (S.dirty) S.saveTimer = setTimeout(save, 900);
  if (S.mode === "split") renderPreview();
}

async function save({ force = false } = {}) {
  clearTimeout(S.saveTimer);
  if (!S.path || (!S.dirty && !force)) return;
  if (S.saving) { await S.saving; if (!S.dirty) return; }
  const path = S.path;
  const content = $("#editor").value;
  setSaveState("saving");
  S.saving = (async () => {
    try {
      const r = await api("PUT", "/api/note", { path, content, expected_hash: force ? null : S.note.hash });
      if (S.path !== path) return;
      S.note.hash = r.hash;
      S.content = content;
      S.dirty = $("#editor").value !== content;
      const d = drafts.get(path);
      if (!S.dirty || (d && d.content === content)) { clearTimeout(draftTimer); drafts.clear(path); }
      setOffline(false);
      setSaveState(S.dirty ? "dirty" : "saved");
      if (S.dirty) S.saveTimer = setTimeout(save, 900);
      refreshMeta();
    } catch (e) {
      if (e.status === 409) return conflict();
      if (e.status === 0) { setSaveState("offline"); drafts.put(path, content, S.note.hash); S.saveTimer = setTimeout(save, 15000); return; }
      setSaveState("error");
      fail(e);
    } finally {
      S.saving = null;
    }
  })();
  return S.saving;
}

let metaTimer;
function refreshMeta() {
  clearTimeout(metaTimer);
  metaTimer = setTimeout(async () => {
    await Promise.all([loadTree(), loadTags()]).catch(() => {});
    highlightTree();
    if (panelOpen()) loadPanel();
  }, 1200);
}

async function conflict() {
  setSaveState("error");
  const d = $("#prompt-dialog");
  const choice = await dialogPrompt({ title: t("conflict.title"), text: t("conflict.body"), input: false, ok: t("action.overwrite"), danger: true });
  if (choice) return save({ force: true });
  // reload server version (local text goes to clipboard-free fallback: keep in history via overwrite only)
  d.close();
  const local = $("#editor").value;
  await openNote(S.path, { keepScroll: true });
  toast(t("action.reload"), { label: t("action.undo"), run: () => { $("#editor").value = local; onEdit(); } });
}

let previewTimer, previewSeq = 0;
function renderPreview(now = false) {
  clearTimeout(previewTimer);
  previewTimer = setTimeout(async () => {
    if (!S.path || S.mode === "edit") return;
    const seq = ++previewSeq;
    try {
      const r = await api("POST", "/api/render", { path: S.path, content: $("#editor").value });
      if (seq !== previewSeq) return;
      const pv = $("#preview");
      const props = Object.entries(r.frontmatter || {}).filter(([k]) => k !== "title");
      const propsHtml = props.length
        ? `<div class="props">${props.map(([k, v]) => `<span class="prop"><b>${esc(k)}</b>${esc(Array.isArray(v) ? v.join(", ") : typeof v === "object" ? JSON.stringify(v) : v)}</span>`).join("")}</div>`
        : "";
      pv.innerHTML = propsHtml + r.html;
      enhance(pv);
      pv.querySelectorAll('input[type="checkbox"]').forEach((cb, i) => { cb.disabled = false; cb.dataset.task = i; });
      if (S.pendingAnchor) { scrollToHeading(S.pendingAnchor); S.pendingAnchor = null; }
    } catch (e) {
      if (e.status === 0 && seq === previewSeq) {
        $("#preview").innerHTML = `<p class="muted">${esc(t("preview.offline"))}</p><pre>${esc($("#editor").value)}</pre>`;
      } else fail(e);
    }
  }, now ? 0 : 250);
}

function scrollToHeading(text) {
  const want = fold(text.replace(/-/g, " ").trim());
  const h = $$("#preview h1, #preview h2, #preview h3, #preview h4, #preview h5, #preview h6").find((x) => fold(x.textContent.trim()) === want);
  h?.scrollIntoView({ behavior: "smooth", block: "start" });
}

// Toggle task checkboxes in the preview → update source.
$("#preview").addEventListener("change", (e) => {
  const cb = e.target;
  if (cb.dataset.task === undefined) return;
  const ed = $("#editor");
  let n = -1, inFence = false;
  const lines = ed.value.split("\n");
  for (let i = 0; i < lines.length; i++) {
    if (/^\s*(```|~~~)/.test(lines[i])) inFence = !inFence;
    if (inFence) continue;
    const m = lines[i].match(/^(\s*(?:[-*+]|\d+[.)])\s+\[)([ xX])(\])/);
    if (m && ++n === +cb.dataset.task) {
      lines[i] = m[1] + (cb.checked ? "x" : " ") + m[3] + lines[i].slice(m[0].length);
      ed.value = lines.join("\n");
      onEdit();
      return;
    }
  }
});
$("#preview").addEventListener("click", (e) => {
  const a = e.target.closest("a.wikilink[data-anchor]");
  if (a && a.dataset.anchor) {
    if (a.dataset.note === S.path) { e.preventDefault(); scrollToHeading(a.dataset.anchor); }
    else S.pendingAnchor = a.dataset.anchor;
  }
});

async function newNote(folder) {
  const base = folder ?? (S.path ? dirOf(S.path) : "");
  const def = (base ? base + "/" : "") + (LANG === "tr" ? "Adsız" : "Untitled");
  let tpls = [];
  try { tpls = (await api("GET", "/api/templates")).templates; } catch {}
  const lastTpl = store.get("template", "");
  const res = await dialogPrompt({
    title: t("prompt.newNote"), label: t("prompt.path"), value: def,
    select: tpls.length ? { label: t("template.label"), value: tpls.some((x) => x.path === lastTpl) ? lastTpl : "", options: [{ value: "", label: t("template.none") }, ...tpls.map((x) => ({ value: x.path, label: stem(x.path) }))] } : null,
  });
  if (!res) return;
  const name = typeof res === "string" ? res : res.value;
  const template = typeof res === "string" ? "" : res.choice;
  store.set("template", template);
  try {
    const r = await api("POST", "/api/note", { path: name, template: template || undefined });
    await loadTree();
    S.mode === "preview" && setMode("edit");
    go(r.path);
    setTimeout(() => { const ed = $("#editor"); ed.focus(); ed.setSelectionRange(ed.value.length, ed.value.length); }, 150);
  } catch (e) {
    if (e.status === 409) go((name.toLowerCase().endsWith(".md") ? name : name + ".md").replace(/^\/+/, ""));
    else fail(e);
  }
}

async function newFolder(parent = "") {
  const name = await dialogPrompt({ title: t("prompt.newFolder"), label: t("prompt.path"), value: parent ? parent + "/" : "" });
  if (!name) return;
  try { await api("POST", "/api/folder", { path: name }); await loadTree(); } catch (e) { fail(e); }
}

async function promptRename(path) {
  const to = await dialogPrompt({ title: t("prompt.rename"), label: t("prompt.path"), value: path, ok: t("action.save") });
  if (to && to !== path) await renameNote(path, to);
}
async function renameNote(from, to) {
  try {
    if (S.path === from && S.dirty) await save();
    const r = await api("POST", "/api/note/rename", { from, to });
    toast(t("toast.renamed", { n: r.updated_links }));
    await loadTree();
    if (S.path === from) { S.path = r.path; location.hash = "#/n/" + encPath(r.path); }
    else if (S.path) openNote(S.path, { keepScroll: true });
  } catch (e) { fail(e); }
}

async function newCanvas(folder = "") {
  const name = await dialogPrompt({ title: t("action.newCanvas"), label: t("canvas.newName"), value: (folder ? folder + "/" : "") + (LANG === "tr" ? "Tuval" : "Canvas") });
  if (!name) return;
  try {
    const r = await api("PUT", "/api/canvas", { path: name, content: { nodes: [], edges: [] } });
    await loadTree();
    location.hash = "#/c/" + encPath(r.path);
  } catch (e) { fail(e); }
}

async function showCanvas(path) {
  if (S.dirty) await save();
  showView("canvas");
  S.canvasPath = path;
  $("#note-title").textContent = path.split("/").pop().replace(/\.canvas$/i, "");
  $("#crumbs").textContent = dirOf(path).split("/").filter(Boolean).join(" › ");
  document.title = `${$("#note-title").textContent} · MindCrab`;
  S.path = null;
  highlightTree();
  renderTree();
  if (isMobile()) setDrawer(false);
  const { CanvasEditor } = await import("/canvas.js");
  if (S.canvasPath !== path) return; // navigated away while loading
  S.canvas = new CanvasEditor($("#canvas-view"), path, { api, I, t, esc, go, fail, dialogPrompt, enhance });
  try { await S.canvas.load(); } catch (e) { fail(e); }
}

async function renameFolder(folder) {
  const to = await dialogPrompt({ title: t("action.renameFolder"), label: t("prompt.path"), value: folder, ok: t("action.save") });
  if (!to || to === folder) return;
  try {
    if (S.dirty) await save();
    const r = await api("POST", "/api/folder/rename", { from: folder, to });
    toast(t("toast.folderMoved", { n: r.moved, l: r.updated_links }));
    if (S.collapsed.delete(folder)) store.set("collapsed", [...S.collapsed]);
    await loadTree();
    if (S.path && S.path.startsWith(folder + "/")) go(r.path + S.path.slice(folder.length));
    else if (S.path) openNote(S.path, { keepScroll: true });
  } catch (e) { fail(e); }
}

async function deleteNote(path) {
  const ok = await dialogPrompt({ title: t("prompt.deleteTitle"), text: `${path}\n\n${t("prompt.deleteBody")}`, input: false, ok: t("action.delete"), danger: true });
  if (!ok) return;
  try {
    await api("DELETE", "/api/note?path=" + encodeURIComponent(path));
    toast(t("toast.deleted"));
    if (store.get("last", null) === path) store.set("last", null);
    await loadTree();
    loadTags();
    if (S.path === path) {
      S.path = null; S.note = null; S.dirty = false;
      showView("empty"); setTitle(); highlightTree();
      history.replaceState(null, "", location.pathname);
    }
  } catch (e) { fail(e); }
}
async function deleteFolder(folder) {
  const ok = await dialogPrompt({ title: t("prompt.deleteFolder"), text: folder, input: false, ok: t("action.delete"), danger: true });
  if (!ok) return;
  try { await api("DELETE", "/api/folder?path=" + encodeURIComponent(folder)); await loadTree(); } catch (e) { fail(e); }
}

async function openDaily() {
  try { const r = await api("POST", "/api/daily", {}); await loadTree(); go(r.path); } catch (e) { fail(e); }
}

async function copy(text) {
  try { await navigator.clipboard.writeText(text); toast(t("action.copied")); }
  catch {
    const ta = document.createElement("textarea");
    ta.value = text; document.body.append(ta); ta.select(); document.execCommand("copy"); ta.remove();
    toast(t("action.copied"));
  }
}

// ------------------------------------------------------------------ editor
const ed = $("#editor");
function insert(text, selectFrom, selectTo) {
  ed.focus();
  const start = ed.selectionStart;
  // execCommand keeps the native undo stack; fall back to setRangeText.
  if (!document.execCommand("insertText", false, text)) {
    ed.setRangeText(text, ed.selectionStart, ed.selectionEnd, "end");
    onEdit();
  }
  if (selectFrom !== undefined) ed.setSelectionRange(start + selectFrom, start + (selectTo ?? selectFrom));
}
function wrap(before, after = before, placeholder = "") {
  const s = ed.selectionStart, e = ed.selectionEnd;
  const sel = ed.value.slice(s, e) || placeholder;
  insert(before + sel + after, before.length, before.length + sel.length);
}
function linePrefix(prefix) {
  const s = ed.selectionStart;
  const ls = ed.value.lastIndexOf("\n", s - 1) + 1;
  ed.setSelectionRange(ls, ls);
  insert(prefix);
  ed.setSelectionRange(s + prefix.length, s + prefix.length);
}
const FMT = {
  bold: () => wrap("**", "**", "text"),
  italic: () => wrap("*", "*", "text"),
  heading: () => linePrefix("## "),
  list: () => linePrefix("- "),
  task: () => linePrefix("- [ ] "),
  quote: () => linePrefix("> "),
  code: () => (ed.value.slice(ed.selectionStart, ed.selectionEnd).includes("\n") ? wrap("```\n", "\n```") : wrap("`", "`", "code")),
  wikilink: () => { wrap("[[", "]]"); setTimeout(updateSuggest, 0); },
  tag: () => insert("#"),
  attach: () => $("#file-input").click(),
  template: () => insertTemplate(),
  base: () => {
    const s = ed.selectionStart;
    const before = ed.value.slice(0, s);
    const pre = before && !before.endsWith("\n\n") ? (before.endsWith("\n") ? "\n" : "\n\n") : "";
    insert(`${pre}\`\`\`base\nfrom: ${dirOf(S.path || "") || "Folder"}\nwhere:\n  - has tags\ncolumns: [title, tags, modified]\nsort: -modified\nview: table\n\`\`\`\n`);
  },
};

async function insertTemplate() {
  let r;
  try { r = await api("GET", "/api/templates"); } catch (e) { return fail(e); }
  const btn = $('#toolbar [data-fmt="template"]');
  if (!r.templates.length) return toast(t("template.empty", { dir: r.folder }));
  const pos = [ed.selectionStart, ed.selectionEnd];
  openMenu(btn, r.templates.map((x) => ({
    icon: "note_stack",
    label: stem(x.path),
    run: async () => {
      try {
        let { content } = await api("POST", "/api/template/render", { template: x.path, title: stem(S.path || "") });
        // Drop the template's frontmatter unless inserting at the very top of an empty-frontmatter note.
        if (pos[0] > 0 || ed.value.startsWith("---")) content = content.replace(/^---\r?\n[\s\S]*?\r?\n(---|\.\.\.)\r?\n/, "");
        const before = ed.value.slice(0, pos[0]);
        if (before && !before.endsWith("\n\n")) content = (before.endsWith("\n") ? "\n" : "\n\n") + content;
        ed.focus();
        ed.setSelectionRange(pos[0], pos[1]);
        insert(content);
      } catch (e) { fail(e); }
    },
  })));
}
$("#toolbar").addEventListener("mousedown", (e) => e.target.closest("button") && e.preventDefault());
$("#toolbar").addEventListener("click", (e) => { const b = e.target.closest("button[data-fmt]"); if (b) FMT[b.dataset.fmt](); });

ed.addEventListener("input", () => { onEdit(); updateSuggest(); });
ed.addEventListener("keydown", (e) => {
  if (suggestKey(e)) return;
  const mod = e.ctrlKey || e.metaKey;
  if (mod && e.key === "b") { e.preventDefault(); FMT.bold(); }
  else if (mod && e.key === "i") { e.preventDefault(); FMT.italic(); }
  else if (e.key === "Tab" && !mod && !e.altKey) {
    e.preventDefault();
    const s = ed.selectionStart;
    const ls = ed.value.lastIndexOf("\n", s - 1) + 1;
    const line = ed.value.slice(ls, s);
    if (/^\s*([-*+]|\d+[.)])\s/.test(line) || e.shiftKey) {
      if (e.shiftKey) {
        const m = ed.value.slice(ls).match(/^( {1,2}|\t)/);
        if (m) { ed.setSelectionRange(ls, ls + m[0].length); insert(""); ed.setSelectionRange(s - m[0].length, s - m[0].length); }
      } else { ed.setSelectionRange(ls, ls); insert("  "); ed.setSelectionRange(s + 2, s + 2); }
    } else insert("  ");
  } else if (e.key === "Enter" && !mod && !e.shiftKey && ed.selectionStart === ed.selectionEnd) {
    const s = ed.selectionStart;
    const ls = ed.value.lastIndexOf("\n", s - 1) + 1;
    const line = ed.value.slice(ls, s);
    const m = line.match(/^(\s*)([-*+]|(\d+)([.)]))(\s+)(\[[ xX]\]\s+)?/);
    if (!m) return;
    e.preventDefault();
    if (line.trim() === m[0].trim()) {
      // empty item → end the list
      ed.setSelectionRange(ls, s);
      insert("");
      ed.setSelectionRange(ls, ls); // Chrome may leave the caret before a trailing newline
      return;
    }
    const marker = m[3] ? `${+m[3] + 1}${m[4]}` : m[2];
    insert(`\n${m[1]}${marker}${m[5]}${m[6] ? "[ ] " : ""}`);
  }
});
ed.addEventListener("blur", () => { if (S.dirty) save(); setTimeout(hideSuggest, 150); });

// Paste / drop files → upload to attachments and embed.
async function uploadFiles(files) {
  for (const f of files) {
    try {
      const r = await api("POST", "/api/upload?name=" + encodeURIComponent(f.name || "pasted.png"), f, { raw: true });
      insert(`![[${r.path.split("/").pop()}]]\n`);
      toast(t("toast.uploaded", { name: r.path.split("/").pop() }));
    } catch (e) { fail(e); }
  }
  loadTree();
}
ed.addEventListener("paste", (e) => {
  const files = [...(e.clipboardData?.files || [])];
  if (files.length) { e.preventDefault(); uploadFiles(files.map((f, i) => (f.name && f.name !== "image.png" ? f : new File([f], `Pasted ${new Date().toISOString().slice(0, 19).replace(/[T:]/g, "-")}${i ? "-" + i : ""}.${(f.type.split("/")[1] || "png").replace("jpeg", "jpg")}`, { type: f.type })))); }
});
ed.addEventListener("dragover", (e) => { if (e.dataTransfer.types.includes("Files")) e.preventDefault(); });
ed.addEventListener("drop", (e) => {
  if (!e.dataTransfer.files.length) return;
  e.preventDefault();
  uploadFiles([...e.dataTransfer.files]);
});
$("#file-input").addEventListener("change", (e) => { uploadFiles([...e.target.files]); e.target.value = ""; });

// ------------------------------------------------------------------ [[ autocomplete
const sug = { items: [], active: 0, start: 0 };
function caretXY() {
  const div = document.createElement("div");
  const cs = getComputedStyle(ed);
  for (const p of ["fontFamily", "fontSize", "lineHeight", "paddingTop", "paddingLeft", "paddingRight", "letterSpacing", "tabSize", "boxSizing"]) div.style[p] = cs[p];
  Object.assign(div.style, { position: "absolute", visibility: "hidden", whiteSpace: "pre-wrap", overflowWrap: "break-word", width: ed.clientWidth + "px", top: 0, left: 0 });
  div.textContent = ed.value.slice(0, ed.selectionStart);
  const span = document.createElement("span");
  span.textContent = "​";
  div.append(span);
  document.body.append(div);
  const r = ed.getBoundingClientRect();
  const x = r.left + span.offsetLeft - ed.scrollLeft;
  const y = r.top + span.offsetTop - ed.scrollTop + parseFloat(cs.lineHeight);
  div.remove();
  return { x, y };
}
function updateSuggest() {
  const s = ed.selectionStart;
  const before = ed.value.slice(Math.max(0, s - 200), s);
  const m = before.match(/\[\[([^\[\]\n|#]*)$/);
  if (!m) return hideSuggest();
  const q = fold(m[1]);
  sug.start = s - m[1].length;
  const scored = [];
  for (const n of S.notes) {
    const name = fold(stem(n.path)), title = fold(n.title), path = fold(n.path);
    let sc = name.startsWith(q) ? 0 : title.startsWith(q) ? 1 : name.includes(q) ? 2 : title.includes(q) || path.includes(q) ? 3 : -1;
    if (sc >= 0) scored.push([sc, n]);
  }
  scored.sort((a, b) => a[0] - b[0] || b[1].mtime - a[1].mtime);
  sug.items = scored.slice(0, 8).map((x) => x[1]);
  sug.active = 0;
  if (!sug.items.length) return hideSuggest();
  const box = $("#suggest");
  box.innerHTML = sug.items
    .map((n, i) => `<button type="button" data-i="${i}" class="${i === 0 ? "active" : ""}">${I("description")}<span>${esc(stem(n.path))}</span><span class="sub">${esc(dirOf(n.path))}</span></button>`)
    .join("");
  box.hidden = false;
  const { x, y } = caretXY();
  box.style.left = Math.min(x, innerWidth - box.offsetWidth - 8) + "px";
  box.style.top = (y + box.offsetHeight > innerHeight - 8 ? y - box.offsetHeight - 28 : y + 4) + "px";
}
function hideSuggest() { $("#suggest").hidden = true; sug.items = []; }
function pickSuggest(i) {
  const n = sug.items[i];
  if (!n) return;
  const after = ed.value.slice(ed.selectionStart);
  const closing = after.startsWith("]]") ? "" : "]]";
  ed.setSelectionRange(sug.start, ed.selectionStart);
  insert(linkName(n.path) + closing);
  if (!closing) ed.setSelectionRange(ed.selectionStart + 2, ed.selectionStart + 2);
  hideSuggest();
}
function suggestKey(e) {
  if ($("#suggest").hidden) return false;
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    sug.active = (sug.active + (e.key === "ArrowDown" ? 1 : sug.items.length - 1)) % sug.items.length;
    $$("#suggest button").forEach((b, i) => b.classList.toggle("active", i === sug.active));
    return true;
  }
  if (e.key === "Enter" || e.key === "Tab") { e.preventDefault(); pickSuggest(sug.active); return true; }
  if (e.key === "Escape") { e.preventDefault(); hideSuggest(); return true; }
  return false;
}
$("#suggest").addEventListener("mousedown", (e) => { e.preventDefault(); const b = e.target.closest("button"); if (b) pickSuggest(+b.dataset.i); });

// ------------------------------------------------------------------ side panel
$$(".tabs [role=tab]").forEach((b) => b.addEventListener("click", () => {
  S.tab = b.dataset.tab;
  $$(".tabs [role=tab]").forEach((x) => x.classList.toggle("active", x === b));
  loadPanel();
}));

async function loadPanel() {
  const body = $("#panel-body");
  if (!S.path || S.view !== "note") { body.innerHTML = `<p class="panel-empty">${esc(t("links.none"))}</p>`; return; }
  const path = S.path;
  try {
    if (S.tab === "links") {
      const [r, rel] = await Promise.all([
        api("GET", "/api/links?path=" + encodeURIComponent(path)),
        S.semantic ? api("GET", "/api/related?path=" + encodeURIComponent(path)).catch(() => ({ related: [] })) : { related: [] },
      ]);
      if (S.path !== path) return;
      const item = (p, title, ctx) => `<a class="panel-item" href="#/n/${encPath(p)}"><span class="t">${esc(title)}</span>${ctx ? `<span class="c">${esc(ctx)}</span>` : ""}</a>`;
      const sec = (title, items) => `<div class="panel-section"><h4>${esc(title)}<span>${items.length}</span></h4>${items.join("") || `<p class="panel-empty">${esc(t("links.none"))}</p>`}</div>`;
      body.innerHTML =
        sec(t("links.back"), r.backlinks.map((b) => item(b.path, stem(b.path), b.context))) +
        sec(t("links.out"), r.outgoing.filter((l) => !l.embed || l.path?.endsWith(".md")).map((l) =>
          l.path ? item(l.path, l.target, dirOf(l.path))
                 : `<a class="panel-item missing" href="#/new/${encodeURIComponent(l.target)}"><span class="t">${esc(l.target)}</span><span class="c">${esc(t("links.missing"))}</span></a>`)) +
        sec(t("links.unlinked"), r.unlinked.map((h) => item(h.path, stem(h.path), h.snippet.replace(/[]/g, "")))) +
        (S.semantic ? sec(t("links.related"), rel.related.map((x) => item(x.path, stem(x.path), x.snippet))) : "");
    } else if (S.tab === "history") {
      const r = await api("GET", "/api/history?path=" + encodeURIComponent(path));
      if (S.path !== path) return;
      body.innerHTML = r.revisions.length
        ? r.revisions.map((v) => `<a class="panel-item" data-rev="${v.id}"><span class="t">${esc(fmtFull(v.created_at))}</span><span class="c">${esc(t("history.replacedBy"))} <span class="badge ${v.source.startsWith("mcp:") ? "ai" : ""}">${esc(v.source)}</span> · ${fmtSize(v.size)}</span></a>`).join("")
        : `<p class="panel-empty">${esc(t("history.none"))}</p>`;
    } else {
      const n = S.note;
      const words = ($("#editor").value.match(/[\p{L}\p{N}]+/gu) || []).length;
      const fm = Object.entries(n.frontmatter || {});
      body.innerHTML = `<div class="panel-section"><dl class="kv">
        <dt>${esc(t("info.path"))}</dt><dd>${esc(n.path)}</dd>
        <dt>${esc(t("info.modified"))}</dt><dd>${esc(fmtFull(n.mtime))}</dd>
        <dt>${esc(t("info.size"))}</dt><dd>${fmtSize(new Blob([$("#editor").value]).size)}</dd>
        <dt>${esc(t("info.words"))}</dt><dd>${words}</dd></dl></div>
        <div class="panel-section"><h4>${esc(t("info.tags"))}</h4><div class="chips" style="padding:0 4px">${n.tags.map((x) => `<a class="chip" href="#/tag/${encodeURIComponent(x)}">#${esc(x)}</a>`).join("") || `<span class="panel-empty">${esc(t("links.none"))}</span>`}</div></div>
        ${fm.length ? `<div class="panel-section"><h4>${esc(t("info.props"))}</h4><dl class="kv">${fm.map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(typeof v === "object" ? JSON.stringify(v) : v)}</dd>`).join("")}</dl></div>` : ""}`;
    }
  } catch (e) { fail(e); }
}
$("#panel-body").addEventListener("click", (e) => {
  const r = e.target.closest("[data-rev]");
  if (r) showRevision(+r.dataset.rev);
  else if (e.target.closest("a.panel-item") && isMobile()) setPanel(false);
});

function diffLines(a, b) {
  const A = a.split("\n"), B = b.split("\n");
  if (A.length * B.length > 4e6) return null;
  const n = A.length, m = B.length;
  const dp = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) dp[i][j] = A[i] === B[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
  const out = [];
  let i = 0, j = 0;
  while (i < n || j < m) {
    if (i < n && j < m && A[i] === B[j]) { out.push(["same", A[i]]); i++; j++; }
    else if (j < m && (i >= n || dp[i][j + 1] >= dp[i + 1][j])) { out.push(["add", B[j]]); j++; }
    else { out.push(["del", A[i]]); i++; }
  }
  return out;
}

/** Line diff as HTML with 2 lines of context: "del" = only in `a`, "add" = only in `b`. */
function renderDiff(a, b) {
  const diff = diffLines(a || "", b || "");
  if (!diff) return esc(b);
  const keep = diff.map((x, i) => x[0] !== "same" || diff.slice(Math.max(0, i - 2), i + 3).some((y) => y[0] !== "same"));
  let html = "", gap = false;
  diff.forEach(([k, line], i) => {
    if (!keep[i]) { if (!gap) { html += `<span class="gap">⋯</span>`; gap = true; } return; }
    gap = false;
    html += `<span class="${k === "add" ? "add" : k === "del" ? "del" : "same"}">${esc(line) || " "}</span>`;
  });
  return html || esc(b);
}

async function showRevision(id) {
  try {
    const r = await api("GET", "/api/revision?id=" + id);
    const d = $("#revision-dialog");
    $("#revision-title").textContent = `${t("history.title")} #${id}`;
    $("#revision-body").innerHTML = renderDiff($("#editor").value, r.content);
    $("#revision-restore").onclick = async () => {
      try {
        if (S.dirty) await save();
        await api("POST", "/api/restore", { id });
        d.close();
        await openNote(r.path, { keepScroll: true });
        toast(t("history.restored"));
      } catch (e) { fail(e); }
    };
    d.showModal();
  } catch (e) { fail(e); }
}

// ------------------------------------------------------------------ list views
function showList(title, items) {
  showView("list");
  $("#note-title").textContent = title;
  document.title = `${title} · MindCrab`;
  $("#list-title").textContent = title;
  if (panelOpen()) loadPanel();
  $("#list-items").innerHTML = items.length
    ? items.map((h) => `<a class="card" href="#/n/${encPath(h.path)}"><span class="title-medium">${esc(h.title)}</span><span class="path">${esc(h.path)} · ${esc(fmtTime(h.mtime))}</span>${h.snippet ? `<div class="snip">${snippetHtml(h.snippet)}</div>` : ""}</a>`).join("")
    : `<p class="muted">${esc(t("search.none"))}</p>`;
  S.path = null;
  highlightTree();
}
const snippetHtml = (s) => esc(s).replace(//g, "<mark>").replace(//g, "</mark>");

// ------------------------------------------------------------------ search
let searchTimer, searchSeq = 0, searchItems = [], searchActive = 0;
function openSearch() {
  const d = $("#search-dialog");
  $(".search-hint").textContent = t("search.hint");
  d.showModal();
  const inp = $("#search-input");
  inp.select();
  runSearch();
}
async function runSearch() {
  const q = $("#search-input").value.trim();
  const seq = ++searchSeq;
  let items;
  if (!q) items = [...S.notes].sort((a, b) => b.mtime - a.mtime).slice(0, 12).map((n) => ({ ...n, snippet: "" }));
  else {
    try { items = (await api("GET", "/api/search?limit=30&q=" + encodeURIComponent(q))).hits; }
    catch (e) {
      if (e.status !== 0) return fail(e);
      // Offline: match titles and paths of the notes we know about.
      const f = fold(q);
      items = S.notes.filter((n) => fold(n.title).includes(f) || fold(n.path).includes(f)).slice(0, 30).map((n) => ({ ...n, snippet: "" }));
      $(".search-hint").textContent = t("search.offline");
    }
  }
  if (seq !== searchSeq) return;
  searchItems = items;
  if (q && !/[:#"]/.test(q) && !items.some((h) => fold(stem(h.path)) === fold(q))) searchItems = [...items, { create: q }];
  searchActive = 0;
  $("#search-results").innerHTML = searchItems.length
    ? searchItems.map((h, i) => h.create
        ? `<li class="create" data-i="${i}"><div class="t">${I("note_add")}${esc(t("search.create"))} “${esc(h.create)}”</div></li>`
        : `<li data-i="${i}"><div class="t">${esc(stem(h.path))}<span class="p">${esc(dirOf(h.path))}</span>${h.semantic ? `<span class="badge sem">≈ ${esc(t("search.semantic"))}</span>` : ""}</div>${h.snippet ? `<div class="s">${snippetHtml(h.snippet)}</div>` : ""}</li>`).join("")
    : `<li class="muted">${esc(t("search.none"))}</li>`;
  markActive();
}
function markActive() {
  $$("#search-results li").forEach((li, i) => li.classList.toggle("active", i === searchActive));
  $$("#search-results li")[searchActive]?.scrollIntoView({ block: "nearest" });
}
async function pickSearch(i) {
  const h = searchItems[i];
  if (!h) return;
  $("#search-dialog").close();
  if (h.create) {
    try { const r = await api("POST", "/api/note", { path: h.create }); await loadTree(); go(r.path); } catch (e) { fail(e); }
  } else go(h.path);
}
$("#search-input").addEventListener("input", () => { clearTimeout(searchTimer); searchTimer = setTimeout(runSearch, 120); });
$("#search-input").addEventListener("keydown", (e) => {
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    const n = searchItems.length || 1;
    searchActive = (searchActive + (e.key === "ArrowDown" ? 1 : n - 1)) % n;
    markActive();
  } else if (e.key === "Enter") { e.preventDefault(); pickSearch(searchActive); }
  else if (e.key === "Escape") { e.preventDefault(); $("#search-dialog").close(); }
});
$("#search-results").addEventListener("click", (e) => { const li = e.target.closest("li[data-i]"); if (li) pickSearch(+li.dataset.i); });

// ------------------------------------------------------------------ graph
async function showGraph() {
  if (S.dirty) await save();
  showView("graph");
  if (panelOpen()) loadPanel();
  $("#note-title").textContent = t("nav.graph");
  document.title = `${t("nav.graph")} · MindCrab`;
  if (!S.graph) {
    const { GraphView } = await import("/graph.js");
    S.graph ??= new GraphView($("#graph-canvas"), { onOpen: (p) => go(p) });
  }
  const data = await api("GET", "/api/graph");
  S.graph.setData(data, { current: store.get("last", null), local: $("#graph-local").checked, orphans: $("#graph-orphans").checked });
  if (isMobile()) setDrawer(false);
}
$("#graph-local").addEventListener("change", showGraph);
$("#graph-orphans").addEventListener("change", showGraph);

// ------------------------------------------------------------------ settings
function renderSettings() {
  const theme = store.get("theme", "system");
  $$("#theme-switch button").forEach((b) => b.classList.toggle("on", b.dataset.theme === theme));
  const hue = store.get("hue", 285);
  const sw = $("#swatches");
  sw.innerHTML =
    HUES.map((h) => `<button type="button" class="swatch${hue === h ? " on" : ""}" data-hue="${h}" style="--sh:${h}" aria-label="hue ${h}"></button>`).join("") +
    `<button type="button" class="swatch mono${hue === "mono" ? " on" : ""}" data-hue="mono" aria-label="monochrome"></button>` +
    `<label class="swatch custom${typeof hue === "number" && !HUES.includes(hue) ? " on" : ""}" aria-label="custom"><input type="color" id="custom-color"></label>`;
  $("#opt-wide").checked = store.get("wide", false);
  $("#opt-spell").checked = store.get("spell", false);
}
$("#theme-switch").addEventListener("click", (e) => { const b = e.target.closest("button"); if (!b) return; store.set("theme", b.dataset.theme); applyTheme(); renderSettings(); });
$("#swatches").addEventListener("click", (e) => {
  const b = e.target.closest("button[data-hue]");
  if (!b) return;
  store.set("hue", b.dataset.hue === "mono" ? "mono" : +b.dataset.hue);
  applyTheme(); renderSettings();
});
$("#swatches").addEventListener("input", (e) => {
  if (e.target.id !== "custom-color") return;
  store.set("hue", hexToHue(e.target.value));
  applyTheme();
});
$("#swatches").addEventListener("change", (e) => { if (e.target.id === "custom-color") renderSettings(); });
$("#opt-wide").addEventListener("change", (e) => { store.set("wide", e.target.checked); applyTheme(); });
$("#opt-spell").addEventListener("change", (e) => { store.set("spell", e.target.checked); applyTheme(); });

async function openSettings() {
  renderSettings();
  $("#settings-dialog").showModal();
  try {
    renderSemantic();
    renderOfflineStatus();
    views.renderPrivacy();
    views.renderConnections();
    const [info, sess] = await Promise.all([api("GET", "/api/mcp-info"), api("GET", "/api/session")]);
    $("#version").textContent = `MindCrab v${sess.version}`;
    const mask = (s) => s.slice(0, 6) + "•".repeat(12) + s.slice(-4);
    const maskedUrl = info.url_with_token.replace(info.token, mask(info.token));
    const snippets = {
      "Claude Code": `claude mcp add --transport http mindcrab ${info.url} \\\n  --header "Authorization: Bearer ${info.token}"`,
      "claude.ai / Desktop": `Settings → Connectors → Add custom connector\nURL: ${info.url_with_token}`,
      "Gemini CLI": JSON.stringify({ mcpServers: { mindcrab: { httpUrl: info.url, headers: { Authorization: `Bearer ${info.token}` } } } }, null, 2),
      "Codex CLI": `# ~/.codex/config.toml\n[mcp_servers.mindcrab]\nurl = "${info.url}"\nbearer_token_env_var = "MINDCRAB_TOKEN"\n\n# shell: export MINDCRAB_TOKEN=${info.token}`,
      "JSON": JSON.stringify({ mcpServers: { mindcrab: { type: "http", url: info.url, headers: { Authorization: `Bearer ${info.token}` } } } }, null, 2),
    };
    const box = $("#mcp-info");
    box.innerHTML = `<div class="mcp-box">
      ${info.read_only ? `<p class="body-small muted">${esc(t("settings.readOnly"))}</p>` : ""}
      <span class="label-medium muted">${esc(t("settings.endpoint"))}</span>
      <div class="copy-row"><code>${esc(info.url)}</code><button class="icon-btn" data-copy="url">${I("content_copy")}</button></div>
      <span class="label-medium muted">${esc(t("settings.token"))}</span>
      <div class="copy-row"><code id="tok">${esc(mask(info.token))}</code><button class="icon-btn" data-copy="token">${I("content_copy")}</button></div>
      <span class="label-medium muted">${esc(t("settings.tokenUrl"))}</span>
      <div class="copy-row"><code>${esc(maskedUrl)}</code><button class="icon-btn" data-copy="url_with_token">${I("content_copy")}</button></div>
      <div class="snippet-tabs">${Object.keys(snippets).map((k, i) => `<button type="button" class="chip${i === 0 ? " on" : ""}" data-snip="${esc(k)}">${esc(k)}</button>`).join("")}</div>
      <div class="snippet"><pre id="snip-pre"></pre><button class="icon-btn" data-copy="snippet">${I("content_copy")}</button></div>
    </div>`;
    let current = Object.keys(snippets)[0];
    const show = () => ($("#snip-pre").textContent = snippets[current].replaceAll(info.token, mask(info.token)));
    show();
    box.onclick = (e) => {
      const c = e.target.closest("[data-copy]");
      if (c) return copy(c.dataset.copy === "snippet" ? snippets[current] : info[c.dataset.copy]);
      const s = e.target.closest("[data-snip]");
      if (s) { current = s.dataset.snip; $$("[data-snip]", box).forEach((x) => x.classList.toggle("on", x === s)); show(); }
    };
  } catch (e) { fail(e); }
}
async function renderSemantic() {
  const el = $("#semantic-info");
  try {
    const st = await api("GET", "/api/embed-status");
    S.semantic = st.enabled;
    if (!st.enabled) { el.innerHTML = `<p class="muted">${esc(t("settings.semanticOff"))}</p>`; return; }
    const pct = st.total ? Math.round((st.done / st.total) * 100) : 100;
    el.innerHTML = `<p>${esc(t("settings.semanticOn", { model: st.model, done: st.done, total: st.total }))}</p>
      <div class="progress"><span style="width:${pct}%"></span></div>
      ${st.error ? `<p class="body-small error-text">${esc(t("settings.semanticError", { err: st.error }))}</p>` : ""}`;
  } catch { el.textContent = ""; }
}

function renderOfflineStatus(msg) {
  const ok = "serviceWorker" in navigator && isSecureContext;
  $("#offline-download").disabled = !ok;
  $("#offline-status").textContent = msg ?? (ok ? "" : t("settings.offlineUnsupported"));
}
$("#offline-download").addEventListener("click", async () => {
  const list = [...S.notes];
  let n = 0;
  const worker = async () => {
    while (list.length) {
      const note = list.shift();
      try { const r = await fetch("/api/note?path=" + encodeURIComponent(note.path), { credentials: "same-origin" }); if (r.ok) n++; } catch {}
      renderOfflineStatus(t("settings.offlineProgress", { n, total: S.notes.length }));
    }
  };
  await Promise.all([worker(), worker(), worker(), worker()]);
  await Promise.all(["/api/tree", "/api/tags", "/api/templates"].map((u) => fetch(u).catch(() => {})));
  renderOfflineStatus(t("settings.offlineDone", { n }));
});
$("#offline-clear").addEventListener("click", async () => {
  if ("caches" in window) await caches.delete("mc-data");
  renderOfflineStatus(t("settings.offlineCleared"));
});

$("#logout").addEventListener("click", async () => {
  await api("POST", "/api/logout", {});
  if ("caches" in window) await caches.delete("mc-data").catch(() => {});
  location.reload();
});

// ------------------------------------------------------------------ routing
function go(path) {
  const h = "#/n/" + encPath(path);
  if (location.hash === h) route(); else location.hash = h;
}
async function route() {
  const h = location.hash.slice(1);
  try {
    if (h.startsWith("/n/")) return await openNote(decodeURIComponent(h.slice(3)));
    if (h.startsWith("/new/")) {
      const target = decodeURIComponent(h.slice(5));
      let path = target;
      try { path = (await api("POST", "/api/note", { path: target })).path; } catch (e) { if (e.status !== 409) throw e; }
      await loadTree();
      return go(path.toLowerCase().endsWith(".md") ? path : path + ".md");
    }
    if (h.startsWith("/tag/")) {
      if (S.dirty) await save();
      const tag = decodeURIComponent(h.slice(5));
      const r = await api("GET", "/api/search?limit=200&q=" + encodeURIComponent("tag:" + tag));
      showList(`#${tag}`, r.hits);
      if (isMobile()) setDrawer(false);
      return;
    }
    if (h === "/graph") return await showGraph();
    if (h === "/tasks") return await views.showTasks();
    if (h === "/calendar") return await views.showCalendar();
    if (h === "/activity") return await views.showActivity();
    if (h === "/review") return await views.showReview();
    if (h === "/daily") { history.replaceState(null, "", location.pathname); return openDaily(); }
    if (h.startsWith("/c/")) return await showCanvas(decodeURIComponent(h.slice(3)));
    if (h === "/share" || h === "/share-failed") {
      history.replaceState(null, "", location.pathname);
      if (h === "/share-failed") toast(t("share.failed"));
      else await views.handleShare();
      if (S.view !== "empty") return;
    }
    const last = store.get("last", null);
    const pick = [last, "Welcome.md", "MEMORY.md"].find((p) => p && S.notes.some((n) => n.path === p)) || S.notes[0]?.path;
    if (pick) return go(pick);
    showView("empty");
  } catch (e) { fail(e); }
}
addEventListener("hashchange", route);

// ------------------------------------------------------------------ sync with other devices / AI edits
async function poll() {
  if (document.hidden || $("#app").hidden) return;
  try {
    const before = S.notes.find((n) => n.path === S.path)?.mtime;
    await loadTree();
    loadTags();
    highlightTree();
    views.refreshTaskBadge();
    const now = S.notes.find((n) => n.path === S.path);
    if (S.view === "note" && S.path && !S.dirty && !S.saving && now && now.mtime !== before && document.activeElement !== ed) {
      const fresh = await api("GET", "/api/note?path=" + encodeURIComponent(S.path));
      if (fresh.hash !== S.note.hash && !S.dirty) { await openNote(S.path, { keepScroll: true }); }
    }
  } catch {}
}
setInterval(poll, 20000);
document.addEventListener("visibilitychange", () => { if (!document.hidden) poll(); else if (S.dirty) save(); });
addEventListener("beforeunload", (e) => { if (S.dirty) { save(); e.preventDefault(); } });

// ------------------------------------------------------------------ wiring
$("#toggle-drawer").onclick = () => setDrawer($("#app").classList.contains("drawer-closed"));
$("#scrim").onclick = () => { setDrawer(false); setPanel(false); };
$("#toggle-panel").onclick = () => setPanel(!panelOpen());
$("#close-panel").onclick = () => setPanel(false);
$("#open-search").onclick = openSearch;
$("#new-note").onclick = () => newNote();
$("#fab").onclick = () => newNote();
$("#new-folder").onclick = () => newFolder();
$("#new-canvas").onclick = () => newCanvas(S.path ? dirOf(S.path) : "");
$("#open-daily").onclick = openDaily;
$("#nav-graph").onclick = () => (location.hash = "#/graph");
$("#nav-settings").onclick = openSettings;
$("#mode-switch").onclick = (e) => { const b = e.target.closest("button"); if (b) setMode(b.dataset.mode); };
$("#note-title").onclick = () => S.view === "note" && S.path && promptRename(S.path);
$("#note-menu").onclick = (e) => {
  if (!S.path) return;
  const path = S.path;
  openMenu(e.currentTarget, [
    { icon: "drive_file_rename", label: t("action.rename"), run: () => promptRename(path) },
    { icon: "content_copy", label: t("action.copyLink"), run: () => copy(`[[${linkName(path)}]]`) },
    { icon: "history", label: t("panel.history"), run: () => { $('.tabs [data-tab="history"]').click(); setPanel(true); } },
    { icon: "hub", label: t("graph.local"), run: () => { $("#graph-local").checked = true; location.hash = "#/graph"; } },
    "-",
    { icon: "delete", label: t("action.delete"), danger: true, run: () => deleteNote(path) },
  ]);
};
document.addEventListener("keydown", (e) => {
  const mod = e.ctrlKey || e.metaKey;
  if (mod && e.key.toLowerCase() === "k") { e.preventDefault(); openSearch(); }
  else if (mod && e.key.toLowerCase() === "s") { e.preventDefault(); save(); }
  else if (mod && e.key.toLowerCase() === "e" && S.view === "note") { e.preventDefault(); setMode(S.mode === "edit" ? "preview" : "edit"); if (S.mode === "edit") ed.focus(); }
  else if (e.altKey && e.key.toLowerCase() === "n") { e.preventDefault(); newNote(); }
  else if (e.altKey && e.key.toLowerCase() === "d") { e.preventDefault(); openDaily(); }
  else if (e.key === "Escape") { closeMenu(); if (isMobile()) { setDrawer(false); setPanel(false); } }
});
addEventListener("resize", () => { if (!isMobile()) $("#app").classList.remove("panel-open-mobile"); });

// ------------------------------------------------------------------ rich rendering: math, mermaid, bases
const loaded = {};
function loadScript(src) {
  return (loaded[src] ||= new Promise((res, rej) => {
    const el = document.createElement("script");
    el.src = src;
    el.onload = res;
    el.onerror = rej;
    document.head.append(el);
  }));
}
function loadCss(href) {
  if (loaded[href]) return;
  loaded[href] = true;
  const l = document.createElement("link");
  l.rel = "stylesheet";
  l.href = href;
  document.head.append(l);
}
let mermaidReady = null;
async function enhance(root) {
  const math = root.querySelectorAll(".math-inline, .math-display");
  if (math.length) {
    loadCss("/vendor/katex/katex.min.css");
    try {
      await loadScript("/vendor/katex/katex.min.js");
      math.forEach((el) => {
        if (el.dataset.done) return;
        el.dataset.done = 1;
        try { window.katex.render(el.textContent, el, { displayMode: el.classList.contains("math-display"), throwOnError: false }); }
        catch { el.classList.add("raw"); }
      });
    } catch { math.forEach((el) => el.classList.add("raw")); }
  }
  const mm = root.querySelectorAll("pre > code.language-mermaid");
  if (mm.length) {
    try {
      await loadScript("/vendor/mermaid.min.js");
      const dark = document.documentElement.classList.contains("dark");
      if (mermaidReady !== dark) {
        window.mermaid.initialize({ startOnLoad: false, securityLevel: "strict", theme: dark ? "dark" : "default", fontFamily: "Roboto Flex, system-ui, sans-serif" });
        mermaidReady = dark;
      }
      let i = 0;
      for (const code of mm) {
        const pre = code.parentElement;
        try {
          const { svg } = await window.mermaid.render(`mm-${Date.now()}-${i++}`, code.textContent);
          const div = document.createElement("div");
          div.className = "mermaid";
          div.innerHTML = svg;
          pre.replaceWith(div);
        } catch (e) { pre.title = String(e?.message || e); }
      }
    } catch {}
  }
  for (const el of root.querySelectorAll(".base-view[data-spec]")) {
    if (el.dataset.done) continue;
    el.dataset.done = 1;
    renderBase(el, el.dataset.spec);
  }
}

function fmtCell(v, col, path) {
  if (v === null || v === undefined || v === "") return "";
  if (col === "title" || col === "name") return `<a class="wikilink" href="#/n/${encPath(path)}">${esc(v)}</a>`;
  if (Array.isArray(v)) return v.map((x) => `<span class="pill">${esc(typeof x === "object" ? JSON.stringify(x) : x)}</span>`).join(" ");
  if (typeof v === "boolean") return v ? "✓" : "✗";
  if (typeof v === "object") return esc(JSON.stringify(v));
  return esc(v);
}

async function renderBase(el, spec) {
  let r;
  try { r = await api("POST", "/api/base", { spec }); }
  catch (e) { el.innerHTML = `<div class="base-error">${esc(e.message)}</div>`; return; }
  let sortCol = null, desc = false;
  const draw = () => {
    let rows = [...r.rows];
    if (sortCol !== null) {
      rows.sort((a, b) => {
        const x = a.values[sortCol], y = b.values[sortCol];
        const o = typeof x === "number" && typeof y === "number" ? x - y : String(x ?? "").localeCompare(String(y ?? ""), LANG, { numeric: true });
        return desc ? -o : o;
      });
    }
    const head = `<div class="base-head">${I("table")}<span>${r.total}</span><span class="sp"></span></div>`;
    if (r.view === "cards") {
      el.innerHTML = head + `<div class="base-cards">${rows.map((row) => `<a class="base-card" href="#/n/${encPath(row.path)}">${row.values.map((v, i) => i === 0 ? `<b>${esc(v ?? stem(row.path))}</b>` : v === null || v === "" ? "" : `<div><span class="muted">${esc(r.columns[i])}:</span> ${fmtCell(v, r.columns[i], row.path)}</div>`).join("")}</a>`).join("")}</div>`;
    } else if (r.view === "list") {
      el.innerHTML = head + `<ul style="margin:8px 0;padding-left:32px">${rows.map((row) => `<li><a class="wikilink" href="#/n/${encPath(row.path)}">${esc(row.values[0] ?? stem(row.path))}</a></li>`).join("")}</ul>`;
    } else {
      el.innerHTML = head + `<div class="scroll"><table><thead><tr>${r.columns.map((c, i) => `<th data-i="${i}">${esc(c)}${sortCol === i ? (desc ? " ↓" : " ↑") : ""}</th>`).join("")}</tr></thead>
        <tbody>${rows.map((row) => `<tr>${row.values.map((v, i) => `<td>${fmtCell(v, r.columns[i], row.path)}</td>`).join("")}</tr>`).join("")}</tbody></table></div>`;
      el.querySelectorAll("th").forEach((th) => th.addEventListener("click", () => {
        const i = +th.dataset.i;
        desc = sortCol === i ? !desc : false;
        sortCol = i;
        draw();
      }));
    }
  };
  draw();
}

// ------------------------------------------------------------------ hover preview for links
const hover = { timer: null, hideTimer: null, cache: new Map(), anchor: null };
function hideHover() { clearTimeout(hover.timer); $("#hover-card").hidden = true; hover.anchor = null; }
async function showHover(a) {
  const key = a.dataset.note + "#" + (a.dataset.anchor || "");
  let r = hover.cache.get(key);
  if (!r) {
    try { r = await api("GET", `/api/preview?path=${encodeURIComponent(a.dataset.note)}&anchor=${encodeURIComponent(a.dataset.anchor || "")}`); }
    catch { return; }
    hover.cache.set(key, r);
    setTimeout(() => hover.cache.delete(key), 30000);
  }
  if (hover.anchor !== a) return;
  const card = $("#hover-card");
  card.innerHTML = `<div class="hc-title">${esc(r.title)}</div><div class="markdown">${r.html}</div>`;
  card.hidden = false;
  enhance(card);
  const rect = a.getBoundingClientRect();
  const w = card.offsetWidth, hgt = card.offsetHeight;
  let x = Math.min(rect.left, innerWidth - w - 12), y = rect.bottom + 8;
  if (y + hgt > innerHeight - 12) y = Math.max(12, rect.top - hgt - 8);
  card.style.left = Math.max(12, x) + "px";
  card.style.top = y + "px";
}
document.addEventListener("mouseover", (e) => {
  if (!matchMedia("(hover: hover)").matches) return;
  const a = e.target.closest("#preview a.wikilink[data-note], #panel-body a.panel-item[href^='#/n/'], .hover-card a.wikilink[data-note]");
  if (e.target.closest("#hover-card")) { clearTimeout(hover.hideTimer); return; }
  if (!a) { if (hover.anchor) { clearTimeout(hover.hideTimer); hover.hideTimer = setTimeout(hideHover, 250); } return; }
  if (!a.dataset.note) a.dataset.note = decodeURIComponent(a.getAttribute("href").slice(4));
  if (hover.anchor === a) return;
  clearTimeout(hover.timer);
  clearTimeout(hover.hideTimer);
  hover.anchor = a;
  hover.timer = setTimeout(() => showHover(a), 450);
});
document.addEventListener("click", () => hideHover(), true);

// ------------------------------------------------------------------ properties (frontmatter) editor
function parseFm(text) {
  const m = text.match(/^---\r?\n([\s\S]*?)\r?\n(?:---|\.\.\.)\s*(?:\r?\n|$)/);
  if (!m) return { ok: true, entries: [], end: 0 };
  const lines = m[1].split(/\r?\n/);
  const entries = [];
  for (let i = 0; i < lines.length; i++) {
    const l = lines[i];
    if (!l.trim() || l.trim().startsWith("#")) continue;
    const kv = l.match(/^([^\s:#][^:]*):\s*(.*)$/);
    if (!kv) return { ok: false };
    const key = kv[1].trim();
    let val = kv[2].trim();
    if (val === "" ) {
      const items = [];
      while (i + 1 < lines.length && /^\s*-\s/.test(lines[i + 1])) items.push(lines[++i].replace(/^\s*-\s+/, "").trim().replace(/^["']|["']$/g, ""));
      if (i + 1 < lines.length && /^\s+\S/.test(lines[i + 1]) && !items.length) return { ok: false };
      entries.push([key, items.length ? items : ""]);
    } else if (val.startsWith("[") && val.endsWith("]")) {
      entries.push([key, val.slice(1, -1).split(",").map((x) => x.trim().replace(/^["']|["']$/g, "")).filter(Boolean)]);
    } else if (val === "|" || val === ">" || val.startsWith("{")) {
      return { ok: false };
    } else entries.push([key, val.replace(/^"(.*)"$/, "$1").replace(/^'(.*)'$/, "$1")]);
  }
  return { ok: true, entries, end: m[0].length };
}
function yamlScalar(v) {
  if (v === "" || /^[\w\-./+ ]*$/u.test(v) && !/^(true|false|null|yes|no|~)$/i.test(v) && !/^\d/.test(v) && !/^\s|\s$/.test(v)) return v;
  if (/^-?\d+(\.\d+)?$/.test(v) || /^\d{4}-\d{2}-\d{2}/.test(v) || /^(true|false)$/i.test(v)) return v;
  return JSON.stringify(v);
}
function writeFm(entries) {
  const body = ed.value.replace(/^---\r?\n[\s\S]*?\r?\n(?:---|\.\.\.)\s*(?:\r?\n|$)/, "");
  const live = entries.filter(([k]) => k.trim());
  if (!live.length) { ed.value = body; onEdit(); return; }
  const yaml = live.map(([k, v]) => Array.isArray(v) ? `${k.trim()}: [${v.map(yamlScalar).join(", ")}]` : `${k.trim()}: ${yamlScalar(String(v))}`).join("\n");
  ed.value = `---\n${yaml}\n---\n${body}`;
  onEdit();
}
const ARRAY_KEYS = new Set(["tags", "tag", "aliases", "alias", "cssclasses"]);
function renderProps() {
  const box = $("#props-rows");
  if (!box || S.view !== "note") return;
  if (document.activeElement && box.contains(document.activeElement)) return; // don't fight the user
  const fm = parseFm(ed.value);
  $("#props-count").textContent = fm.ok ? (fm.entries.length || "") : "";
  if (!fm.ok) { box.innerHTML = `<p class="props-note">${esc(t("props.complex"))}</p>`; return; }
  box.innerHTML = fm.entries.map(([k, v], i) => `<div class="prop-row" data-i="${i}" data-arr="${Array.isArray(v) ? 1 : 0}">
      <input class="k" value="${esc(k)}" placeholder="${esc(t("props.key"))}">
      <input class="v" value="${esc(Array.isArray(v) ? v.join(", ") : v)}" placeholder="${esc(t("props.value"))}">
      <button class="icon-btn small" type="button" data-del>${I("close")}</button></div>`).join("")
    + `<div><button class="btn text" type="button" id="prop-add">${I("add")}${esc(t("props.add"))}</button></div>`;
}
function collectProps() {
  return $$("#props-rows .prop-row").map((r) => {
    const k = r.querySelector(".k").value, raw = r.querySelector(".v").value;
    const arr = r.dataset.arr === "1" || ARRAY_KEYS.has(k.trim().toLowerCase());
    return [k, arr ? raw.split(",").map((x) => x.trim()).filter(Boolean) : raw.trim()];
  });
}
$("#props-rows").addEventListener("change", () => writeFm(collectProps()));
$("#props-rows").addEventListener("click", (e) => {
  if (e.target.closest("[data-del]")) {
    e.target.closest(".prop-row").remove();
    writeFm(collectProps());
    renderProps();
  } else if (e.target.closest("#prop-add")) {
    const entries = collectProps();
    entries.push(["", ""]);
    const box = $("#props-rows");
    box.querySelector("#prop-add").parentElement.insertAdjacentHTML("beforebegin", `<div class="prop-row" data-i="${entries.length - 1}" data-arr="0">
      <input class="k" placeholder="${esc(t("props.key"))}"><input class="v" placeholder="${esc(t("props.value"))}">
      <button class="icon-btn small" type="button" data-del>${I("close")}</button></div>`);
    box.querySelector(".prop-row:last-of-type .k")?.focus();
  }
});
let propsTimer;
ed.addEventListener("input", () => { clearTimeout(propsTimer); propsTimer = setTimeout(renderProps, 600); });

const views = createViews({ $, $$, esc, api, t, toast, fail, I, go, S, fmtTime, fmtFull, stem, dirOf, encPath, dialogPrompt, store, LANG, renderDiff, copy, showPage, loadTree });

// ------------------------------------------------------------------ login & boot
function showLogin() {
  $("#app").hidden = true;
  $("#login").hidden = false;
  $("#login-token").focus();
}
$("#login-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  $("#login-error").hidden = true;
  try {
    await api("POST", "/api/login", { token: $("#login-token").value }, { noAuthRedirect: true });
    $("#login-token").value = "";
    boot();
  } catch (err) {
    $("#login-error").textContent = err.status === 401 ? t("login.wrong") : err.message;
    $("#login-error").hidden = false;
  }
});

async function boot() {
  try { S.settings = (await api("GET", "/api/session", undefined, { noAuthRedirect: true })).settings; }
  catch (e) { if (e.status === 401) return showLogin(); return toast(e.message); }
  $("#login").hidden = true;
  $("#app").hidden = false;
  setDrawer(isMobile() ? false : store.get("drawer", true));
  setPanel(isMobile() ? false : store.get("panel", true));
  setMode(S.mode);
  $("#offline-banner").hidden = !S.offline;
  await Promise.all([loadTree(), loadTags(), api("GET", "/api/embed-status").then((st) => (S.semantic = st.enabled)).catch(() => {})]);
  route();
  views.refreshTaskBadge();
  if ("serviceWorker" in navigator && isSecureContext) navigator.serviceWorker.register("/sw.js").catch(() => {});
}

applyI18n();
hydrate();
applyTheme();
boot();
