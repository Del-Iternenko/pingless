use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use openlag_proto::{Header, MAX_DATAGRAM};
use parking_lot::Mutex;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tracing::{debug, info};

const MAPPING_IDLE: Duration = Duration::from_secs(120);
const MAX_MAPPINGS: usize = 8192;

#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub listen: Vec<SocketAddr>,
    /// The exit this bridge forwards to. Bridges never pick a destination themselves,
    /// so they cannot be abused as open relays.
    pub exit: SocketAddr,
}

/// One upstream socket per client address, so the exit sees each path separately.
struct Mapping {
    upstream: Arc<UdpSocket>,
    last_active: Arc<Mutex<Instant>>,
    reader: JoinHandle<()>,
}

impl Drop for Mapping {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// A bound bridge. `bind` first to learn the actual ports, then `run`.
pub struct BridgeServer {
    cfg: BridgeConfig,
    listeners: Vec<Arc<UdpSocket>>,
}

impl BridgeServer {
    pub async fn bind(cfg: BridgeConfig) -> Result<BridgeServer> {
        let mut listeners = Vec::new();
        for addr in &cfg.listen {
            let l = UdpSocket::bind(addr)
                .await
                .with_context(|| format!("bind {addr}"))?;
            info!(
                "bridge listening on {} -> exit {}",
                l.local_addr()?,
                cfg.exit
            );
            listeners.push(Arc::new(l));
        }
        Ok(BridgeServer { cfg, listeners })
    }

    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.listeners
            .iter()
            .filter_map(|s| s.local_addr().ok())
            .collect()
    }

    pub async fn run(self) -> Result<()> {
        let mappings: Arc<Mutex<HashMap<(usize, SocketAddr), Mapping>>> = Arc::default();
        let mut tasks = tokio::task::JoinSet::new();
        for (idx, listener) in self.listeners.into_iter().enumerate() {
            let mappings = mappings.clone();
            let exit = self.cfg.exit;
            tasks.spawn(async move {
                let mut buf = vec![0u8; MAX_DATAGRAM];
                loop {
                    let (len, from) = match listener.recv_from(&mut buf).await {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    // Cheap sanity check; authentication happens at the exit.
                    if Header::decode(&buf[..len]).is_err() {
                        continue;
                    }
                    let upstream = match upstream_for(&mappings, idx, from, &listener, exit) {
                        Ok(u) => u,
                        Err(e) => {
                            debug!("no mapping for {from}: {e}");
                            continue;
                        }
                    };
                    let _ = upstream.send_to(&buf[..len], exit).await;
                }
            });
        }
        let sweeper = mappings.clone();
        tasks.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let now = Instant::now();
                sweeper
                    .lock()
                    .retain(|_, m| now - *m.last_active.lock() < MAPPING_IDLE);
            }
        });
        // Dropping the JoinSet (when this future is cancelled) aborts every task.
        if let Some(res) = tasks.join_next().await {
            res?;
        }
        anyhow::bail!("relay task stopped unexpectedly")
    }
}

pub async fn run_bridge(cfg: BridgeConfig) -> Result<()> {
    BridgeServer::bind(cfg).await?.run().await
}

fn upstream_for(
    mappings: &Arc<Mutex<HashMap<(usize, SocketAddr), Mapping>>>,
    idx: usize,
    client: SocketAddr,
    listener: &Arc<UdpSocket>,
    exit: SocketAddr,
) -> Result<Arc<UdpSocket>> {
    let mut map = mappings.lock();
    if let Some(m) = map.get(&(idx, client)) {
        *m.last_active.lock() = Instant::now();
        return Ok(m.upstream.clone());
    }
    anyhow::ensure!(map.len() < MAX_MAPPINGS, "mapping limit reached");
    let bind: SocketAddr = match exit {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let std_sock = std::net::UdpSocket::bind(bind)?;
    std_sock.set_nonblocking(true)?;
    let upstream = Arc::new(UdpSocket::from_std(std_sock)?);
    let last_active = Arc::new(Mutex::new(Instant::now()));
    let reader = tokio::spawn({
        let upstream = upstream.clone();
        let listener = listener.clone();
        let last_active = last_active.clone();
        async move {
            let mut buf = vec![0u8; MAX_DATAGRAM];
            loop {
                let (len, from) = match upstream.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if from != exit {
                    continue;
                }
                *last_active.lock() = Instant::now();
                let _ = listener.send_to(&buf[..len], client).await;
            }
        }
    });
    debug!("new mapping {client} via {}", upstream.local_addr()?);
    map.insert(
        (idx, client),
        Mapping {
            upstream: upstream.clone(),
            last_active,
            reader,
        },
    );
    Ok(upstream)
}
