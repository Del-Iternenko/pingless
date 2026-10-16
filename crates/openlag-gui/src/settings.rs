//! Persistent settings, stored as JSON in the app data folder.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use openlag_core::ExitSpec;
use openlag_proxy::{ProxyServer, Skipped, SubscriptionInfo};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `only`: tunnel the processes of enabled groups; `all_except`: tunnel everything
    /// except them.
    pub mode: String,
    /// Groups used in `only` mode.
    pub groups: Vec<ProcessGroup>,
    /// Groups used in `all_except` mode; kept apart so switching modes never mixes
    /// "tunnel these" with "never tunnel these".
    pub exclude_groups: Vec<ProcessGroup>,
    /// In `only` mode: start capture by itself when a listed program starts, stop it
    /// when they have all exited.
    pub auto_connect: bool,
    /// Bind xray to the physical adapter so traffic skips other VPN clients' TUN.
    pub bypass_tun: bool,
    /// Server keys the user switched off: never started, never picked by "auto".
    pub disabled_servers: Vec<String>,
    /// Servers whose name or address contains any of these words are off too. Unlike
    /// keys, this survives subscription refreshes that rename or add servers.
    pub disabled_keywords: Vec<String>,
    /// Exit countries (ISO codes, as measured) that are switched off.
    pub disabled_countries: Vec<String>,
    /// Switch off every "auto"/balancer server of the subscriptions.
    pub disable_auto: bool,
    /// Latency probe interval for our own relays, ms.
    pub probe_interval_ms: u64,
    /// Start with Windows (scheduled task with highest privileges).
    pub autostart: bool,
    pub start_minimized: bool,
    /// The window's close button hides Pingless to the tray instead of quitting.
    pub close_to_tray: bool,
    /// Block the selected programs' traffic when no server is reachable instead of
    /// letting it go direct.
    pub kill_switch: bool,
    /// Look for new releases on GitHub.
    pub auto_update: bool,
    pub ui: UiPrefs,
    /// Global hotkey that toggles optimization, e.g. "Ctrl+Alt+O"; empty = none.
    pub hotkey: String,
    /// Raise the priority of the selected programs while optimization is on.
    pub boost_priority: bool,
    /// How xray connects to the VPN servers.
    pub conn: ConnPrefs,
    /// Saved SSH servers for the terminal tab.
    pub ssh_hosts: Vec<crate::term::SshHost>,
    /// Pre-groups versions stored a flat list here; migrated on load.
    #[serde(skip_serializing)]
    games: Vec<String>,
    pub multipath: usize,
    pub tunnel_existing: bool,
    /// Exit key the user pinned; None = automatic.
    pub preferred_exit: Option<String>,
    pub subscriptions: Vec<Subscription>,
    /// Own relays in TOML (`psk = ...` and `[[exits]]`), for advanced users.
    pub relays_toml: String,
    pub user_agent: String,
    /// Stable device id sent to subscription panels that limit devices.
    pub hwid: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: "only".into(),
            groups: Vec::new(),
            exclude_groups: Vec::new(),
            auto_connect: false,
            bypass_tun: true,
            disabled_servers: Vec::new(),
            disabled_keywords: Vec::new(),
            disabled_countries: Vec::new(),
            disable_auto: false,
            probe_interval_ms: 250,
            autostart: false,
            start_minimized: false,
            close_to_tray: true,
            kill_switch: false,
            auto_update: true,
            ui: UiPrefs::default(),
            hotkey: "Ctrl+Alt+O".into(),
            boost_priority: false,
            conn: ConnPrefs::default(),
            ssh_hosts: Vec::new(),
            games: Vec::new(),
            multipath: 2,
            tunnel_existing: false,
            preferred_exit: None,
            subscriptions: Vec::new(),
            relays_toml: String::new(),
            user_agent: openlag_proxy::FetchOptions::default().user_agent,
            hwid: format!(
                "{:016x}{:016x}",
                rand::random::<u64>(),
                rand::random::<u64>()
            ),
        }
    }
}

/// Connection options applied to every VPN server (like the advanced settings of
/// Happ or v2rayN).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnPrefs {
    /// uTLS fingerprint override; empty = as in the subscription.
    pub fingerprint: String,
    /// Fragment the TLS ClientHello to get past DPI.
    pub fragment: bool,
    pub frag_packets: String,
    pub frag_length: String,
    pub frag_interval: String,
    /// mux.cool concurrency; 0 = off (best for games).
    pub mux: u32,
    /// DoH URL used to resolve server addresses; empty = system DNS.
    pub dns: String,
    /// "ip:port" of the DNS server the latency probes query.
    pub probe_target: String,
}

impl Default for ConnPrefs {
    fn default() -> Self {
        ConnPrefs {
            fingerprint: String::new(),
            fragment: false,
            frag_packets: "tlshello".into(),
            frag_length: "100-200".into(),
            frag_interval: "10-20".into(),
            mux: 0,
            dns: String::new(),
            probe_target: "1.1.1.1:53".into(),
        }
    }
}

/// Look-and-feel options; only the UI reads them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPrefs {
    /// Accent colour, `#rrggbb`.
    pub accent: String,
    /// Seconds of history on the charts.
    pub chart_seconds: u32,
    /// Ping below this is green, below `ping_mid` yellow, above red.
    pub ping_good: u32,
    pub ping_mid: u32,
    pub compact: bool,
    /// "ru" or "en"; empty until the UI first starts.
    #[serde(default)]
    pub lang: String,
    /// Blocks the user switched off (charts, tiles...), by widget name.
    #[serde(default)]
    pub hidden: Vec<String>,
}

impl Default for UiPrefs {
    fn default() -> Self {
        UiPrefs {
            accent: "#10b981".into(),
            chart_seconds: 120,
            ping_good: 60,
            ping_mid: 120,
            compact: false,
            lang: String::new(),
            hidden: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessGroup {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// Lower-case executable names.
    pub processes: Vec<String>,
    /// Server for this group's programs ("only" mode); None follows the global choice.
    #[serde(default)]
    pub exit: Option<String>,
    /// Tunnel only UDP of these programs; their TCP goes direct.
    #[serde(default)]
    pub udp_only: bool,
}

impl Settings {
    /// Executable names of all enabled groups.
    pub fn selected_processes(&self) -> Vec<String> {
        let list = if self.all_except() {
            &self.exclude_groups
        } else {
            &self.groups
        };
        let mut v: Vec<String> = list
            .iter()
            .filter(|g| g.enabled)
            .flat_map(|g| g.processes.iter().cloned())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn all_except(&self) -> bool {
        self.mode == "all_except"
    }

    /// Why a server is switched off, or None if it may be used.
    /// `country` is the measured exit country, if known.
    pub fn server_disabled(
        &self,
        key: &str,
        srv: &ProxyServer,
        country: Option<&str>,
    ) -> Option<String> {
        if self.disabled_servers.iter().any(|k| k == key) {
            return Some("отключён вручную".into());
        }
        if self.disable_auto && is_auto(&srv.name) {
            return Some("авто-серверы отключены".into());
        }
        if let Some(c) = country
            && self
                .disabled_countries
                .iter()
                .any(|d| d.eq_ignore_ascii_case(c))
        {
            return Some(format!("страна {c} отключена"));
        }
        let text = format!("{} {}", srv.name, srv.address).to_lowercase();
        self.disabled_keywords
            .iter()
            .find(|w| !w.trim().is_empty() && text.contains(&w.trim().to_lowercase()))
            .map(|w| format!("фильтр «{}»", w.trim()))
    }
}

/// Balancer servers that pick the exit themselves ("Auto", "Оптимальная"...).
pub fn is_auto(name: &str) -> bool {
    let n = name.to_lowercase();
    [
        "auto",
        "авто",
        "оптимал",
        "balanc",
        "fastest",
        "best",
        "лучш",
    ]
    .iter()
    .any(|w| n.contains(w))
}

pub fn new_id() -> String {
    format!("{:08x}", rand::random::<u32>())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub name: String,
    /// None for links and keys pasted by hand.
    pub url: Option<String>,
    pub enabled: bool,
    pub servers: Vec<ProxyServer>,
    pub skipped: Vec<Skipped>,
    pub info: SubscriptionInfo,
    /// Unix seconds.
    pub updated_at: u64,
}

impl Subscription {
    /// Key of a server inside this subscription; unique across subscriptions.
    pub fn server_key(&self, s: &ProxyServer) -> String {
        format!("{}|{}", self.id, s.key())
    }
}

/// The `[relays]` text parsed into engine pieces.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relays {
    #[serde(default)]
    pub psk: String,
    #[serde(default)]
    pub exits: Vec<ExitSpec>,
}

pub fn parse_relays(text: &str) -> Result<Relays> {
    if text.trim().is_empty() {
        return Ok(Relays::default());
    }
    let r: Relays = toml::from_str(text).context("relay settings")?;
    anyhow::ensure!(
        r.exits.is_empty() || !r.psk.is_empty(),
        "psk is required for relays"
    );
    Ok(r)
}

pub fn load(path: &Path) -> Settings {
    let mut s: Settings = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if !s.games.is_empty() && s.groups.is_empty() {
        s.groups.push(ProcessGroup {
            id: new_id(),
            name: "Игры".into(),
            enabled: true,
            processes: std::mem::take(&mut s.games),
            exit: None,
            udp_only: false,
        });
    }
    if s.mode != "all_except" {
        s.mode = "only".into();
    }
    // Before separate lists existed, one list served both modes.
    if s.all_except() && s.exclude_groups.is_empty() && !s.groups.is_empty() {
        s.exclude_groups = std::mem::take(&mut s.groups);
    }
    s
}

pub fn save(path: &PathBuf, s: &Settings) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write-then-rename so a crash never leaves half a settings file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
