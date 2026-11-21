// Canvas editor for JSON Canvas files (https://jsoncanvas.org) — the same format
// Obsidian uses, so .canvas files open in both.

const COLORS = { 1: "oklch(0.65 0.2 25)", 2: "oklch(0.72 0.17 55)", 3: "oklch(0.85 0.16 95)", 4: "oklch(0.7 0.17 145)", 5: "oklch(0.72 0.12 210)", 6: "oklch(0.62 0.18 300)" };
const colorOf = (c) => (c ? COLORS[c] || c : null);
const uid = () => Math.random().toString(16).slice(2, 18);

export class CanvasEditor {
  constructor(root, path, h) {
    this.root = root;
    this.path = path;
    this.h = h;
    this.data = { nodes: [], edges: [] };
    this.view = { x: 0, y: 0, k: 1 };
    this.sel = null; // {type:'node'|'edge', id}
    this.pointers = new Map();
    this.saveTimer = null;
    this.renderCache = new Map();
  }

  async load() {
    const { api } = this.h;
    this.data = await api("GET", "/api/canvas?path=" + encodeURIComponent(this.path));
    this.data.nodes ||= [];
    this.data.edges ||= [];
    this.build();
    this.fit();
  }

  build() {
    const { I, t } = this.h;
    this.root.innerHTML = `
      <div class="cv-world"><svg class="cv-edges"></svg></div>
      <div class="cv-zoom"></div>
      <div class="cv-toolbar">
        <button class="icon-btn" data-tool="text" title="${t("canvas.text")}">${I("text_fields")}</button>
        <button class="icon-btn" data-tool="note" title="${t("canvas.note")}">${I("description")}</button>
        <button class="icon-btn" data-tool="link" title="${t("canvas.link")}">${I("link")}</button>
        <button class="icon-btn" data-tool="group" title="${t("canvas.group")}">${I("select_all")}</button>
        <span class="sep"></span>
        <span class="cv-colors">${["", 1, 2, 3, 4, 5, 6].map((c) => `<button data-color="${c}" style="background:${colorOf(c) || "var(--surface-container-highest)"}"></button>`).join("")}</span>
        <span class="sep"></span>
        <button class="icon-btn" data-tool="delete" title="${t("canvas.delete")}">${I("delete")}</button>
        <button class="icon-btn" data-tool="fit" title="${t("canvas.fit")}">${I("fit_screen")}</button>
      </div>`;
    this.world = this.root.querySelector(".cv-world");
    this.svg = this.root.querySelector(".cv-edges");
    this.zoomLabel = this.root.querySelector(".cv-zoom");
    this.root.querySelector(".cv-toolbar").addEventListener("click", (e) => this.toolbar(e));
    this.bind();
    this.renderAll();
  }

  // ---------------------------------------------------------------- rendering
  renderAll() {
    this.world.querySelectorAll(".cv-node").forEach((n) => n.remove());
    // groups first so they sit behind cards
    const sorted = [...this.data.nodes].sort((a, b) => (a.type === "group" ? 0 : 1) - (b.type === "group" ? 0 : 1));
    for (const n of sorted) this.world.appendChild(this.nodeEl(n));
    this.renderEdges();
    this.applyView();
  }

  nodeEl(n) {
    const { esc, t } = this.h;
    const el = document.createElement("div");
    el.className = "cv-node" + (n.type === "group" ? " group" : "") + (this.sel?.id === n.id ? " sel" : "");
    el.dataset.id = n.id;
    this.place(el, n);
    const c = colorOf(n.color);
    if (c) { el.style.borderColor = c; if (n.type !== "group") el.style.background = `color-mix(in oklch, ${c} 14%, var(--surface-container-low))`; }
    let label = "", body = "";
    if (n.type === "text") body = `<div class="cv-body markdown" data-md></div>`;
    else if (n.type === "file") { label = esc(n.file.replace(/\.md$/i, "")); body = `<div class="cv-body markdown" data-file></div>`; }
    else if (n.type === "link") { label = esc(n.url); body = `<div class="cv-body"><a href="${esc(n.url)}" target="_blank" rel="noopener noreferrer">${esc(n.url)}</a></div>`; }
    else if (n.type === "group") label = esc(n.label || t("canvas.group"));
    el.innerHTML = `${label || n.type === "group" ? `<div class="cv-label">${label}</div>` : ""}${body}
      <span class="cv-handle top" data-side="top"></span><span class="cv-handle right" data-side="right"></span>
      <span class="cv-handle bottom" data-side="bottom"></span><span class="cv-handle left" data-side="left"></span>
      <span class="cv-resize"></span>`;
    if (n.type === "text") this.fillMarkdown(el.querySelector("[data-md]"), n.text || "", n.id);
    if (n.type === "file") this.fillFile(el.querySelector("[data-file]"), n.file);
    return el;
  }

  async fillMarkdown(box, md, key) {
    const k = key + "|" + md;
    if (this.renderCache.has(k)) { box.innerHTML = this.renderCache.get(k); return; }
    try {
      const r = await this.h.api("POST", "/api/render", { path: this.path, content: md });
      this.renderCache.set(k, r.html);
      box.innerHTML = r.html;
      this.h.enhance(box);
    } catch { box.textContent = md; }
  }

  async fillFile(box, file) {
    try {
      const r = await this.h.api("GET", "/api/preview?path=" + encodeURIComponent(file));
      box.innerHTML = r.html;
      this.h.enhance(box);
    } catch { box.textContent = file; }
  }

  place(el, n) {
    Object.assign(el.style, { left: n.x + "px", top: n.y + "px", width: n.width + "px", height: n.height + "px" });
  }

  anchor(n, side) {
    switch (side) {
      case "top": return [n.x + n.width / 2, n.y];
      case "bottom": return [n.x + n.width / 2, n.y + n.height];
      case "left": return [n.x, n.y + n.height / 2];
      default: return [n.x + n.width, n.y + n.height / 2];
    }
  }

  bestSides(a, b) {
    const dx = b.x + b.width / 2 - (a.x + a.width / 2), dy = b.y + b.height / 2 - (a.y + a.height / 2);
    return Math.abs(dx) > Math.abs(dy) ? (dx > 0 ? ["right", "left"] : ["left", "right"]) : dy > 0 ? ["bottom", "top"] : ["top", "bottom"];
  }

  curve(p, side, q, side2) {
    const off = (s, d) => ({ top: [0, -d], bottom: [0, d], left: [-d, 0], right: [d, 0] })[s] || [0, 0];
    const d = Math.max(40, Math.hypot(q[0] - p[0], q[1] - p[1]) / 3);
    const [ax, ay] = off(side, d), [bx, by] = off(side2, d);
    return `M${p[0]},${p[1]} C${p[0] + ax},${p[1] + ay} ${q[0] + bx},${q[1] + by} ${q[0]},${q[1]}`;
  }

  renderEdges(temp) {
    const byId = new Map(this.data.nodes.map((n) => [n.id, n]));
    let html = `<defs><marker id="cv-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" fill="var(--outline)"/></marker></defs>`;
    for (const e of this.data.edges) {
      const a = byId.get(e.fromNode), b = byId.get(e.toNode);
      if (!a || !b) continue;
      const [s1, s2] = e.fromSide && e.toSide ? [e.fromSide, e.toSide] : this.bestSides(a, b);
      const p = this.anchor(a, s1), q = this.anchor(b, s2);
      const c = colorOf(e.color);
      const start = e.fromEnd === "arrow" ? ' marker-start="url(#cv-arrow)"' : "";
      const end = e.toEnd === "none" ? "" : ' marker-end="url(#cv-arrow)"';
      html += `<path data-edge="${e.id}" class="${this.sel?.id === e.id ? "sel" : ""}" d="${this.curve(p, s1, q, s2)}"${start}${end}${c ? ` style="stroke:${c}"` : ""}/>`;
      if (e.label) html += `<text class="lbl" x="${(p[0] + q[0]) / 2}" y="${(p[1] + q[1]) / 2 - 6}" text-anchor="middle">${this.h.esc(e.label)}</text>`;
    }
    if (temp) html += `<path d="${this.curve(temp.p, temp.side, temp.q, "left")}" style="stroke:var(--primary);stroke-dasharray:6 4"/>`;
    this.svg.innerHTML = html;
  }

  applyView() {
    const { x, y, k } = this.view;
    this.world.style.transform = `translate(${x}px,${y}px) scale(${k})`;
    this.root.style.backgroundPosition = `${x}px ${y}px`;
    this.root.style.backgroundSize = `${24 * k}px ${24 * k}px`;
    this.zoomLabel.textContent = Math.round(k * 100) + "%";
  }

  fit() {
    const ns = this.data.nodes;
    const r = this.root.getBoundingClientRect();
    if (!ns.length) { this.view = { x: r.width / 2, y: r.height / 3, k: 1 }; return this.applyView(); }
    const x0 = Math.min(...ns.map((n) => n.x)), y0 = Math.min(...ns.map((n) => n.y));
    const x1 = Math.max(...ns.map((n) => n.x + n.width)), y1 = Math.max(...ns.map((n) => n.y + n.height));
    const k = Math.min(1.2, Math.max(0.15, Math.min((r.width - 80) / (x1 - x0 || 1), (r.height - 140) / (y1 - y0 || 1))));
    this.view = { k, x: r.width / 2 - ((x0 + x1) / 2) * k, y: (r.height - 60) / 2 - ((y0 + y1) / 2) * k };
    this.applyView();
  }

  toWorld(cx, cy) {
    const r = this.root.getBoundingClientRect();
    return [(cx - r.left - this.view.x) / this.view.k, (cy - r.top - this.view.y) / this.view.k];
  }

  select(s) {
    this.sel = s;
    this.world.querySelectorAll(".cv-node").forEach((el) => el.classList.toggle("sel", s?.type === "node" && el.dataset.id === s.id));
    this.renderEdges();
  }

  changed() {
    clearTimeout(this.saveTimer);
    this.saveTimer = setTimeout(() => this.save(), 700);
  }

  async save() {
    clearTimeout(this.saveTimer);
    try { await this.h.api("PUT", "/api/canvas", { path: this.path, content: this.data }); }
    catch (e) { this.h.fail(e); }
  }

  // ---------------------------------------------------------------- editing
  /** Moves a new card right/down until it doesn't cover an existing one. */
  freeSpot(n) {
    const hit = (m) => m.type !== "group" && n.x < m.x + m.width + 20 && n.x + n.width + 20 > m.x && n.y < m.y + m.height + 20 && n.y + n.height + 20 > m.y;
    for (let i = 0; i < 40 && this.data.nodes.some(hit); i++) {
      const blocker = this.data.nodes.find(hit);
      n.x = blocker.x + blocker.width + 40;
      if (i % 4 === 3) { n.x -= (blocker.width + 40) * 4; n.y += n.height + 40; }
    }
  }

  addNode(n, { place = true } = {}) {
    n.id ||= uid();
    if (place && n.type !== "group") this.freeSpot(n);
    this.data.nodes.push(n);
    this.world.appendChild(this.nodeEl(n));
    this.select({ type: "node", id: n.id });
    this.changed();
    return n;
  }

  center() {
    const r = this.root.getBoundingClientRect();
    const [x, y] = this.toWorld(r.left + r.width / 2, r.top + r.height / 2);
    return [Math.round(x - 130), Math.round(y - 70)];
  }

  async toolbar(e) {
    const b = e.target.closest("button");
    if (!b) return;
    const { dialogPrompt, t } = this.h;
    if (b.dataset.color !== undefined) {
      const c = b.dataset.color || undefined;
      if (this.sel?.type === "node") { const n = this.node(this.sel.id); n.color = c; this.rerender(n); }
      if (this.sel?.type === "edge") { const ed = this.data.edges.find((x) => x.id === this.sel.id); ed.color = c; this.renderEdges(); }
      return this.changed();
    }
    const [x, y] = this.center();
    switch (b.dataset.tool) {
      case "text": { const n = this.addNode({ type: "text", text: "", x, y, width: 260, height: 140 }); this.editText(n); break; }
      case "note": {
        const name = await dialogPrompt({ title: t("canvas.note"), label: t("canvas.pickNote"), value: "", ok: t("action.create") });
        if (!name) return;
        try {
          const r = await this.h.api("GET", "/api/preview?path=" + encodeURIComponent(name));
          this.addNode({ type: "file", file: r.path, x, y, width: 340, height: 260 });
        } catch (er) { this.h.fail(er); }
        break;
      }
      case "link": {
        const url = await dialogPrompt({ title: t("canvas.link"), label: t("canvas.url"), value: "https://", ok: t("action.create") });
        if (url && /^https?:\/\//.test(url)) this.addNode({ type: "link", url, x, y, width: 320, height: 100 });
        break;
      }
      case "group": this.addNode({ type: "group", label: t("canvas.group"), x: x - 40, y: y - 40, width: 480, height: 320 }); this.renderAll(); break;
      case "delete": this.deleteSel(); break;
      case "fit": this.fit(); break;
    }
  }

  node(id) { return this.data.nodes.find((n) => n.id === id); }

  rerender(n) {
    const old = this.world.querySelector(`.cv-node[data-id="${CSS.escape(n.id)}"]`);
    const el = this.nodeEl(n);
    old ? old.replaceWith(el) : this.world.appendChild(el);
    this.renderEdges();
  }

  deleteSel() {
    if (!this.sel) return;
    if (this.sel.type === "node") {
      this.data.nodes = this.data.nodes.filter((n) => n.id !== this.sel.id);
      this.data.edges = this.data.edges.filter((e) => e.fromNode !== this.sel.id && e.toNode !== this.sel.id);
      this.world.querySelector(`.cv-node[data-id="${CSS.escape(this.sel.id)}"]`)?.remove();
    } else this.data.edges = this.data.edges.filter((e) => e.id !== this.sel.id);
    this.sel = null;
    this.renderEdges();
    this.changed();
  }

  editText(n) {
    const el = this.world.querySelector(`.cv-node[data-id="${CSS.escape(n.id)}"]`);
    const body = el.querySelector(".cv-body");
    const ta = document.createElement("textarea");
    ta.value = n.text || "";
    body.replaceWith(ta);
    ta.focus();
    ta.addEventListener("pointerdown", (e) => e.stopPropagation());
    ta.addEventListener("blur", () => { n.text = ta.value; this.rerender(n); this.changed(); });
  }

  editGroupLabel(n) {
    this.h.dialogPrompt({ title: this.h.t("canvas.group"), label: "", value: n.label || "", ok: this.h.t("action.save") }).then((v) => {
      if (v !== null) { n.label = v; this.rerender(n); this.changed(); }
    });
  }

  // ---------------------------------------------------------------- pointer handling
  bind() {
    const root = this.root;
    root.addEventListener("wheel", (e) => {
      e.preventDefault();
      const r = root.getBoundingClientRect();
      const px = e.clientX - r.left, py = e.clientY - r.top;
      const k = Math.min(3, Math.max(0.1, this.view.k * Math.exp(-e.deltaY * 0.0015)));
      this.view.x = px - ((px - this.view.x) * k) / this.view.k;
      this.view.y = py - ((py - this.view.y) * k) / this.view.k;
      this.view.k = k;
      this.applyView();
    }, { passive: false });

    root.addEventListener("dblclick", (e) => {
      const el = e.target.closest(".cv-node");
      if (e.target.closest(".cv-toolbar")) return;
      if (!el) {
        const [x, y] = this.toWorld(e.clientX, e.clientY);
        const n = this.addNode({ type: "text", text: "", x: Math.round(x - 130), y: Math.round(y - 60), width: 260, height: 120 }, { place: false });
        return this.editText(n);
      }
      const n = this.node(el.dataset.id);
      if (n.type === "text") this.editText(n);
      else if (n.type === "file") this.h.go(n.file);
      else if (n.type === "group") this.editGroupLabel(n);
    });

    root.addEventListener("pointerdown", (e) => {
      if (e.target.closest(".cv-toolbar") || e.target.tagName === "TEXTAREA" || e.target.closest("a")) return;
      root.setPointerCapture(e.pointerId);
      this.pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
      if (this.pointers.size === 2) {
        const [a, b] = [...this.pointers.values()];
        this.pinch = { d: Math.hypot(a.x - b.x, a.y - b.y), k: this.view.k, cx: (a.x + b.x) / 2, cy: (a.y + b.y) / 2, vx: this.view.x, vy: this.view.y };
        this.drag = null;
        return;
      }
      const edge = e.target.closest("[data-edge]");
      if (edge) { this.select({ type: "edge", id: edge.dataset.edge }); return; }
      const el = e.target.closest(".cv-node");
      const [wx, wy] = this.toWorld(e.clientX, e.clientY);
      if (!el) {
        this.select(null);
        this.drag = { kind: "pan", sx: e.clientX, sy: e.clientY, vx: this.view.x, vy: this.view.y };
        return;
      }
      const n = this.node(el.dataset.id);
      this.select({ type: "node", id: n.id });
      if (e.target.classList.contains("cv-handle")) {
        this.drag = { kind: "link", from: n, side: e.target.dataset.side };
      } else if (e.target.classList.contains("cv-resize")) {
        this.drag = { kind: "resize", n, sx: wx, sy: wy, w: n.width, h: n.height };
      } else {
        // Moving a group drags the cards inside it too.
        const inside = n.type === "group"
          ? this.data.nodes.filter((m) => m !== n && m.x >= n.x && m.y >= n.y && m.x + m.width <= n.x + n.width && m.y + m.height <= n.y + n.height)
          : [];
        this.drag = { kind: "move", n, sx: wx, sy: wy, items: [n, ...inside].map((m) => ({ m, x: m.x, y: m.y })) };
      }
    });

    root.addEventListener("pointermove", (e) => {
      if (!this.pointers.has(e.pointerId)) return;
      this.pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
      if (this.pinch && this.pointers.size === 2) {
        const [a, b] = [...this.pointers.values()];
        const k = Math.min(3, Math.max(0.1, (this.pinch.k * Math.hypot(a.x - b.x, a.y - b.y)) / this.pinch.d));
        const r = root.getBoundingClientRect();
        const cx = this.pinch.cx - r.left, cy = this.pinch.cy - r.top;
        this.view.x = cx - ((cx - this.pinch.vx) * k) / this.pinch.k;
        this.view.y = cy - ((cy - this.pinch.vy) * k) / this.pinch.k;
        this.view.k = k;
        return this.applyView();
      }
      const d = this.drag;
      if (!d) return;
      const [wx, wy] = this.toWorld(e.clientX, e.clientY);
      if (d.kind === "pan") {
        this.view.x = d.vx + e.clientX - d.sx;
        this.view.y = d.vy + e.clientY - d.sy;
        this.applyView();
      } else if (d.kind === "move") {
        const dx = Math.round(wx - d.sx), dy = Math.round(wy - d.sy);
        for (const it of d.items) {
          it.m.x = it.x + dx; it.m.y = it.y + dy;
          const el = this.world.querySelector(`.cv-node[data-id="${CSS.escape(it.m.id)}"]`);
          if (el) this.place(el, it.m);
        }
        d.moved = true;
        this.renderEdges();
      } else if (d.kind === "resize") {
        d.n.width = Math.max(120, Math.round(d.w + wx - d.sx));
        d.n.height = Math.max(60, Math.round(d.h + wy - d.sy));
        this.place(this.world.querySelector(`.cv-node[data-id="${CSS.escape(d.n.id)}"]`), d.n);
        d.moved = true;
        this.renderEdges();
      } else if (d.kind === "link") {
        this.renderEdges({ p: this.anchor(d.from, d.side), side: d.side, q: [wx, wy] });
      }
    });

    const up = (e) => {
      this.pointers.delete(e.pointerId);
      if (this.pointers.size < 2) this.pinch = null;
      const d = this.drag;
      this.drag = null;
      if (!d) return;
      if (d.kind === "link") {
        const target = document.elementFromPoint(e.clientX, e.clientY)?.closest(".cv-node");
        const to = target && this.node(target.dataset.id);
        if (to && to.id !== d.from.id) {
          const [, toSide] = this.bestSides(d.from, to);
          this.data.edges.push({ id: uid(), fromNode: d.from.id, fromSide: d.side, toNode: to.id, toSide, toEnd: "arrow" });
          this.changed();
        }
        this.renderEdges();
      } else if (d.moved) this.changed();
    };
    root.addEventListener("pointerup", up);
    root.addEventListener("pointercancel", up);

    this.keyHandler = (e) => {
      if (!this.root.isConnected || this.root.hidden) return;
      if ((e.key === "Delete" || e.key === "Backspace") && !/INPUT|TEXTAREA/.test(document.activeElement?.tagName)) {
        e.preventDefault();
        this.deleteSel();
      }
    };
    document.addEventListener("keydown", this.keyHandler);
  }

  destroy() {
    if (this.saveTimer) this.save();
    document.removeEventListener("keydown", this.keyHandler);
  }
}
