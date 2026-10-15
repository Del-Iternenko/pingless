use std::cell::RefCell;
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use openlag_proto::{Body, DataBody, MAX_DATAGRAM, PacketCipher, PingBody, SeqWindow, SessionKeys};
use parking_lot::Mutex;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config::{EngineConfig, RouteSpec};
use crate::stats::{FlowSnapshot, PathSnapshot, PathStats, Snapshot};

/// A captured UDP flow, as the game sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FlowKey {
    pub local: SocketAddr,
    pub remote: SocketAddr,
}

/// What the capture layer should do with a packet it handed to the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upstream {
    /// The engine took the packet; drop the original.
    Tunneled,
    /// No relay is reachable for this flow; let the original packet through unchanged.
    /// The game keeps working without optimisation instead of losing connection.
    Direct,
}

/// A packet for the game, to be injected as if `from` had sent it to `key.local`.
#[derive(Debug)]
pub struct Downstream<'a> {
    pub key: FlowKey,
    pub from: SocketAddr,
    pub payload: &'a [u8],
}

pub type DownstreamFn = Arc<dyn Fn(Downstream<'_>) + Send + Sync>;

struct ExitState {
    name: String,
    cipher: PacketCipher,
    paths: Vec<u16>,
    /// Routes currently carrying traffic, best first.
    active: Mutex<Vec<u16>>,
}

struct Path {
    id: u16,
    exit: usize,
    spec: RouteSpec,
    socket: Arc<UdpSocket>,
    stats: Mutex<PathStats>,
}

struct Flow {
    id: u32,
    key: FlowKey,
    /// Pinned at creation: the game sees the exit's IP, so a flow never moves to
    /// another exit. None means the flow goes direct.
    exit: Option<usize>,
    tx_seq: u64,
    rx: SeqWindow,
    last_active: Instant,
    packets_up: u64,
    packets_down: u64,
    duplicates: u64,
}

#[derive(Default)]
struct Flows {
    by_key: HashMap<FlowKey, u32>,
    by_id: HashMap<u32, Flow>,
    next_id: u32,
}

struct Inner {
    cfg: EngineConfig,
    exits: Vec<ExitState>,
    paths: Vec<Path>,
    flows: Mutex<Flows>,
    downstream: DownstreamFn,
    dropped_upstream: AtomicU64,
    probe_seq: AtomicU32,
    started: Instant,
}

pub struct Engine {
    inner: Arc<Inner>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

thread_local! {
    static SEAL_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(2048));
}

impl Engine {
    /// Opens one socket per route and starts probing. Must run inside a Tokio runtime.
    pub async fn start(cfg: EngineConfig, downstream: DownstreamFn) -> Result<Engine> {
        cfg.validate()?;
        let keys = SessionKeys::from_psk(&cfg.psk);
        let mut exits = Vec::new();
        let mut paths = Vec::new();
        for (exit_idx, spec) in cfg.exits.iter().enumerate() {
            // A separate session id per exit keeps their keys apart.
            let session: u64 = rand::random();
            let mut ids = Vec::new();
            for route in &spec.routes {
                let id = paths.len() as u16;
                let socket = open_socket(route).with_context(|| format!("route {}", route.name))?;
                info!(
                    "route {} ({}) -> {} via {}",
                    id, route.name, spec.name, route.via
                );
                paths.push(Path {
                    id,
                    exit: exit_idx,
                    spec: route.clone(),
                    socket: Arc::new(socket),
                    stats: Mutex::new(PathStats::new()),
                });
                ids.push(id);
            }
            exits.push(ExitState {
                name: spec.name.clone(),
                cipher: keys.session(session, false),
                paths: ids,
                active: Mutex::default(),
            });
        }

        let inner = Arc::new(Inner {
            cfg,
            exits,
            paths,
            flows: Mutex::default(),
            downstream,
            dropped_upstream: AtomicU64::new(0),
            probe_seq: AtomicU32::new(0),
            started: Instant::now(),
        });

        let mut tasks = Vec::new();
        for id in 0..inner.paths.len() {
            let inner = inner.clone();
            tasks.push(tokio::spawn(
                async move { inner.recv_loop(id as u16).await },
            ));
        }
        let prober = inner.clone();
        tasks.push(tokio::spawn(async move { prober.probe_loop().await }));
        Ok(Engine { inner, tasks })
    }

    /// Hands one UDP payload of a game flow to the engine. Never blocks: when a socket
    /// buffer is full the copy is dropped, as the network would do.
    pub fn send_upstream(&self, key: FlowKey, payload: &[u8]) -> Upstream {
        self.inner.send_upstream(key, payload)
    }

    /// Returns true once at least one route has answered a probe.
    pub fn any_route_alive(&self) -> bool {
        let now = Instant::now();
        self.inner.paths.iter().any(|p| p.stats.lock().alive(now))
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner.snapshot()
    }
}

fn open_socket(route: &RouteSpec) -> Result<UdpSocket> {
    let domain = if route.via.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    let local: SocketAddr = match (route.bind, route.via) {
        (Some(ip), via) => {
            anyhow::ensure!(
                ip.is_ipv4() == via.is_ipv4(),
                "bind address family differs from {via}"
            );
            SocketAddr::new(ip, 0)
        }
        (None, SocketAddr::V4(_)) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        (None, SocketAddr::V6(_)) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    socket.bind(&local.into())?;
    let _ = socket.set_recv_buffer_size(1 << 20);
    let _ = socket.set_send_buffer_size(1 << 20);
    socket.set_nonblocking(true)?;
    Ok(UdpSocket::from_std(socket.into())?)
}

impl Inner {
    fn send_upstream(&self, key: FlowKey, payload: &[u8]) -> Upstream {
        let mut flows = self.flows.lock();
        let id = match flows.by_key.get(&key) {
            Some(id) => *id,
            None => {
                let exit = self.best_exit();
                let id = flows.next_id;
                flows.next_id = flows.next_id.wrapping_add(1);
                match exit {
                    Some(e) => info!(
                        "flow {id} {} -> {} via exit {}",
                        key.local, key.remote, self.exits[e].name
                    ),
                    None => warn!(
                        "flow {id} {} -> {} goes direct: no relay is reachable",
                        key.local, key.remote
                    ),
                }
                flows.by_key.insert(key, id);
                flows.by_id.insert(
                    id,
                    Flow {
                        id,
                        key,
                        exit,
                        tx_seq: 0,
                        rx: SeqWindow::default(),
                        last_active: Instant::now(),
                        packets_up: 0,
                        packets_down: 0,
                        duplicates: 0,
                    },
                );
                id
            }
        };
        let flow = flows.by_id.get_mut(&id).expect("flow index out of sync");
        flow.last_active = Instant::now();
        flow.packets_up += 1;
        let Some(exit_idx) = flow.exit else {
            return Upstream::Direct;
        };
        flow.tx_seq += 1;
        let seq = flow.tx_seq;
        drop(flows);

        let exit = &self.exits[exit_idx];
        let body = Body::Data(DataBody {
            flow: id,
            seq,
            peer: key.remote,
            payload,
        });
        SEAL_BUF.with_borrow_mut(|buf| {
            if exit.cipher.seal(&body, buf).is_err() {
                self.dropped_upstream.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let active = exit.active.lock().clone();
            // While no route is confirmed (start-up, outage) try every route of the exit.
            let targets = if active.is_empty() {
                &exit.paths
            } else {
                &active
            };
            let mut sent = false;
            for &pid in targets {
                let path = &self.paths[pid as usize];
                if path.socket.try_send_to(buf, path.spec.via).is_ok() {
                    path.stats.lock().bytes_up += buf.len() as u64;
                    sent = true;
                }
            }
            if !sent {
                self.dropped_upstream.fetch_add(1, Ordering::Relaxed);
            }
        });
        Upstream::Tunneled
    }

    fn best_exit(&self) -> Option<usize> {
        let now = Instant::now();
        self.exits
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let best = e
                    .paths
                    .iter()
                    .filter_map(|&p| self.paths[p as usize].stats.lock().score(now))
                    .min_by(f64::total_cmp)?;
                Some((i, best))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    async fn recv_loop(self: Arc<Self>, id: u16) {
        let path = &self.paths[id as usize];
        let cipher = &self.exits[path.exit].cipher;
        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            let (len, from) = match path.socket.recv_from(&mut buf).await {
                Ok(v) => v,
                // ICMP port unreachable from a dead relay surfaces here on Windows.
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                Err(e) => {
                    debug!("route {id} recv error: {e}");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            };
            if from != path.spec.via {
                continue;
            }
            let Ok((_, body)) = cipher.open(&mut buf[..len]) else {
                continue;
            };
            let now = Instant::now();
            match body {
                Body::Pong(p) if p.path == id => {
                    path.stats.lock().on_pong(p.probe, now);
                }
                Body::Data(d) => {
                    path.stats.lock().bytes_down += len as u64;
                    let key = {
                        let mut flows = self.flows.lock();
                        let Some(flow) = flows.by_id.get_mut(&d.flow) else {
                            continue;
                        };
                        if flow.exit != Some(path.exit) {
                            continue;
                        }
                        if !flow.rx.accept(d.seq) {
                            flow.duplicates += 1;
                            continue;
                        }
                        flow.packets_down += 1;
                        flow.last_active = now;
                        flow.key
                    };
                    (self.downstream)(Downstream {
                        key,
                        from: d.peer,
                        payload: d.payload,
                    });
                }
                _ => {}
            }
        }
    }

    async fn probe_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(self.cfg.probe_interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_sweep = Instant::now();
        let mut buf = Vec::with_capacity(64);
        loop {
            tick.tick().await;
            let now = Instant::now();
            let sent_us = (now - self.started).as_micros() as u64;
            for path in &self.paths {
                let probe = self.probe_seq.fetch_add(1, Ordering::Relaxed);
                let ping = Body::Ping(PingBody {
                    path: path.id,
                    probe,
                    sent_us,
                });
                if self.exits[path.exit].cipher.seal(&ping, &mut buf).is_err() {
                    continue;
                }
                // Record before sending so a fast answer always finds its probe.
                path.stats.lock().on_probe_sent(probe, now);
                let _ = path.socket.try_send_to(&buf, path.spec.via);
            }
            for exit in &self.exits {
                self.reselect(exit, now);
            }
            if now - last_sweep >= Duration::from_secs(1) {
                last_sweep = now;
                self.sweep_flows(now);
            }
        }
    }

    /// Picks up to `multipath` routes with the best score, with hysteresis.
    fn reselect(&self, exit: &ExitState, now: Instant) {
        let score = |pid: u16| self.paths[pid as usize].stats.lock().score(now);
        let mut candidates: Vec<(u16, f64)> = exit
            .paths
            .iter()
            .filter_map(|&p| Some((p, score(p)?)))
            .collect();
        candidates.sort_by(|a, b| a.1.total_cmp(&b.1));

        let mut active = exit.active.lock();
        let before = active.clone();
        // Keep current routes that are still alive.
        let mut next: Vec<(u16, f64)> = active
            .iter()
            .filter_map(|&p| Some((p, score(p)?)))
            .collect();
        for &(pid, s) in &candidates {
            if next.iter().any(|(p, _)| *p == pid) {
                continue;
            }
            if next.len() < self.cfg.multipath {
                next.push((pid, s));
                continue;
            }
            let (worst_i, worst) = next
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.1.total_cmp(&b.1.1))
                .map(|(i, v)| (i, v.1))
                .unwrap();
            if s + self.cfg.switch_margin_ms < worst {
                next[worst_i] = (pid, s);
            }
        }
        next.sort_by(|a, b| a.1.total_cmp(&b.1));
        *active = next.into_iter().map(|(p, _)| p).collect();
        if *active != before {
            let names: Vec<_> = active
                .iter()
                .map(|&p| self.paths[p as usize].spec.name.as_str())
                .collect();
            info!("exit {}: active routes {:?}", exit.name, names);
        }
    }

    fn sweep_flows(&self, now: Instant) {
        let idle = Duration::from_secs(self.cfg.flow_idle_secs);
        let mut closed = Vec::new();
        {
            let mut flows = self.flows.lock();
            let Flows { by_key, by_id, .. } = &mut *flows;
            by_id.retain(|_, f| {
                let keep = now - f.last_active < idle;
                if !keep {
                    by_key.remove(&f.key);
                    if let Some(e) = f.exit {
                        closed.push((e, f.id));
                    }
                }
                keep
            });
        }
        let mut buf = Vec::with_capacity(64);
        for (exit_idx, flow) in closed {
            debug!("flow {flow} idle, closing");
            let exit = &self.exits[exit_idx];
            if exit.cipher.seal(&Body::Close { flow }, &mut buf).is_err() {
                continue;
            }
            for &pid in exit.active.lock().iter() {
                let path = &self.paths[pid as usize];
                let _ = path.socket.try_send_to(&buf, path.spec.via);
            }
        }
    }

    fn snapshot(&self) -> Snapshot {
        let now = Instant::now();
        let paths = self
            .paths
            .iter()
            .map(|p| {
                let st = p.stats.lock();
                let (srtt, last, jitter, loss, alive) = st.snapshot(now);
                PathSnapshot {
                    id: p.id,
                    exit: self.exits[p.exit].name.clone(),
                    route: p.spec.name.clone(),
                    via: p.spec.via,
                    srtt_ms: srtt,
                    last_rtt_ms: last,
                    jitter_ms: jitter,
                    loss_pct: loss,
                    alive,
                    active: self.exits[p.exit].active.lock().contains(&p.id),
                    bytes_up: st.bytes_up,
                    bytes_down: st.bytes_down,
                }
            })
            .collect();
        let flows = self
            .flows
            .lock()
            .by_id
            .values()
            .map(|f| FlowSnapshot {
                id: f.id,
                local: f.key.local,
                remote: f.key.remote,
                exit: f.exit.map(|e| self.exits[e].name.clone()),
                packets_up: f.packets_up,
                packets_down: f.packets_down,
                duplicates: f.duplicates,
                idle_ms: (now - f.last_active).as_millis() as u64,
            })
            .collect();
        Snapshot {
            paths,
            flows,
            dropped_upstream: self.dropped_upstream.load(Ordering::Relaxed),
        }
    }
}
