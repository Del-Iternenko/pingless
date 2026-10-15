use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct EngineConfig {
    /// Pre-shared key of our own relays. Not needed when only proxies are used.
    #[serde(default)]
    pub psk: String,
    #[serde(default)]
    pub exits: Vec<ExitSpec>,
    /// VPN servers reachable through local SOCKS5 ports (xray).
    #[serde(default)]
    pub proxies: Vec<ProxySpec>,
    /// Exit (relay or proxy name) to use while it is alive.
    #[serde(default)]
    pub preferred_exit: Option<String>,
    /// How many routes to the chosen exit carry each packet at the same time.
    /// 1 disables multipath.
    #[serde(default = "default_multipath")]
    pub multipath: usize,
    /// Interval between latency probes on every route.
    #[serde(default = "default_probe_ms", rename = "probe_interval_ms")]
    pub probe_interval_ms: u64,
    /// A route must beat the worst active route by this many ms to replace it. Keeps
    /// the selection from flapping between routes with similar latency.
    #[serde(default = "default_margin")]
    pub switch_margin_ms: f64,
    /// Flows without packets for this long are forgotten.
    #[serde(default = "default_flow_idle")]
    pub flow_idle_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExitSpec {
    pub name: String,
    pub routes: Vec<RouteSpec>,
}

/// A VPN server exposed by xray as a local SOCKS5 proxy with UDP support.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxySpec {
    pub name: String,
    pub socks: SocketAddr,
}

/// One way to reach an exit: directly, or through a bridge, optionally pinned to a
/// local network interface (multi-internet).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSpec {
    pub name: String,
    /// The exit itself or a bridge in front of it.
    pub via: SocketAddr,
    /// Local address to send from, to use a specific network interface.
    #[serde(default)]
    pub bind: Option<IpAddr>,
}

fn default_multipath() -> usize {
    2
}
fn default_probe_ms() -> u64 {
    250
}
fn default_margin() -> f64 {
    5.0
}
fn default_flow_idle() -> u64 {
    120
}

impl EngineConfig {
    pub fn probe_interval(&self) -> Duration {
        Duration::from_millis(self.probe_interval_ms.max(20))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.exits.is_empty() || !self.proxies.is_empty(),
            "add at least one relay or VPN server"
        );
        anyhow::ensure!(
            self.exits.is_empty() || !self.psk.is_empty(),
            "psk must not be empty when relays are used"
        );
        anyhow::ensure!(self.multipath >= 1, "multipath must be at least 1");
        for e in &self.exits {
            anyhow::ensure!(!e.routes.is_empty(), "relay {} has no routes", e.name);
        }
        let routes: usize =
            self.exits.iter().map(|e| e.routes.len()).sum::<usize>() + self.proxies.len();
        anyhow::ensure!(routes < u16::MAX as usize, "too many routes");
        Ok(())
    }
}
