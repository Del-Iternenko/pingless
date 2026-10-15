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
    pub groups: Vec<ProcessGroup>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessGroup {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// Lower-case executable names.
    pub processes: Vec<String>,
}

impl Settings {
    /// Executable names of all enabled groups.
    pub fn selected_processes(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .groups
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
        });
    }
    if s.mode != "all_except" {
        s.mode = "only".into();
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
