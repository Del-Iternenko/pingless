#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod compare;
#[macro_use]
mod i18n;
mod geo;
mod relay;
mod secret;
mod settings;
mod speed;
mod sys;
mod term;
mod trace;
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
    /// Measured exit country of each server, persisted between runs.
    geo: parking_lot::Mutex<geo::GeoCache>,
    geo_path: PathBuf,
    app: std::sync::OnceLock<tauri::AppHandle>,
    /// Direct (no tunnel) ping to the game servers currently tunnelled.
    compare: parking_lot::Mutex<compare::Comparator>,
    terminals: Arc<term::Terminals>,
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
    /// Local SOCKS port of each subscription server, by key.
    socks: HashMap<String, std::net::SocketAddr>,
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
    /// Measured exit: country code, IP and nearest Cloudflare data centre.
    country: Option<String>,
    exit_ip: Option<String>,
    colo: Option<String>,
    auto: bool,
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
    blocked: u64,
}

#[derive(Serialize, Clone)]
struct Tick {
    snapshot: Option<Snapshot>,
    capture: Option<CaptureView>,
    capturing: bool,
    /// Game server IP -> direct ping without OpenLag.
    direct: HashMap<String, compare::Direct>,
}

impl AppState {
    async fn view(&self) -> StateView {
        let settings = self.settings.lock().clone();
        // The guard must not live across the await below (parking_lot guards are !Send).
        let servers = {
            let geo = self.geo.lock();
            let mut servers = Vec::new();
            for sub in settings.subscriptions.iter().filter(|s| s.enabled) {
                for srv in &sub.servers {
                    let key = sub.server_key(srv);
                    let g = geo.servers.get(&key);
                    servers.push(ServerView {
                        disabled: settings.server_disabled(
                            &key,
                            srv,
                            g.map(|g| g.country.as_str()),
                        ),
                        country: g.map(|g| g.country.clone()),
                        exit_ip: g.map(|g| g.ip.clone()),
                        colo: g.map(|g| g.colo.clone()),
                        auto: settings::is_auto(&srv.name),
                        key,
                        name: srv.name.clone(),
                        subscription: sub.name.clone(),
                        protocol: srv.protocol.clone(),
                        address: srv.address.clone(),
                        port: srv.port,
                    });
                }
            }
            servers
        };
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
                let country = self.geo.lock().servers.get(&key).map(|g| g.country.clone());
                if !keys.contains(&key)
                    && s.server_disabled(&key, srv, country.as_deref()).is_none()
                {
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
            let c = &s.conn;
            let opts = XrayOptions {
                bind_interface: net.bind_interface.clone(),
                fingerprint: Some(c.fingerprint.clone()).filter(|f| !f.is_empty()),
                fragment: c.fragment.then(|| openlag_proxy::Fragment {
                    packets: c.frag_packets.clone(),
                    length: c.frag_length.clone(),
                    interval: c.frag_interval.clone(),
                }),
                mux_concurrency: c.mux.min(128),
                dns: Some(c.dns.clone()).filter(|d| !d.is_empty()),
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
            proxy_probe_target: s.conn.probe_target.parse().ok(),
            multipath: s.multipath.max(1),
            probe_interval_ms: s.probe_interval_ms.clamp(100, 5000),
            switch_margin_ms: 5.0,
            flow_idle_secs: 120,
        };
        let probe: Vec<(String, std::net::SocketAddr)> = cfg
            .proxies
            .iter()
            .map(|p| (p.name.clone(), p.socks))
            .collect();
        net.engine = Some(Arc::new(Engine::start(cfg, downstream).await?));
        net.socks = probe.iter().cloned().collect();
        self.spawn_geo_probe(probe, false);
        Ok(())
    }

    /// Measures exit countries of servers whose cached result is missing or stale
    /// (or all of them with `force`), a few at a time, then tells the UI.
    fn spawn_geo_probe(&self, servers: Vec<(String, std::net::SocketAddr)>, force: bool) {
        let Some(app) = self.app.get().cloned() else {
            return;
        };
        let now = settings::now();
        let todo: Vec<_> = {
            let cache = self.geo.lock();
            servers
                .into_iter()
                .filter(|(k, _)| {
                    force
                        || cache
                            .servers
                            .get(k)
                            .is_none_or(|g| now.saturating_sub(g.checked_at) > geo::STALE_SECS)
                })
                .collect()
        };
        if todo.is_empty() {
            return;
        }
        tauri::async_runtime::spawn(async move {
            // Give xray a moment to open its ports.
            tokio::time::sleep(Duration::from_secs(2)).await;
            let st = app.state::<Arc<AppState>>().inner().clone();
            // Up to three rounds: a busy server or a slow first XHTTP handshake can
            // time out once and answer fine a little later.
            let mut todo = todo;
            for round in 0..3 {
                if round > 0 {
                    tokio::time::sleep(Duration::from_secs(20)).await;
                }
                let mut failed = Vec::new();
                for chunk in todo.chunks(6) {
                    let results =
                        futures_util::future::join_all(chunk.iter().map(|(k, socks)| async move {
                            (k.clone(), *socks, geo::probe(*socks).await)
                        }))
                        .await;
                    let mut cache = st.geo.lock();
                    for (k, socks, r) in results {
                        match r {
                            Ok(g) => {
                                cache.servers.insert(k, g);
                            }
                            Err(e) => {
                                tracing::debug!("geo {k}: {e:#}");
                                failed.push((k, socks));
                            }
                        }
                    }
                    cache.save(&st.geo_path);
                    drop(cache);
                    let _ = app.emit("refresh", ());
                }
                if failed.is_empty() {
                    break;
                }
                todo = failed;
            }
        });
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
            // In "only" mode each enabled group may pin its programs to a server.
            exit_for: if s.all_except() {
                HashMap::new()
            } else {
                s.groups
                    .iter()
                    .filter(|g| g.enabled)
                    .filter_map(|g| g.exit.as_ref().map(|e| (g, e)))
                    .flat_map(|(g, e)| g.processes.iter().map(move |p| (p.clone(), e.clone())))
                    .collect()
            },
            kill_switch: s.kill_switch,
            udp_only: if s.all_except() {
                Vec::new()
            } else {
                s.groups
                    .iter()
                    .filter(|g| g.enabled && g.udp_only)
                    .flat_map(|g| g.processes.iter().cloned())
                    .collect()
            },
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
        self.terminals.close_all();
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

/// A WireGuard `.conf`, a list of links or an Amnezia key saved to a file.
#[tauri::command]
async fn pick_config_file(app: tauri::AppHandle) -> CmdResult<Option<String>> {
    use tauri_plugin_dialog::DialogExt;
    let file = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_title(tr!(
                "Файл с ключами или конфигом",
                "File with keys or a config"
            ))
            .add_filter(tr!("Конфиги", "Configs"), &["conf", "txt", "json", "vpn"])
            .add_filter(tr!("Все файлы", "All files"), &["*"])
            .blocking_pick_file()
    })
    .await
    .map_err(|e| e.to_string())?;
    let Some(path) = file.and_then(|f| f.into_path().ok()) else {
        return Ok(None);
    };
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if meta.len() > 4 << 20 {
        return Err(tr!(
            "файл больше 4 МБ: это не конфиг",
            "the file is over 4 MB: not a config"
        ));
    }
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

#[tauri::command]
async fn pick_exe(app: tauri::AppHandle) -> CmdResult<Vec<String>> {
    use tauri_plugin_dialog::DialogExt;
    let files = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_title(tr!("Выберите программы", "Choose programs"))
            .add_filter(tr!("Программы", "Programs"), &["exe"])
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
    i18n::set(&ui.lang);
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
    let args: Vec<String> = std::env::args().collect();
    // After "restart as administrator" the old instance is still exiting.
    if let Some(pid) = args
        .iter()
        .position(|a| a == "--relaunch")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
    {
        sys::wait_for_pid(pid, 10_000);
    }
    if let Some(link) = args.iter().find_map(|a| deep_link(a)) {
        *PENDING_LINK.lock() = Some(link);
    }

    let mut builder = tauri::Builder::default();
    // A second launch (or an openlag:// link) brings up the running one. Dev builds
    // skip it unless asked: they would hand over to an installed, elevated OpenLag.
    if !cfg!(debug_assertions) || std::env::var_os("OPENLAG_SINGLE_INSTANCE").is_some() {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            tray::show_window(app);
            if let Some(link) = argv.iter().find_map(|a| deep_link(a)) {
                let _ = app.emit("deep-link", link);
            }
        }));
    }
    builder
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let settings_path = data_dir.join("settings.json");
            let settings = settings::load(&settings_path);
            i18n::set(&settings.ui.lang);
            let state = Arc::new(AppState {
                settings: parking_lot::Mutex::new(settings),
                settings_path,
                work_dir: data_dir.clone(),
                xray_exe: find_xray(app.path().resource_dir().ok()),
                net: tokio::sync::Mutex::new(Net::default()),
                sink: Arc::default(),
                geo: parking_lot::Mutex::new(geo::GeoCache::load(&data_dir.join("geo.json"))),
                geo_path: data_dir.join("geo.json"),
                app: std::sync::OnceLock::new(),
                compare: parking_lot::Mutex::default(),
                terminals: Arc::default(),
            });
            let _ = state.app.set(app.handle().clone());
            let _ = state.save(); // persist the generated device id
            app.manage(state.clone());
            if let Err(e) = tray::create(app.handle()) {
                warn!("tray icon: {e}");
            }
            if let Err(e) = sys::register_url_scheme() {
                warn!("openlag:// links: {e:#}");
            }
            let hotkey = state.settings.lock().hotkey.clone();
            if let Err(e) = apply_hotkey(app.handle(), &hotkey) {
                warn!("hotkey {hotkey}: {e:#}");
            }
            // The window starts hidden; the UI calls `ui_ready` after its first render.
            // Fallback in case the page never gets there.
            let fallback = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_secs(6)).await;
                show_on_start(&fallback);
            });

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                state.rebuild().await;
                tauri::async_runtime::spawn(auto_refresh(state.clone()));
                tauri::async_runtime::spawn(compare_loop(state.clone()));
                tauri::async_runtime::spawn(auto_connect(state.clone(), handle.clone()));
                tauri::async_runtime::spawn(auto_update_check(state.clone(), handle.clone()));
                tauri::async_runtime::spawn(boost_loop(state.clone()));
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
                                blocked: s.blocked,
                            }
                        }),
                        capturing: net.capture.is_some(),
                        direct: state.compare.lock().view(),
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
            pick_config_file,
            set_options,
            set_relays,
            set_server_enabled,
            set_filters,
            set_country_enabled,
            set_disable_auto,
            refresh_geo,
            set_ui_prefs,
            set_startup,
            list_processes,
            start,
            stop,
            relaunch_admin,
            check_update,
            install_update,
            ui_ready,
            term_open_local,
            term_open_ssh,
            term_write,
            term_resize,
            term_close,
            ssh_save,
            ssh_delete,
            ssh_forget_key,
            pick_key_file,
            open_url,
            set_kill_switch,
            set_conn,
            take_deep_link,
            set_extras,
            speed_test,
            trace_route,
            test_delays,
            export_settings,
            import_settings,
            relay_deploy,
            relay_remove,
            start_page,
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
                            let _ = handle.emit(
                                "auto",
                                tr!(
                                    "Запущен {name}: туннель включён",
                                    "{name} started: tunnel on"
                                ),
                            );
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
                        tr!(
                            "Программы из списка закрыты: туннель выключен",
                            "Listed programs closed: tunnel off"
                        ),
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

static SHOWN_ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Shows the main window the first time, unless OpenLag was started minimized (then it
/// stays in the tray).
fn show_on_start(app: &tauri::AppHandle) {
    if SHOWN_ONCE.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let minimized = (app.state::<Arc<AppState>>().settings.lock().start_minimized
        && std::env::var_os("OPENLAG_START_PAGE").is_none())
        || std::env::args().any(|a| a == "--minimized");
    if !minimized {
        tray::show_window(app);
    }
}

#[tauri::command]
fn ui_ready(app: tauri::AppHandle) {
    show_on_start(&app);
}

/// Switches a whole exit country on or off.
#[tauri::command]
async fn set_country_enabled(st: Shared<'_>, country: String, enabled: bool) -> CmdResult<()> {
    {
        let mut s = st.settings.lock();
        s.disabled_countries
            .retain(|c| !c.eq_ignore_ascii_case(&country));
        if !enabled {
            s.disabled_countries.push(country.to_uppercase());
        }
    }
    st.save().map_err(err)?;
    st.rebuild().await;
    Ok(())
}

#[tauri::command]
async fn set_disable_auto(st: Shared<'_>, disabled: bool) -> CmdResult<()> {
    st.settings.lock().disable_auto = disabled;
    st.save().map_err(err)?;
    st.rebuild().await;
    Ok(())
}

/// Re-measures the exit country of every running server.
#[tauri::command]
async fn refresh_geo(st: Shared<'_>) -> CmdResult<()> {
    let servers: Vec<_> = {
        let net = st.net.lock().await;
        net.engine
            .as_ref()
            .map(|e| {
                e.snapshot()
                    .paths
                    .into_iter()
                    .filter(|p| p.kind == "proxy")
                    .map(|p| (p.exit, p.via))
                    .collect()
            })
            .unwrap_or_default()
    };
    st.spawn_geo_probe(servers, true);
    Ok(())
}

/// Development aid: `OPENLAG_START_PAGE=servers` opens that tab at start.
#[tauri::command]
fn start_page() -> Option<String> {
    std::env::var("OPENLAG_START_PAGE").ok()
}

/// Every 2 s, pings directly (no tunnel) the public IPv4 game servers that tunnelled
/// flows talk to, most recently active first.
async fn compare_loop(st: Arc<AppState>) {
    let mut source: Option<std::net::Ipv4Addr> = None;
    let mut source_checked = std::time::Instant::now() - Duration::from_secs(3600);
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let mut flows = match st.net.try_lock() {
            Ok(net) => net
                .engine
                .as_ref()
                .map(|e| e.snapshot().flows)
                .unwrap_or_default(),
            Err(_) => continue,
        };
        flows.retain(|f| f.exit.is_some() && f.idle_ms < 30_000);
        flows.sort_by_key(|f| f.idle_ms);
        let mut ips = Vec::new();
        for f in &flows {
            if let std::net::IpAddr::V4(ip) = f.remote.ip()
                && !openlag_capture::is_local_scope(ip.into())
                && !ips.contains(&ip)
            {
                ips.push(ip);
            }
        }
        if ips.is_empty() {
            continue;
        }
        if source_checked.elapsed() > Duration::from_secs(30) {
            source_checked = std::time::Instant::now();
            source = openlag_proxy::physical_interface().and_then(|i| i.ipv4);
        }
        // Ping without holding the lock: the UI tick reads it every second.
        let results = tauri::async_runtime::spawn_blocking(move || compare::ping_all(&ips, source))
            .await
            .unwrap_or_default();
        st.compare.lock().record(results);
    }
}

// ---------------------------------------------------------------------------
// Terminal

fn known_hosts_path(st: &AppState) -> PathBuf {
    st.work_dir.join("known_hosts.json")
}

#[tauri::command]
fn term_open_local(
    app: tauri::AppHandle,
    st: Shared<'_>,
    shell: String,
    cols: u16,
    rows: u16,
) -> CmdResult<u32> {
    st.terminals
        .open_local(&app, &shell, cols, rows)
        .map_err(err)
}

#[tauri::command]
fn term_open_ssh(
    app: tauri::AppHandle,
    st: Shared<'_>,
    id: String,
    cols: u16,
    rows: u16,
) -> CmdResult<u32> {
    let host = st
        .settings
        .lock()
        .ssh_hosts
        .iter()
        .find(|h| h.id == id)
        .cloned()
        .ok_or("server not found")?;
    let password = secret::unprotect(&host.secret).map_err(err)?;
    st.terminals
        .open_ssh(&app, host, password, known_hosts_path(&st), cols, rows)
        .map_err(err)
}

#[tauri::command]
fn term_write(st: Shared<'_>, id: u32, data: String) -> CmdResult<()> {
    st.terminals.write(id, data.as_bytes()).map_err(err)
}

#[tauri::command]
fn term_resize(st: Shared<'_>, id: u32, cols: u16, rows: u16) -> CmdResult<()> {
    st.terminals.resize(id, cols, rows).map_err(err)
}

#[tauri::command]
fn term_close(st: Shared<'_>, id: u32) {
    st.terminals.close(id);
}

#[derive(serde::Deserialize)]
struct SshInput {
    id: Option<String>,
    name: String,
    host: String,
    port: u16,
    user: String,
    auth: String,
    key_path: String,
    /// New password or passphrase; None keeps the saved one.
    secret: Option<String>,
}

#[tauri::command]
fn ssh_save(st: Shared<'_>, input: SshInput) -> CmdResult<String> {
    if input.host.trim().is_empty() || input.user.trim().is_empty() {
        return Err("host and user are required".into());
    }
    let mut s = st.settings.lock();
    let id = input
        .id
        .filter(|i| !i.is_empty())
        .unwrap_or_else(settings::new_id);
    let old = s
        .ssh_hosts
        .iter()
        .find(|h| h.id == id)
        .map(|h| h.secret.clone())
        .unwrap_or_default();
    let secret = match input.secret {
        Some(plain) => secret::protect(&plain).map_err(err)?,
        None => old,
    };
    let host = term::SshHost {
        id: id.clone(),
        name: if input.name.trim().is_empty() {
            input.host.trim().to_string()
        } else {
            input.name.trim().to_string()
        },
        host: input.host.trim().to_string(),
        port: if input.port == 0 { 22 } else { input.port },
        user: input.user.trim().to_string(),
        auth: if input.auth == "key" {
            "key".into()
        } else {
            "password".into()
        },
        key_path: input.key_path.trim().to_string(),
        secret,
    };
    match s.ssh_hosts.iter_mut().find(|h| h.id == id) {
        Some(h) => *h = host,
        None => s.ssh_hosts.push(host),
    }
    drop(s);
    st.save().map_err(err)?;
    Ok(id)
}

#[tauri::command]
fn ssh_delete(st: Shared<'_>, id: String) -> CmdResult<()> {
    st.settings.lock().ssh_hosts.retain(|h| h.id != id);
    st.save().map_err(err)
}

/// Forgets the remembered key of a server (after it was reinstalled).
#[tauri::command]
fn ssh_forget_key(st: Shared<'_>, host: String, port: u16) -> CmdResult<()> {
    let path = known_hosts_path(&st);
    let mut known: HashMap<String, String> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    known.remove(&format!("{host}:{port}"));
    std::fs::write(&path, serde_json::to_vec_pretty(&known).unwrap_or_default())
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn pick_key_file(app: tauri::AppHandle) -> CmdResult<Option<String>> {
    use tauri_plugin_dialog::DialogExt;
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let mut d = app
            .dialog()
            .file()
            .set_title(tr!("Приватный SSH-ключ", "Private SSH key"));
        if let Some(home) = std::env::var_os("USERPROFILE") {
            d = d.set_directory(PathBuf::from(home).join(".ssh"));
        }
        d.blocking_pick_file()
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(picked
        .and_then(|f| f.into_path().ok())
        .map(|p| p.display().to_string()))
}

/// Opens an https link in the default browser.
#[tauri::command]
fn open_url(url: String) {
    if url.starts_with("https://") {
        sys::open_url(&url);
    }
}

#[tauri::command]
async fn set_kill_switch(st: Shared<'_>, on: bool) -> CmdResult<()> {
    st.settings.lock().kill_switch = on;
    st.save().map_err(err)?;
    // Capture reads it at start.
    let mut net = st.net.lock().await;
    if net.capture.is_some() {
        st.stop_capture(&mut net);
        st.start_capture(&mut net).map_err(err)?;
    }
    Ok(())
}

#[tauri::command]
async fn set_conn(st: Shared<'_>, conn: settings::ConnPrefs) -> CmdResult<()> {
    if !conn.probe_target.is_empty() && conn.probe_target.parse::<std::net::SocketAddr>().is_err() {
        return Err("probe target must look like 1.1.1.1:53".into());
    }
    st.settings.lock().conn = conn;
    st.save().map_err(err)?;
    st.rebuild().await;
    Ok(())
}

fn ssh_host_with_password(st: &AppState, id: &str) -> CmdResult<(term::SshHost, String)> {
    let host = st
        .settings
        .lock()
        .ssh_hosts
        .iter()
        .find(|h| h.id == id)
        .cloned()
        .ok_or("server not found")?;
    let password = secret::unprotect(&host.secret).map_err(err)?;
    Ok((host, password))
}

/// Installs our relay on a saved SSH server and adds it to the relays; the install
/// log goes to the returned terminal tab.
#[tauri::command]
fn relay_deploy(
    app: tauri::AppHandle,
    st: Shared<'_>,
    host_id: String,
    name: String,
    port: u16,
) -> CmdResult<u32> {
    let (host, password) = ssh_host_with_password(&st, &host_id)?;
    let psk = settings::parse_relays(&st.settings.lock().relays_toml)
        .map_err(err)?
        .psk;
    let psk = if psk.is_empty() {
        format!(
            "{:016x}{:016x}",
            rand::random::<u64>(),
            rand::random::<u64>()
        )
    } else {
        psk
    };
    let name = Some(name.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| host.name.clone());
    let id = st.terminals.open_task();
    let state = st.inner().clone();
    tauri::async_runtime::spawn(async move {
        let dirs = [exe_dir()];
        let res = relay::deploy(
            &app,
            id,
            &host,
            &password,
            &known_hosts_path(&state),
            &dirs,
            port,
            &psk,
        )
        .await
        .and_then(|d| {
            let mut s = state.settings.lock();
            s.relays_toml = relay::add_exit(&s.relays_toml, &psk, &name, d.via)?;
            drop(s);
            state.save()
        });
        let reason = match res {
            Ok(()) => {
                state.rebuild().await;
                let _ = app.emit("relays-changed", ());
                tr!(
                    "relay «{name}» установлен и добавлен в OpenLag",
                    "relay “{name}” installed and added to OpenLag"
                )
            }
            Err(e) => {
                term::emit_data(&app, id, format!("\r\n\x1b[31m{e:#}\x1b[0m\r\n").as_bytes());
                tr!("установка не удалась", "installation failed")
            }
        };
        state.terminals.finish_task(&app, id, reason);
    });
    Ok(id)
}

#[tauri::command]
fn relay_remove(
    app: tauri::AppHandle,
    st: Shared<'_>,
    host_id: String,
    name: String,
    port: u16,
) -> CmdResult<u32> {
    let _ = (name, port);
    let (host, password) = ssh_host_with_password(&st, &host_id)?;
    let id = st.terminals.open_task();
    let state = st.inner().clone();
    tauri::async_runtime::spawn(async move {
        let res = relay::remove(&app, id, &host, &password, &known_hosts_path(&state))
            .await
            .and_then(|ip| {
                let mut s = state.settings.lock();
                s.relays_toml = relay::remove_exit(&s.relays_toml, &ip.to_string())?;
                drop(s);
                state.save()
            });
        let reason = match res {
            Ok(()) => {
                state.rebuild().await;
                let _ = app.emit("relays-changed", ());
                tr!("relay удалён", "relay removed")
            }
            Err(e) => {
                term::emit_data(&app, id, format!("\r\n\x1b[31m{e:#}\x1b[0m\r\n").as_bytes());
                tr!("удаление не удалось", "removal failed")
            }
        };
        state.terminals.finish_task(&app, id, reason);
    });
    Ok(id)
}

// ---------------------------------------------------------------------------
// openlag:// links, hotkey, priority boost, speed test, backup

static PENDING_LINK: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);

/// What to import from an `openlag://` link: `openlag://import?url=<encoded>`,
/// `openlag://import/<link>` or `openlag://<link>` (a subscription URL or keys).
fn deep_link(arg: &str) -> Option<String> {
    let rest = arg.trim().strip_prefix("openlag://")?;
    let rest = rest.trim_end_matches('/');
    let payload = if let Some(q) = rest
        .strip_prefix("import?")
        .or_else(|| rest.strip_prefix("install-config?"))
        .or_else(|| rest.strip_prefix("add?"))
    {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("url="))
            .map(percent_decode)?
    } else if let Some(r) = rest
        .strip_prefix("import/")
        .or_else(|| rest.strip_prefix("add/"))
    {
        percent_decode(r)
    } else {
        percent_decode(rest)
    };
    let payload = payload.trim().to_string();
    (payload.contains("://") && payload.len() < 64 * 1024).then_some(payload)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[tauri::command]
fn take_deep_link() -> Option<String> {
    PENDING_LINK.lock().take()
}

fn apply_hotkey(app: &tauri::AppHandle, hotkey: &str) -> Result<()> {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
    let gs = app.global_shortcut();
    gs.unregister_all()?;
    if hotkey.trim().is_empty() {
        return Ok(());
    }
    gs.on_shortcut(hotkey.trim(), |app, _shortcut, event| {
        if event.state != ShortcutState::Pressed {
            return;
        }
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let st = app.state::<Arc<AppState>>().inner().clone();
            match st.toggle_capture().await {
                Ok(()) => {
                    let on = st.net.lock().await.capture.is_some();
                    let msg = if on {
                        tr!("Оптимизация включена", "Optimization on")
                    } else {
                        tr!("Оптимизация выключена", "Optimization off")
                    };
                    let _ = app.emit("auto", msg);
                }
                Err(e) => {
                    let _ = app.emit(
                        "notify",
                        tr!("Не удалось включить: {e:#}", "Could not turn on: {e:#}"),
                    );
                }
            }
        });
    })?;
    Ok(())
}

#[tauri::command]
async fn set_extras(
    app: tauri::AppHandle,
    st: Shared<'_>,
    hotkey: String,
    boost_priority: bool,
) -> CmdResult<()> {
    let old = st.settings.lock().hotkey.clone();
    if let Err(e) = apply_hotkey(&app, &hotkey) {
        let _ = apply_hotkey(&app, &old);
        return Err(tr!(
            "Не удалось занять {hotkey}: {e:#}. Возможно, сочетание занято другой программой",
            "Cannot take {hotkey}: {e:#}. Another program may be using it"
        ));
    }
    {
        let mut s = st.settings.lock();
        s.hotkey = hotkey;
        s.boost_priority = boost_priority;
    }
    st.save().map_err(err)
}

/// While optimization is on, keeps the selected programs at high priority (like the
/// "boost" of commercial game VPNs); puts them back to normal when it goes off.
async fn boost_loop(st: Arc<AppState>) {
    let mut boosted: HashMap<u32, String> = HashMap::new();
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let capturing = st.net.lock().await.capture.is_some();
        let names: std::collections::HashSet<String> = {
            let s = st.settings.lock();
            if capturing && s.boost_priority && !s.all_except() {
                s.groups
                    .iter()
                    .filter(|g| g.enabled)
                    .flat_map(|g| g.processes.iter().cloned())
                    .collect()
            } else {
                Default::default()
            }
        };
        let procs = sys::processes();
        // Restore what no longer qualifies (still running under the same name).
        boosted.retain(|pid, name| {
            if names.contains(name) {
                return true;
            }
            if procs.iter().any(|(p, n)| p == pid && n == name) {
                sys::set_priority(*pid, sys::NORMAL_PRIORITY);
            }
            false
        });
        for (pid, name) in procs {
            if !names.contains(&name) || boosted.contains_key(&pid) {
                continue;
            }
            match sys::set_priority(pid, sys::HIGH_PRIORITY) {
                sys::NORMAL_PRIORITY => {
                    info!("priority boost: {name} ({pid})");
                    boosted.insert(pid, name);
                }
                0 | sys::HIGH_PRIORITY => {}
                // The user set something else on purpose: leave it as it was.
                other => {
                    sys::set_priority(pid, other);
                    boosted.insert(pid, name);
                }
            }
        }
    }
}

#[tauri::command]
async fn speed_test(st: Shared<'_>, key: String) -> CmdResult<speed::Speed> {
    let socks = st
        .net
        .lock()
        .await
        .socks
        .get(&key)
        .copied()
        .ok_or_else(|| tr!("сервер не запущен", "the server is not running"))?;
    speed::run(socks).await.map_err(err)
}

/// Saves settings (subscriptions, groups, filters, SSH servers) to a file. Saved
/// passwords only decrypt on this PC under this Windows account.
#[tauri::command]
async fn export_settings(app: tauri::AppHandle, st: Shared<'_>) -> CmdResult<bool> {
    use tauri_plugin_dialog::DialogExt;
    let json = serde_json::to_vec_pretty(&*st.settings.lock()).map_err(|e| e.to_string())?;
    let file = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_title(tr!("Сохранить резервную копию", "Save a backup"))
            .set_file_name("openlag-settings.json")
            .add_filter("JSON", &["json"])
            .blocking_save_file()
    })
    .await
    .map_err(|e| e.to_string())?;
    let Some(path) = file.and_then(|f| f.into_path().ok()) else {
        return Ok(false);
    };
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(true)
}

#[tauri::command]
async fn import_settings(app: tauri::AppHandle, st: Shared<'_>) -> CmdResult<bool> {
    use tauri_plugin_dialog::DialogExt;
    let file = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_title(tr!(
                "Восстановить из резервной копии",
                "Restore from a backup"
            ))
            .add_filter("JSON", &["json"])
            .blocking_pick_file()
    })
    .await
    .map_err(|e| e.to_string())?;
    let Some(path) = file.and_then(|f| f.into_path().ok()) else {
        return Ok(false);
    };
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let mut new: Settings = serde_json::from_slice(&bytes).map_err(|e| {
        tr!(
            "это не резервная копия OpenLag: {e}",
            "not an OpenLag backup: {e}"
        )
    })?;
    {
        let mut s = st.settings.lock();
        // Keep this device's id: subscription panels count devices by it.
        new.hwid = s.hwid.clone();
        *s = new;
    }
    st.save().map_err(err)?;
    st.rebuild().await;
    Ok(true)
}

#[cfg(test)]
mod link_tests {
    use super::deep_link;

    #[test]
    fn deep_links() {
        assert_eq!(
            deep_link("openlag://import?url=https%3A%2F%2Fsub.example.com%2Fabc%3Fx%3D1")
                .as_deref(),
            Some("https://sub.example.com/abc?x=1")
        );
        assert_eq!(
            deep_link("openlag://import/vless://u@h:443?type=tcp#n").as_deref(),
            Some("vless://u@h:443?type=tcp#n")
        );
        assert_eq!(
            deep_link("openlag://https://a.b/c/").as_deref(),
            Some("https://a.b/c")
        );
        assert_eq!(deep_link("openlag://nonsense"), None);
        assert_eq!(deep_link("--minimized"), None);
    }
}

#[derive(Serialize)]
struct TraceView {
    ip: String,
    reached: bool,
    hops: Vec<trace::Hop>,
}

/// MTR-style route check to an IP or host name, directly from the network card.
#[tauri::command]
async fn trace_route(target: String) -> CmdResult<TraceView> {
    let target = target.trim().to_string();
    let ip: std::net::Ipv4Addr = match target.parse() {
        Ok(ip) => ip,
        Err(_) => tokio::net::lookup_host((target.as_str(), 0))
            .await
            .map_err(|e| {
                tr!(
                    "не удалось найти {target}: {e}",
                    "cannot resolve {target}: {e}"
                )
            })?
            .find_map(|a| match a.ip() {
                std::net::IpAddr::V4(v4) => Some(v4),
                _ => None,
            })
            .ok_or_else(|| tr!("у {target} нет IPv4-адреса", "{target} has no IPv4 address"))?,
    };
    let source = openlag_proxy::physical_interface().and_then(|i| i.ipv4);
    let hops = tauri::async_runtime::spawn_blocking(move || trace::trace(ip, source, 4))
        .await
        .map_err(|e| e.to_string())?;
    let reached = hops.last().and_then(|h| h.ip.as_deref()) == Some(ip.to_string().as_str());
    Ok(TraceView {
        ip: ip.to_string(),
        reached,
        hops,
    })
}

/// "Real delay" through every running server at once (an HTTPS request on an open
/// connection), like v2rayN's test. None = the server did not answer.
#[tauri::command]
async fn test_delays(st: Shared<'_>) -> CmdResult<Vec<(String, Option<f64>)>> {
    let socks: Vec<(String, std::net::SocketAddr)> = st
        .net
        .lock()
        .await
        .socks
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    if socks.is_empty() {
        return Err(tr!("серверы не запущены", "no servers are running"));
    }
    let mut out = Vec::with_capacity(socks.len());
    for chunk in socks.chunks(8) {
        let results = futures_util::future::join_all(
            chunk
                .iter()
                .map(|(k, s)| async move { (k.clone(), speed::delay(*s).await.ok()) }),
        )
        .await;
        out.extend(results);
    }
    Ok(out)
}
