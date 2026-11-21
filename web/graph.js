// Force-directed note graph on a canvas, with pan/zoom/drag and touch support.

export class GraphView {
  constructor(canvas, { onOpen }) {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d");
    this.onOpen = onOpen;
    this.nodes = [];
    this.edges = [];
    this.view = { x: 0, y: 0, k: 1 };
    this.hover = null;
    this.current = null;
    this.alpha = 0;
    this.running = false;
    this.pointers = new Map();
    this.bind();
    new ResizeObserver(() => this.resize()).observe(canvas);
  }

  colors() {
    const cs = getComputedStyle(document.documentElement);
    const v = (n) => cs.getPropertyValue(n).trim();
    return {
      node: v("--on-surface-variant"),
      current: v("--primary"),
      neighbor: v("--tertiary"),
      edge: v("--outline-variant"),
      edgeHi: v("--primary"),
      label: v("--on-surface"),
      bg: v("--surface"),
    };
  }

  setData(data, { current, local, orphans }) {
    this.current = current;
    let nodes = data.nodes;
    let edges = data.edges;
    if (local && current) {
      const keep = new Set([current]);
      for (const [a, b] of edges) {
        if (a === current) keep.add(b);
        if (b === current) keep.add(a);
      }
      // second ring for context
      const ring = new Set(keep);
      for (const [a, b] of edges) {
        if (ring.has(a)) keep.add(b);
        if (ring.has(b)) keep.add(a);
      }
      nodes = nodes.filter((n) => keep.has(n.id));
      edges = edges.filter(([a, b]) => keep.has(a) && keep.has(b));
    } else if (!orphans) {
      nodes = nodes.filter((n) => n.degree > 0 || n.id === current);
    }
    const old = new Map(this.nodes.map((n) => [n.id, n]));
    const r = Math.sqrt(nodes.length) * 40 + 40;
    this.nodes = nodes.map((n, i) => {
      const o = old.get(n.id);
      const a = (i / Math.max(1, nodes.length)) * Math.PI * 2;
      return { ...n, x: o?.x ?? Math.cos(a) * r * Math.random(), y: o?.y ?? Math.sin(a) * r * Math.random(), vx: 0, vy: 0 };
    });
    const idx = new Map(this.nodes.map((n, i) => [n.id, i]));
    this.edges = edges.map(([a, b]) => [idx.get(a), idx.get(b)]).filter(([a, b]) => a !== undefined && b !== undefined);
    this.neighbors = new Set();
    for (const [a, b] of this.edges) {
      if (this.nodes[a].id === current) this.neighbors.add(b);
      if (this.nodes[b].id === current) this.neighbors.add(a);
    }
    this.resize();
    this.view = { x: this.w / 2, y: this.h / 2, k: nodes.length > 150 ? 0.6 : 1 };
    this.fitted = false;
    this.kick(1);
  }

  resize() {
    const dpr = window.devicePixelRatio || 1;
    const rect = this.canvas.getBoundingClientRect();
    this.w = rect.width;
    this.h = rect.height;
    this.canvas.width = Math.max(1, rect.width * dpr);
    this.canvas.height = Math.max(1, rect.height * dpr);
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    this.draw();
  }

  kick(a = 0.6) {
    this.alpha = Math.max(this.alpha, a);
    if (!this.running) {
      this.running = true;
      requestAnimationFrame(() => this.tick());
    }
  }

  tick() {
    if (!this.canvas.isConnected || this.canvas.offsetParent === null) {
      this.running = false;
      return;
    }
    const n = this.nodes;
    const a = this.alpha;
    // Repulsion only acts within REACH, so bucket nodes into REACH-sized cells and
    // compare each node with its own and neighbouring cells instead of every node.
    const REACH = 500, R2 = REACH * REACH;
    const push = (p, q) => {
      let dx = q.x - p.x, dy = q.y - p.y;
      const d2 = dx * dx + dy * dy || 0.01;
      if (d2 > R2) return;
      const f = (900 / d2) * a;
      dx *= f; dy *= f;
      p.vx -= dx; p.vy -= dy;
      q.vx += dx; q.vy += dy;
    };
    if (n.length < 400) {
      for (let i = 0; i < n.length; i++) for (let j = i + 1; j < n.length; j++) push(n[i], n[j]);
    } else {
      const grid = new Map();
      for (const p of n) {
        const k = Math.floor(p.x / REACH) * 65536 + Math.floor(p.y / REACH);
        const cell = grid.get(k);
        if (cell) cell.push(p); else grid.set(k, [p]);
      }
      // Each unordered pair of cells once: same cell, then 4 of the 8 neighbours.
      const NB = [[1, -1], [1, 0], [1, 1], [0, 1]];
      for (const [k, cell] of grid) {
        for (let i = 0; i < cell.length; i++) for (let j = i + 1; j < cell.length; j++) push(cell[i], cell[j]);
        const cx = Math.round(k / 65536), cy = k - cx * 65536;
        for (const [ox, oy] of NB) {
          const other = grid.get((cx + ox) * 65536 + cy + oy);
          if (other) for (const p of cell) for (const q of other) push(p, q);
        }
      }
    }
    for (const [i, j] of this.edges) {
      const dx = n[j].x - n[i].x, dy = n[j].y - n[i].y;
      const d = Math.sqrt(dx * dx + dy * dy) || 1;
      const f = ((d - 60) / d) * 0.08 * a;
      n[i].vx += dx * f; n[i].vy += dy * f;
      n[j].vx -= dx * f; n[j].vy -= dy * f;
    }
    for (const p of n) {
      p.vx -= p.x * 0.012 * a;
      p.vy -= p.y * 0.012 * a;
      if (p === this.dragging) { p.vx = p.vy = 0; continue; }
      p.vx *= 0.82; p.vy *= 0.82;
      p.x += p.vx; p.y += p.vy;
    }
    this.alpha *= 0.985;
    if (!this.fitted && this.alpha < 0.15) { this.fitted = true; this.fit(); }
    this.draw();
    if (this.alpha > 0.01 || this.dragging) requestAnimationFrame(() => this.tick());
    else this.running = false;
  }

  /** Zooms and pans so every node is visible. */
  fit() {
    if (!this.nodes.length || !this.w) return;
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (const p of this.nodes) { x0 = Math.min(x0, p.x); y0 = Math.min(y0, p.y); x1 = Math.max(x1, p.x); y1 = Math.max(y1, p.y); }
    const pad = 60;
    const k = Math.min(2, Math.max(0.15, Math.min((this.w - pad * 2) / Math.max(1, x1 - x0), (this.h - pad * 2) / Math.max(1, y1 - y0))));
    this.view = { k, x: this.w / 2 - ((x0 + x1) / 2) * k, y: this.h / 2 - ((y0 + y1) / 2) * k };
  }

  radius(p) {
    return 4 + Math.min(10, Math.sqrt(p.degree) * 2);
  }

  draw() {
    const { ctx, view } = this;
    if (!this.w) return;
    const c = this.colors();
    ctx.clearRect(0, 0, this.w, this.h);
    ctx.save();
    ctx.translate(view.x, view.y);
    ctx.scale(view.k, view.k);
    const focus = this.hover ?? this.nodes.find((p) => p.id === this.current);
    const fi = focus ? this.nodes.indexOf(focus) : -1;
    ctx.lineWidth = 1 / view.k;
    for (const [i, j] of this.edges) {
      const hi = i === fi || j === fi;
      ctx.strokeStyle = hi ? c.edgeHi : c.edge;
      ctx.globalAlpha = hi ? 0.9 : 0.7;
      ctx.beginPath();
      ctx.moveTo(this.nodes[i].x, this.nodes[i].y);
      ctx.lineTo(this.nodes[j].x, this.nodes[j].y);
      ctx.stroke();
    }
    ctx.globalAlpha = 1;
    const showAll = view.k > 1.1 || this.nodes.length < 40;
    ctx.font = `${12 / Math.max(view.k, 0.8)}px "Roboto Flex", system-ui, sans-serif`;
    ctx.textAlign = "center";
    this.nodes.forEach((p, i) => {
      const isCur = p.id === this.current;
      const isNb = this.neighbors.has(i);
      ctx.fillStyle = isCur ? c.current : isNb ? c.neighbor : c.node;
      ctx.beginPath();
      ctx.arc(p.x, p.y, this.radius(p) + (p === this.hover ? 2 : 0), 0, Math.PI * 2);
      ctx.fill();
      if (showAll || isCur || p === this.hover || isNb || p.degree > 6) {
        ctx.fillStyle = c.label;
        ctx.globalAlpha = p === this.hover || isCur ? 1 : 0.8;
        ctx.fillText(p.title, p.x, p.y + this.radius(p) + 13 / Math.max(view.k, 0.8));
        ctx.globalAlpha = 1;
      }
    });
    ctx.restore();
  }

  toWorld(x, y) {
    return { x: (x - this.view.x) / this.view.k, y: (y - this.view.y) / this.view.k };
  }

  pick(x, y) {
    const w = this.toWorld(x, y);
    let best = null, bd = Infinity;
    for (const p of this.nodes) {
      const d = (p.x - w.x) ** 2 + (p.y - w.y) ** 2;
      const r = this.radius(p) + 6 / this.view.k;
      if (d < r * r && d < bd) { best = p; bd = d; }
    }
    return best;
  }

  bind() {
    const cv = this.canvas;
    const pos = (e) => {
      const r = cv.getBoundingClientRect();
      return { x: e.clientX - r.left, y: e.clientY - r.top };
    };
    cv.addEventListener("pointerdown", (e) => {
      cv.setPointerCapture(e.pointerId);
      const p = pos(e);
      this.pointers.set(e.pointerId, p);
      if (this.pointers.size === 1) {
        const hit = this.pick(p.x, p.y);
        this.down = { ...p, hit, moved: false, vx: this.view.x, vy: this.view.y };
        this.fitted = true;
        if (hit) { this.dragging = hit; this.kick(0.3); }
      } else if (this.pointers.size === 2) {
        const [a, b] = [...this.pointers.values()];
        this.pinch = { d: Math.hypot(a.x - b.x, a.y - b.y), k: this.view.k, cx: (a.x + b.x) / 2, cy: (a.y + b.y) / 2, vx: this.view.x, vy: this.view.y };
        this.dragging = null;
      }
    });
    cv.addEventListener("pointermove", (e) => {
      const p = pos(e);
      if (!this.pointers.has(e.pointerId)) {
        const h = this.pick(p.x, p.y);
        if (h !== this.hover) { this.hover = h; cv.style.cursor = h ? "pointer" : "grab"; this.draw(); }
        return;
      }
      this.pointers.set(e.pointerId, p);
      if (this.pinch && this.pointers.size === 2) {
        const [a, b] = [...this.pointers.values()];
        const k = Math.min(4, Math.max(0.15, (this.pinch.k * Math.hypot(a.x - b.x, a.y - b.y)) / this.pinch.d));
        this.view.x = this.pinch.cx - ((this.pinch.cx - this.pinch.vx) * k) / this.pinch.k;
        this.view.y = this.pinch.cy - ((this.pinch.cy - this.pinch.vy) * k) / this.pinch.k;
        this.view.k = k;
        this.draw();
        return;
      }
      if (!this.down) return;
      if (Math.hypot(p.x - this.down.x, p.y - this.down.y) > 4) this.down.moved = true;
      if (this.dragging) {
        const w = this.toWorld(p.x, p.y);
        this.dragging.x = w.x; this.dragging.y = w.y;
        this.kick(0.3);
      } else {
        this.view.x = this.down.vx + p.x - this.down.x;
        this.view.y = this.down.vy + p.y - this.down.y;
        this.draw();
      }
    });
    const up = (e) => {
      this.pointers.delete(e.pointerId);
      if (this.pointers.size < 2) this.pinch = null;
      if (this.down && !this.down.moved && this.down.hit && this.pointers.size === 0) this.onOpen(this.down.hit.id);
      if (this.pointers.size === 0) { this.down = null; this.dragging = null; }
    };
    cv.addEventListener("pointerup", up);
    cv.addEventListener("pointercancel", up);
    cv.addEventListener("wheel", (e) => {
      e.preventDefault();
      this.fitted = true;
      const p = pos(e);
      const k = Math.min(4, Math.max(0.15, this.view.k * Math.exp(-e.deltaY * 0.0015)));
      this.view.x = p.x - ((p.x - this.view.x) * k) / this.view.k;
      this.view.y = p.y - ((p.y - this.view.y) * k) / this.view.k;
      this.view.k = k;
      this.draw();
    }, { passive: false });
  }
}
