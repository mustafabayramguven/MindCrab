// Full-page views: tasks, calendar, AI activity, review & cleanup — plus the
// privacy/connection settings and the share-target dialog.

export function createViews(h) {
  const { $, $$, esc, api, t, toast, fail, I, go, S, fmtTime, fmtFull, stem, dirOf, encPath, dialogPrompt, store, LANG, renderDiff, copy } = h;
  const page = () => $("#page-view");
  const pad = (n) => String(n).padStart(2, "0");
  const iso = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  const todayIso = () => iso(new Date());
  const addDays = (d, n) => { const x = new Date(d); x.setDate(x.getDate() + n); return x; };
  const linkText = (s) => s.replace(/\[\[([^\]|#]+)(?:#[^\]|]*)?(?:\|([^\]]+))?\]\]/g, (_, a, b) => b || a);
  const dailyDir = () => S.settings?.daily_dir || "Journal";

  // ---------------------------------------------------------------- tasks
  async function refreshTaskBadge() {
    try {
      const { tasks } = await api("GET", "/api/tasks");
      const today = todayIso();
      const n = tasks.filter((x) => (x.due || x.scheduled) && (x.due || x.scheduled) <= today).length;
      const b = $("#tasks-badge");
      b.hidden = !n;
      b.textContent = n;
    } catch {}
  }

  function taskRow(x, today) {
    const d = x.due || x.scheduled;
    const cls = !x.done && d ? (d < today ? "late" : d === today ? "today" : "") : "";
    return `<div class="task-row${x.done ? " done" : ""}">
      <input type="checkbox" ${x.done ? "checked" : ""} data-path="${esc(x.path)}" data-line="${x.line}" data-raw="${esc(x.raw)}">
      <div class="tx"><div class="t">${esc(linkText(x.text))}</div>
        <div class="meta">
          ${d ? `<span class="pill ${cls}">${x.due ? "📅" : "⏳"} ${esc(d)}</span>` : ""}
          ${x.priority > 0 ? `<span class="pill prio">${x.priority >= 2 ? "⏫" : "🔼"}</span>` : ""}
          ${x.done && x.done_date ? `<span class="pill">✅ ${esc(x.done_date)}</span>` : ""}
          <a href="#/n/${encPath(x.path)}">📄 ${esc(stem(x.path))}</a>
        </div></div></div>`;
  }

  async function showTasks() {
    h.showPage("tasks", t("tasks.title"));
    const showDone = store.get("tasksDone", false);
    let tasks;
    try { ({ tasks } = await api("GET", "/api/tasks" + (showDone ? "?done=1" : ""))); } catch (e) { return fail(e); }
    const today = todayIso(), week = iso(addDays(new Date(), 7));
    const open = tasks.filter((x) => !x.done);
    const groups = [
      ["tasks.overdue", open.filter((x) => (x.due || x.scheduled) && (x.due || x.scheduled) < today)],
      ["tasks.today", open.filter((x) => (x.due || x.scheduled) === today)],
      ["tasks.week", open.filter((x) => { const d = x.due || x.scheduled; return d && d > today && d <= week; })],
      ["tasks.later", open.filter((x) => { const d = x.due || x.scheduled; return d && d > week; })],
      ["tasks.nodate", open.filter((x) => !x.due && !x.scheduled)],
    ];
    if (showDone) groups.push(["tasks.done", tasks.filter((x) => x.done).sort((a, b) => (b.done_date || "").localeCompare(a.done_date || "")).slice(0, 50)]);
    page().innerHTML = `
      <h2 class="headline-small">${esc(t("tasks.title"))}<span class="spacer"></span>
        <label class="switch-row label-large"><input type="checkbox" id="tasks-done" ${showDone ? "checked" : ""}><span class="switch"></span><span>${esc(t("tasks.showDone"))}</span></label></h2>
      <form class="inline-form" id="task-form">
        ${I("add_task")}<input type="text" id="task-text" placeholder="${esc(t("tasks.add"))}" required autocomplete="off">
        <input type="date" id="task-due">
        <button class="btn filled" type="submit">${esc(t("tasks.addBtn"))}</button>
      </form>
      ${groups.filter(([, l]) => l.length).map(([k, l]) => `<h3>${esc(t(k))} <span class="count">${l.length}</span></h3><div class="task-list">${l.map((x) => taskRow(x, today)).join("")}</div>`).join("")
        || `<p class="page-empty">${esc(t("tasks.empty"))}</p>`}`;
    $("#tasks-done").onchange = (e) => { store.set("tasksDone", e.target.checked); showTasks(); };
    $("#task-form").onsubmit = async (e) => {
      e.preventDefault();
      try {
        await api("POST", "/api/tasks", { text: $("#task-text").value, due: $("#task-due").value || null });
        showTasks(); refreshTaskBadge();
      } catch (er) { fail(er); }
    };
    page().onchange = async (e) => {
      const cb = e.target.closest('.task-row input[type="checkbox"]');
      if (!cb) return;
      try { await api("POST", "/api/tasks/toggle", { path: cb.dataset.path, line: +cb.dataset.line, raw: cb.dataset.raw }); }
      catch (er) { fail(er); }
      setTimeout(() => { showTasks(); refreshTaskBadge(); }, 250);
    };
  }

  // ---------------------------------------------------------------- calendar
  function isoWeek(d) {
    const x = new Date(Date.UTC(d.getFullYear(), d.getMonth(), d.getDate()));
    const day = x.getUTCDay() || 7;
    x.setUTCDate(x.getUTCDate() + 4 - day);
    const y0 = new Date(Date.UTC(x.getUTCFullYear(), 0, 1));
    return [x.getUTCFullYear(), Math.ceil(((x - y0) / 86400000 + 1) / 7)];
  }

  async function openPeriod(period, date) {
    try { const r = await api("POST", "/api/daily", { date, period }); await h.loadTree(); go(r.path); } catch (e) { fail(e); }
  }

  async function showCalendar() {
    h.showPage("calendar", t("nav.calendar"));
    const base = S.calMonth || new Date(new Date().getFullYear(), new Date().getMonth(), 1);
    S.calMonth = base;
    let tasks = [];
    try { ({ tasks } = await api("GET", "/api/tasks")); } catch {}
    const byDay = {};
    for (const x of tasks) { const d = x.due || x.scheduled; if (d) (byDay[d] ||= []).push(x); }
    const notes = new Set(S.notes.map((n) => n.path));
    const dir = dailyDir();
    const today = todayIso();
    const first = new Date(base);
    const offset = (first.getDay() + 6) % 7; // Monday first
    const start = addDays(first, -offset);
    const fmtMonth = new Intl.DateTimeFormat(LANG, { month: "long", year: "numeric" });
    const dows = [...Array(7)].map((_, i) => new Intl.DateTimeFormat(LANG, { weekday: "short" }).format(addDays(start, i)));
    let cells = `<span></span>${dows.map((d) => `<span class="dow">${esc(d)}</span>`).join("")}`;
    for (let w = 0; w < 6; w++) {
      const wd = addDays(start, w * 7);
      if (w > 3 && wd.getMonth() !== base.getMonth()) break;
      const [wy, wn] = isoWeek(wd);
      const wpath = `${dir}/${wy}-W${pad(wn)}.md`;
      cells += `<button class="wk${notes.has(wpath) ? " has" : ""}" data-week="${iso(wd)}" title="${wy}-W${pad(wn)}">${esc(t("cal.week"))}${wn}</button>`;
      for (let i = 0; i < 7; i++) {
        const d = addDays(wd, i), key = iso(d);
        const tl = byDay[key] || [];
        cells += `<button class="cal-day${d.getMonth() !== base.getMonth() ? " out" : ""}${key === today ? " today" : ""}" data-day="${key}">
          <span class="n">${d.getDate()}</span>${notes.has(`${dir}/${key}.md`) ? `<span class="dot"></span>` : ""}
          ${tl.length ? `<span class="tasks${key < today ? " late" : ""}">${esc(t("cal.tasks", { n: tl.length }))}</span>` : ""}</button>`;
      }
    }
    const mpath = `${dir}/${base.getFullYear()}-${pad(base.getMonth() + 1)}.md`;
    page().innerHTML = `
      <div class="cal-head">
        <button class="icon-btn" id="cal-prev">${I("chevron_left")}</button>
        <button class="month headline-small" id="cal-month" title="${esc(t("cal.monthNote"))}">${esc(fmtMonth.format(base))}${notes.has(mpath) ? " •" : ""}</button>
        <button class="btn text" id="cal-today">${esc(t("cal.today"))}</button>
        <button class="icon-btn" id="cal-next">${I("chevron_right")}</button>
      </div>
      <div class="cal-grid">${cells}</div>`;
    $("#cal-prev").onclick = () => { S.calMonth = new Date(base.getFullYear(), base.getMonth() - 1, 1); showCalendar(); };
    $("#cal-next").onclick = () => { S.calMonth = new Date(base.getFullYear(), base.getMonth() + 1, 1); showCalendar(); };
    $("#cal-today").onclick = () => { S.calMonth = null; showCalendar(); };
    $("#cal-month").onclick = () => openPeriod("month", iso(base));
    page().onclick = (e) => {
      const d = e.target.closest("[data-day]");
      if (d) return openPeriod("day", d.dataset.day);
      const w = e.target.closest("[data-week]");
      if (w) openPeriod("week", w.dataset.week);
    };
  }

  // ---------------------------------------------------------------- AI activity
  async function showActivity(append = false) {
    if (!append) h.showPage("activity", t("act.title"));
    const all = store.get("actAll", false);
    const before = append ? S.actLast : null;
    let items;
    try { ({ items } = await api("GET", `/api/activity?limit=40${all ? "&all=1" : ""}${before ? `&before=${before}` : ""}`)); } catch (e) { return fail(e); }
    S.actLast = items.length ? items[items.length - 1].id : null;
    const fmtDay = new Intl.DateTimeFormat(LANG, { weekday: "long", day: "numeric", month: "long" });
    let lastDay = append ? S.actDay : null, html = "";
    for (const a of items) {
      const day = fmtDay.format(new Date(a.ts));
      if (day !== lastDay) { html += `<div class="act-day">${esc(day)}</div>`; lastDay = day; }
      const exists = S.notes.some((n) => n.path === a.path);
      const who = a.source.replace(/^mcp:/, "");
      html += `<div class="act-item" data-id="${a.id}">
        <div class="act-head">
          <span class="badge ${a.source.startsWith("mcp:") ? "ai" : ""}">${esc(who)}</span>
          <span class="what">${esc(t("act." + a.kind))} ${exists ? `<a href="#/n/${encPath(a.path)}">${esc(a.path)}</a>` : `<b>${esc(a.path)}</b>`}
            ${a.kind === "move" && a.detail ? `<span class="muted"> · ${esc(t("act.from", { from: a.detail }))}</span>` : ""}</span>
          <time title="${esc(fmtFull(a.ts))}">${esc(new Date(a.ts).toLocaleTimeString(LANG, { hour: "2-digit", minute: "2-digit" }))}</time>
          <button class="btn text" data-act="diff">${I("compare_arrows")}${esc(t("act.diff"))}</button>
          <button class="btn text" data-act="undo">${I("undo")}${esc(t("act.undo"))}</button>
        </div><div class="act-diff" hidden></div></div>`;
    }
    S.actDay = lastDay;
    if (!append) {
      page().innerHTML = `<h2 class="headline-small">${esc(t("act.title"))}<span class="spacer"></span>
        <label class="switch-row label-large"><input type="checkbox" id="act-all" ${all ? "checked" : ""}><span class="switch"></span><span>${esc(t("act.all"))}</span></label></h2>
        <div id="act-list">${html || `<p class="page-empty">${esc(t("act.empty"))}</p>`}</div>
        <div class="row-actions" style="justify-content:center;margin-top:12px"><button class="btn outlined" id="act-more" ${items.length < 40 ? "hidden" : ""}>${esc(t("act.more"))}</button></div>`;
      $("#act-all").onchange = (e) => { store.set("actAll", e.target.checked); showActivity(); };
      $("#act-more").onclick = () => showActivity(true);
      page().onclick = async (e) => {
        const b = e.target.closest("[data-act]");
        if (!b) return;
        const item = b.closest(".act-item"), id = +item.dataset.id;
        if (b.dataset.act === "diff") {
          const box = item.querySelector(".act-diff");
          if (!box.hidden) { box.hidden = true; return; }
          try {
            const d = await api("GET", `/api/activity/${id}`);
            box.innerHTML = `<pre class="revision-diff">${renderDiff(d.before, d.after)}</pre>`;
            box.hidden = false;
          } catch (er) { fail(er); }
        } else {
          try { await api("POST", `/api/activity/${id}/undo`); toast(t("act.undone")); await h.loadTree(); showActivity(); } catch (er) { fail(er); }
        }
      };
    } else {
      $("#act-list").insertAdjacentHTML("beforeend", html);
      $("#act-more").hidden = items.length < 40;
    }
  }

  // ---------------------------------------------------------------- review & cleanup
  async function showReview() {
    h.showPage("review", t("rev.title"));
    let facts = [], rev = null;
    try { [{ facts }, rev] = await Promise.all([api("GET", "/api/pending"), api("GET", "/api/memory-review")]); } catch (e) { return fail(e); }
    const factCard = (f) => `<div class="review-card" data-id="${f.id}">
      <textarea rows="2">${esc(f.fact)}</textarea>
      <div class="meta"><span class="badge ai">${esc(f.source.replace(/^mcp:/, ""))}</span><span>${esc(fmtTime(f.ts))}</span>
        <span>${esc(t("rev.topic"))}:</span><input value="${esc(f.topic || "Inbox")}">
        ${f.tags.map((x) => `<span class="pill">#${esc(x)}</span>`).join("")}</div>
      <div class="acts"><button class="btn text" data-r="reject">${I("close")}${esc(t("rev.reject"))}</button>
        <button class="btn filled" data-r="approve">${I("check")}${esc(t("rev.approve"))}</button></div></div>`;
    const factBox = (f) => `<div class="fact">${esc(f.text)}<small>${esc(f.path)}:${f.line}${f.date ? " · " + esc(f.date) : ""}</small></div>`;
    const dups = rev.duplicates || [];
    page().innerHTML = `
      <h2 class="headline-small">${esc(t("rev.pending"))} <span class="count body-medium muted">${facts.length || ""}</span></h2>
      ${facts.length ? facts.map(factCard).join("") : `<p class="page-empty">${esc(t("rev.pendingEmpty"))}</p>`}
      <h2 class="headline-small">${I("cleaning_services")}${esc(t("rev.cleanup"))}<span class="body-medium muted">${esc(t("rev.facts", { n: rev.facts }))}</span></h2>
      <h3>${esc(t("rev.dups"))} <span class="count">${dups.length}</span></h3>
      ${dups.length ? dups.map((d, i) => `<div class="review-card" data-dup="${i}">
          <div class="dup">${factBox(d.a)}<button class="btn text" data-del="a">${I("delete")}</button>${factBox(d.b)}<button class="btn text" data-del="b">${I("delete")}</button></div>
          <div class="meta">≈ ${Math.round(d.score * 100)}%</div></div>`).join("") : `<p class="page-empty">${esc(t("rev.dupsEmpty"))}</p>`}
      ${(rev.old || []).length ? `<h3>${esc(t("rev.old"))} <span class="count">${rev.old.length}</span></h3>
        <div class="task-list">${rev.old.map((f) => `<a class="panel-item" href="#/n/${encPath(f.path)}"><span class="t">${esc(f.text)}</span><span class="c">${esc(f.path)} · ${esc(f.date)}</span></a>`).join("")}</div>` : ""}`;
    page().onclick = async (e) => {
      const r = e.target.closest("[data-r]");
      if (r) {
        const card = r.closest(".review-card"), id = card.dataset.id;
        try {
          if (r.dataset.r === "approve") {
            await api("POST", `/api/pending/${id}/approve`, { fact: card.querySelector("textarea").value, topic: card.querySelector("input").value });
            toast(t("rev.approved"));
          } else { await api("POST", `/api/pending/${id}/reject`); toast(t("rev.rejected")); }
        } catch (er) { fail(er); }
        h.loadTree(); return showReview();
      }
      const del = e.target.closest("[data-del]");
      if (del) {
        const d = dups[+del.closest("[data-dup]").dataset.dup][del.dataset.del];
        try { await api("POST", "/api/remove-line", { path: d.path, line: d.line, raw: d.raw }); toast(t("rev.removed")); } catch (er) { fail(er); }
        showReview();
      }
    };
  }

  // ---------------------------------------------------------------- settings: privacy & connections
  async function renderPrivacy() {
    try {
      const r = await api("GET", "/api/ai-ignore");
      $("#ai-ignore").value = r.text;
      $("#ai-hidden-count").textContent = t("settings.hiddenCount", { n: r.hidden.length });
    } catch {}
  }
  $("#ai-ignore-save").addEventListener("click", async () => {
    try {
      const r = await api("PUT", "/api/ai-ignore", { text: $("#ai-ignore").value });
      $("#ai-hidden-count").textContent = t("settings.hiddenCount", { n: r.hidden.length });
      toast(t("save.saved"));
      h.loadTree();
    } catch (e) { fail(e); }
  });

  async function renderConnections() {
    const box = $("#conn-list");
    try {
      const { connections } = await api("GET", "/api/connections");
      box.innerHTML = connections.length ? connections.map((c) => `<div class="conn-item" data-id="${c.id}">
          ${I(c.read_only ? "visibility" : "smart_toy")}
          <div class="ci"><b>${esc(c.name)}</b><span>${esc(c.folders.length ? c.folders.join(", ") : "—")} · ${c.read_only ? esc(t("conn.readOnly")) + " · " : ""}${c.review ? esc(t("conn.review")) + " · " : ""}${esc(c.last_used ? t("settings.connUsed", { t: fmtTime(c.last_used) }) : t("settings.connNever"))} · ${esc(c.token_hint)}</span></div>
          <button class="icon-btn small" data-c="edit">${I("edit")}</button>
          <button class="icon-btn small" data-c="del">${I("delete")}</button></div>`).join("")
        : `<p class="body-small muted">${esc(t("settings.connNone"))}</p>`;
      box.onclick = (e) => {
        const b = e.target.closest("[data-c]");
        if (!b) return;
        const c = connections.find((x) => x.id === +b.closest(".conn-item").dataset.id);
        if (b.dataset.c === "edit") connDialog(c);
        else deleteConn(c);
      };
    } catch {}
  }

  async function deleteConn(c) {
    const ok = await dialogPrompt({ title: t("action.delete"), text: t("conn.deleteQ", { n: c.name }), input: false, ok: t("action.delete"), danger: true });
    if (!ok) return;
    try { await api("DELETE", `/api/connections/${c.id}`); } catch (e) { fail(e); }
    renderConnections();
  }

  function connDialog(c) {
    const d = $("#conn-dialog");
    $("#conn-title").textContent = c ? t("conn.edit") : t("conn.new");
    $("#conn-name").value = c?.name || "";
    $("#conn-folders").value = c?.folders.join(", ") || "";
    $("#conn-readonly").checked = !!c?.read_only;
    $("#conn-review").checked = !!c?.review;
    $("#conn-token").hidden = true;
    $("#conn-ok").hidden = false;
    $("#conn-form").onsubmit = async (e) => {
      if (e.submitter?.value !== "ok") return;
      e.preventDefault();
      const body = {
        name: $("#conn-name").value,
        folders: $("#conn-folders").value.split(",").map((x) => x.trim()).filter(Boolean),
        read_only: $("#conn-readonly").checked,
        review: $("#conn-review").checked,
      };
      try {
        if (c) { await api("PUT", `/api/connections/${c.id}`, body); d.close(); }
        else {
          const r = await api("POST", "/api/connections", body);
          const info = await api("GET", "/api/mcp-info");
          const url = info.url.replace(/\/mcp$/, "/mcp/" + r.token);
          $("#conn-token").innerHTML = `<b>${esc(t("conn.tokenOnce"))}</b><code>${esc(r.token)}</code><span>${esc(t("conn.url"))}</span><code>${esc(url)}</code>
            <div class="row-actions"><button class="btn tonal" type="button" id="conn-copy">${I("content_copy")}${esc(t("action.copy"))}</button></div>`;
          $("#conn-token").hidden = false;
          $("#conn-ok").hidden = true;
          $("#conn-copy").onclick = () => copy(r.token);
        }
        renderConnections();
      } catch (er) { fail(er); }
    };
    d.showModal();
  }
  $("#conn-add").addEventListener("click", () => connDialog(null));

  // ---------------------------------------------------------------- share target
  async function handleShare() {
    if (!("caches" in window)) return;
    const cache = await caches.open("mc-share");
    const meta = await cache.match("/__share__");
    if (!meta) return;
    const data = await meta.json();
    const files = [];
    for (let i = 0; i < (data.files || []).length; i++) {
      const r = await cache.match(`/__share__/file/${i}`);
      if (r) files.push(new File([await r.blob()], data.files[i].name, { type: data.files[i].type }));
    }
    const d = $("#share-dialog");
    const firstLine = (data.text || "").split("\n")[0].slice(0, 80);
    $("#share-name").value = data.title || firstLine || data.url || (files[0]?.name ?? "");
    $("#share-text").value = [data.text, data.url && !(data.text || "").includes(data.url) ? data.url : ""].filter(Boolean).join("\n\n");
    $("#share-files").innerHTML = files.map((f) => `<span class="pill">${I("attach_file")}${esc(f.name)}</span>`).join("");
    $("#share-target").value = store.get("shareTarget", "daily");
    d.returnValue = "";
    d.onclose = async () => {
      await cache.delete("/__share__");
      for (let i = 0; i < files.length; i++) await cache.delete(`/__share__/file/${i}`);
      if (d.returnValue !== "ok") return;
      const target = $("#share-target").value;
      store.set("shareTarget", target);
      try {
        let body = $("#share-text").value.trim();
        for (const f of files) {
          const r = await api("POST", "/api/upload?name=" + encodeURIComponent(f.name), f, { raw: true });
          body += `\n\n![[${r.path.split("/").pop()}]]`;
        }
        const name = $("#share-name").value.trim() || "Paylaşım";
        const time = new Date().toLocaleTimeString(LANG, { hour: "2-digit", minute: "2-digit" });
        let path;
        if (target === "new") {
          path = (await api("POST", "/api/note", { path: "Inbox/" + name.replace(/[\\/:*?"<>|#^[\]]/g, "-"), content: `# ${name}\n\n${body}\n` })).path;
        } else {
          const p = target === "daily" ? (await api("POST", "/api/daily", {})).path : "Inbox.md";
          const text = `### ${time} ${name}\n\n${body}`;
          path = (await api("POST", "/api/append", { path: p, text, heading: target === "daily" ? "Shared" : null })).path;
        }
        toast(t("share.saved"));
        await h.loadTree();
        go(path);
      } catch (e) { fail(e); }
    };
    d.showModal();
  }

  return { showTasks, showCalendar, showActivity, showReview, renderPrivacy, renderConnections, handleShare, refreshTaskBadge, openPeriod };
}
