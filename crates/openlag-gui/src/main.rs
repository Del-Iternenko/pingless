#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod settings;
mod sys;
mod tray;
mod update;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use openlag_capture::{Capture, CaptureConfig, Selection};
use openlag_core::{Downstream, DownstreamFn, Engine, EngineConfig, ProxySpec, Snapshot};
use openlag_proxy::{FetchOptions, Skipped, Xray, XrayOptions};
use serde::Serialize;
use settings::{ProcessGroup, Settings, Subscription};
use tauri::{Emitter, Manager, State};
use tracing::{info, warn};

struct AppState {
    settings: parking_lot::Mutex<Settings>,
    settings_path: PathBuf,
    work_dir: PathBuf,
    xray_exe: Option<PathBuf>,
    net: tokio::sync::Mutex<Net>,
    /// Where the engine delivers answers; set while capture runs.
    sink: Arc<parking_lot::RwLock<Option<DownstreamFn>>>,
}

#[derive(Default)]
struct Net {
    xray: Option<Xray>,
    engine: Option<Arc<Engine>>,
    capture: Option<Capture>,
    server_errors: HashMap<String, String>,
    error: Option<String>,
    /// Adapter xray is bound to, if any.
    bind_interface: Option<String>,
    /// Capture was started by auto-connect (and may be stopped by it).
    auto_started: bool,
    /// The user switched capture off while a listed program ran; auto-connect waits
    /// until those programs exit before acting again.
    suppress_auto: bool,
}

type Shared<'a> = State<'a, Arc<AppState>>;
type CmdResult<T> = Result<T, String>;

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

// ---------------------------------------------------------------------------
// State sent to the UI

#[derive(Serialize)]
struct ServerView {
    key: String,
    name: String,
    subscription: String,
    protocol: String,
    address: String,
    port: u16,
    /// Why the server is off (manual or keyword filter); None if it is on.
    disabled: Option<String>,
}

#[derive(Serialize)]
struct StateView {
    settings: Settings,
    servers: Vec<ServerView>,
    relay_names: Vec<String>,
    server_errors: HashMap<String, String>,
    net_error: Option<String>,
    admin: bool,
    windivert: bool,
    xray: bool,
    capturing: bool,
    bind_interface: Option<String>,
    version: &'static str,
}

#[derive(Serialize, Clone)]
struct CaptureView {
    tcp_redirected: u64,
    tunneled: u64,
    direct: u64,
    injected: u64,
    inject_errors: u64,
}

#[derive(Serialize, Clone)]
struct Tick {
    snapshot: Option<Snapshot>,
    capture: Option<CaptureView>,
    capturing: bool,
}

impl AppState {
    async fn view(&self) -> StateView {
        let settings = self.settings.lock().clone();
        let mut servers = Vec::new();
        for sub in settings.subscriptions.iter().filter(|s| s.enabled) {
            for srv in &sub.servers {
                let key = sub.server_key(srv);
                servers.push(ServerView {
                    disabled: settings.server_disabled(&key, srv),
                    key,
                    name: srv.name.clone(),
                    subscription: sub.name.clone(),
                    protocol: srv.protocol.clone(),
                    address: srv.address.clone(),
                    port: srv.port,
                });
            }
        }
        let relay_names = settings::parse_relays(&settings.relays_toml)
            .map(|r| r.exits.into_iter().map(|e| e.name).collect())
            .unwrap_or_default();
        let net = self.net.lock().await;
        let exe_dir = exe_dir();
        StateView {
            settings,
            servers,
            relay_names,
            server_errors: net.server_errors.clone(),
            net_error: net.error.clone(),
            admin: sys::is_admin(),
            windivert: exe_dir.join("WinDivert.dll").exists()
                && exe_dir.join("WinDivert64.sys").exists(),
            xray: self.xray_exe.is_some(),
            capturing: net.capture.is_some(),
            bind_interface: net.bind_interface.clone(),
            version: update::CURRENT,
        }
    }

    fn save(&self) -> Result<()> {
        settings::save(&self.settings_path, &self.settings.lock())
    }

    /// Restarts xray and the engine from the current settings, keeping capture on if
    /// it was on.
    async fn rebuild(&self) {
        let mut net = self.net.lock().await;
        let was_capturing = net.capture.is_some();
        self.stop_capture(&mut net);
        net.engine = None;
        net.xray = None;
        net.server_errors.clear();
        net.error = None;
        if let Err(e) = self.build(&mut net).await {
            warn!("network setup failed: {e:#}");
            net.error = Some(format!("{e:#}"));
        }
        if was_capturing && let Err(e) = self.start_capture(&mut net) {
            net.error = Some(format!("{e:#}"));
        }
    }

    async fn build(&self, net: &mut Net) -> Result<()> {
        let s = self.settings.lock().clone();
        let relays = settings::parse_relays(&s.relays_toml)?;

        let mut keys = Vec::new();
        let mut servers = Vec::new();
        for sub in s.subscriptions.iter().filter(|s| s.enabled) {
            for srv in &sub.servers {
                let key = sub.server_key(srv);
                if !keys.contains(&key) && s.server_disabled(&key, srv).is_none() {
                    keys.push(key);
                    servers.push(srv.clone());
                }
            }
        }
        let mut proxies = Vec::new();
        if !servers.is_empty() {
            let exe = self
                .xray_exe
                .as_ref()
                .context("xray.exe is missing next to OpenLag")?;
            net.bind_interface = if s.bypass_tun {
                openlag_proxy::physical_interface().map(|i| i.name)
            } else {
                None
            };
            let opts = XrayOptions {
                bind_interface: net.bind_interface.clone(),
            };
            let xray = Xray::start_with(exe, &self.work_dir.join("xray"), &servers, &opts).await?;
            for (i, key) in keys.iter().enumerate() {
                match (xray.socks[i], &xray.errors[i]) {
                    (Some(socks), _) => proxies.push(ProxySpec {
                        name: key.clone(),
                        socks,
                    }),
                    (None, Some(e)) => {
                        net.server_errors.insert(key.clone(), e.clone());
                    }
                    _ => {}
                }
            }
            net.xray = Some(xray);
        }
        if proxies.is_empty() && relays.exits.is_empty() {
            return Ok(());
        }
        let sink = self.sink.clone();
        let downstream: DownstreamFn = Arc::new(move |d: Downstream<'_>| {
            if let Some(f) = sink.read().as_ref() {
                f(d);
            }
        });
        let cfg = EngineConfig {
            psk: relays.psk,
            exits: relays.exits,
            proxies,
            preferred_exit: s.preferred_exit.clone(),
            multipath: s.multipath.max(1),
            probe_interval_ms: s.probe_interval_ms.clamp(100, 5000),
            switch_margin_ms: 5.0,
            flow_idle_secs: 120,
        };
        net.engine = Some(Arc::new(Engine::start(cfg, downstream).await?));
        Ok(())
    }

    fn start_capture(&self, net: &mut Net) -> Result<()> {
        if net.capture.is_some() {
            return Ok(());
        }
        let s = self.settings.lock().clone();
        let processes = s.selected_processes();
        anyhow::ensure!(
            s.all_except() || !processes.is_empty(),
            "select at least one process"
        );
        let engine = net
            .engine
            .clone()
            .context("add a subscription or a relay first")?;
        anyhow::ensure!(sys::is_admin(), "capture needs Administrator rights");
        // TCP is redirected to a local listener; Windows Firewall must let it in.
        if let Err(e) = sys::ensure_firewall_rule() {
            warn!("cannot add firewall rule: {e:#}");
        }
        let mut capture = Capture::open(&CaptureConfig {
            processes,
            mode: if s.all_except() {
                Selection::AllExcept
            } else {
                Selection::Only
            },
            tunnel_existing: s.tunnel_existing,
            // xray carries the tunnel itself and must never be captured.
            exclude_pids: net
                .xray
                .as_ref()
                .and_then(|x| x.pid())
                .into_iter()
                .collect(),
        })?;
        *self.sink.write() = Some(capture.downstream());
        capture.start(engine);
        net.capture = Some(capture);
        info!("capture started");
        Ok(())
    }

    fn stop_capture(&self, net: &mut Net) {
        if let Some(mut c) = net.capture.take() {
            c.stop();
            info!("capture stopped");
        }
        *self.sink.write() = None;
    }

    async fn shutdown(&self) {
        let mut net = self.net.lock().await;
        self.stop_capture(&mut net);
        net.engine = None;
        net.xray = None;
    }

    async fn set_preferred_exit(&self, key: Option<String>) {
        self.settings.lock().preferred_exit = key.clone();
        let _ = self.save();
        if let Some(engine) = &self.net.lock().await.engine {
            engine.set_preferred_exit(key);
        }
    }

    /// Switches capture on or off (the tray's "Optimisation" item).
    async fn toggle_capture(&self) -> Result<()> {
        let mut net = self.net.lock().await;
        net.auto_started = false;
        if net.capture.is_some() {
            net.suppress_auto = self.settings.lock().auto_connect;
            self.stop_capture(&mut net);
            Ok(())
        } else {
            net.suppress_auto = false;
            self.start_capture(&mut net)
        }
    }

    /// Alive exits for the tray menu, best ping first, with display names.
    fn tray_servers(&self, snapshot: &Snapshot) -> Vec<tray::TrayServer> {
        let s = self.settings.lock();
        let name = |key: &str| {
            s.subscriptions
                .iter()
                .flat_map(|sub| {
                    sub.servers
                        .iter()
                        .map(move |srv| (sub.server_key(srv), srv.name.clone()))
                })
                .find(|(k, _)| k == key)
                .map(|(_, n)| n)
                .unwrap_or_else(|| key.to_string())
        };
        let mut best: HashMap<&str, Option<f64>> = HashMap::new();
        for p in &snapshot.paths {
            let ping = if p.alive { p.srtt_ms } else { None };
            let e = best.entry(p.exit.as_str()).or_insert(None);
            if ping.is_some() && (e.is_none() || ping < *e) {
                *e = ping;
            }
        }
        let mut v: Vec<tray::TrayServer> = best
            .into_iter()
            .map(|(k, ping)| tray::TrayServer {
                key: k.to_string(),
                label: name(k),
                ping_ms: ping,
            })
            .collect();
        v.sort_by(|a, b| {
            a.ping_ms
                .unwrap_or(f64::MAX)
                .total_cmp(&b.ping_ms.unwrap_or(f64::MAX))
        });
        v
    }

    fn fetch_options(&self) -> FetchOptions {
        let s = self.settings.lock();
        FetchOptions {
            user_agent: s.user_agent.clone(),
            hwid: Some(s.hwid.clone()),
        }
    }
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_default()
}

/// Our xray ships as `openlag-xray.exe`: other VPN clients (Happ) kill every process
/// named `xray.exe` when they disconnect, which took our tunnel down with theirs.
fn find_xray(resource_dir: Option<PathBuf>) -> Option<PathBuf> {
    let mut dirs = vec![exe_dir()];
    dirs.extend(resource_dir);
    let mut candidates: Vec<PathBuf> = dirs
        .iter()
        .flat_map(|d| [d.join("openlag-xray.exe"), d.join("xray.exe")])
        .collect();
    if cfg!(debug_assertions) {
        candidates
            .push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/xray/xray.exe"));
    }
    candidates.into_iter().find(|p| p.exists())
}

// ---------------------------------------------------------------------------
// Commands

#[tauri::command]
async fn get_state(st: Shared<'_>) -> CmdResult<StateView> {
    Ok(st.view().await)
}

#[derive(Serialize)]
struct AddResult {
    name: String,
    added: usize,
    skipped: Vec<Skipped>,
}

#[tauri::command]
async fn add_subscription(st: Shared<'_>, input: String) -> CmdResult<AddResult> {
    let input = input.trim().to_string();
    if input.is_empty() {
        return Err("paste a subscription link or keys".into());
    }
    let is_url = ["http://", "https://", "happ://"]
        .iter()
        .any(|p| input.starts_with(p));
    let (parsed, info, url) = if is_url {
        let (p, i) = openlag_proxy::fetch(&input, &st.fetch_options())
            .await
            .map_err(err)?;
        (p, i, Some(input.clone()))
    } else {
        (openlag_proxy::parse_text(&input), Default::default(), None)
    };
    if parsed.servers.is_empty() {
        let reason = parsed
            .skipped
            .first()
            .map(|s| s.reason.clone())
            .unwrap_or_else(|| "no servers found".into());
        return Err(reason);
    }
    let name = info
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| url.as_deref().and_then(host_of))
        .or_else(|| (parsed.servers.len() == 1).then(|| parsed.servers[0].name.clone()))
        .unwrap_or_else(|| "Мои ключи".into());
    let result = AddResult {
        name: name.clone(),
        added: parsed.servers.len(),
        skipped: parsed.skipped.clone(),
    };
    st.settings.lock().subscriptions.push(Subscription {
        id: format!("{:08x}", rand::random::<u32>()),
        name,
        url,
        enabled: true,
        servers: parsed.servers,
        skipped: parsed.skipped,
        info,
        updated_at: settings::now(),
    });
    st.save().map_err(err)?;
    st.rebuild().await;
    Ok(result)
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    Some(rest.split(['/', '?', ':']).next()?.to_string()).filter(|h| !h.is_empty())
}

/// Re-downloads one subscription. Returns false if it has no URL.
async fn refresh_one(st: &AppState, id: &str) -> Result<bool> {
    let url = {
        let s = st.settings.lock();
        let sub = s
            .subscriptions
            .iter()
            .find(|s| s.id == id)
            .context("subscription not found")?;
        match &sub.url {
            Some(u) => u.clone(),
            None => return Ok(false),
        }
    };
    let (parsed, info) = openlag_proxy::fetch(&url, &st.fetch_options()).await?;
    anyhow::ensure!(
        !parsed.servers.is_empty(),
        "the subscription returned no usable servers"
    );
    let mut s = st.settings.lock();
    if let Some(sub) = s.subscriptions.iter_mut().find(|s| s.id == id) {
        sub.servers = parsed.servers;
        sub.skipped = parsed.skipped;
        if let Some(t) = info.title.clone().filter(|t| !t.trim().is_empty()) {
            sub.name = t;
        }
        sub.info = info;
        sub.updated_at = settings::now();
    }
    Ok(true)
}

#[tauri::command]
async fn refresh_subscription(st: Shared<'_>, id: String) -> CmdResult<()> {
    refresh_one(&st, &id).await.map_err(err)?;
    st.save().map_err(err)?;
    st.rebuild().await;
    Ok(())
}

#[tauri::command]
async fn remove_subscription(st: Shared<'_>, id: String) -> CmdResult<()> {
    st.settings.lock().subscriptions.retain(|s| s.id != id);
    st.save().map_err(err)?;
    st.rebuild().await;
    Ok(())
}

#[tauri::command]
async fn toggle_subscription(st: Shared<'_>, id: String, enabled: bool) -> CmdResult<()> {
    if let Some(s) = st
        .settings
        .lock()
        .subscriptions
        .iter_mut()
        .find(|s| s.id == id)
    {
        s.enabled = enabled;
    }
    st.save().map_err(err)?;
    st.rebuild().await;
    Ok(())
}

#[tauri::command]
async fn set_preferred(st: Shared<'_>, key: Option<String>) -> CmdResult<()> {
    st.set_preferred_exit(key).await;
    st.save().map_err(err)
}

#[tauri::command]
async fn set_selection(
    st: Shared<'_>,
    groups: Vec<ProcessGroup>,
    exclude_groups: Vec<ProcessGroup>,
    mode: String,
    auto_connect: bool,
) -> CmdResult<()> {
    let groups = clean_groups(groups);
    let exclude_groups = clean_groups(exclude_groups);
    {
        let mut s = st.settings.lock();
        s.groups = groups;
        s.exclude_groups = exclude_groups;
        s.auto_connect = auto_connect;
        s.mode = if mode == "all_except" {
            mode
        } else {
            "only".into()
        };
    }
    st.save().map_err(err)?;
    // Capture reads the selection at start.
    let mut net = st.net.lock().await;
    if net.capture.is_some() {
        st.stop_capture(&mut net);
        st.start_capture(&mut net).map_err(err)?;
    }
    Ok(())
}

fn clean_groups(groups: Vec<ProcessGroup>) -> Vec<ProcessGroup> {
    groups
        .into_iter()
        .map(|mut g| {
            let mut p: Vec<String> = g
                .processes
                .iter()
                .map(|x| exe_name(x))
                .filter(|x| !x.is_empty())
                .collect();
            p.sort();
            p.dedup();
            g.processes = p;
            if g.id.is_empty() {
                g.id = settings::new_id();
            }
            g
        })
        .collect()
}

/// `C:\Games\Foo\Foo.exe` or `Foo` -> `foo.exe`.
fn exe_name(s: &str) -> String {
    let name = s
        .trim()
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if name.is_empty() || name.ends_with(".exe") {
        name
    } else {
        format!("{name}.exe")
    }
}

#[tauri::command]
async fn pick_exe(app: tauri::AppHandle) -> CmdResult<Vec<String>> {
    use tauri_plugin_dialog::DialogExt;
    let files = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_title("Выберите программы")
            .add_filter("Программы", &["exe"])
            .blocking_pick_files()
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(files
        .unwrap_or_default()
        .into_iter()
        .filter_map(|f| f.into_path().ok())
        .map(|p| exe_name(&p.to_string_lossy()))
        .collect())
}

#[tauri::command]
async fn set_options(
    st: Shared<'_>,
    multipath: usize,
    tunnel_existing: bool,
    user_agent: String,
    bypass_tun: bool,
) -> CmdResult<()> {
    {
        let mut s = st.settings.lock();
        s.bypass_tun = bypass_tun;
        s.multipath = multipath.clamp(1, 4);
        s.tunnel_existing = tunnel_existing;
        if !user_agent.trim().is_empty() {
            s.user_agent = user_agent.trim().to_string();
        }
    }
    st.save().map_err(err)?;
    st.rebuild().await;
    Ok(())
}

/// Switches one server on or off. Off servers are not started at all.
#[tauri::command]
async fn set_server_enabled(st: Shared<'_>, key: String, enabled: bool) -> CmdResult<()> {
    {
        let mut s = st.settings.lock();
        s.disabled_servers.retain(|k| k != &key);
        if !enabled {
            s.disabled_servers.push(key.clone());
            if s.preferred_exit.as_deref() == Some(key.as_str()) {
                s.preferred_exit = None;
            }
        }
    }
    st.save().map_err(err)?;
    st.rebuild().await;
    Ok(())
}

/// Words that switch off every matching server (name or address).
#[tauri::command]
async fn set_filters(st: Shared<'_>, keywords: Vec<String>) -> CmdResult<()> {
    let mut k: Vec<String> = keywords
        .into_iter()
        .map(|w| w.trim().to_string())
        .filter(|w| !w.is_empty())
        .collect();
    k.sort_by_key(|w| w.to_lowercase());
    k.dedup_by_key(|w| w.to_lowercase());
    st.settings.lock().disabled_keywords = k;
    st.save().map_err(err)?;
    st.rebuild().await;
    Ok(())
}

#[tauri::command]
async fn set_ui_prefs(st: Shared<'_>, ui: settings::UiPrefs) -> CmdResult<()> {
    st.settings.lock().ui = ui;
    st.save().map_err(err)
}

#[tauri::command]
async fn set_startup(
    st: Shared<'_>,
    autostart: bool,
    start_minimized: bool,
    probe_ms: u64,
    close_to_tray: bool,
    auto_update: bool,
) -> CmdResult<()> {
    {
        let mut s = st.settings.lock();
        s.close_to_tray = close_to_tray;
        s.auto_update = auto_update;
    }
    let rebuild = {
        let mut s = st.settings.lock();
        s.start_minimized = start_minimized;
        let changed = s.probe_interval_ms != probe_ms;
        s.probe_interval_ms = probe_ms.clamp(100, 5000);
        changed
    };
    sys::set_autostart(autostart).map_err(err)?;
    st.settings.lock().autostart = autostart;
    st.save().map_err(err)?;
    if rebuild {
        st.rebuild().await;
    }
    Ok(())
}

#[tauri::command]
async fn set_relays(st: Shared<'_>, text: String) -> CmdResult<()> {
    settings::parse_relays(&text).map_err(err)?;
    st.settings.lock().relays_toml = text;
    st.save().map_err(err)?;
    st.rebuild().await;
    Ok(())
}

#[tauri::command]
async fn list_processes() -> CmdResult<Vec<String>> {
    tauri::async_runtime::spawn_blocking(sys::process_names)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn start(st: Shared<'_>) -> CmdResult<()> {
    let mut net = st.net.lock().await;
    net.suppress_auto = false;
    net.auto_started = false;
    st.start_capture(&mut net).map_err(err)
}

#[tauri::command]
async fn stop(st: Shared<'_>) -> CmdResult<()> {
    let mut net = st.net.lock().await;
    // Don't let auto-connect switch it right back on.
    net.suppress_auto = st.settings.lock().auto_connect;
    net.auto_started = false;
    st.stop_capture(&mut net);
    Ok(())
}

#[tauri::command]
async fn relaunch_admin(app: tauri::AppHandle, st: Shared<'_>) -> CmdResult<()> {
    // Release WinDivert and xray first; the new instance starts its own.
    st.shutdown().await;
    if !sys::relaunch_as_admin() {
        st.rebuild().await;
        return Err("Administrator rights were not granted".into());
    }
    app.exit(0);
    Ok(())
}

// ---------------------------------------------------------------------------

fn main() {
    init_log();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let settings_path = data_dir.join("settings.json");
            let settings = settings::load(&settings_path);
            let state = Arc::new(AppState {
                settings: parking_lot::Mutex::new(settings),
                settings_path,
                work_dir: data_dir,
                xray_exe: find_xray(app.path().resource_dir().ok()),
                net: tokio::sync::Mutex::new(Net::default()),
                sink: Arc::default(),
            });
            let _ = state.save(); // persist the generated device id
            app.manage(state.clone());
            if let Err(e) = tray::create(app.handle()) {
                warn!("tray icon: {e}");
            }
            let minimized = state.settings.lock().start_minimized
                || std::env::args().any(|a| a == "--minimized");
            if minimized && let Some(w) = app.get_webview_window("main") {
                let _ = w.minimize();
            }

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                state.rebuild().await;
                tauri::async_runtime::spawn(auto_refresh(state.clone()));
                tauri::async_runtime::spawn(auto_connect(state.clone(), handle.clone()));
                tauri::async_runtime::spawn(auto_update_check(state.clone(), handle.clone()));
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    // Skip a tick while a rebuild holds the lock.
                    let Ok(mut net) = state.net.try_lock() else {
                        continue;
                    };
                    // Watchdog: if xray was killed from outside, bring everything back.
                    if net.xray.as_mut().is_some_and(|x| !x.is_running()) {
                        warn!("xray exited unexpectedly, restarting");
                        drop(net);
                        state.rebuild().await;
                        continue;
                    }
                    let tick = Tick {
                        snapshot: net.engine.as_ref().map(|e| e.snapshot()),
                        capture: net.capture.as_ref().map(|c| {
                            let s = c.stats();
                            CaptureView {
                                tcp_redirected: s.tcp_redirected,
                                tunneled: s.tunneled,
                                direct: s.direct,
                                injected: s.injected,
                                inject_errors: s.inject_errors,
                            }
                        }),
                        capturing: net.capture.is_some(),
                    };
                    drop(net);
                    if let Some(snap) = &tick.snapshot {
                        let servers = state.tray_servers(snap);
                        let preferred = state.settings.lock().preferred_exit.clone();
                        tray::update(&handle, tick.capturing, preferred.as_deref(), &servers);
                    }
                    let _ = handle.emit("tick", tick);
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // The close button hides to the tray unless the user picked "Exit" there.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event
                && !tray::QUITTING.load(std::sync::atomic::Ordering::SeqCst)
                && window
                    .app_handle()
                    .state::<Arc<AppState>>()
                    .settings
                    .lock()
                    .close_to_tray
            {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            add_subscription,
            refresh_subscription,
            remove_subscription,
            toggle_subscription,
            set_preferred,
            set_selection,
            pick_exe,
            set_options,
            set_relays,
            set_server_enabled,
            set_filters,
            set_ui_prefs,
            set_startup,
            list_processes,
            start,
            stop,
            relaunch_admin,
            check_update,
            install_update,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build OpenLag")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // Close WinDivert handles and stop xray before the process ends.
                let st = app.state::<Arc<AppState>>().inner().clone();
                tauri::async_runtime::block_on(st.shutdown());
            }
        });
}

/// Refreshes subscriptions whose update interval has passed, on start and every hour.
async fn auto_refresh(st: Arc<AppState>) {
    loop {
        let due: Vec<String> = {
            let s = st.settings.lock();
            let now = settings::now();
            s.subscriptions
                .iter()
                .filter(|sub| sub.url.is_some())
                .filter(|sub| {
                    let hours = sub.info.update_interval_hours.unwrap_or(12).max(1) as u64;
                    now.saturating_sub(sub.updated_at) >= hours * 3600
                })
                .map(|sub| sub.id.clone())
                .collect()
        };
        let mut changed = false;
        for id in due {
            match refresh_one(&st, &id).await {
                Ok(true) => changed = true,
                Ok(false) => {}
                Err(e) => warn!("auto-refresh of subscription {id} failed: {e:#}"),
            }
        }
        if changed {
            let _ = st.save();
            st.rebuild().await;
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

/// Starts capture when a program from the `only` list starts, and stops it 15 s
/// after the last one exits (only if auto-connect started it).
async fn auto_connect(st: Arc<AppState>, handle: tauri::AppHandle) {
    let mut last_running = std::time::Instant::now() - Duration::from_secs(3600);
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let (enabled, wanted) = {
            let s = st.settings.lock();
            (s.auto_connect && !s.all_except(), s.selected_processes())
        };
        if !enabled || wanted.is_empty() {
            continue;
        }
        let running = tauri::async_runtime::spawn_blocking(sys::process_names)
            .await
            .unwrap_or_default();
        let found = wanted.iter().find(|w| running.contains(w)).cloned();
        let Ok(mut net) = st.net.try_lock() else {
            continue;
        };
        match found {
            Some(name) => {
                last_running = std::time::Instant::now();
                if net.capture.is_none()
                    && !net.suppress_auto
                    && net.engine.is_some()
                    && sys::is_admin()
                {
                    match st.start_capture(&mut net) {
                        Ok(()) => {
                            net.auto_started = true;
                            let _ = handle.emit("auto", format!("Запущен {name}: туннель включён"));
                        }
                        Err(e) => {
                            net.error = Some(format!("{e:#}"));
                            net.suppress_auto = true;
                        }
                    }
                }
            }
            None => {
                net.suppress_auto = false;
                if net.capture.is_some()
                    && net.auto_started
                    && last_running.elapsed() > Duration::from_secs(15)
                {
                    st.stop_capture(&mut net);
                    net.auto_started = false;
                    let _ = handle.emit(
                        "auto",
                        "Программы из списка закрыты: туннель выключен".to_string(),
                    );
                }
            }
        }
    }
}

/// Logs to `%APPDATA%\dev.openlag.app\openlag.log` (the release build has no console).
/// The file starts over when it grows past 5 MB.
fn init_log() {
    let filter =
        || tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let path = std::env::var_os("APPDATA")
        .map(|a| PathBuf::from(a).join("dev.openlag.app").join("openlag.log"));
    let file = path.and_then(|p| {
        std::fs::create_dir_all(p.parent()?).ok()?;
        let big = std::fs::metadata(&p).is_ok_and(|m| m.len() > 5 * 1024 * 1024);
        std::fs::OpenOptions::new()
            .create(true)
            .append(!big)
            .write(true)
            .truncate(big)
            .open(&p)
            .ok()
    });
    match file {
        Some(f) => tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_target(false)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(f))
            .init(),
        None => tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_target(false)
            .init(),
    }
    std::panic::set_hook(Box::new(|p| tracing::error!("panic: {p}")));
}

#[tauri::command]
async fn check_update() -> CmdResult<update::UpdateInfo> {
    update::check().await.map_err(err)
}

/// Downloads the new installer, releases WinDivert and xray, and hands over to it.
#[tauri::command]
async fn install_update(app: tauri::AppHandle, st: Shared<'_>) -> CmdResult<()> {
    let info = update::check().await.map_err(err)?;
    if !info.available {
        return Err(format!("already up to date ({})", info.current));
    }
    let setup = update::download(&app, &info).await.map_err(err)?;
    info!("installing update {} from {}", info.latest, setup.display());
    st.shutdown().await;
    std::process::Command::new(&setup)
        .arg("--update")
        .spawn()
        .map_err(|e| format!("cannot start the installer: {e}"))?;
    tray::QUITTING.store(true, std::sync::atomic::Ordering::SeqCst);
    app.exit(0);
    Ok(())
}

/// Checks GitHub for a newer release 20 s after start and then every 6 hours.
async fn auto_update_check(st: Arc<AppState>, handle: tauri::AppHandle) {
    tokio::time::sleep(Duration::from_secs(20)).await;
    loop {
        if st.settings.lock().auto_update {
            match update::check().await {
                Ok(info) if info.available => {
                    info!("update available: {} -> {}", info.current, info.latest);
                    let _ = handle.emit("update-available", info);
                }
                Ok(_) => {}
                Err(e) => tracing::debug!("update check: {e:#}"),
            }
        }
        tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
    }
}
