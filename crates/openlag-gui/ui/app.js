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
const warmup = new Map(); // exit key -> ticks seen alive

const $ = (id) => document.getElementById(id);
let regionNames = null;
function countryName(cc) {
  if (regionNames?.lang !== I18N.lang) regionNames = { lang: I18N.lang, names: new Intl.DisplayNames([I18N.lang], { type: "region" }) };
  try { return regionNames.names.of(cc) ?? cc; } catch { return cc; }
}
function flagHtml(cc) {
  return cc && /^[A-Z]{2}$/.test(cc)
    ? `<img class="flag" src="flags/${cc.toLowerCase()}.svg" alt="${cc}">`
    : `<span class="flag none">?</span>`;
}
// A country without a flag file falls back to its code (inline onerror is blocked by CSP).
document.addEventListener("error", (e) => {
  const img = e.target;
  if (img instanceof HTMLImageElement && img.classList.contains("flag")) {
    const span = document.createElement("span");
    span.className = "flag none";
    span.textContent = img.alt;
    img.replaceWith(span);
  }
}, true);
let serverSort = (() => { try { return localStorage.getItem("server-sort") || "ping"; } catch { return "ping"; } })();
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
  // The backend speaks English; in English mode its messages are shown as is.
  if (I18N.lang === "en") return msg;
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
  const hidden = new Set(ui.hidden ?? []);
  document.querySelectorAll("[data-widget]").forEach((el) => { el.hidden = hidden.has(el.dataset.widget); });
  document.body.classList.toggle("no-spark", hidden.has("spark"));
  document.body.classList.toggle("no-hero-ping", hidden.has("hero-ping"));
}

// Blocks the user can switch off in Settings → Appearance.
const WIDGETS = [
  ["hero-ping", "Большой пинг на главной"],
  ["kpis", "Плитки: джиттер, потери, сравнение, соединения"],
  ["chart-ping", "График пинга"],
  ["chart-loss", "График потерь"],
  ["flows", "Таблица соединений"],
  ["trace", "Проверка маршрута"],
  ["countries", "Страны выхода на «Серверах»"],
  ["filters", "Фильтры на «Серверах»"],
  ["spark", "Мини-графики у серверов"],
];
function widgetOn(name) {
  return !(state?.settings.ui.hidden ?? []).includes(name);
}
function renderWidgetList() {
  const hidden = new Set(state.settings.ui.hidden ?? []);
  const html = WIDGETS.map(([id, label]) => `<label class="wchk"><input type="checkbox" data-w="${id}" ${hidden.has(id) ? "" : "checked"}><span>${label}</span></label>`).join("");
  if ($("widget-list").dataset.sig !== html) {
    $("widget-list").innerHTML = html;
    $("widget-list").dataset.sig = html;
  }
}
$("widget-list").addEventListener("change", async () => {
  const hidden = [...$("widget-list").querySelectorAll("input")].filter((i) => !i.checked).map((i) => i.dataset.w);
  await saveUi({ hidden });
});

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
function fmtDate(sec) { return sec ? new Date(sec * 1000).toLocaleDateString(I18N.lang === "en" ? "en-GB" : "ru-RU") : "—"; }
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
    if (b.dataset.page === "term" && window.Term) Term.focus();
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
  renderCompare();
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
  const legend = series.map((s) => `<span><i style="background:${s.color}"></i>${esc(displayName(s.key))}</span>`).join("");
  if ($("legend").innerHTML !== legend) $("legend").innerHTML = legend;
  chartSeries = { key, series };

  const rows = [
    ...flows.sort((a, b) => a.id - b.id).map((f) => {
      const d = (tick?.direct ?? {})[f.remote.split(":")[0]];
      const dTxt = d?.ms != null ? `${fmtMs(d.ms)} мс <span class="muted">±${d.jitter_ms.toFixed(0)}</span>` : d ? `<span class="muted">нет ответа</span>` : "—";
      const tTxt = f.rtt_ms != null ? `<span class="${f.rtt_ms <= (d?.ms ?? Infinity) ? "good" : ""}">${fmtMs(f.rtt_ms)} мс</span> <span class="muted">±${(f.rtt_jitter_ms ?? 0).toFixed(0)}</span>` : `<span class="muted" title="Поток матча: игра шлёт пакеты потоком, пинг видно в самой игре">поток</span>`;
      return `<tr><td><span class="proto udp">UDP</span></td><td class="mono">${esc(f.remote)} <button class="trace-btn" data-trace="${esc(f.remote.split(":")[0])}" title="Проверить маршрут">↯</button></td><td>${esc(f.exit ? displayName(f.exit) : "напрямую")}</td><td class="num">${dTxt}</td><td class="num">${tTxt}</td><td class="num">${f.packets_up}↑ ${f.packets_down}↓${f.duplicates ? ` <span class="muted">(+${f.duplicates})</span>` : ""}</td></tr>`;
    }),
    ...tcp.slice(0, 30).map((t) => `<tr><td><span class="proto tcp">TCP</span></td><td class="mono">${esc(t.remote)}</td><td>${esc(displayName(t.exit))}</td><td class="num">—</td><td class="num">—</td><td class="num">${fmtBytes(t.bytes_up)}↑ ${fmtBytes(t.bytes_down)}↓</td></tr>`),
  ];
  $("flows-count").textContent = tcp.length > 30 ? `показано 30 из ${tcp.length} TCP` : "";
  const html = rows.length ? rows.join("") : `<tr><td colspan="5" class="empty">Пока нет. Включите оптимизацию и запустите программу.</td></tr>`;
  if ($("flows").innerHTML !== html) $("flows").innerHTML = html;
}

// Average over game servers measured both ways: direct ICMP vs the game's own
// request/response timing through the tunnel.
function renderCompare() {
  const direct = tick?.direct ?? {};
  const pairs = (tick?.snapshot?.flows ?? [])
    .map((f) => [f, direct[f.remote.split(":")[0]]])
    .filter(([f, d]) => f.rtt_ms != null && d?.ms != null);
  if (!pairs.length) {
    setText($("kpi-compare"), "—");
    $("kpi-compare").className = "kpi-val";
    setText($("kpi-compare-sub"), Object.keys(direct).length ? "ждём, пока игра пропингует свои серверы" : "появится, когда игра начнёт пинговать свои серверы");
    return;
  }
  const avg = (xs) => xs.reduce((a, b) => a + b, 0) / xs.length;
  const dMs = avg(pairs.map(([, d]) => d.ms)), tMs = avg(pairs.map(([f]) => f.rtt_ms));
  const dJ = avg(pairs.map(([, d]) => d.jitter_ms)), tJ = avg(pairs.map(([f]) => f.rtt_jitter_ms ?? 0));
  setText($("kpi-compare"), `${fmtMs(dMs)} → ${fmtMs(tMs)} мс`);
  $("kpi-compare").className = `kpi-val ${tMs < dMs - 1 ? "good" : tMs > dMs + 5 ? "bad" : ""}`;
  setText($("kpi-compare-sub"), `джиттер ${dJ.toFixed(0)} → ${tJ.toFixed(0)} мс · серверов: ${pairs.length}`);
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

// Rows are created once per server and updated in place: rebuilding 50 rows with
// canvases every second made the page stutter and swallowed clicks.
const rowEls = new Map();

function serverRow(key) {
  const el = document.createElement("div");
  el.className = "srv";
  el.dataset.key = key;
  el.innerHTML = `
    <div class="dot"></div>
    <div style="min-width:0"><div class="name"><span class="fl"></span><span class="n"></span><span class="badge"></span></div><div class="sub"></div></div>
    <div class="metric m-ping"><span></span><small>пинг</small></div>
    <div class="metric m-jit"><span></span><small>джиттер</small></div>
    <div class="metric m-loss"><span></span><small>потери</small></div>
    <canvas class="spark"></canvas>
    <div class="srv-actions">
      <button class="btn icon" data-act="speed" title="Тест скорости">${icon("zap")}</button>
      <label class="switch"><input type="checkbox" data-act="enable"><span></span></label>
      <button class="btn" data-act="pick"></button>
    </div>`;
  el.querySelector('[data-act="speed"]').addEventListener("click", async (e) => {
    const b = e.currentTarget;
    if (b.classList.contains("busy")) return;
    b.classList.add("busy");
    toast(`Тест скорости: ${displayName(key)}…`);
    try {
      const r = await invoke("speed_test", { key });
      toast(`${displayName(key)}: ${r.mbps.toFixed(1)} Мбит/с · отклик ${Math.round(r.delay_ms)} мс`, "ok");
    } catch (err) {
      toast(`${displayName(key)}: ${ru(err)}`, "error");
    }
    b.classList.remove("busy");
  });
  el.querySelector('[data-act="enable"]').addEventListener("change", async (e) => {
    await call("set_server_enabled", { key, enabled: e.target.checked }, e.target.checked ? `${displayName(key)}: включён` : `${displayName(key)}: отключён`);
    await refreshState();
  });
  el.querySelector('[data-act="pick"]').addEventListener("click", async () => {
    const next = state.settings.preferred_exit === key ? null : key;
    await call("set_preferred", { key: next }, next ? `Новые соединения пойдут через ${displayName(key)}` : "Автовыбор сервера");
    await refreshState();
  });
  return el;
}

function setText(el, text) {
  if (el.textContent !== text) el.textContent = text;
}

function updateRow(el, r, p, pref) {
  const error = r.off ? null : state.server_errors[r.key];
  const ping = p?.alive ? p.srtt_ms : null;
  const selected = pref === r.key;
  const byFilter = !!r.off && r.off.startsWith("фильтр");
  el.querySelector('[data-act="speed"]').hidden = r.proto === "openlag" || !!r.off;
  el.className = `srv${selected ? " selected" : ""}${r.off ? " off" : ""}`;
  el.querySelector(".dot").className = `dot ${r.off ? "" : error ? "dead" : p?.alive ? "alive" : ""}`;
  setText(el.querySelector(".n"), r.name);
  const fl = el.querySelector(".fl");
  if (fl.dataset.cc !== (r.country ?? "")) {
    fl.dataset.cc = r.country ?? "";
    fl.innerHTML = flagHtml(r.country);
  }
  setText(el.querySelector(".badge"), r.proto);
  const sub = el.querySelector(".sub");
  const where = r.country ? `${countryName(r.country)} · выход ${r.exit_ip}${r.colo ? " (" + r.colo + ")" : ""} · ` : "";
  setText(sub, r.off ? `Отключён · ${r.off}` : error ? error : where + r.sub);
  sub.className = `sub${error ? " err-text" : ""}`;
  const mp = el.querySelector(".m-ping");
  mp.className = `metric m-ping ${pingClass(ping)}`;
  setText(mp.firstElementChild, r.off ? "—" : ping == null ? (error ? "ошибка" : p ? "нет ответа" : "…") : `${fmtMs(ping)} мс`);
  setText(el.querySelector(".m-jit span"), p?.alive ? p.jitter_ms.toFixed(1) : "—");
  setText(el.querySelector(".m-loss span"), p ? p.loss_pct.toFixed(0) + "%" : "—");
  const sw = el.querySelector(".switch");
  sw.hidden = r.proto === "openlag";
  sw.title = byFilter ? "Отключён фильтром: уберите слово в фильтрах выше" : "Включить / отключить сервер";
  const cb = sw.querySelector("input");
  cb.checked = !r.off;
  cb.disabled = byFilter;
  const btn = el.querySelector('[data-act="pick"]');
  btn.className = `btn${selected ? " on" : ""}`;
  btn.disabled = !!error || !!r.off;
  const label = selected ? `${icon("check")}Выбран` : "Выбрать";
  if (btn.dataset.label !== label) {
    btn.innerHTML = label;
    btn.dataset.label = label;
  }
  if (widgetOn("spark")) Chart.spark(el.querySelector("canvas"), (history.get(r.key) ?? []).slice(-40).map((h) => h.rtt), accentColor());
}

function renderServers() {
  const filter = $("server-filter").value.trim().toLowerCase();
  const ex = exits();
  const pref = state.settings.preferred_exit;
  $("auto-btn").classList.toggle("on", !pref);
  renderFilters();
  const rows = [
    ...state.servers.map((s) => ({ key: s.key, name: s.name, sub: `${s.subscription} · ${s.address}:${s.port}`, proto: s.protocol, off: s.disabled, country: s.country, exit_ip: s.exit_ip, colo: s.colo })),
    ...state.relay_names.map((n) => ({ key: n, name: n, sub: "свой relay", proto: "openlag", off: null, country: null })),
  ].filter((r) => !filter || (r.name + r.sub + (r.country ? countryName(r.country) : "")).toLowerCase().includes(filter));
  const pingOf = (r) => { const p = ex.get(r.key); return p?.alive ? p.srtt_ms : 1e9; };
  rows.sort((a, b) => {
    if (!!a.off !== !!b.off) return a.off ? 1 : -1;
    if (serverSort === "country") {
      const ca = a.country ? countryName(a.country) : "яяя", cb = b.country ? countryName(b.country) : "яяя";
      if (ca !== cb) return ca.localeCompare(cb, "ru");
    }
    return pingOf(a) - pingOf(b);
  });
  renderCountries(ex);

  // Ordered elements: optional country headers, then rows.
  const order = [];
  let lastGroup = null;
  for (const r of rows) {
    if (serverSort === "country") {
      const g = r.off ? "off" : r.country ?? "?";
      if (g !== lastGroup) {
        lastGroup = g;
        order.push(groupHeader(g));
      }
    }
    let el = rowEls.get(r.key);
    if (!el) rowEls.set(r.key, (el = serverRow(r.key)));
    updateRow(el, r, r.off ? null : ex.get(r.key), pref);
    order.push(el);
  }
  const list = $("server-list");
  $("server-empty").hidden = rows.length > 0;
  const keep = new Set(order);
  for (const el of [...list.children]) if (!keep.has(el)) el.remove();
  for (const [k, el] of rowEls) if (!el.isConnected && !keep.has(el)) rowEls.delete(k);
  order.forEach((el, i) => {
    // Move nodes only where the order changed.
    if (list.children[i] !== el) list.insertBefore(el, list.children[i] ?? null);
  });
}

const groupEls = new Map();
function groupHeader(g) {
  let el = groupEls.get(g);
  if (!el) {
    el = document.createElement("div");
    el.className = "srv-group";
    el.innerHTML = g === "off" ? `<span class="flag none">—</span>Отключённые` : g === "?" ? `${flagHtml(null)}Страна ещё не определена` : `${flagHtml(g)}${esc(countryName(g))}`;
    groupEls.set(g, el);
  }
  return el;
}

// Country chips: every measured exit country with its server count and best ping.
function renderCountries(ex) {
  const by = new Map();
  let unknown = 0;
  for (const s of state.servers) {
    if (!s.country) { if (!s.disabled) unknown++; continue; }
    const c = by.get(s.country) ?? { n: 0, on: 0, best: null };
    c.n++;
    if (!s.disabled) c.on++;
    const p = ex.get(s.key);
    if (p?.alive && (c.best == null || p.srtt_ms < c.best)) c.best = p.srtt_ms;
    by.set(s.country, c);
  }
  const disabled = new Set(state.settings.disabled_countries.map((c) => c.toUpperCase()));
  const autos = state.servers.filter((s) => s.auto).length;
  const chips = [...by].sort((a, b) => countryName(a[0]).localeCompare(countryName(b[0]), "ru")).map(([cc, c]) => {
    const off = disabled.has(cc);
    return `<button class="cchip${off ? " off" : ""}" data-cc="${cc}" title="${off ? "Включить" : "Отключить"} все серверы: ${esc(countryName(cc))}">${flagHtml(cc)}<b>${esc(countryName(cc))}</b><span>${c.n}</span>${c.best != null && !off ? `<i class="${pingClass(c.best)}">${fmtMs(c.best)} мс</i>` : ""}</button>`;
  });
  if (autos) {
    const off = state.settings.disable_auto;
    chips.unshift(`<button class="cchip auto${off ? " off" : ""}" data-auto="1" title="${off ? "Включить" : "Отключить"} все авто-серверы">${icon("zap")}<b>Авто-серверы</b><span>${autos}</span></button>`);
  }
  const html = chips.join("") || `<span class="hint">Страны появятся через несколько секунд после запуска серверов.</span>`;
  const box = $("countries");
  if (box.dataset.html !== html) {
    box.dataset.html = html;
    box.innerHTML = html;
    box.querySelectorAll("[data-cc]").forEach((b) => b.addEventListener("click", async () => {
      const cc = b.dataset.cc;
      const enable = disabled.has(cc);
      await call("set_country_enabled", { country: cc, enabled: enable }, `${countryName(cc)}: ${enable ? "включена" : "отключена"}`);
      await refreshState();
    }));
    box.querySelector("[data-auto]")?.addEventListener("click", async () => {
      const off = !state.settings.disable_auto;
      await call("set_disable_auto", { disabled: off }, off ? "Авто-серверы отключены" : "Авто-серверы включены");
      await refreshState();
    });
  }
  $("geo-note").textContent = unknown ? `· не определено: ${unknown}` : "";
}

document.querySelectorAll("#sort-seg button").forEach((b) => {
  b.classList.toggle("active", b.dataset.sort === serverSort);
  b.addEventListener("click", () => {
    serverSort = b.dataset.sort;
    try { localStorage.setItem("server-sort", serverSort); } catch { /* ignore */ }
    document.querySelectorAll("#sort-seg button").forEach((x) => x.classList.toggle("active", x === b));
    renderServers();
  });
});
$("geo-refresh").addEventListener("click", async () => {
  await call("refresh_geo", {}, "Перепроверяю страны выхода…");
});

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

$("sub-clip").addEventListener("click", async () => {
  let text = "";
  try { text = await navigator.clipboard.readText(); } catch { /* denied */ }
  if (!text.trim()) return toast("В буфере обмена нет ссылки или ключей", "error");
  $("sub-input").value = text.trim();
  $("sub-add").click();
});
// "Add to OpenLag" buttons on subscription pages: openlag://import?url=...
function importLink(text) {
  document.querySelector('.nav[data-page="subs"]').click();
  $("sub-input").value = text;
  toast("Ссылка из браузера: проверьте её и нажмите «Добавить»", "ok");
}
window.__TAURI__.event.listen("deep-link", (e) => importLink(e.payload));
$("sub-file").addEventListener("click", async () => {
  try {
    const text = await call("pick_config_file");
    if (text == null) return;
    $("sub-input").value = text;
    $("sub-add").click();
  } catch { /* toast */ }
});
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
        ${state.settings.mode === "only" ? `<label class="group-exit"><span>Сервер группы</span><select data-act="exit">${groupExitOptions(g.exit)}</select></label>
        <label class="group-udp" title="Через VPN идёт только UDP, то есть сама игра. Лаунчер, загрузки и чат по TCP остаются напрямую на полной скорости"><input type="checkbox" data-act="udp" ${g.udp_only ? "checked" : ""}><span>Только UDP (игра), TCP напрямую</span></label>` : ""}
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
    el.querySelector('[data-act="exit"]')?.addEventListener("change", (e) => saveSelection(edit((g) => { g.exit = e.target.value || null; })));
    el.querySelector('[data-act="udp"]')?.addEventListener("change", (e) => saveSelection(edit((g) => { g.udp_only = e.target.checked; })));
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

// Servers a group can be pinned to: enabled ones, grouped by measured country.
function groupExitOptions(current) {
  const opts = [`<option value="">Как выбрано в «Серверах» (${state.settings.preferred_exit ? esc(displayName(state.settings.preferred_exit)) : "авто"})</option>`];
  const servers = state.servers.filter((s) => !s.disabled).sort((a, b) => (a.country ? countryName(a.country) : "яя").localeCompare(b.country ? countryName(b.country) : "яя", "ru") || a.name.localeCompare(b.name));
  for (const s of servers) {
    const where = s.country ? `${countryName(s.country)} · ` : "";
    opts.push(`<option value="${esc(s.key)}" ${s.key === current ? "selected" : ""}>${esc(where + s.name)}</option>`);
  }
  for (const r of state.relay_names) opts.push(`<option value="${esc(r)}" ${r === current ? "selected" : ""}>Свой relay · ${esc(r)}</option>`);
  if (current && !servers.some((s) => s.key === current) && !state.relay_names.includes(current)) {
    opts.push(`<option value="${esc(current)}" selected>Недоступен: ${esc(displayName(current))}</option>`);
  }
  return opts.join("");
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
    const hosts = s.ssh_hosts ?? [];
    const keep = $("rd-host").value;
    $("rd-host").innerHTML = hosts.length
      ? hosts.map((h) => `<option value="${esc(h.id)}">${esc(h.name)} — ${esc(h.host)}</option>`).join("")
      : `<option value="">Сначала добавьте сервер во вкладке «Терминал»</option>`;
    if (hosts.some((h) => h.id === keep)) $("rd-host").value = keep;
    $("rd-deploy").disabled = $("rd-remove").disabled = !hosts.length;
    $("opt-autostart").checked = s.autostart;
    $("opt-minimized").checked = s.start_minimized;
    $("opt-tray").checked = s.close_to_tray;
    $("opt-kill").checked = s.kill_switch;
    $("opt-hotkey").value = s.hotkey;
    $("opt-boost").checked = s.boost_priority;
    const c = s.conn;
    $("conn-fp").value = c.fingerprint;
    $("conn-frag").checked = c.fragment;
    $("conn-frag-packets").value = c.frag_packets;
    $("conn-frag-length").value = c.frag_length;
    $("conn-frag-interval").value = c.frag_interval;
    $("conn-mux").value = String(c.mux);
    $("conn-dns").value = c.dns;
    $("conn-probe").value = c.probe_target;
    $("conn-frag-row").hidden = !c.fragment;
    $("opt-update").checked = s.auto_update;
    $("opt-probe").value = String(s.probe_interval_ms);
    $("ui-chart").value = String(s.ui.chart_seconds);
    $("ui-good").value = s.ui.ping_good;
    $("ui-mid").value = s.ui.ping_mid;
    $("ui-compact").checked = s.ui.compact;
    settingsFilled = true;
  }
  $("ui-lang").value = I18N.lang;
  renderWidgetList();
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
$("ui-lang").addEventListener("change", async (e) => {
  await saveUi({ lang: e.target.value });
  I18N.set(e.target.value);
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
$("conn-frag").addEventListener("change", (e) => ($("conn-frag-row").hidden = !e.target.checked));
$("conn-save").addEventListener("click", async () => {
  const conn = {
    fingerprint: $("conn-fp").value,
    fragment: $("conn-frag").checked,
    frag_packets: $("conn-frag-packets").value.trim() || "tlshello",
    frag_length: $("conn-frag-length").value.trim() || "100-200",
    frag_interval: $("conn-frag-interval").value.trim() || "10-20",
    mux: Number($("conn-mux").value),
    dns: $("conn-dns").value,
    probe_target: $("conn-probe").value,
  };
  try { await call("set_conn", { conn }, "Применено: серверы перезапущены"); } catch { /* toast */ }
  await refreshState();
});
async function saveExtras() {
  try {
    await call("set_extras", { hotkey: $("opt-hotkey").value, boostPriority: $("opt-boost").checked }, "Сохранено");
  } catch { /* toast */ }
  await refreshState();
}
$("opt-hotkey").addEventListener("change", saveExtras);
$("opt-boost").addEventListener("change", saveExtras);
$("backup-export").addEventListener("click", async () => {
  try { if (await invoke("export_settings")) toast("Резервная копия сохранена", "ok"); } catch (e) { toast(e, "error"); }
});
$("backup-import").addEventListener("click", async () => {
  try {
    if (await invoke("import_settings")) { toast("Настройки восстановлены", "ok"); await refreshState(); }
  } catch (e) { toast(e, "error"); }
});
$("opt-kill").addEventListener("change", async (e) => {
  try {
    await call("set_kill_switch", { on: e.target.checked }, e.target.checked ? "Kill switch включён: без сервера трафик выбранных программ блокируется" : "Kill switch выключен");
  } catch { e.target.checked = !e.target.checked; }
  await refreshState();
});
function relayTask(cmd, title) {
  const hostId = $("rd-host").value;
  if (!hostId) return;
  const name = $("rd-name").value.trim();
  const port = Number($("rd-port").value) || 7000;
  if (port < 1024 || port > 65535) { toast("Порт должен быть от 1024 до 65535", "error"); return; }
  document.querySelector('.nav[data-page="term"]').click();
  Term.openTask(title, () => invoke(cmd, { hostId, name, port }));
}
$("rd-deploy").addEventListener("click", () => relayTask("relay_deploy", "Установка relay"));
$("rd-remove").addEventListener("click", () => relayTask("relay_remove", "Удаление relay"));
window.__TAURI__.event.listen("relays-changed", async () => {
  await refreshState();
  toast("Relay-серверы обновлены");
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
  if (page === "page-term" && window.Term) Term.render(state);
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
    // Skip a server's first seconds: they include connection setup through the
    // proxy (often 500+ ms) and would squash the chart for two minutes.
    const seen = (warmup.get(k) ?? 0) + (p.alive ? 1 : 0);
    warmup.set(k, seen);
    const rtt = p.alive && seen > 3 ? p.last_rtt_ms ?? p.srtt_ms : null;
    h.push({ rtt, loss: seen > 3 ? p.loss_pct : null });
    if (h.length > HISTORY_MAX) h.shift();
    history.set(k, h);
  }
  lastTickAt = performance.now();
  const page = document.querySelector(".page.active").id;
  if (page === "page-home") renderHome();
  if (page === "page-servers") renderServers();
});

// ---------------------------------------------------------------- live charts

let chartSeries = null;
let lastTickAt = performance.now();
let lastFrame = 0;
function frame(now) {
  requestAnimationFrame(frame);
  // ~40 fps is plenty for a chart and keeps the CPU cool.
  if (now - lastFrame < 24 || document.hidden || !$("page-home").classList.contains("active") || !chartSeries) return;
  if (!widgetOn("chart-ping") && !widgetOn("chart-loss")) return;
  lastFrame = now;
  const phase = Math.min(1, (now - lastTickAt) / 1000);
  const { key, series } = chartSeries;
  if (widgetOn("chart-ping")) Chart.line($("chart-ping"), series, { points: HISTORY, minMax: 20, phase });
  if (widgetOn("chart-loss")) Chart.line($("chart-loss"), key ? [{ color: "#ef4444", values: (history.get(key) ?? []).map((h) => h.loss) }] : [], { points: HISTORY, minMax: 5, phase, smoothing: false });
}
requestAnimationFrame(frame);

// ---------------------------------------------------------------- window

const appWindow = window.__TAURI__.window.getCurrentWindow();
$("tb-min").addEventListener("click", () => appWindow.minimize());
$("tb-max").addEventListener("click", () => appWindow.toggleMaximize());
$("tb-close").addEventListener("click", () => appWindow.close());
$("titlebar").addEventListener("dblclick", (e) => { if (!e.target.closest("button")) appWindow.toggleMaximize(); });

window.addEventListener("resize", renderAll);
Fun.init();
// The window is created hidden and shown after the first render: no white flash and
// no half-drawn page, like a native app.
refreshState().finally(async () => {
  let page = await invoke("start_page").catch(() => null);
  // Development aid: OPENLAG_START_PAGE=en/servers previews English without saving it.
  if (page?.startsWith("en/")) { I18N.preview("en"); page = page.slice(3); }
  // The saved language wins; switching reloads the page before it is shown.
  else if (state?.settings.ui.lang) I18N.sync(state.settings.ui.lang);
  else if (state) saveUi({ lang: I18N.lang }).catch(() => {});
  I18N.start();
  const link = await invoke("take_deep_link").catch(() => null);
  if (link) importLink(link);
  if (page) document.querySelector(`.nav[data-page="${page.split(":")[0]}"]`)?.click();
  // Development aid: OPENLAG_START_PAGE=term:powershell also opens a shell.
  const shell = page?.split(":")[1];
  if (shell) document.querySelector(`[data-shell="${shell}"]`)?.click();
  // ...and OPENLAG_START_PAGE=settings:#rd-host scrolls to an element.
  if (shell?.startsWith("#")) document.querySelector(shell)?.scrollIntoView({ block: "center" });
  // ...and OPENLAG_START_PAGE=home:trace=1.1.1.1 runs a route check.
  if (shell?.startsWith("trace=")) { await runTrace(shell.slice(6)); $("trace-table").scrollIntoView({ block: "center" }); }
  invoke("ui_ready");
});
// Pick up background changes (auto-refresh, xray restarts).
setInterval(refreshState, 15000);

// ---------------------------------------------------------------- route trace

async function runTrace(target) {
  target = target.trim();
  if (!target) return;
  $("trace-target").value = target;
  const btn = $("trace-run");
  btn.disabled = true;
  btn.textContent = "Проверяю…";
  $("trace-note").textContent = "";
  try {
    const r = await invoke("trace_route", { target });
    const cls = (h) => (h.sent && h.lost === h.sent ? "bad" : h.lost ? "mid" : "");
    $("trace-body").innerHTML = r.hops.map((h) => `<tr>
      <td class="num">${h.ttl}</td>
      <td class="mono">${h.ip ? esc(h.ip) : '<span class="muted">* узел не отвечает</span>'}</td>
      <td class="num ${cls(h)}">${h.sent ? Math.round((h.lost / h.sent) * 100) : 0}%</td>
      <td class="num">${fmtMs(h.best_ms)}</td>
      <td class="num ${pingClass(h.avg_ms)}">${fmtMs(h.avg_ms)}</td>
      <td class="num">${fmtMs(h.worst_ms)}</td></tr>`).join("");
    $("trace-table").hidden = false;
    $("trace-note").textContent = `${r.ip} · ${r.reached ? "сервер ответил" : "сервер не отвечает на ICMP"}`;
  } catch (e) {
    toast(ru(e), "error");
  }
  btn.disabled = false;
  btn.textContent = "Проверить маршрут";
}
$("trace-run").addEventListener("click", () => runTrace($("trace-target").value));
$("trace-target").addEventListener("keydown", (e) => { if (e.key === "Enter") runTrace(e.target.value); });
$("flows").addEventListener("click", (e) => {
  const b = e.target.closest("[data-trace]");
  if (b) runTrace(b.dataset.trace);
});
