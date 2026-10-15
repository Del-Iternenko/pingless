"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const win = window.__TAURI__.window.getCurrentWindow();
const $ = (id) => document.getElementById(id);

const INSTALL_STEPS = [
  ["welcome", "Приветствие"],
  ["license", "Соглашение"],
  ["options", "Параметры"],
  ["progress", "Установка"],
  ["done", "Готово"],
];
const UPDATE_STEPS = [
  ["progress", "Обновление"],
  ["done", "Готово"],
];
const UNINSTALL_STEPS = [
  ["uninstall", "Удаление"],
  ["progress", "Процесс"],
  ["done", "Готово"],
];
const TIPS = [
  "Совет: включите «Автоподключение» на вкладке «Процессы», и туннель будет подниматься сам, как только вы запустите игру.",
  "Совет: фильтры серверов отключают целые страны одним словом, например «Russia», даже после обновления подписки.",
  "Совет: multipath отправляет каждый пакет по нескольким маршрутам, и потери на одном из них игра просто не заметит.",
  "Совет: режим «Всё, кроме выбранных» пустит через VPN весь компьютер, кроме программ из списка.",
  "Совет: включён Happ или другой VPN в режиме TUN? OpenLag обойдёт его, чтобы трафик не шёл через два VPN подряд.",
  "Совет: на вкладке «Фан» есть змейка. Просто чтобы было.",
];

let info = null;
let steps = INSTALL_STEPS;
let current = 0;

function showStep(name) {
  current = steps.findIndex(([s]) => s === name);
  document.querySelectorAll(".step").forEach((el) => el.classList.toggle("active", el.dataset.step === name));
  $("steps").innerHTML = steps
    .map(([, title], i) => `<li class="${i === current ? "active" : i < current ? "past" : ""}"><i>${i < current ? "✓" : i + 1}</i>${title}</li>`)
    .join("");
}

function showError(msg) {
  const e = $("error");
  e.textContent = msg;
  e.hidden = false;
  setTimeout(() => (e.hidden = true), 9000);
}

// ---------------------------------------------------------------- music

const music = $("music");
music.volume = 0.45;
function syncPlayer() {
  document.body.classList.toggle("playing", !music.paused && !music.muted);
  document.body.classList.toggle("muted-state", music.muted);
}
async function startMusic() {
  try { await music.play(); } catch { /* blocked until a click */ }
  syncPlayer();
}

// A music.mp3 next to the installer wins over the built-in track.
async function loadMusic() {
  try {
    const buf = await invoke("external_music");
    if (buf && buf.byteLength > 0) {
      music.src = URL.createObjectURL(new Blob([buf], { type: "audio/mpeg" }));
      $("player").hidden = false;
    }
  } catch { /* keep the built-in one */ }
}
music.addEventListener("play", syncPlayer);
music.addEventListener("pause", syncPlayer);
music.addEventListener("error", () => ($("player").hidden = true));
music.addEventListener("loadedmetadata", () => { if (!music.duration) $("player").hidden = true; });
$("mute").addEventListener("click", () => {
  if (music.paused) { music.muted = false; startMusic(); }
  else music.muted = !music.muted;
  syncPlayer();
});
$("vol").addEventListener("input", (e) => {
  music.volume = e.target.value / 100;
  music.muted = music.volume === 0;
  if (music.paused && music.volume > 0) startMusic();
  syncPlayer();
});
// Autoplay is allowed by the window's browser args; the first click is a fallback.
document.addEventListener("click", () => { if (music.paused && !music.muted) startMusic(); }, { once: true });

// ---------------------------------------------------------------- window

$("btn-min").addEventListener("click", () => win.minimize());
$("btn-close").addEventListener("click", () => {
  if (steps[current][0] === "progress") return; // never leave a half-written install
  win.close();
});
document.querySelectorAll("[data-url='github']").forEach((el) =>
  el.addEventListener("click", (e) => { e.preventDefault(); invoke("open_url", { url: info.github }); }),
);

// ---------------------------------------------------------------- install flow

document.querySelectorAll("[data-next]").forEach((b) => b.addEventListener("click", () => showStep(steps[current + 1][0])));
document.querySelectorAll("[data-back]").forEach((b) => b.addEventListener("click", () => showStep(steps[current - 1][0])));
$("accept").addEventListener("change", (e) => ($("accept-next").disabled = !e.target.checked));

function updateSpace() {
  const mb = info.core_mb + ($("opt-extras").checked ? info.extras_mb : 0);
  $("space").textContent = `Потребуется на диске: ${mb.toFixed(0)} МБ` + (info.installed_dir ? " · найдена прежняя установка, она будет обновлена" : "");
}
$("opt-extras").addEventListener("change", (e) => {
  updateSpace();
  if (!e.target.checked) showError("Без дополнительного ПО OpenLag не сможет перехватывать трафик: ставьте только если WinDivert и xray уже лежат в папке программы.");
});
$("browse").addEventListener("click", async () => {
  const dir = await invoke("pick_dir", { current: $("dir").value });
  if (dir) $("dir").value = dir;
});

let tipTimer = null;
function startTips() {
  let i = Math.floor(Math.random() * TIPS.length);
  const show = () => { $("tip").textContent = TIPS[i++ % TIPS.length]; $("tip").style.animation = "none"; void $("tip").offsetWidth; $("tip").style.animation = ""; };
  show();
  tipTimer = setInterval(show, 5000);
}

listen("progress", (e) => {
  const { pct, text } = e.payload;
  $("pct").textContent = `${Math.floor(pct)}%`;
  $("bar").style.width = `${pct}%`;
  $("file").textContent = text;
});

$("go").addEventListener("click", async () => {
  const opts = {
    dir: $("dir").value.trim(),
    desktop: $("opt-desktop").checked,
    start_menu: $("opt-start").checked,
    extras: $("opt-extras").checked,
    autostart: $("opt-autostart").checked,
  };
  if (!opts.dir) return showError("Укажите папку установки");
  showStep("progress");
  startTips();
  try {
    await invoke("install", { opts });
    clearInterval(tipTimer);
    showStep("done");
    $("finish").onclick = async () => {
      if ($("opt-launch").checked) {
        try { await invoke("launch", { dir: opts.dir }); } catch (e) { showError(String(e)); return; }
      }
      win.close();
    };
  } catch (e) {
    clearInterval(tipTimer);
    showStep("options");
    showError(`Установка не удалась: ${e}`);
  }
});

// ---------------------------------------------------------------- update flow

async function runUpdate() {
  $("progress-title").textContent = `Обновление до ${info.version}…`;
  showStep("progress");
  startTips();
  const opts = {
    dir: info.installed_dir,
    desktop: info.keep_desktop,
    start_menu: info.keep_start_menu,
    extras: true,
    autostart: info.keep_autostart,
  };
  try {
    await invoke("install", { opts });
    clearInterval(tipTimer);
    $("done-title").textContent = `OpenLag обновлён до ${info.version}`;
    $("done-text").textContent = "Настройки, подписки и группы на месте. OpenLag сейчас запустится.";
    showStep("done");
    $("finish").onclick = () => win.close();
    setTimeout(async () => {
      try { await invoke("launch", { dir: opts.dir }); } catch (e) { showError(String(e)); return; }
      win.close();
    }, 2500);
  } catch (e) {
    clearInterval(tipTimer);
    showError(`Обновление не удалось: ${e}`);
    steps = INSTALL_STEPS;
    $("dir").value = info.default_dir;
    updateSpace();
    showStep("options");
  }
}

// ---------------------------------------------------------------- uninstall flow

$("cancel-un").addEventListener("click", () => win.close());
$("go-un").addEventListener("click", async () => {
  const removeSettings = $("rm-settings").checked;
  $("progress-title").textContent = "Удаление…";
  showStep("progress");
  $("tip").textContent = "Спасибо, что попробовали OpenLag. Будем рады issue и звёздочке на GitHub.";
  try {
    await invoke("uninstall", { removeSettings });
    $("done-title").textContent = "OpenLag удалён";
    $("done-text").textContent = removeSettings ? "Программа, ярлыки и настройки удалены." : "Программа и ярлыки удалены, настройки и подписки сохранены на случай, если вернётесь.";
    showStep("done");
    $("finish").onclick = () => win.close();
  } catch (e) {
    showStep("uninstall");
    showError(`Удаление не удалось: ${e}`);
  }
});

// ---------------------------------------------------------------- background

(function background() {
  const c = $("bg");
  const ctx = c.getContext("2d");
  const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;
  let w, h, pts;
  function resize() {
    const dpr = devicePixelRatio || 1;
    w = c.clientWidth; h = c.clientHeight;
    c.width = w * dpr; c.height = h * dpr;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    // Packets streaming from the left towards one exit node on the right, like the logo.
    pts = Array.from({ length: 70 }, () => spawn(true));
  }
  function spawn(anywhere) {
    const lane = Math.floor(Math.random() * 5);
    return { x: anywhere ? Math.random() * w : -10, y0: h * (0.18 + lane * 0.16), t: Math.random(), s: 0.6 + Math.random() * 1.6, hue: Math.random() < 0.7 ? 160 : 240 };
  }
  const target = () => ({ x: w * 0.82, y: h * 0.52 });
  function frame() {
    ctx.fillStyle = "rgba(7, 9, 15, 0.22)";
    ctx.fillRect(0, 0, w, h);
    const g = ctx.createRadialGradient(w * 0.82, h * 0.52, 0, w * 0.82, h * 0.52, h * 0.5);
    g.addColorStop(0, "rgba(16, 185, 129, 0.10)");
    g.addColorStop(1, "rgba(16, 185, 129, 0)");
    ctx.fillStyle = g;
    ctx.fillRect(0, 0, w, h);
    const tg = target();
    for (const p of pts) {
      p.x += p.s;
      const k = Math.min(1, Math.max(0, (p.x - w * 0.25) / (tg.x - w * 0.25)));
      const y = p.y0 + (tg.y - p.y0) * k * k;
      ctx.fillStyle = `hsla(${p.hue}, 80%, 60%, ${0.25 + 0.5 * k})`;
      ctx.beginPath();
      ctx.arc(p.x, y, 1.2 + k * 1.3, 0, Math.PI * 2);
      ctx.fill();
      if (p.x > tg.x) Object.assign(p, spawn(false));
    }
    if (!reduce) requestAnimationFrame(frame);
  }
  addEventListener("resize", resize);
  resize();
  frame();
})();

// ---------------------------------------------------------------- start

(async () => {
  info = await invoke("info");
  $("ver").textContent = `v${info.version}`;
  await loadMusic();
  if (info.mode === "uninstall") {
    steps = UNINSTALL_STEPS;
    showStep("uninstall");
  } else if (info.mode === "update") {
    steps = UPDATE_STEPS;
    runUpdate();
  } else {
    $("dir").value = info.default_dir;
    updateSpace();
    showStep("welcome");
  }
  startMusic();
})();
