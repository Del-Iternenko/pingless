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
use tokio::io::AsyncReadExt;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config::{EngineConfig, ProxySpec, RouteSpec};
use crate::socks::{self, Association};
use crate::stats::{FlowRtt, FlowSnapshot, PathSnapshot, PathStats, Snapshot};
use crate::tcp;

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

/// Where a flow leaves: one of our relays, or a VPN server reached through xray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitRef {
    Relay(usize),
    Proxy(usize),
}

/// Latency probes through a proxy go to this DNS server.
const PROXY_PROBE_TARGET: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 53);
const PROXY_PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// Packets queued per proxy flow while its SOCKS association is being set up.
const PROXY_QUEUE: usize = 512;

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

struct ProxyState {
    id: u16,
    spec: ProxySpec,
    stats: Mutex<PathStats>,
}

struct Flow {
    id: u32,
    key: FlowKey,
    /// Pinned at creation: the game sees the exit's IP, so a flow never moves to
    /// another exit. None means the flow goes direct.
    exit: Option<ExitRef>,
    tx_seq: u64,
    rx: SeqWindow,
    /// Queue to the flow's SOCKS association task (proxy exits only). Dropping it
    /// ends the task and closes the association.
    proxy_tx: Option<mpsc::Sender<(SocketAddr, Vec<u8>)>>,
    last_active: Instant,
    packets_up: u64,
    packets_down: u64,
    duplicates: u64,
    rtt: FlowRtt,
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
    proxies: Vec<ProxyState>,
    preferred: Mutex<Option<String>>,
    flows: Mutex<Flows>,
    downstream: DownstreamFn,
    dropped_upstream: AtomicU64,
    probe_seq: AtomicU32,
    started: Instant,
    /// Capture threads call into the engine from outside the runtime.
    rt: tokio::runtime::Handle,
    tcp_table: Arc<tcp::Table>,
    /// Local port of the TCP redirect listener; None without proxy exits.
    tcp_port: Option<u16>,
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
        // Ends proxy flow tasks, which hold the inner state alive.
        let mut flows = self.inner.flows.lock();
        flows.by_id.clear();
        flows.by_key.clear();
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
        let proxies = cfg
            .proxies
            .iter()
            .enumerate()
            .map(|(i, spec)| ProxyState {
                id: (paths.len() + i) as u16,
                spec: spec.clone(),
                stats: Mutex::new(PathStats::new()),
            })
            .collect();

        // TCP can only go through proxies: our relays carry UDP only.
        let tcp_listener = if cfg.proxies.is_empty() {
            None
        } else {
            Some(tcp::bind().await?)
        };
        let tcp_port = match &tcp_listener {
            Some(l) => Some(l.local_addr()?.port()),
            None => None,
        };
        let inner = Arc::new(Inner {
            tcp_table: Arc::default(),
            tcp_port,
            preferred: Mutex::new(cfg.preferred_exit.clone()),
            cfg,
            exits,
            paths,
            proxies,
            flows: Mutex::default(),
            downstream,
            dropped_upstream: AtomicU64::new(0),
            probe_seq: AtomicU32::new(0),
            started: Instant::now(),
            rt: tokio::runtime::Handle::current(),
        });

        let mut tasks = Vec::new();
        for id in 0..inner.paths.len() {
            let inner = inner.clone();
            tasks.push(tokio::spawn(
                async move { inner.recv_loop(id as u16).await },
            ));
        }
        for idx in 0..inner.proxies.len() {
            let inner = inner.clone();
            tasks.push(tokio::spawn(
                async move { inner.proxy_probe_loop(idx).await },
            ));
        }
        if let Some(listener) = tcp_listener {
            tasks.push(tokio::spawn(tcp::accept_loop(
                listener,
                inner.tcp_table.clone(),
            )));
        }
        let prober = inner.clone();
        tasks.push(tokio::spawn(async move { prober.probe_loop().await }));
        Ok(Engine { inner, tasks })
    }

    /// Hands one UDP payload of a game flow to the engine. Never blocks: when a socket
    /// buffer is full the copy is dropped, as the network would do.
    pub fn send_upstream(&self, key: FlowKey, payload: &[u8]) -> Upstream {
        self.inner.send_upstream(key, payload, None)
    }

    /// Like [`Engine::send_upstream`], but a new flow prefers the exit `prefer` (for
    /// example the server chosen for the program's group) while it is alive.
    pub fn send_upstream_via(
        &self,
        key: FlowKey,
        payload: &[u8],
        prefer: Option<&str>,
    ) -> Upstream {
        self.inner.send_upstream(key, payload, prefer)
    }

    /// Returns true once at least one route has answered a probe.
    pub fn any_route_alive(&self) -> bool {
        let now = Instant::now();
        self.inner.paths.iter().any(|p| p.stats.lock().alive(now))
            || self.inner.proxies.iter().any(|p| p.stats.lock().alive(now))
    }

    /// New flows go to this exit (relay or proxy name) while it is alive.
    /// `None` picks the exit with the best score.
    pub fn set_preferred_exit(&self, name: Option<String>) {
        *self.inner.preferred.lock() = name;
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner.snapshot()
    }

    /// Port of the local TCP redirect listener, if TCP can be tunnelled at all.
    pub fn tcp_listen_port(&self) -> Option<u16> {
        self.inner.tcp_port
    }

    /// Registers a new TCP connection from `client` to `orig` for redirection and
    /// returns true, or false if it should go direct (no reachable proxy exit).
    pub fn tcp_open(&self, client: SocketAddr, orig: SocketAddr) -> bool {
        self.tcp_open_via(client, orig, None)
    }

    /// Like [`Engine::tcp_open`] with a preferred exit for this connection.
    pub fn tcp_open_via(&self, client: SocketAddr, orig: SocketAddr, prefer: Option<&str>) -> bool {
        let inner = &self.inner;
        if inner.tcp_port.is_none() || !orig.is_ipv4() {
            return false;
        }
        let Some(idx) = inner.best_proxy(prefer) else {
            return false;
        };
        let p = &inner.proxies[idx];
        debug!("tcp {client} -> {orig} via {}", p.spec.name);
        inner
            .tcp_table
            .insert(client, orig, p.spec.socks, p.spec.name.clone());
        true
    }

    /// Where the redirected connection from local port `client_port` was going.
    pub fn tcp_original(&self, client_port: u16) -> Option<SocketAddr> {
        self.inner.tcp_table.original(client_port)
    }

    /// Closes tunnelled TCP connections whose app-side port `drop` picks (used when
    /// the selection or mode changed and those programs must not use the tunnel).
    pub fn tcp_revoke(&self, drop: impl Fn(u16) -> bool) -> usize {
        self.inner.tcp_table.revoke(drop)
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
    fn exit_name(&self, exit: ExitRef) -> &str {
        match exit {
            ExitRef::Relay(i) => &self.exits[i].name,
            ExitRef::Proxy(i) => &self.proxies[i].spec.name,
        }
    }

    fn send_upstream(
        self: &Arc<Self>,
        key: FlowKey,
        payload: &[u8],
        prefer: Option<&str>,
    ) -> Upstream {
        let mut flows = self.flows.lock();
        let id = match flows.by_key.get(&key) {
            Some(id) => *id,
            None => {
                let exit = self.best_exit(prefer);
                let id = flows.next_id;
                flows.next_id = flows.next_id.wrapping_add(1);
                match exit {
                    Some(e) => info!(
                        "flow {id} {} -> {} via {}",
                        key.local,
                        key.remote,
                        self.exit_name(e)
                    ),
                    None => warn!(
                        "flow {id} {} -> {} goes direct: no exit is reachable",
                        key.local, key.remote
                    ),
                }
                let proxy_tx = match exit {
                    Some(ExitRef::Proxy(p)) => {
                        let (tx, rx) = mpsc::channel(PROXY_QUEUE);
                        let inner = self.clone();
                        self.rt
                            .spawn(async move { inner.proxy_flow(id, key, p, rx).await });
                        Some(tx)
                    }
                    _ => None,
                };
                flows.by_key.insert(key, id);
                flows.by_id.insert(
                    id,
                    Flow {
                        id,
                        key,
                        exit,
                        tx_seq: 0,
                        rx: SeqWindow::default(),
                        proxy_tx,
                        last_active: Instant::now(),
                        packets_up: 0,
                        packets_down: 0,
                        duplicates: 0,
                        rtt: FlowRtt::new(Instant::now()),
                    },
                );
                id
            }
        };
        let flow = flows.by_id.get_mut(&id).expect("flow index out of sync");
        let now = Instant::now();
        flow.last_active = now;
        flow.packets_up += 1;
        if flow.exit.is_some() {
            flow.rtt.on_up(now);
        }
        let exit_idx = match flow.exit {
            None => return Upstream::Direct,
            Some(ExitRef::Proxy(_)) => {
                let tx = flow.proxy_tx.as_ref().expect("proxy flow without queue");
                if tx.try_send((key.remote, payload.to_vec())).is_err() {
                    self.dropped_upstream.fetch_add(1, Ordering::Relaxed);
                }
                return Upstream::Tunneled;
            }
            Some(ExitRef::Relay(i)) => i,
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

    /// `prefer` (a group's server) wins over the global preferred exit; both only
    /// while alive.
    fn best_exit(&self, prefer: Option<&str>) -> Option<ExitRef> {
        let now = Instant::now();
        let mut candidates: Vec<(ExitRef, f64)> = Vec::new();
        for (i, e) in self.exits.iter().enumerate() {
            if let Some(best) = e
                .paths
                .iter()
                .filter_map(|&p| self.paths[p as usize].stats.lock().score(now))
                .min_by(f64::total_cmp)
            {
                candidates.push((ExitRef::Relay(i), best));
            }
        }
        for (i, p) in self.proxies.iter().enumerate() {
            if let Some(score) = p.stats.lock().score(now) {
                candidates.push((ExitRef::Proxy(i), score));
            }
        }
        let global = self.preferred.lock().clone();
        for name in [prefer, global.as_deref()].into_iter().flatten() {
            if let Some((e, _)) = candidates.iter().find(|(e, _)| self.exit_name(*e) == name) {
                return Some(*e);
            }
            warn!("preferred exit {name} is not reachable");
        }
        candidates
            .into_iter()
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(e, _)| e)
    }

    /// Best alive proxy exit, honouring the preferred exit when it is a proxy.
    fn best_proxy(&self, prefer: Option<&str>) -> Option<usize> {
        let now = Instant::now();
        let global = self.preferred.lock().clone();
        for name in [prefer, global.as_deref()].into_iter().flatten() {
            if let Some(i) = self.proxies.iter().position(|p| p.spec.name == name)
                && self.proxies[i].stats.lock().alive(now)
            {
                return Some(i);
            }
        }
        self.proxies
            .iter()
            .enumerate()
            .filter_map(|(i, p)| Some((i, p.stats.lock().score(now)?)))
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
                        if flow.exit != Some(ExitRef::Relay(path.exit)) {
                            continue;
                        }
                        if !flow.rx.accept(d.seq) {
                            flow.duplicates += 1;
                            continue;
                        }
                        flow.packets_down += 1;
                        flow.last_active = now;
                        flow.rtt.on_down(now);
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

    /// Carries one flow through a SOCKS5 UDP association, re-opening it if xray or
    /// the server drops it.
    async fn proxy_flow(
        self: Arc<Self>,
        flow_id: u32,
        key: FlowKey,
        proxy: usize,
        mut rx: mpsc::Receiver<(SocketAddr, Vec<u8>)>,
    ) {
        let socks_addr = self.proxies[proxy].spec.socks;
        let mut buf = vec![0u8; 65_536];
        let mut out = Vec::with_capacity(2048);
        let mut ctrl_buf = [0u8; 64];
        loop {
            let Association { mut ctrl, udp } = match Association::open(socks_addr).await {
                Ok(a) => a,
                Err(e) => {
                    warn!("flow {flow_id}: cannot open SOCKS association: {e:#}");
                    // Keep draining so the queue does not fill with stale packets.
                    let retry = tokio::time::sleep(Duration::from_secs(1));
                    tokio::pin!(retry);
                    loop {
                        tokio::select! {
                            _ = &mut retry => break,
                            m = rx.recv() => if m.is_none() { return },
                        }
                    }
                    continue;
                }
            };
            loop {
                tokio::select! {
                    m = rx.recv() => {
                        let Some((dst, payload)) = m else { return };
                        socks::encode(dst, &payload, &mut out);
                        if udp.send(&out).await.is_ok() {
                            self.proxies[proxy].stats.lock().bytes_up += out.len() as u64;
                        }
                    }
                    r = udp.recv(&mut buf) => {
                        let n = match r {
                            Ok(n) => n,
                            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                            Err(_) => break,
                        };
                        let Ok((from, payload)) = socks::decode(&buf[..n]) else { continue };
                        self.proxies[proxy].stats.lock().bytes_down += n as u64;
                        {
                            let mut flows = self.flows.lock();
                            let Some(flow) = flows.by_id.get_mut(&flow_id) else { return };
                            let now = Instant::now();
                            flow.packets_down += 1;
                            flow.last_active = now;
                            flow.rtt.on_down(now);
                        }
                        (self.downstream)(Downstream { key, from, payload });
                    }
                    r = ctrl.read(&mut ctrl_buf) => {
                        if matches!(r, Ok(0) | Err(_)) {
                            debug!("flow {flow_id}: SOCKS association closed, reopening");
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Measures a proxy with DNS queries sent through it over UDP: exactly the path a
    /// game's packets take, including whether the server supports UDP at all.
    async fn proxy_probe_loop(self: Arc<Self>, idx: usize) {
        let proxy = &self.proxies[idx];
        let mut buf = vec![0u8; 4096];
        let mut out = Vec::with_capacity(64);
        let mut ctrl_buf = [0u8; 64];
        let mut txid: u16 = rand::random();
        loop {
            let Association { mut ctrl, udp } = match Association::open(proxy.spec.socks).await {
                Ok(a) => a,
                Err(e) => {
                    debug!("proxy {}: {e:#}", proxy.spec.name);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };
            let mut tick = tokio::time::interval(PROXY_PROBE_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first probes of a new association include the proxy's connection
            // setup (hundreds of ms over XHTTP/REALITY). Send them unrecorded, so
            // their answers find no outstanding probe and are ignored.
            let mut warmup = 2;
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        txid = txid.wrapping_add(1);
                        socks::encode(self.cfg.proxy_probe_target.unwrap_or(PROXY_PROBE_TARGET), &socks::dns_probe(txid), &mut out);
                        if warmup > 0 {
                            warmup -= 1;
                        } else {
                            proxy.stats.lock().on_probe_sent(txid as u32, Instant::now());
                        }
                        if udp.send(&out).await.is_err() {
                            break;
                        }
                    }
                    r = udp.recv(&mut buf) => {
                        let n = match r {
                            Ok(n) => n,
                            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                            Err(_) => break,
                        };
                        if let Ok((_, payload)) = socks::decode(&buf[..n])
                            && payload.len() >= 2
                        {
                            let id = u16::from_be_bytes([payload[0], payload[1]]);
                            proxy.stats.lock().on_pong(id as u32, Instant::now());
                        }
                    }
                    r = ctrl.read(&mut ctrl_buf) => {
                        if matches!(r, Ok(0) | Err(_)) {
                            break;
                        }
                    }
                }
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
                self.tcp_table.sweep();
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
                    // Proxy flows end when their queue sender is dropped here.
                    if let Some(ExitRef::Relay(e)) = f.exit {
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
        let mut paths: Vec<PathSnapshot> = self
            .paths
            .iter()
            .map(|p| {
                let st = p.stats.lock();
                let (srtt, last, jitter, loss, alive) = st.snapshot(now);
                PathSnapshot {
                    id: p.id,
                    exit: self.exits[p.exit].name.clone(),
                    route: p.spec.name.clone(),
                    kind: "relay".into(),
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
        for p in &self.proxies {
            let st = p.stats.lock();
            let (srtt, last, jitter, loss, alive) = st.snapshot(now);
            paths.push(PathSnapshot {
                id: p.id,
                exit: p.spec.name.clone(),
                route: "proxy".into(),
                kind: "proxy".into(),
                via: p.spec.socks,
                srtt_ms: srtt,
                last_rtt_ms: last,
                jitter_ms: jitter,
                loss_pct: loss,
                alive,
                active: alive,
                bytes_up: st.bytes_up,
                bytes_down: st.bytes_down,
            });
        }
        let flows = self
            .flows
            .lock()
            .by_id
            .values()
            .map(|f| FlowSnapshot {
                id: f.id,
                local: f.key.local,
                remote: f.key.remote,
                exit: f.exit.map(|e| self.exit_name(e).to_string()),
                packets_up: f.packets_up,
                packets_down: f.packets_down,
                duplicates: f.duplicates,
                idle_ms: (now - f.last_active).as_millis() as u64,
                rtt_ms: f.rtt.get().map(|r| r.0),
                rtt_jitter_ms: f.rtt.get().map(|r| r.1),
            })
            .collect();
        Snapshot {
            paths,
            flows,
            dropped_upstream: self.dropped_upstream.load(Ordering::Relaxed),
            preferred_exit: self.preferred.lock().clone(),
            tcp: self.tcp_table.snapshot(),
        }
    }
}
