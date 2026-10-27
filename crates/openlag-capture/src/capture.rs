use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

use anyhow::Result;
use openlag_core::{Downstream, Engine, FlowKey, Upstream};
use parking_lot::{Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::packet::{
    TcpInfo, build_tcp_rst_v4, build_udp, is_local_scope, parse_tcp_v4, parse_udp,
    rewrite_tcp_v4,
};
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
    /// Preferred exit per executable name (the server of the program's group).
    pub exit_for: HashMap<String, String>,
    /// When no exit is reachable, drop the selected programs' traffic instead of
    /// letting it go direct.
    pub kill_switch: bool,
    /// Programs whose TCP stays direct: only their UDP (the game itself) is tunnelled,
    /// while launchers, downloads and chat over TCP keep the full home bandwidth.
    pub udp_only: Vec<String>,
    /// Programs that always go through the tunnel and never leave directly: while the
    /// tunnel is down (restart, dead server, no exit) their traffic is dropped, and
    /// connections they opened outside the tunnel are reset so they reopen inside it.
    pub protect: Vec<String>,
}

/// Our own tunnel process, never captured (its pid changes on every restart).
const OWN_XRAY: &str = "pingless-xray.exe";

#[derive(Debug, Default, Clone, Copy)]
pub struct CaptureStats {
    pub tunneled: u64,
    pub direct: u64,
    pub injected: u64,
    pub inject_errors: u64,
    pub tcp_redirected: u64,
    /// Packets dropped by the kill switch.
    pub blocked: u64,
}

struct Shared {
    names: HashSet<String>,
    mode: Selection,
    exclude_pids: RwLock<HashSet<u32>>,
    protect: HashSet<String>,
    /// None while the tunnel is being rebuilt or could not start: protected and
    /// kill-switched programs are then blocked instead of going direct.
    engine: RwLock<Option<Arc<Engine>>>,
    exit_for: HashMap<String, String>,
    kill_switch: bool,
    udp_only: HashSet<String>,
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
    blocked: AtomicU64,
    last_direct_note: Mutex<Option<std::time::Instant>>,
    last_reset_note: Mutex<Option<std::time::Instant>>,
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
            exclude_pids: RwLock::new(exclude_pids),
            protect: cfg
                .protect
                .iter()
                .map(|p| p.trim().to_lowercase())
                .filter(|p| !p.is_empty())
                .collect(),
            engine: RwLock::new(None),
            exit_for: cfg
                .exit_for
                .iter()
                .map(|(k, v)| (k.to_lowercase(), v.clone()))
                .collect(),
            kill_switch: cfg.kill_switch,
            udp_only: cfg.udp_only.iter().map(|p| p.to_lowercase()).collect(),
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
            blocked: AtomicU64::new(0),
            last_direct_note: Mutex::default(),
            last_reset_note: Mutex::default(),
        });

        for (port, pid) in port_table(TCP) {
            shared.procs.on_bind(TCP, port, pid);
        }
        let mut existing = Vec::new();
        for (port, pid) in port_table(UDP) {
            shared.procs.on_bind(UDP, port, pid);
            if shared.selected(pid).is_some_and(|n| !shared.strict(&n)) {
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

    /// Starts the capture threads. Without an engine protected programs are blocked
    /// until [`Capture::set_engine`] brings the tunnel.
    pub fn start(&mut self, engine: Option<Arc<Engine>>) {
        self.set_engine(engine, None);
        let shared = self.shared.clone();
        self.threads.push(
            std::thread::Builder::new()
                .name("openlag-net".into())
                .spawn(move || shared.network_loop())
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
        if !self.shared.protect.is_empty() {
            info!("never direct: {:?}", self.shared.protect);
        }
    }

    /// Swaps the tunnel under a running capture. None blocks protected programs (the
    /// capture itself stays on, so nothing slips out while the tunnel restarts).
    /// `xray_pid` is the new tunnel process, excluded from capture.
    pub fn set_engine(&self, engine: Option<Arc<Engine>>, xray_pid: Option<u32>) {
        if let Some(pid) = xray_pid {
            self.shared.exclude_pids.write().insert(pid);
        }
        let Some(engine) = engine else {
            *self.shared.engine.write() = None;
            return;
        };
        // Connections tunnelled under the previous selection or mode stay in the tunnel
        // until they close, and browsers, launchers and chats keep theirs open for
        // hours: it looked as if both modes ran at once. Cut those that no longer
        // qualify; the programs reconnect under the new rules.
        let s = &self.shared;
        // Only programs we can name are judged: a failed name lookup must not cut a
        // program that is still selected (its app then sees ECONNREFUSED/RESET).
        let cut = engine.tcp_revoke(|port| match s.procs.pid_for_port(TCP, port) {
            Some(pid) if s.procs.name(pid).is_some() => s
                .selected(pid)
                .is_none_or(|name| s.udp_only.contains(&name)),
            _ => false,
        });
        if cut > 0 {
            info!("closed {cut} tunnelled TCP connection(s) of programs no longer selected");
        }
        *self.shared.engine.write() = Some(engine);
    }

    pub fn stats(&self) -> CaptureStats {
        let s = &self.shared;
        CaptureStats {
            tunneled: s.tunneled.load(Ordering::Relaxed),
            direct: s.direct.load(Ordering::Relaxed),
            injected: s.injected.load(Ordering::Relaxed),
            inject_errors: s.inject_errors.load(Ordering::Relaxed),
            tcp_redirected: s.tcp_redirected.load(Ordering::Relaxed),
            blocked: s.blocked.load(Ordering::Relaxed),
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
        if self.exclude_pids.read().contains(&pid) {
            return None;
        }
        // Unknown name (protected or exited process): leave it alone in both modes.
        let name = self.procs.name(pid)?;
        if name == OWN_XRAY {
            return None;
        }
        if self.protect.contains(&name) {
            return Some(name);
        }
        let listed = self.names.contains(&name);
        match self.mode {
            Selection::Only => listed.then_some(name),
            Selection::AllExcept => (!listed).then_some(name),
        }
    }

    /// Must never leave outside the tunnel.
    fn strict(&self, name: &str) -> bool {
        self.kill_switch || self.protect.contains(name)
    }

    fn network_loop(&self) {
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
            let handled = {
                let engine = self.engine.read();
                match proto {
                    UDP => self.try_udp(engine.as_deref(), pkt, &addr),
                    TCP => self.try_tcp(engine.as_deref(), pkt, &addr),
                    _ => false,
                }
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
    fn try_udp(&self, engine: Option<&Engine>, pkt: &[u8], addr: &Address) -> bool {
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
        let strict = self.strict(&name);
        if !strict && self.preexisting.lock().contains(&port) {
            self.direct.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let Some(engine) = engine else {
            // Tunnel restarting or down.
            if strict {
                self.blocked.fetch_add(1, Ordering::Relaxed);
            }
            return strict;
        };
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
        let prefer = self.exit_for.get(&name).map(String::as_str);
        match engine.send_upstream_via(key, udp.payload, prefer) {
            Upstream::Tunneled => {
                self.tunneled.fetch_add(1, Ordering::Relaxed);
                true
            }
            // Kill switch: swallow the packet rather than leak it outside the tunnel.
            Upstream::Direct if strict => {
                self.blocked.fetch_add(1, Ordering::Relaxed);
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
    fn try_tcp(&self, engine: Option<&Engine>, pkt: &mut [u8], addr: &Address) -> bool {
        if !addr.outbound() || addr.loopback() {
            return false;
        }
        let Some(t) = parse_tcp_v4(pkt) else {
            // IPv6 TCP is not tunnelled: protected programs must use IPv4.
            return self.block_v6_tcp(pkt);
        };
        let listen = engine.and_then(|e| e.tcp_listen_port());
        let (Some(engine), Some(listen)) = (engine, listen) else {
            return self.guard_tcp(pkt, addr, &t);
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
        let mut redirect = known;
        if !known && t.syn && !t.ack && !is_local_scope((*t.dst.ip()).into()) {
            let name = self
                .syn_owner(t.src.port())
                .filter(|name| !self.udp_only.contains(name));
            if let Some(name) = name {
                let prefer = self.exit_for.get(&name).map(String::as_str);
                redirect =
                    engine.tcp_open_via(SocketAddr::V4(t.src), SocketAddr::V4(t.dst), prefer);
                if !redirect {
                    self.note_direct(&name, t.dst);
                }
                if !redirect && self.strict(&name) {
                    // Kill switch: drop the SYN; the program sees no connectivity.
                    self.blocked.fetch_add(1, Ordering::Relaxed);
                    return true;
                }
            }
        }
        if !redirect {
            // A connection opened outside the tunnel (while it restarted, or before
            // capture started) by a program that must never go direct: reset it.
            return self.guard_tcp(pkt, addr, &t);
        }
        if !known {
            self.tcp_redirected.fetch_add(1, Ordering::Relaxed);
        }
        let from = SocketAddrV4::new(*t.dst.ip(), t.src.port());
        let to = SocketAddrV4::new(*t.src.ip(), listen);
        self.inject_tcp(pkt, addr, from, to)
    }

    /// Without a tunnel (or for a connection the tunnel does not know): drops the
    /// packets of protected programs, resetting established connections so the
    /// program reconnects at once instead of hanging. Others pass.
    fn guard_tcp(&self, pkt: &[u8], addr: &Address, t: &TcpInfo) -> bool {
        if is_local_scope((*t.dst.ip()).into()) {
            return false;
        }
        let owner = if t.syn && !t.ack {
            self.syn_owner(t.src.port())
        } else {
            self.procs
                .pid_for_port(TCP, t.src.port())
                .and_then(|pid| self.selected(pid))
        };
        let Some(name) = owner.filter(|n| self.strict(n) && !self.udp_only.contains(n)) else {
            return false;
        };
        self.blocked.fetch_add(1, Ordering::Relaxed);
        if !t.syn
            && !t.rst
            && let Some(rst) = build_tcp_rst_v4(pkt)
        {
            self.note_reset(&name, t.dst);
            if let Err(e) = self.net.send(&rst, &addr.turned_inbound_tcp()) {
                debug!("rst inject failed: {e:#}");
            }
        }
        true
    }

    /// Drops outbound IPv6 TCP of protected programs (they fall back to IPv4).
    fn block_v6_tcp(&self, pkt: &[u8]) -> bool {
        if pkt.len() < 44 || pkt[0] >> 4 != 6 || pkt[6] != TCP {
            return false;
        }
        let mut dst = [0u8; 16];
        dst.copy_from_slice(&pkt[24..40]);
        if is_local_scope(std::net::Ipv6Addr::from(dst).into()) {
            return false;
        }
        let port = u16::from_be_bytes([pkt[40], pkt[41]]);
        let blocked = self
            .procs
            .pid_for_port(TCP, port)
            .and_then(|pid| self.selected(pid))
            .is_some_and(|n| self.protect.contains(&n));
        if blocked {
            self.blocked.fetch_add(1, Ordering::Relaxed);
        }
        blocked
    }

    /// Logs (at most every 10 s) that a direct connection of a protected program was cut.
    fn note_reset(&self, name: &str, dst: SocketAddrV4) {
        let now = std::time::Instant::now();
        let mut last = self.last_reset_note.lock();
        if last.is_some_and(|t| now - t < std::time::Duration::from_secs(10)) {
            return;
        }
        *last = Some(now);
        info!("{name}: tcp to {dst} was outside the tunnel: reset, it reconnects through it");
    }

    /// Selected program that sends this SYN, if any. When the live map names nobody
    /// or an unselected owner, the system table is read again: a stale entry for a
    /// reused port otherwise sends a selected program's connections direct, and
    /// where the direct route is blocked they hang or are refused.
    fn syn_owner(&self, port: u16) -> Option<String> {
        let cached = self.procs.pid_for_port(TCP, port);
        if let Some(name) = cached.and_then(|pid| self.selected(pid)) {
            return Some(name);
        }
        let fresh = self.procs.recheck(TCP, port)?;
        if Some(fresh) == cached {
            return None;
        }
        let name = self.selected(fresh)?;
        info!("tcp :{port} belongs to {name} (pid {fresh}), not pid {cached:?}: tunnelled");
        Some(name)
    }

    /// Logs (at most every 10 s) that a selected program's TCP went direct.
    fn note_direct(&self, name: &str, dst: SocketAddrV4) {
        let now = std::time::Instant::now();
        let mut last = self.last_direct_note.lock();
        if last.is_some_and(|t| now - t < std::time::Duration::from_secs(10)) {
            return;
        }
        *last = Some(now);
        warn!("{name}: tcp to {dst} goes direct: no proxy exit is reachable");
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
