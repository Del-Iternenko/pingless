"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

let HISTORY = 120; // seconds shown on charts (settings: ui.chart_seconds)
const HISTORY_MAX = 300;
const FILTER_PRESETS = ["Russia", "Белые списки", "Auto", "Торренты", "YouTube", "AI"];
const COLORS = ["#10b981", "#6366f1", "#f59e0b", "#ec4899", "#22d3ee"];
const PRESETS = ["cs2.exe", "dota2.exe", "r5apex.exe", "RobloxPlayerBeta.exe", "FortniteClient-Win64-Shipping.exe", "GTA5.exe", "RustClient.exe", "deadlock.exe"];

let state = null;      // StateView from the backend
let tick = null;       // latest Tick
let busy = false;
let processes = [];
const history = new Map(); // exit key -> [{ rtt, loss }]

const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
const icon = (name) => `<svg class="ic"><use href="#i-${name}"/></svg>`;

// Backend errors are English; show the common ones in Russian.
const RU = [
  [/capture needs Administrator/i, "Для перехвата нужны права администратора"],
  [/select at least one process/i, "Выберите хотя бы один процесс на вкладке «Процессы»"],
  [/add a subscription or a relay first/i, "Сначала добавьте подписку или свой relay-сервер"],
  [/cannot reach the subscription server/i, "Не удалось подключиться к серверу подписки"],
  [/web page instead of a subscription/i, "Сервер вернул веб-страницу вместо подписки. Проверьте ссылку или поменяйте User-Agent в настройках"],
  [/happ:\/\/crypt/i, "Зашифрованные ссылки happ://crypt открывает только Happ. Возьмите обычную https-ссылку у провайдера"],
  [/AmneziaWG/i, "Ключи AmneziaWG пока не поддерживаются (скоро будут). Работают ключи Amnezia с протоколом Xray"],
  [/no servers found|returned no usable servers/i, "В подписке не нашлось поддерживаемых серверов"],
  [/answered HTTP (\d+)/i, "Сервер подписки ответил ошибкой HTTP $1"],
  [/Administrator rights were not granted/i, "Права администратора не выданы"],
  [/xray\.exe is missing/i, "Не найден xray.exe рядом с OpenLag"],
];
function ru(msg) {
  msg = String(msg);
  for (const [re, text] of RU) {
    const m = msg.match(re);
    if (m) return text ? text.replace("$1", m[1] ?? "") : msg;
  }
  return msg;
}

function toast(msg, kind = "") {
  const t = document.createElement("div");
  t.className = `toast ${kind}`;
  t.textContent = kind === "error" ? ru(msg) : msg;
  $("toasts").appendChild(t);
  setTimeout(() => t.remove(), kind === "error" ? 7000 : 3500);
}

async function call(cmd, args, okMsg) {
  try {
    const r = await invoke(cmd, args);
    if (okMsg) toast(okMsg, "ok");
    return r;
  } catch (e) {
    toast(e, "error");
    throw e;
  }
}

async function refreshState() {
  state = await invoke("get_state");
  applyUi();
  renderAll();
}

// Accent colour, compact mode, chart length from settings.
function applyUi() {
  const ui = state.settings.ui;
  const hex = /^#[0-9a-f]{6}$/i.test(ui.accent) ? ui.accent : "#10b981";
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16));
  const root = document.documentElement.style;
  root.setProperty("--accent", hex);
  root.setProperty("--accent-soft", `rgba(${r}, ${g}, ${b}, 0.14)`);
  document.body.classList.toggle("compact", !!ui.compact);
  HISTORY = Math.min(HISTORY_MAX, ui.chart_seconds || 120);
}

// ---------------------------------------------------------------- helpers

function displayName(key) {
  if (!key) return "—";
  const s = state?.servers.find((x) => x.key === key);
  return s ? s.name : key;
}

function accentColor() {
  return state?.settings.ui.accent || "#10b981";
}

function pingClass(ms) {
  if (ms == null) return "";
  const ui = state?.settings.ui ?? { ping_good: 60, ping_mid: 120 };
  return ms < ui.ping_good ? "good" : ms < ui.ping_mid ? "mid" : "bad";
}

function fmtMs(v) { return v == null ? "—" : Math.round(v).toString(); }
function fmtBytes(n) {
  if (n == null) return "—";
  const u = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i ? 1 : 0)} ${u[i]}`;
}
function fmtDate(sec) { return sec ? new Date(sec * 1000).toLocaleDateString("ru-RU") : "—"; }
function ago(sec) {
  const d = Math.floor(Date.now() / 1000) - sec;
  if (d < 60) return "только что";
  if (d < 3600) return `${Math.floor(d / 60)} мин назад`;
  if (d < 86400) return `${Math.floor(d / 3600)} ч назад`;
  return fmtDate(sec);
}

/// One row per exit: relays aggregate their routes (best active route), proxies are 1:1.
function exits() {
  const out = new Map();
  for (const p of tick?.snapshot?.paths ?? []) {
    const cur = out.get(p.exit);
    const better = !cur || (p.alive && (!cur.alive || (p.srtt_ms ?? 1e9) < (cur.srtt_ms ?? 1e9)));
    if (better) out.set(p.exit, { ...p });
  }
  return out;
}

function currentExit() {
  const ex = exits();
  const flows = tick?.snapshot?.flows ?? [];
  const recent = flows.filter((f) => f.exit).sort((a, b) => a.idle_ms - b.idle_ms)[0];
  if (recent) return recent.exit;
  const pref = state?.settings.preferred_exit;
  if (pref && ex.get(pref)?.alive) return pref;
  let best = null;
  for (const [k, p] of ex) if (p.alive && (!best || p.srtt_ms < ex.get(best).srtt_ms)) best = k;
  return best;
}

// ---------------------------------------------------------------- navigation

document.querySelectorAll(".nav").forEach((b) =>
  b.addEventListener("click", () => {
    document.querySelectorAll(".nav").forEach((x) => x.classList.toggle("active", x === b));
    document.querySelectorAll(".page").forEach((p) => p.classList.toggle("active", p.id === `page-${b.dataset.page}`));
    if (b.dataset.page !== "fun") Fun.hide();
    if (b.dataset.page === "procs") loadProcesses();
    renderAll();
  }),
);

// ---------------------------------------------------------------- home

function renderAlerts() {
  const a = [];
  if (!state.admin) a.push(["warn", "Без прав администратора перехват трафика не работает. Пинг до серверов меряется и так.", `<button class="btn" id="relaunch">${icon("shield")}Перезапустить от администратора</button>`]);
  if (!state.windivert) a.push(["error", "Нет WinDivert.dll / WinDivert64.sys рядом с OpenLag: перехват недоступен.", ""]);
  if (!state.servers.length && !state.relay_names.length) a.push(["warn", "Нет серверов. Добавьте подписку от VPN-сервиса.", `<button class="btn" data-go="subs">${icon("plus")}Добавить подписку</button>`]);
  const selected = activeProcesses().length;
  if (state.settings.mode === "only" && !selected) a.push(["warn", "Не выбраны процессы.", `<button class="btn" data-go="procs">${icon("cpu")}Выбрать процессы</button>`]);
  if (state.net_error) a.push(["error", ru(state.net_error), ""]);
  if (updateInfo?.available) a.push(["update", `Доступна новая версия OpenLag ${updateInfo.latest} (у вас ${updateInfo.current}).`, `<button class="btn primary" id="do-update">${icon("refresh")}Обновить</button>`]);
  $("alerts").innerHTML = a.map(([k, t, b]) => `<div class="alert ${k === "error" || k === "update" ? k : ""}">${icon("shield")}<span>${esc(t)}</span>${b}</div>`).join("");
  $("relaunch")?.addEventListener("click", () => call("relaunch_admin"));
  $("do-update")?.addEventListener("click", installUpdate);
  document.querySelectorAll("[data-go]").forEach((b) => b.addEventListener("click", () => document.querySelector(`.nav[data-page="${b.dataset.go}"]`).click()));
}

function renderHome() {
  const on = !!tick?.capturing || !!state.capturing;
  $("power").classList.toggle("on", on);
  $("power").classList.toggle("busy", busy);
  $("hero-state").textContent = on ? "Оптимизация включена" : "Выключено";
  $("hero-state").classList.toggle("on", on);
  const names = activeProcesses();
  const what = state.settings.mode === "all_except"
    ? `весь трафик${names.length ? `, кроме ${names.length} процесс.` : ""}`
    : names.length ? names.slice(0, 4).join(", ") + (names.length > 4 ? ` и ещё ${names.length - 4}` : "") : "процессы не выбраны";
  $("hero-sub").textContent = on ? `Через туннель: ${what}` : `Будет через туннель: ${what}`;

  const key = currentExit();
  const ex = exits().get(key);
  const mode = state.settings.preferred_exit ? "" : " · авто";
  $("hero-exit").textContent = key ? displayName(key) + mode : "нет доступных серверов";
  const ping = ex?.alive ? ex.srtt_ms : null;
  $("kpi-ping").textContent = fmtMs(ping);
  $("kpi-ping").className = `kpi-big ${pingClass(ping)}`;
  $("kpi-jitter").textContent = ex?.alive ? `${ex.jitter_ms.toFixed(1)} мс` : "—";
  $("kpi-loss").textContent = ex ? `${ex.loss_pct.toFixed(1)} %` : "—";
  $("kpi-loss").className = `kpi-val ${ex ? (ex.loss_pct < 1 ? "good" : ex.loss_pct < 5 ? "mid" : "bad") : ""}`;
  $("kpi-tunneled").textContent = (tick?.capture?.tunneled ?? 0).toLocaleString("ru-RU");
  const flows = tick?.snapshot?.flows ?? [];
  const tcp = (tick?.snapshot?.tcp ?? []).filter((t) => t.open);
  $("kpi-flows").textContent = flows.length + tcp.length;

  // Chart: the current exit plus up to two other alive exits for comparison.
  const keys = [];
  if (key) keys.push(key);
  const others = [...exits()].filter(([k, p]) => p.alive && k !== key).sort((a, b) => a[1].srtt_ms - b[1].srtt_ms).slice(0, 2);
  for (const [k] of others) keys.push(k);
  const series = keys.map((k, i) => ({ key: k, color: i ? COLORS[i] : accentColor(), values: (history.get(k) ?? []).map((h) => h.rtt) }));
  $("chart-title").textContent = key ? `· ${displayName(key)}` : "";
  $("legend").innerHTML = series.map((s) => `<span><i style="background:${s.color}"></i>${esc(displayName(s.key))}</span>`).join("");
  Chart.line($("chart-ping"), series, { points: HISTORY, minMax: 20 });
  Chart.line($("chart-loss"), key ? [{ color: "#ef4444", values: (history.get(key) ?? []).map((h) => h.loss) }] : [], { points: HISTORY, minMax: 5 });

  const rows = [
    ...flows.sort((a, b) => a.id - b.id).map((f) => `<tr><td><span class="proto udp">UDP</span></td><td class="mono">${esc(f.remote)}</td><td>${esc(f.exit ? displayName(f.exit) : "напрямую")}</td><td class="num">${f.packets_up} пак.</td><td class="num">${f.packets_down} пак.${f.duplicates ? ` <span class="muted">(+${f.duplicates} дубл.)</span>` : ""}</td></tr>`),
    ...tcp.slice(0, 30).map((t) => `<tr><td><span class="proto tcp">TCP</span></td><td class="mono">${esc(t.remote)}</td><td>${esc(displayName(t.exit))}</td><td class="num">${fmtBytes(t.bytes_up)}</td><td class="num">${fmtBytes(t.bytes_down)}</td></tr>`),
  ];
  $("flows-count").textContent = tcp.length > 30 ? `показано 30 из ${tcp.length} TCP` : "";
  $("flows").innerHTML = rows.length ? rows.join("") : `<tr><td colspan="5" class="empty">Пока нет. Включите оптимизацию и запустите программу.</td></tr>`;
}

$("power").addEventListener("click", async () => {
  if (busy) return;
  busy = true;
  renderHome();
  try {
    if (state.capturing) await call("stop", {}, "Оптимизация выключена");
    else await call("start", {}, "Оптимизация включена");
  } catch { /* toast shown */ }
  busy = false;
  await refreshState();
});

// ---------------------------------------------------------------- servers

function renderServers() {
  const filter = $("server-filter").value.trim().toLowerCase();
  const ex = exits();
  const pref = state.settings.preferred_exit;
  $("auto-btn").classList.toggle("on", !pref);
  renderFilters();
  const rows = [
    ...state.servers.map((s) => ({ key: s.key, name: s.name, sub: `${s.subscription} · ${s.address}:${s.port}`, proto: s.protocol, off: s.disabled })),
    ...state.relay_names.map((n) => ({ key: n, name: n, sub: "свой relay", proto: "openlag", off: null })),
  ].filter((r) => !filter || (r.name + r.sub).toLowerCase().includes(filter));
  rows.sort((a, b) => {
    if (!!a.off !== !!b.off) return a.off ? 1 : -1;
    const pa = ex.get(a.key), pb = ex.get(b.key);
    const va = pa?.alive ? pa.srtt_ms : 1e9, vb = pb?.alive ? pb.srtt_ms : 1e9;
    return va - vb;
  });
  if (!rows.length) {
    $("server-list").innerHTML = `<div class="card empty">Серверов нет. Добавьте подписку на вкладке «Подписки».</div>`;
    return;
  }
  $("server-list").innerHTML = rows
    .map((r) => {
      const p = r.off ? null : ex.get(r.key);
      const error = r.off ? null : state.server_errors[r.key];
      const status = r.off ? "" : error ? "dead" : p?.alive ? "alive" : "";
      const ping = p?.alive ? p.srtt_ms : null;
      const selected = pref === r.key;
      const byFilter = r.off && r.off.startsWith("фильтр");
      const isRelay = r.proto === "openlag";
      const sub = r.off ? `<span class="muted">Отключён · ${esc(r.off)}</span>` : error ? `<span class="err-text">${esc(error)}</span>` : esc(r.sub);
      return `<div class="srv ${selected ? "selected" : ""} ${r.off ? "off" : ""}" data-key="${esc(r.key)}">
        <div class="dot ${status}"></div>
        <div style="min-width:0"><div class="name">${esc(r.name)}<span class="badge">${esc(r.proto)}</span></div>
          <div class="sub">${sub}</div></div>
        <div class="metric ${pingClass(ping)}">${r.off ? "—" : ping == null ? (error ? "ошибка" : p ? "нет ответа" : "…") : `${fmtMs(ping)} мс`}<small>пинг</small></div>
        <div class="metric">${p?.alive ? p.jitter_ms.toFixed(1) : "—"}<small>джиттер</small></div>
        <div class="metric">${p ? p.loss_pct.toFixed(0) + "%" : "—"}<small>потери</small></div>
        <canvas class="spark"></canvas>
        <div class="srv-actions">
          ${isRelay ? "" : `<label class="switch" title="${byFilter ? "Отключён фильтром: уберите слово в фильтрах выше" : "Включить / отключить сервер"}"><input type="checkbox" data-act="enable" ${r.off ? "" : "checked"} ${byFilter ? "disabled" : ""}><span></span></label>`}
          <button class="btn ${selected ? "on" : ""}" data-act="pick" ${error || r.off ? "disabled" : ""}>${selected ? icon("check") + "Выбран" : "Выбрать"}</button>
        </div>
      </div>`;
    })
    .join("");
  $("server-list").querySelectorAll(".srv").forEach((el) => {
    const key = el.dataset.key;
    Chart.spark(el.querySelector("canvas"), (history.get(key) ?? []).slice(-40).map((h) => h.rtt), accentColor());
    el.querySelector('[data-act="enable"]')?.addEventListener("change", async (e) => {
      await call("set_server_enabled", { key, enabled: e.target.checked }, e.target.checked ? `${displayName(key)}: включён` : `${displayName(key)}: отключён`);
      await refreshState();
    });
    el.querySelector('[data-act="pick"]').addEventListener("click", async () => {
      const next = state.settings.preferred_exit === key ? null : key;
      await call("set_preferred", { key: next }, next ? `Новые соединения пойдут через ${displayName(key)}` : "Автовыбор сервера");
      await refreshState();
    });
  });
}

function renderFilters() {
  const words = state.settings.disabled_keywords;
  const off = state.servers.filter((s) => s.disabled).length;
  $("filter-count").textContent = off ? `отключено серверов: ${off} из ${state.servers.length}` : "";
  $("filter-chips").innerHTML = words.length
    ? words.map((w) => `<span class="chip">${esc(w)}<button data-w="${esc(w)}" aria-label="Убрать фильтр">${icon("x")}</button></span>`).join("")
    : `<span class="hint">Нет фильтров. Серверы со словом из списка в названии или адресе отключаются целиком и не выбираются в «Авто», даже после обновления подписки.</span>`;
  $("filter-chips").querySelectorAll("button").forEach((b) => b.addEventListener("click", () => setFilters(words.filter((w) => w !== b.dataset.w))));
  $("filter-presets").innerHTML = FILTER_PRESETS.filter((p) => !words.some((w) => w.toLowerCase() === p.toLowerCase()))
    .map((p) => `<button data-p="${esc(p)}">+ ${esc(p)}</button>`).join("");
  $("filter-presets").querySelectorAll("button").forEach((b) => b.addEventListener("click", () => setFilters([...words, b.dataset.p])));
}

async function setFilters(words) {
  await call("set_filters", { keywords: words });
  await refreshState();
}

$("filter-add").addEventListener("click", () => {
  const w = $("filter-input").value.trim();
  if (!w) return;
  $("filter-input").value = "";
  setFilters([...state.settings.disabled_keywords, w]);
});
$("filter-input").addEventListener("keydown", (e) => e.key === "Enter" && $("filter-add").click());
$("server-filter").addEventListener("input", renderServers);
$("auto-btn").addEventListener("click", async () => {
  await call("set_preferred", { key: null }, "Автовыбор: сервер с лучшим пингом");
  await refreshState();
});

// ---------------------------------------------------------------- subscriptions

function renderSubs() {
  const subs = state.settings.subscriptions;
  if (!subs.length) {
    $("sub-list").innerHTML = `<div class="card empty">Подписок пока нет.</div>`;
    return;
  }
  $("sub-list").innerHTML = subs
    .map((s) => {
      const i = s.info || {};
      const used = (i.upload || 0) + (i.download || 0);
      const traffic = i.total ? `${fmtBytes(used)} из ${fmtBytes(i.total)}` : used ? fmtBytes(used) : null;
      const pct = i.total ? Math.min(100, (used / i.total) * 100) : null;
      return `<div class="card sub-card" data-id="${esc(s.id)}">
        <div class="sub-top">
          <label class="switch" title="Включить/выключить"><input type="checkbox" ${s.enabled ? "checked" : ""}><span></span></label>
          <div class="grow"><h3>${esc(s.name)}</h3><div class="hint">${s.servers.length} серв. · ${s.url ? "обновлено " + ago(s.updated_at) : "добавлено вручную"}</div></div>
          ${s.url ? `<button class="btn icon" data-act="refresh" aria-label="Обновить">${icon("refresh")}</button>` : ""}
          <button class="btn icon danger" data-act="remove" aria-label="Удалить">${icon("trash")}</button>
        </div>
        ${traffic || i.expire ? `<div class="sub-meta">${traffic ? `<span>Трафик: ${traffic}</span>` : ""}${i.expire ? `<span>До: ${fmtDate(i.expire)}</span>` : ""}</div>` : ""}
        ${pct != null ? `<div class="bar"><div style="width:${pct}%"></div></div>` : ""}
        ${s.skipped.length ? `<details><summary>Пропущено: ${s.skipped.length}</summary><ul>${s.skipped.map((k) => `<li>${esc(k.entry)}: ${esc(ru(k.reason))}</li>`).join("")}</ul></details>` : ""}
      </div>`;
    })
    .join("");
  $("sub-list").querySelectorAll(".sub-card").forEach((el) => {
    const id = el.dataset.id;
    el.querySelector("input").addEventListener("change", async (e) => {
      await call("toggle_subscription", { id, enabled: e.target.checked });
      await refreshState();
    });
    el.querySelector('[data-act="refresh"]')?.addEventListener("click", async (e) => {
      e.currentTarget.disabled = true;
      try { await call("refresh_subscription", { id }, "Подписка обновлена"); } catch { /* toast */ }
      await refreshState();
    });
    el.querySelector('[data-act="remove"]').addEventListener("click", async () => {
      await call("remove_subscription", { id }, "Подписка удалена");
      await refreshState();
    });
  });
}

$("sub-add").addEventListener("click", async () => {
  const input = $("sub-input").value;
  if (!input.trim()) return toast("Вставьте ссылку или ключи", "error");
  const btn = $("sub-add");
  btn.disabled = true;
  btn.lastChild.textContent = "Загрузка…";
  try {
    const r = await call("add_subscription", { input });
    toast(`«${r.name}»: добавлено серверов ${r.added}${r.skipped.length ? `, пропущено ${r.skipped.length}` : ""}`, "ok");
    $("sub-input").value = "";
  } catch { /* toast */ }
  btn.disabled = false;
  btn.lastChild.textContent = "Добавить";
  await refreshState();
});

// ---------------------------------------------------------------- processes

let selectedGroup = null;

// Each mode has its own list: "tunnel these" and "never tunnel these" never mix.
function groups() {
  return state.settings.mode === "all_except" ? state.settings.exclude_groups : state.settings.groups;
}
function activeProcesses() {
  return groups().filter((g) => g.enabled).flatMap((g) => g.processes);
}

async function saveSelection(gs = groups(), mode = state.settings.mode, autoConnect = state.settings.auto_connect) {
  const cur = state.settings.mode;
  const args = {
    groups: cur === "only" ? gs : state.settings.groups,
    excludeGroups: cur === "all_except" ? gs : state.settings.exclude_groups,
    mode,
    autoConnect,
  };
  try { await call("set_selection", args); } catch { /* toast */ }
  await refreshState();
}

function addToGroup(id, names) {
  const gs = structuredClone(groups());
  const g = gs.find((x) => x.id === id);
  if (!g) return;
  for (const n of names) {
    let x = n.trim().toLowerCase();
    if (!x) continue;
    if (!x.endsWith(".exe")) x += ".exe";
    if (!g.processes.includes(x)) g.processes.push(x);
  }
  saveSelection(gs);
}

function renderProcsPage() {
  const mode = state.settings.mode;
  document.querySelectorAll("#mode-seg button").forEach((b) => b.classList.toggle("active", b.dataset.mode === mode));
  const n = activeProcesses().length;
  $("mode-hint").textContent = mode === "all_except"
    ? `Через туннель идёт весь трафик компьютера, кроме процессов из включённых групп ниже (${n}). Локальная сеть всегда идёт напрямую. У этого режима свой список.`
    : `Через туннель идут только процессы из включённых групп ниже (${n}). Остальное работает напрямую, без VPN и без TUN. У этого режима свой список.`;
  $("auto-wrap").hidden = mode !== "only";
  $("auto-connect").checked = !!state.settings.auto_connect;

  const gs = groups();
  if (!gs.some((g) => g.id === selectedGroup)) selectedGroup = gs[0]?.id ?? null;
  $("group-list").innerHTML = gs.length
    ? gs.map((g) => `
      <div class="card group ${g.id === selectedGroup ? "selected" : ""} ${g.enabled ? "" : "off"}" data-id="${esc(g.id)}">
        <div class="group-head">
          <label class="switch" title="Включить группу"><input type="checkbox" data-act="toggle" ${g.enabled ? "checked" : ""}><span></span></label>
          <input class="gname" data-act="rename" value="${esc(g.name)}" aria-label="Название группы">
          <span class="muted">${g.processes.length}</span>
          <button class="btn icon danger" data-act="delete" aria-label="Удалить группу">${icon("trash")}</button>
        </div>
        <div class="chips">${g.processes.length
          ? g.processes.map((p) => `<span class="chip">${esc(p)}<button data-act="remove" data-p="${esc(p)}" aria-label="Убрать">${icon("x")}</button></span>`).join("")
          : `<span class="hint">Пусто. Добавьте программы справа, через «Обзор» или вручную.</span>`}</div>
        <div class="row">
          <input data-act="manual" placeholder="program.exe">
          <button class="btn" data-act="add">${icon("plus")}Добавить</button>
          <button class="btn" data-act="browse">${icon("folder")}Обзор…</button>
        </div>
        ${g.id === selectedGroup ? `<div class="presets">${PRESETS.filter((p) => !g.processes.includes(p.toLowerCase())).map((p) => `<button data-act="preset" data-p="${esc(p)}">+ ${esc(p)}</button>`).join("")}</div>` : ""}
      </div>`).join("")
    : `<div class="card empty">Групп пока нет. Нажмите «Новая группа».</div>`;

  $("group-list").querySelectorAll(".group").forEach((el) => {
    const id = el.dataset.id;
    el.addEventListener("click", (e) => {
      if (selectedGroup !== id && !e.target.closest("button, input, label")) {
        selectedGroup = id;
        renderProcsPage();
      }
    });
    const edit = (fn) => {
      const c = structuredClone(groups());
      fn(c.find((x) => x.id === id));
      return c;
    };
    el.querySelector('[data-act="toggle"]').addEventListener("change", (e) => saveSelection(edit((g) => { g.enabled = e.target.checked; })));
    el.querySelector('[data-act="rename"]').addEventListener("change", (e) => saveSelection(edit((g) => { g.name = e.target.value.trim() || "Группа"; })));
    el.querySelector('[data-act="delete"]').addEventListener("click", () => saveSelection(groups().filter((x) => x.id !== id)));
    el.querySelectorAll('[data-act="remove"]').forEach((b) => b.addEventListener("click", () => saveSelection(edit((g) => { g.processes = g.processes.filter((p) => p !== b.dataset.p); }))));
    el.querySelectorAll('[data-act="preset"]').forEach((b) => b.addEventListener("click", () => addToGroup(id, [b.dataset.p])));
    const manual = el.querySelector('[data-act="manual"]');
    const addManual = () => { if (manual.value.trim()) addToGroup(id, manual.value.split(/[\s,;]+/)); };
    el.querySelector('[data-act="add"]').addEventListener("click", addManual);
    manual.addEventListener("keydown", (e) => e.key === "Enter" && addManual());
    el.querySelector('[data-act="browse"]').addEventListener("click", async () => {
      const files = await call("pick_exe");
      if (files.length) addToGroup(id, files);
    });
  });
  renderProcs();
}

function renderProcs() {
  const g = groups().find((x) => x.id === selectedGroup);
  $("proc-target").textContent = g ? `→ «${g.name}»` : "";
  const f = $("proc-filter").value.trim().toLowerCase();
  const list = processes.filter((p) => !(g?.processes.includes(p)) && (!f || p.includes(f)));
  $("proc-list").innerHTML = !g
    ? `<div class="hint">Сначала создайте группу.</div>`
    : list.length
      ? list.map((p) => `<div class="proc"><span title="${esc(p)}">${esc(p)}</span><button data-p="${esc(p)}" aria-label="Добавить в группу">${icon("plus")}</button></div>`).join("")
      : `<div class="hint">Ничего не найдено. Запустите программу и нажмите «Обновить».</div>`;
  $("proc-list").querySelectorAll("button").forEach((b) => b.addEventListener("click", () => addToGroup(selectedGroup, [b.dataset.p])));
}

async function loadProcesses() {
  try { processes = await invoke("list_processes"); } catch { processes = []; }
  if (state) renderProcs();
}

document.querySelectorAll("#mode-seg button").forEach((b) => b.addEventListener("click", () => saveSelection(groups(), b.dataset.mode)));
$("group-new").addEventListener("click", () => {
  const id = Math.random().toString(16).slice(2, 10);
  selectedGroup = id;
  const first = state.settings.mode === "all_except" ? "Не через VPN" : "Через VPN";
  saveSelection([...groups(), { id, name: groups().length ? `Группа ${groups().length + 1}` : first, enabled: true, processes: [] }]);
});
$("auto-connect").addEventListener("change", (e) => saveSelection(groups(), state.settings.mode, e.target.checked));
$("proc-filter").addEventListener("input", renderProcs);
$("proc-refresh").addEventListener("click", loadProcesses);

// ---------------------------------------------------------------- settings

let settingsFilled = false;
function renderSettings() {
  const s = state.settings;
  if (!settingsFilled) {
    $("opt-multipath").value = String(s.multipath);
    $("opt-existing").checked = s.tunnel_existing;
    $("opt-ua").value = s.user_agent;
    $("opt-bypass").checked = s.bypass_tun;
    $("opt-relays").value = s.relays_toml;
    $("opt-autostart").checked = s.autostart;
    $("opt-minimized").checked = s.start_minimized;
    $("opt-tray").checked = s.close_to_tray;
    $("opt-update").checked = s.auto_update;
    $("opt-probe").value = String(s.probe_interval_ms);
    $("ui-chart").value = String(s.ui.chart_seconds);
    $("ui-good").value = s.ui.ping_good;
    $("ui-mid").value = s.ui.ping_mid;
    $("ui-compact").checked = s.ui.compact;
    settingsFilled = true;
  }
  $("accent-list").querySelectorAll("button").forEach((b) => b.classList.toggle("active", b.dataset.c.toLowerCase() === s.ui.accent.toLowerCase()));
  const yes = (b) => (b ? `<span class="good">есть</span>` : `<span class="bad">нет</span>`);
  $("ver-info").textContent = `Версия ${state.version}` + (updateInfo?.available ? ` · доступна ${updateInfo.latest}` : "");
  $("sys-info").innerHTML = `
    <span class="muted">Права администратора</span><span>${yes(state.admin)}</span>
    <span class="muted">WinDivert</span><span>${yes(state.windivert)}</span>
    <span class="muted">xray-core</span><span>${yes(state.xray)}</span>
    <span class="muted">Выход к VPN-серверам</span><span>${state.bind_interface ? esc(state.bind_interface) + " (в обход чужих TUN)" : "по таблице маршрутов"}</span>
    <span class="muted">Перехват</span><span>${state.capturing ? '<span class="good">работает</span>' : "выключен"}</span>
    <span class="muted">Серверов</span><span>${state.servers.length + state.relay_names.length}</span>`;
}

$("opt-save").addEventListener("click", async () => {
  await call("set_options", { multipath: Number($("opt-multipath").value), tunnelExisting: $("opt-existing").checked, userAgent: $("opt-ua").value, bypassTun: $("opt-bypass").checked }, "Сохранено");
  await refreshState();
});
async function saveUi(patch) {
  const ui = { ...state.settings.ui, ...patch };
  await call("set_ui_prefs", { ui });
  await refreshState();
}
$("accent-list").querySelectorAll("button").forEach((b) => {
  b.style.background = b.dataset.c;
  b.addEventListener("click", () => saveUi({ accent: b.dataset.c }));
});
$("ui-save").addEventListener("click", () => {
  const good = Math.max(1, Number($("ui-good").value) || 60);
  const mid = Math.max(good + 1, Number($("ui-mid").value) || 120);
  saveUi({ chart_seconds: Number($("ui-chart").value), ping_good: good, ping_mid: mid, compact: $("ui-compact").checked });
  toast("Оформление сохранено", "ok");
});
$("startup-save").addEventListener("click", async () => {
  try {
    await call("set_startup", { autostart: $("opt-autostart").checked, startMinimized: $("opt-minimized").checked, probeMs: Number($("opt-probe").value), closeToTray: $("opt-tray").checked, autoUpdate: $("opt-update").checked }, "Сохранено");
  } catch { /* toast */ }
  await refreshState();
});
$("relays-save").addEventListener("click", async () => {
  try { await call("set_relays", { text: $("opt-relays").value }, "Relay-серверы сохранены"); } catch { /* toast */ }
  await refreshState();
});

// ---------------------------------------------------------------- side status

function renderSide() {
  const on = state.capturing;
  $("side-status").innerHTML = `
    <div class="pill"><span class="dot ${on ? "alive" : ""}"></span>${on ? "Оптимизация включена" : "Выключено"}</div>
    <div class="pill"><span class="dot ${state.admin ? "alive" : "dead"}"></span>${state.admin ? "Администратор" : "Без прав администратора"}</div>`;
}

// ---------------------------------------------------------------- loop

function renderAll() {
  if (!state) return;
  renderSide();
  renderAlerts();
  const page = document.querySelector(".page.active").id;
  if (page === "page-home") renderHome();
  if (page === "page-servers") renderServers();
  if (page === "page-subs") renderSubs();
  if (page === "page-procs") renderProcsPage();
  if (page === "page-settings") renderSettings();
}

// ---------------------------------------------------------------- updates

let updateInfo = null;
let updating = false;

async function installUpdate() {
  if (updating) return;
  updating = true;
  const btn = $("do-update");
  if (btn) btn.disabled = true;
  toast(`Скачиваю OpenLag ${updateInfo?.latest ?? ""}…`, "ok");
  try {
    await invoke("install_update");
  } catch (e) {
    toast(e, "error");
    updating = false;
    if (btn) btn.disabled = false;
  }
}

listen("update-available", (e) => {
  updateInfo = e.payload;
  renderAll();
});
listen("update-progress", (e) => {
  const btn = $("do-update");
  if (btn) btn.lastChild.textContent = `Загрузка ${e.payload}%`;
});

$("check-update").addEventListener("click", async () => {
  try {
    const info = await invoke("check_update");
    updateInfo = info;
    toast(info.available ? `Доступна версия ${info.latest}` : `У вас последняя версия (${info.current})`, "ok");
    renderAll();
  } catch (e) {
    toast(`Не удалось проверить обновления: ${e}`, "error");
  }
});

listen("refresh", () => refreshState());
listen("notify", (e) => toast(e.payload, "error"));

listen("auto", (e) => {
  toast(e.payload, "ok");
  refreshState();
});

listen("tick", (e) => {
  tick = e.payload;
  if (state && state.capturing !== tick.capturing) state.capturing = tick.capturing;
  for (const [k, p] of exits()) {
    const h = history.get(k) ?? [];
    h.push({ rtt: p.alive ? p.last_rtt_ms ?? p.srtt_ms : null, loss: p.loss_pct });
    if (h.length > HISTORY_MAX) h.shift();
    history.set(k, h);
  }
  const page = document.querySelector(".page.active").id;
  if (page === "page-home") renderHome();
  // Don't rebuild the list under the cursor: clicks would land on replaced nodes.
  if (page === "page-servers" && !$("server-list").matches(":hover")) renderServers();
});

window.addEventListener("resize", renderAll);
Fun.init();
refreshState();
// Pick up background changes (auto-refresh, xray restarts).
setInterval(refreshState, 15000);
