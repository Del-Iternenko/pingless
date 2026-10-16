// "Терминал" tab: local shells and SSH sessions to the user's servers, on xterm.js.
"use strict";

const Term = (() => {
  const { invoke } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;
  const $ = (id) => document.getElementById(id);
  const sessions = new Map(); // id -> { term, fit, pane, tab, title, alive }
  let active = null;
  let editing = null; // ssh host id being edited, or "" for a new one

  const theme = () => {
    const accent = getComputedStyle(document.documentElement).getPropertyValue("--accent").trim() || "#10b981";
    return {
      background: "#0b0f17", foreground: "#e6eaf2", cursor: accent, cursorAccent: "#0b0f17",
      selectionBackground: accent + "55",
      black: "#1f2a40", red: "#ef4444", green: "#10b981", yellow: "#f59e0b", blue: "#6366f1", magenta: "#ec4899", cyan: "#22d3ee", white: "#e6eaf2",
      brightBlack: "#5b6478", brightRed: "#f87171", brightGreen: "#34d399", brightYellow: "#fbbf24", brightBlue: "#818cf8", brightMagenta: "#f472b6", brightCyan: "#67e8f9", brightWhite: "#ffffff",
    };
  };

  function decode(b64) {
    const bin = atob(b64);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  }

  async function open(kind, arg, title) {
    const pane = document.createElement("div");
    pane.className = "term-pane";
    $("term-panes").appendChild(pane);
    $("term-empty").hidden = true;
    const term = new Terminal({
      fontFamily: '"Cascadia Mono", Consolas, monospace',
      fontSize: 13.5,
      cursorBlink: true,
      scrollback: 5000,
      allowProposedApi: false,
      theme: theme(),
    });
    const fit = new FitAddon.FitAddon();
    term.loadAddon(fit);
    term.open(pane);
    fit.fit();
    let id;
    try {
      id = kind === "local"
        ? await invoke("term_open_local", { shell: arg, cols: term.cols, rows: term.rows })
        : kind === "task"
          ? await arg()
          : await invoke("term_open_ssh", { id: arg, cols: term.cols, rows: term.rows });
    } catch (e) {
      pane.remove();
      term.dispose();
      if (!sessions.size) $("term-empty").hidden = false;
      toast(`Не удалось открыть терминал: ${e}`, "error");
      return;
    }
    const tab = document.createElement("button");
    tab.className = "term-tab";
    tab.innerHTML = `<span></span><i aria-label="Закрыть">×</i>`;
    tab.firstElementChild.textContent = title;
    tab.addEventListener("click", (e) => {
      if (e.target.tagName === "I") close(id);
      else activate(id);
    });
    $("term-tabs").appendChild(tab);
    term.onData((data) => invoke("term_write", { id, data }).catch(() => {}));
    term.onResize(({ cols, rows }) => invoke("term_resize", { id, cols, rows }).catch(() => {}));
    // Ctrl+Shift+C / Ctrl+Shift+V like other terminals; Ctrl+C stays an interrupt.
    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== "keydown" || !e.ctrlKey || !e.shiftKey) return true;
      if (e.code === "KeyC") { navigator.clipboard.writeText(term.getSelection()); return false; }
      if (e.code === "KeyV") { navigator.clipboard.readText().then((t) => invoke("term_write", { id, data: t })); return false; }
      return true;
    });
    sessions.set(id, { term, fit, pane, tab, title, alive: true });
    activate(id);
  }

  function activate(id) {
    active = id;
    for (const [sid, s] of sessions) {
      s.pane.hidden = sid !== id;
      s.tab.classList.toggle("active", sid === id);
    }
    const s = sessions.get(id);
    if (s) {
      requestAnimationFrame(() => { s.fit.fit(); s.term.focus(); });
    }
  }

  function close(id) {
    const s = sessions.get(id);
    if (!s) return;
    if (s.alive) invoke("term_close", { id });
    s.term.dispose();
    s.pane.remove();
    s.tab.remove();
    sessions.delete(id);
    if (active === id) {
      const next = [...sessions.keys()].pop();
      if (next != null) activate(next);
      else { active = null; $("term-empty").hidden = false; }
    }
  }

  listen("term-data", (e) => sessions.get(e.payload.id)?.term.write(decode(e.payload.data)));
  listen("term-exit", (e) => {
    const s = sessions.get(e.payload.id);
    if (!s) return;
    s.alive = false;
    s.tab.classList.add("ended");
    s.term.write(`\r\n\x1b[90m— ${e.payload.reason}. ${I18N.lang === "en" ? "Close the tab or open a new session." : "Закройте вкладку или откройте новую сессию."}\x1b[0m\r\n`);
  });

  new ResizeObserver(() => {
    const s = sessions.get(active);
    if (s && !s.pane.hidden && $("page-term").classList.contains("active")) s.fit.fit();
  }).observe($("term-panes"));

  // ---------------------------------------------------------------- saved servers

  function renderHosts(hosts) {
    $("ssh-list").innerHTML = hosts.length
      ? hosts.map((h) => `<div class="ssh-host" data-id="${esc(h.id)}">
          <button class="ssh-connect" title="Подключиться">${icon("server")}<span><b>${esc(h.name)}</b><small>${esc(h.user)}@${esc(h.host)}:${h.port}</small></span></button>
          <button class="btn icon" data-act="edit" aria-label="Изменить">${icon("sliders")}</button>
        </div>`).join("")
      : `<p class="hint">Добавьте свой VPS, чтобы подключаться к нему по SSH прямо отсюда.</p>`;
    $("ssh-list").querySelectorAll(".ssh-host").forEach((el) => {
      const h = hosts.find((x) => x.id === el.dataset.id);
      el.querySelector(".ssh-connect").addEventListener("click", () => open("ssh", h.id, h.name));
      el.querySelector('[data-act="edit"]').addEventListener("click", () => showForm(h));
    });
  }

  function showForm(h) {
    editing = h?.id ?? "";
    $("ssh-form").hidden = false;
    $("ssh-form-title").textContent = h ? `Сервер «${h.name}»` : "Новый сервер";
    $("ssh-name").value = h?.name ?? "";
    $("ssh-host").value = h?.host ?? "";
    $("ssh-port").value = h?.port ?? 22;
    $("ssh-user").value = h?.user ?? "root";
    $("ssh-auth").value = h?.auth ?? "password";
    $("ssh-key").value = h?.key_path ?? "";
    $("ssh-secret").value = "";
    $("ssh-secret").placeholder = h?.secret ? "сохранён (оставьте пустым, чтобы не менять)" : "пароль";
    $("ssh-delete").hidden = !h;
    $("ssh-forget").hidden = !h;
    syncAuth();
  }

  function syncAuth() {
    const key = $("ssh-auth").value === "key";
    $("ssh-key-row").hidden = !key;
    $("ssh-secret-label").textContent = key ? "Пароль от ключа (если есть)" : "Пароль";
  }

  $("ssh-auth").addEventListener("change", syncAuth);
  $("ssh-add").addEventListener("click", () => showForm(null));
  $("ssh-cancel").addEventListener("click", () => ($("ssh-form").hidden = true));
  $("ssh-key-browse").addEventListener("click", async () => {
    const p = await invoke("pick_key_file");
    if (p) $("ssh-key").value = p;
  });
  $("ssh-save").addEventListener("click", async () => {
    const secret = $("ssh-secret").value;
    const input = {
      id: editing || null,
      name: $("ssh-name").value,
      host: $("ssh-host").value,
      port: Number($("ssh-port").value) || 22,
      user: $("ssh-user").value,
      auth: $("ssh-auth").value,
      key_path: $("ssh-key").value,
      secret: secret || editing === "" ? secret : null,
    };
    try {
      await invoke("ssh_save", { input });
      $("ssh-form").hidden = true;
      toast("Сервер сохранён", "ok");
      await refreshState();
    } catch (e) { toast(e, "error"); }
  });
  $("ssh-delete").addEventListener("click", async () => {
    await invoke("ssh_delete", { id: editing });
    $("ssh-form").hidden = true;
    await refreshState();
  });
  $("ssh-forget").addEventListener("click", async () => {
    await invoke("ssh_forget_key", { host: $("ssh-host").value, port: Number($("ssh-port").value) || 22 });
    toast("Ключ сервера забыт: при следующем подключении будет запомнен новый", "ok");
  });

  document.querySelectorAll("[data-shell]").forEach((b) =>
    b.addEventListener("click", () => open("local", b.dataset.shell, b.dataset.title)),
  );
  $("term-credit").addEventListener("click", (e) => {
    e.preventDefault();
    invoke("open_url", { url: "https://github.com/xtermjs/xterm.js" });
  });

  return {
    render(state) { renderHosts(state.settings.ssh_hosts ?? []); },
    // Output-only tab of a background job; `start` resolves to its terminal id.
    openTask(title, start) { return open("task", start, title); },
    focus() { const s = sessions.get(active); if (s) requestAnimationFrame(() => { s.fit.fit(); s.term.focus(); }); },
  };
})();
