use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use openlag_proto::{Body, DataBody, Header, MAX_DATAGRAM, PacketCipher, SeqWindow, SessionKeys};
use parking_lot::Mutex;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tracing::{debug, info};

/// A client address stops receiving downstream copies after this long without traffic.
const PATH_TTL: Duration = Duration::from_secs(5);
/// Flows without traffic in either direction are closed after this long.
const FLOW_IDLE: Duration = Duration::from_secs(120);
/// Sessions without any flow and without traffic are dropped after this long.
const SESSION_IDLE: Duration = Duration::from_secs(300);
/// Hard limits so a single client cannot exhaust the relay.
const MAX_FLOWS_PER_SESSION: usize = 256;
const MAX_SESSIONS: usize = 4096;

#[derive(Debug, Clone)]
pub struct ExitConfig {
    /// One or more listen addresses. A second, high port helps on ISPs that throttle
    /// well-known UDP ports.
    pub listen: Vec<SocketAddr>,
    pub psk: String,
    /// Allow flows to private, loopback and link-local destinations. Off by default so
    /// the relay cannot be used to reach its own LAN.
    pub allow_private: bool,
}

struct Session {
    cipher: PacketCipher,
    state: Mutex<SessionState>,
}

struct SessionState {
    /// Client addresses (one per path), keyed by (listen socket index, address).
    paths: HashMap<(usize, SocketAddr), Instant>,
    flows: HashMap<u32, Flow>,
    last_seen: Instant,
}

struct Flow {
    socket: Arc<UdpSocket>,
    window: SeqWindow,
    last_active: Arc<Mutex<Instant>>,
    reader: JoinHandle<()>,
}

impl Drop for Flow {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

struct Exit {
    cfg: ExitConfig,
    keys: SessionKeys,
    sockets: Vec<Arc<UdpSocket>>,
    sessions: Mutex<HashMap<u64, Arc<Session>>>,
}

/// A bound exit node. `bind` first to learn the actual ports, then `run`.
pub struct ExitServer {
    inner: Arc<Exit>,
}

impl ExitServer {
    pub async fn bind(cfg: ExitConfig) -> Result<ExitServer> {
        let mut sockets = Vec::new();
        for addr in &cfg.listen {
            let s = UdpSocket::bind(addr)
                .await
                .with_context(|| format!("bind {addr}"))?;
            info!("exit listening on {}", s.local_addr()?);
            sockets.push(Arc::new(s));
        }
        let inner = Arc::new(Exit {
            keys: SessionKeys::from_psk(&cfg.psk),
            cfg,
            sockets,
            sessions: Mutex::default(),
        });
        Ok(ExitServer { inner })
    }

    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.inner
            .sockets
            .iter()
            .filter_map(|s| s.local_addr().ok())
            .collect()
    }

    pub async fn run(self) -> Result<()> {
        let exit = self.inner;
        let mut tasks = tokio::task::JoinSet::new();
        for idx in 0..exit.sockets.len() {
            let exit = exit.clone();
            tasks.spawn(async move { exit.recv_loop(idx).await });
        }
        let sweeper = exit.clone();
        tasks.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                sweeper.sweep();
            }
        });
        // Dropping the JoinSet (when this future is cancelled) aborts every task.
        if let Some(res) = tasks.join_next().await {
            res?;
        }
        anyhow::bail!("relay task stopped unexpectedly")
    }
}

pub async fn run_exit(cfg: ExitConfig) -> Result<()> {
    ExitServer::bind(cfg).await?.run().await
}

impl Exit {
    async fn recv_loop(self: Arc<Self>, idx: usize) {
        let socket = self.sockets[idx].clone();
        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            let (len, from) = match socket.recv_from(&mut buf).await {
                Ok(v) => v,
                // On Windows an ICMP port-unreachable surfaces as an error on the next
                // recv. It must never kill the loop.
                Err(e) => {
                    debug!("recv error on listener {idx}: {e}");
                    continue;
                }
            };
            if let Err(e) = self.handle(idx, from, &mut buf[..len]).await {
                debug!("drop packet from {from}: {e}");
            }
        }
    }

    async fn handle(self: &Arc<Self>, idx: usize, from: SocketAddr, pkt: &mut [u8]) -> Result<()> {
        let header = Header::decode(pkt)?;
        let existing = self.sessions.lock().get(&header.session).cloned();
        let (session, body) = match existing {
            Some(s) => {
                let body = s.cipher.open(pkt)?.1;
                (s, body)
            }
            None => {
                // Unknown session: authenticate first, register only on success, so
                // garbage cannot fill the session table.
                let s = self.new_session(header.session)?;
                let body = s.cipher.open(pkt)?.1;
                self.sessions
                    .lock()
                    .entry(header.session)
                    .or_insert_with(|| s.clone());
                (s, body)
            }
        };

        let now = Instant::now();
        {
            let mut st = session.state.lock();
            st.last_seen = now;
            st.paths.insert((idx, from), now);
        }

        match body {
            Body::Ping(p) => {
                let mut out = Vec::with_capacity(64);
                session.cipher.seal(&Body::Pong(p), &mut out)?;
                let _ = self.sockets[idx].send_to(&out, from).await;
            }
            Body::Data(d) => self.upstream(&session, d).await?,
            Body::Close { flow } => {
                session.state.lock().flows.remove(&flow);
            }
            Body::Pong(_) => {}
        }
        Ok(())
    }

    fn new_session(&self, id: u64) -> Result<Arc<Session>> {
        anyhow::ensure!(
            self.sessions.lock().len() < MAX_SESSIONS,
            "session limit reached"
        );
        Ok(Arc::new(Session {
            cipher: self.keys.session(id, true),
            state: Mutex::new(SessionState {
                paths: HashMap::new(),
                flows: HashMap::new(),
                last_seen: Instant::now(),
            }),
        }))
    }

    async fn upstream(self: &Arc<Self>, session: &Arc<Session>, d: DataBody<'_>) -> Result<()> {
        anyhow::ensure!(d.peer.port() != 0, "destination port 0");
        anyhow::ensure!(
            self.cfg.allow_private || is_public(d.peer.ip()),
            "destination {} is not public",
            d.peer
        );

        let socket = {
            let mut st = session.state.lock();
            if !st.flows.contains_key(&d.flow) {
                anyhow::ensure!(st.flows.len() < MAX_FLOWS_PER_SESSION, "flow limit reached");
                drop(st);
                let flow = self.open_flow(session, d.flow, d.peer)?;
                st = session.state.lock();
                st.flows.entry(d.flow).or_insert(flow);
            }
            let flow = st.flows.get_mut(&d.flow).unwrap();
            if !flow.window.accept(d.seq) {
                return Ok(()); // copy that arrived over another path
            }
            *flow.last_active.lock() = Instant::now();
            flow.socket.clone()
        };
        if let Err(e) = socket.send_to(d.payload, d.peer).await {
            debug!("send to game server {} failed: {e}", d.peer);
        }
        Ok(())
    }

    fn open_flow(
        self: &Arc<Self>,
        session: &Arc<Session>,
        flow_id: u32,
        peer: SocketAddr,
    ) -> Result<Flow> {
        let bind: SocketAddr = match peer {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let std_sock = std::net::UdpSocket::bind(bind)?;
        std_sock.set_nonblocking(true)?;
        let socket = Arc::new(UdpSocket::from_std(std_sock)?);
        let last_active = Arc::new(Mutex::new(Instant::now()));
        debug!(
            "session {:016x} flow {flow_id} -> {peer} via {}",
            session.cipher.session_id(),
            socket.local_addr()?
        );

        let reader = tokio::spawn({
            // Only the listener sockets, not the whole Exit: a strong reference here
            // would form a cycle (Exit -> session -> flow -> reader -> Exit).
            let listeners = self.sockets.clone();
            let session = Arc::downgrade(session);
            let socket = socket.clone();
            let last_active = last_active.clone();
            async move {
                let mut buf = vec![0u8; MAX_DATAGRAM];
                let mut out = Vec::with_capacity(2048);
                let mut seq = 0u64;
                loop {
                    let (len, src) = match socket.recv_from(&mut buf).await {
                        Ok(v) => v,
                        // ICMP unreachable from the game server shows up here on Windows.
                        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                        Err(_) => {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            continue;
                        }
                    };
                    let Some(session) = session.upgrade() else {
                        return;
                    };
                    *last_active.lock() = Instant::now();
                    seq += 1;
                    let body = Body::Data(DataBody {
                        flow: flow_id,
                        seq,
                        peer: src,
                        payload: &buf[..len],
                    });
                    if session.cipher.seal(&body, &mut out).is_err() {
                        continue;
                    }
                    // Send a copy down every live path; the client keeps the first one.
                    let targets: Vec<_> = {
                        let st = session.state.lock();
                        let now = Instant::now();
                        st.paths
                            .iter()
                            .filter(|(_, t)| now - **t < PATH_TTL)
                            .map(|(k, _)| *k)
                            .collect()
                    };
                    for (idx, addr) in targets {
                        let _ = listeners[idx].send_to(&out, addr).await;
                    }
                }
            }
        });
        Ok(Flow {
            socket,
            window: SeqWindow::default(),
            last_active,
            reader,
        })
    }

    fn sweep(&self) {
        let now = Instant::now();
        let mut sessions = self.sessions.lock();
        sessions.retain(|id, s| {
            let mut st = s.state.lock();
            st.flows.retain(|flow, f| {
                let keep = now - *f.last_active.lock() < FLOW_IDLE;
                if !keep {
                    debug!("session {id:016x} flow {flow} idle, closing");
                }
                keep
            });
            st.paths.retain(|_, t| now - *t < SESSION_IDLE);
            !st.flows.is_empty() || now - st.last_seen < SESSION_IDLE
        });
    }
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_unspecified()
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64) // CGNAT 100.64/10
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(v4.into());
            }
            !(v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique local
                || (v6.segments()[0] & 0xffc0) == 0xfe80) // link local
        }
    }
}
