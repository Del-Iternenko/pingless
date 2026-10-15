use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

use anyhow::Result;
use openlag_core::{Downstream, Engine, FlowKey, Upstream};
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use crate::packet::{build_udp, is_local_scope, parse_udp};
use crate::procmap::{ProcessMap, udp_table};
use crate::windivert::{self as wd, Address, Handle};

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Executable names whose UDP traffic is tunnelled, e.g. `cs2.exe`.
    pub processes: Vec<String>,
    /// Also tunnel UDP sockets that were open before capture started. Off by default:
    /// moving a live match to another IP usually disconnects it.
    pub tunnel_existing: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CaptureStats {
    pub tunneled: u64,
    pub direct: u64,
    pub injected: u64,
    pub inject_errors: u64,
}

struct Shared {
    targets: HashSet<String>,
    procs: ProcessMap,
    /// Interface a flow was seen on, needed to inject answers.
    flow_if: Mutex<HashMap<FlowKey, (u32, u32)>>,
    /// Ports of target sockets that existed at start and stay direct.
    preexisting: Mutex<HashSet<u16>>,
    net: Arc<Handle>,
    stopping: AtomicBool,
    tunneled: AtomicU64,
    direct: AtomicU64,
    injected: AtomicU64,
    inject_errors: AtomicU64,
}

/// Captures outbound UDP of the configured processes and injects answers back.
pub struct Capture {
    shared: Arc<Shared>,
    sock: Arc<Handle>,
    threads: Vec<JoinHandle<()>>,
}

impl Capture {
    /// Opens the WinDivert handles. Requires Administrator rights and the driver files.
    pub fn open(cfg: &CaptureConfig) -> Result<Capture> {
        let net = Handle::open("outbound and udp and !loopback", wd::LAYER_NETWORK, 0, 0)?;
        let sock = Handle::open(
            "udp",
            wd::LAYER_SOCKET,
            0,
            wd::FLAG_SNIFF | wd::FLAG_RECV_ONLY,
        )?;
        let targets: HashSet<String> = cfg.processes.iter().map(|p| p.to_lowercase()).collect();
        let shared = Arc::new(Shared {
            targets,
            procs: ProcessMap::default(),
            flow_if: Mutex::default(),
            preexisting: Mutex::default(),
            net,
            stopping: AtomicBool::new(false),
            tunneled: AtomicU64::new(0),
            direct: AtomicU64::new(0),
            injected: AtomicU64::new(0),
            inject_errors: AtomicU64::new(0),
        });

        let mut existing = Vec::new();
        for (port, pid) in udp_table() {
            shared.procs.on_bind(port, pid);
            if shared
                .procs
                .name(pid)
                .is_some_and(|n| shared.targets.contains(&n))
            {
                existing.push(port);
            }
        }
        if !existing.is_empty() && !cfg.tunnel_existing {
            warn!(
                "{} UDP socket(s) of target processes were already open and will stay direct; \
                 restart the game (or the match) to tunnel it",
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

    /// Callback for [`Engine::start`]: injects packets from relays into the game.
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
        info!("capturing UDP of {:?}", self.shared.targets);
    }

    pub fn stats(&self) -> CaptureStats {
        let s = &self.shared;
        CaptureStats {
            tunneled: s.tunneled.load(Ordering::Relaxed),
            direct: s.direct.load(Ordering::Relaxed),
            injected: s.injected.load(Ordering::Relaxed),
            inject_errors: s.inject_errors.load(Ordering::Relaxed),
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
            let pkt = &buf[..len];
            if self.try_tunnel(engine, pkt, &addr) {
                continue;
            }
            // Not ours, or no relay: let the original packet go.
            if let Err(e) = self.net.send(pkt, &addr) {
                debug!("reinject failed: {e:#}");
            }
        }
    }

    /// Returns true if the engine took the packet and the original must be dropped.
    fn try_tunnel(&self, engine: &Engine, pkt: &[u8], addr: &Address) -> bool {
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
        let Some(pid) = self.procs.pid_for_port(port) else {
            return false;
        };
        if pid == std::process::id() {
            return false;
        }
        let Some(name) = self.procs.name(pid) else {
            return false;
        };
        if !self.targets.contains(&name) {
            return false;
        }
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
                info!("{name} (pid {pid}) {} -> {}", udp.src, udp.dst);
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

    fn inject(&self, d: Downstream<'_>) {
        let Some((if_idx, sub_if_idx)) = self.flow_if.lock().get(&d.key).copied() else {
            return;
        };
        // Answer must come "from" a peer of the same family as the game's socket.
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
            if addr.protocol() != 17 {
                continue;
            }
            let (port, pid) = (addr.local_port(), addr.process_id());
            match addr.event() {
                wd::EVENT_SOCKET_BIND | wd::EVENT_SOCKET_CONNECT => self.procs.on_bind(port, pid),
                wd::EVENT_SOCKET_CLOSE => {
                    self.procs.on_close(port, pid);
                    if self.preexisting.lock().remove(&port) {
                        debug!("pre-existing socket :{port} closed; new sockets will be tunnelled");
                    }
                }
                _ => {}
            }
        }
    }
}
