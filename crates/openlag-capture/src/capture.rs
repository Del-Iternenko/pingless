use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

use anyhow::Result;
use openlag_core::{Downstream, Engine, FlowKey, Upstream};
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use crate::packet::{build_udp, is_local_scope, parse_tcp_v4, parse_udp, rewrite_tcp_v4};
use crate::procmap::{ProcessMap, TCP, UDP, port_table};
use crate::windivert::{self as wd, Address, Handle};

/// Which processes go through the tunnel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Selection {
    /// Only the listed processes.
    #[default]
    Only,
    /// Everything except the listed processes.
    AllExcept,
}

#[derive(Debug, Clone, Default)]
pub struct CaptureConfig {
    /// Executable names, e.g. `cs2.exe` (case-insensitive).
    pub processes: Vec<String>,
    pub mode: Selection,
    /// Also tunnel UDP sockets that were open before capture started. Off by default:
    /// moving a live match to another IP usually disconnects it.
    pub tunnel_existing: bool,
    /// Never tunnel these processes (our own xray, which carries the tunnel itself).
    pub exclude_pids: Vec<u32>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CaptureStats {
    pub tunneled: u64,
    pub direct: u64,
    pub injected: u64,
    pub inject_errors: u64,
    pub tcp_redirected: u64,
}

struct Shared {
    names: HashSet<String>,
    mode: Selection,
    exclude_pids: HashSet<u32>,
    procs: ProcessMap,
    /// Interface a UDP flow was seen on, needed to inject answers.
    flow_if: Mutex<HashMap<FlowKey, (u32, u32)>>,
    /// UDP ports of selected sockets that existed at start and stay direct.
    preexisting: Mutex<HashSet<u16>>,
    net: Arc<Handle>,
    stopping: AtomicBool,
    tunneled: AtomicU64,
    direct: AtomicU64,
    injected: AtomicU64,
    inject_errors: AtomicU64,
    tcp_redirected: AtomicU64,
}

/// Captures outbound traffic of the selected processes and injects answers back.
pub struct Capture {
    shared: Arc<Shared>,
    sock: Arc<Handle>,
    threads: Vec<JoinHandle<()>>,
}

impl Capture {
    /// Opens the WinDivert handles. Requires Administrator rights and the driver files.
    pub fn open(cfg: &CaptureConfig) -> Result<Capture> {
        let net = Handle::open(
            "outbound and !loopback and (udp or tcp)",
            wd::LAYER_NETWORK,
            0,
            0,
        )?;
        let sock = Handle::open(
            "udp or tcp",
            wd::LAYER_SOCKET,
            0,
            wd::FLAG_SNIFF | wd::FLAG_RECV_ONLY,
        )?;
        let mut exclude_pids: HashSet<u32> = cfg.exclude_pids.iter().copied().collect();
        exclude_pids.insert(std::process::id());
        let shared = Arc::new(Shared {
            names: cfg
                .processes
                .iter()
                .map(|p| p.trim().to_lowercase())
                .filter(|p| !p.is_empty())
                .collect(),
            mode: cfg.mode,
            exclude_pids,
            procs: ProcessMap::default(),
            flow_if: Mutex::default(),
            preexisting: Mutex::default(),
            net,
            stopping: AtomicBool::new(false),
            tunneled: AtomicU64::new(0),
            direct: AtomicU64::new(0),
            injected: AtomicU64::new(0),
            inject_errors: AtomicU64::new(0),
            tcp_redirected: AtomicU64::new(0),
        });

        for (port, pid) in port_table(TCP) {
            shared.procs.on_bind(TCP, port, pid);
        }
        let mut existing = Vec::new();
        for (port, pid) in port_table(UDP) {
            shared.procs.on_bind(UDP, port, pid);
            if shared.selected(pid).is_some() {
                existing.push(port);
            }
        }
        if !existing.is_empty() && !cfg.tunnel_existing {
            info!(
                "{} UDP socket(s) of selected processes were already open and stay direct \
                 until the program reopens them",
                existing.len()
            );
            shared.preexisting.lock().extend(existing);
        }
        Ok(Capture {
            shared,
            sock,
            threads: Vec::new(),
        })
    }

    /// Callback for [`Engine::start`]: injects packets from relays into the program.
    pub fn downstream(&self) -> openlag_core::DownstreamFn {
        let shared = self.shared.clone();
        Arc::new(move |d: Downstream<'_>| shared.inject(d))
    }

    /// Starts the capture threads. The engine must already be running.
    pub fn start(&mut self, engine: Arc<Engine>) {
        let shared = self.shared.clone();
        self.threads.push(
            std::thread::Builder::new()
                .name("openlag-net".into())
                .spawn(move || shared.network_loop(&engine))
                .expect("spawn network thread"),
        );
        let shared = self.shared.clone();
        let sock = self.sock.clone();
        self.threads.push(
            std::thread::Builder::new()
                .name("openlag-sock".into())
                .spawn(move || shared.socket_loop(&sock))
                .expect("spawn socket thread"),
        );
        match self.shared.mode {
            Selection::Only => info!("capturing {:?}", self.shared.names),
            Selection::AllExcept => info!("capturing everything except {:?}", self.shared.names),
        }
    }

    pub fn stats(&self) -> CaptureStats {
        let s = &self.shared;
        CaptureStats {
            tunneled: s.tunneled.load(Ordering::Relaxed),
            direct: s.direct.load(Ordering::Relaxed),
            injected: s.injected.load(Ordering::Relaxed),
            inject_errors: s.inject_errors.load(Ordering::Relaxed),
            tcp_redirected: s.tcp_redirected.load(Ordering::Relaxed),
        }
    }

    /// Stops capturing. Packets flow normally again as soon as the handles close.
    pub fn stop(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        self.shared.net.shutdown();
        self.sock.shutdown();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Shared {
    /// The process name if this process should go through the tunnel.
    fn selected(&self, pid: u32) -> Option<String> {
        if self.exclude_pids.contains(&pid) {
            return None;
        }
        // Unknown name (protected or exited process): leave it alone in both modes.
        let name = self.procs.name(pid)?;
        let listed = self.names.contains(&name);
        match self.mode {
            Selection::Only => listed.then_some(name),
            Selection::AllExcept => (!listed).then_some(name),
        }
    }

    fn network_loop(&self, engine: &Engine) {
        let mut buf = vec![0u8; 65_535 + 1024];
        let mut addr = Address::default();
        loop {
            let len = match self.net.recv(&mut buf, &mut addr) {
                Ok(n) => n,
                Err(e) => {
                    if !self.stopping.load(Ordering::SeqCst) {
                        warn!("capture stopped: {e:#}");
                    }
                    return;
                }
            };
            let pkt = &mut buf[..len];
            let proto = match pkt.first().map(|b| b >> 4) {
                Some(4) if pkt.len() >= 20 => pkt[9],
                Some(6) if pkt.len() >= 40 => pkt[6],
                _ => 0,
            };
            let handled = match proto {
                UDP => self.try_udp(engine, pkt, &addr),
                TCP => self.try_tcp(engine, pkt, &addr),
                _ => false,
            };
            if handled {
                continue;
            }
            // Not ours, or no exit: let the original packet go.
            if let Err(e) = self.net.send(pkt, &addr) {
                debug!("reinject failed: {e:#}");
            }
        }
    }

    /// Returns true if the engine took the packet and the original must be dropped.
    fn try_udp(&self, engine: &Engine, pkt: &[u8], addr: &Address) -> bool {
        if !addr.outbound() || addr.loopback() {
            return false;
        }
        let Some(udp) = parse_udp(pkt) else {
            return false;
        };
        if is_local_scope(udp.dst.ip()) {
            return false;
        }
        let port = udp.src.port();
        let Some(pid) = self.procs.pid_for_port(UDP, port) else {
            return false;
        };
        let Some(name) = self.selected(pid) else {
            return false;
        };
        if self.preexisting.lock().contains(&port) {
            self.direct.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let key = FlowKey {
            local: udp.src,
            remote: udp.dst,
        };
        {
            let mut flows = self.flow_if.lock();
            if flows
                .insert(key, (addr.if_idx(), addr.sub_if_idx()))
                .is_none()
            {
                info!("{name} (pid {pid}) udp {} -> {}", udp.src, udp.dst);
            }
        }
        match engine.send_upstream(key, udp.payload) {
            Upstream::Tunneled => {
                self.tunneled.fetch_add(1, Ordering::Relaxed);
                true
            }
            Upstream::Direct => {
                self.direct.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Redirects TCP of selected processes to the engine's local listener, and turns
    /// the listener's answers back into packets from the real server. Returns true if
    /// the packet was rewritten and injected.
    fn try_tcp(&self, engine: &Engine, pkt: &mut [u8], addr: &Address) -> bool {
        let Some(listen) = engine.tcp_listen_port() else {
            return false;
        };
        if !addr.outbound() || addr.loopback() {
            return false;
        }
        let Some(t) = parse_tcp_v4(pkt) else {
            return false;
        };

        // Listener -> app: (client_ip:listen) -> (server_ip:client_port) becomes
        // (server_ip:orig_port) -> (client_ip:client_port).
        if t.src.port() == listen {
            let Some(orig) = engine.tcp_original(t.dst.port()) else {
                return false;
            };
            let from = SocketAddrV4::new(*t.dst.ip(), orig.port());
            let to = SocketAddrV4::new(*t.src.ip(), t.dst.port());
            return self.inject_tcp(pkt, addr, from, to);
        }

        // App -> server: (client_ip:client_port) -> (server_ip:orig_port) becomes
        // (server_ip:client_port) -> (client_ip:listen).
        let known = engine.tcp_original(t.src.port()) == Some(SocketAddr::V4(t.dst));
        let redirect = known
            || (t.syn
                && !t.ack
                && !is_local_scope((*t.dst.ip()).into())
                && self
                    .procs
                    .pid_for_port(TCP, t.src.port())
                    .and_then(|pid| self.selected(pid))
                    .is_some()
                && engine.tcp_open(SocketAddr::V4(t.src), SocketAddr::V4(t.dst)));
        if !redirect {
            return false;
        }
        if !known {
            self.tcp_redirected.fetch_add(1, Ordering::Relaxed);
        }
        let from = SocketAddrV4::new(*t.dst.ip(), t.src.port());
        let to = SocketAddrV4::new(*t.src.ip(), listen);
        self.inject_tcp(pkt, addr, from, to)
    }

    fn inject_tcp(
        &self,
        pkt: &mut [u8],
        addr: &Address,
        from: SocketAddrV4,
        to: SocketAddrV4,
    ) -> bool {
        if rewrite_tcp_v4(pkt, from, to).is_none() {
            return false;
        }
        if let Err(e) = self.net.send(pkt, &addr.turned_inbound_tcp())
            && self.inject_errors.fetch_add(1, Ordering::Relaxed) == 0
        {
            warn!("tcp inject failed: {e:#}");
        }
        true
    }

    fn inject(&self, d: Downstream<'_>) {
        let Some((if_idx, sub_if_idx)) = self.flow_if.lock().get(&d.key).copied() else {
            return;
        };
        // Answer must come "from" a peer of the same family as the program's socket.
        let from: SocketAddr = match (d.from, d.key.local) {
            (SocketAddr::V4(_), SocketAddr::V4(_)) | (SocketAddr::V6(_), SocketAddr::V6(_)) => {
                d.from
            }
            _ => return,
        };
        thread_local! {
            static BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::with_capacity(2048));
        }
        BUF.with_borrow_mut(|buf| {
            if build_udp(from, d.key.local, d.payload, buf).is_none() {
                return;
            }
            let addr = Address::inbound(if_idx, sub_if_idx, from.is_ipv6());
            match self.net.send(buf, &addr) {
                Ok(()) => {
                    self.injected.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    if self.inject_errors.fetch_add(1, Ordering::Relaxed) == 0 {
                        warn!("inject failed: {e:#}");
                    }
                }
            }
        });
    }

    fn socket_loop(&self, sock: &Handle) {
        let mut addr = Address::default();
        loop {
            if let Err(e) = sock.recv(&mut [], &mut addr) {
                if !self.stopping.load(Ordering::SeqCst) {
                    warn!("socket events stopped: {e:#}");
                }
                return;
            }
            let proto = addr.protocol();
            if proto != UDP && proto != TCP {
                continue;
            }
            let (port, pid) = (addr.local_port(), addr.process_id());
            match addr.event() {
                wd::EVENT_SOCKET_BIND | wd::EVENT_SOCKET_CONNECT => {
                    self.procs.on_bind(proto, port, pid)
                }
                wd::EVENT_SOCKET_CLOSE => {
                    self.procs.on_close(proto, port, pid);
                    if proto == UDP && self.preexisting.lock().remove(&port) {
                        debug!("pre-existing socket :{port} closed; new sockets will be tunnelled");
                    }
                }
                _ => {}
            }
        }
    }
}
