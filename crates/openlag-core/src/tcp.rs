//! TCP through proxy exits.
//!
//! The capture layer redirects a captured TCP connection to a local listener by
//! rewriting packets (the WinDivert "streamdump" trick): the app's packets arrive at
//! the listener looking like they come from `server_ip:client_port`. The listener looks
//! up where the connection was really going and opens it through the proxy's SOCKS5
//! port, then copies bytes both ways.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use parking_lot::Mutex;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info};

use crate::socks;

/// Unaccepted redirects (lost SYN, app gave up) are forgotten after this long.
const PENDING_TTL: Duration = Duration::from_secs(60);
/// After a connection ends its mapping is kept a little for late FIN/ACK packets.
const CLOSED_TTL: Duration = Duration::from_secs(30);
/// How long to wait for the program's TLS ClientHello before giving up on fallback.
const CLIENT_HELLO_WAIT: Duration = Duration::from_secs(2);
/// A server that does not answer a ClientHello through one exit this long is tried
/// through the next exit. Some sites drop whole VPN subnets: the connection then
/// hangs at the TLS handshake while other exits reach the site fine.
const SERVER_HELLO_WAIT: Duration = Duration::from_secs(5);
/// A site that needed another exit keeps using it this long.
const LEARNED_TTL: Duration = Duration::from_secs(600);
/// Exits tried per connection, the first one included.
pub(crate) const MAX_TRIES: usize = 3;

pub(crate) struct Entry {
    pub(crate) orig: SocketAddr,
    pub(crate) client: SocketAddr,
    pub(crate) socks: SocketAddr,
    pub(crate) exit: String,
    /// Other exits to try when the first one does not reach the server.
    alts: Vec<(SocketAddr, String)>,
    created: Instant,
    accepted: bool,
    closed_at: Option<Instant>,
    up: Arc<AtomicU64>,
    down: Arc<AtomicU64>,
    /// The task relaying this connection, to cut it when the selection changes.
    task: Option<tokio::task::AbortHandle>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TcpSnapshot {
    pub local: SocketAddr,
    pub remote: SocketAddr,
    pub exit: String,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub open: bool,
}

/// Redirect table keyed by the app's local port.
#[derive(Default)]
pub(crate) struct Table {
    map: Mutex<HashMap<u16, Entry>>,
    /// Server IP -> exit that reached it after the first choice did not.
    learned: Mutex<HashMap<IpAddr, (String, Instant)>>,
}

impl Table {
    pub(crate) fn insert(
        &self,
        client: SocketAddr,
        orig: SocketAddr,
        socks: SocketAddr,
        exit: String,
        alts: Vec<(SocketAddr, String)>,
    ) {
        self.map.lock().insert(
            client.port(),
            Entry {
                orig,
                client,
                socks,
                exit,
                alts,
                created: Instant::now(),
                accepted: false,
                closed_at: None,
                up: Arc::default(),
                down: Arc::default(),
                task: None,
            },
        );
    }

    /// Cuts the open connections whose app-side port `drop` picks: their relay task
    /// stops and the app sees the connection close, then reconnects under the new
    /// rules. Returns how many were cut.
    pub(crate) fn revoke(&self, drop: impl Fn(u16) -> bool) -> usize {
        let now = Instant::now();
        let mut n = 0;
        for (port, e) in self.map.lock().iter_mut() {
            if e.closed_at.is_none() && drop(*port) {
                if let Some(t) = e.task.take() {
                    t.abort();
                }
                e.closed_at = Some(now);
                n += 1;
            }
        }
        n
    }

    pub(crate) fn original(&self, client_port: u16) -> Option<SocketAddr> {
        self.map.lock().get(&client_port).map(|e| e.orig)
    }

    /// The exit that last reached `ip` after another one failed, if recent.
    #[allow(dead_code)]
    pub(crate) fn learned(&self, ip: IpAddr) -> Option<String> {
        let map = self.learned.lock();
        let (name, at) = map.get(&ip)?;
        (at.elapsed() < LEARNED_TTL).then(|| name.clone())
    }

    fn learn(&self, ip: IpAddr, exit: &str) {
        self.learned.lock().insert(ip, (exit.to_owned(), Instant::now()));
    }

    pub(crate) fn sweep(&self) {
        let now = Instant::now();
        self.learned.lock().retain(|_, (_, at)| now - *at < LEARNED_TTL);
        self.map.lock().retain(|_, e| match e.closed_at {
            Some(t) => now - t < CLOSED_TTL,
            None => e.accepted || now - e.created < PENDING_TTL,
        });
    }

    pub(crate) fn snapshot(&self) -> Vec<TcpSnapshot> {
        let mut v: Vec<_> = self
            .map
            .lock()
            .values()
            .map(|e| TcpSnapshot {
                local: e.client,
                remote: e.orig,
                exit: e.exit.clone(),
                bytes_up: e.up.load(Ordering::Relaxed),
                bytes_down: e.down.load(Ordering::Relaxed),
                open: e.closed_at.is_none(),
            })
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.bytes_up + s.bytes_down));
        v.truncate(200);
        v
    }
}

/// Binds the local listener on all IPv4 interfaces (redirected packets arrive on the
/// interface the app used).
pub(crate) async fn bind() -> Result<TcpListener> {
    Ok(TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).await?)
}

pub(crate) async fn accept_loop(listener: TcpListener, table: Arc<Table>) {
    loop {
        let (inbound, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                debug!("tcp accept: {e}");
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
        };
        let table2 = table.clone();
        let task = tokio::spawn(async move {
            let table = table2;
            let found = {
                let mut map = table.map.lock();
                map.get_mut(&peer.port()).map(|e| {
                    e.accepted = true;
                    let mut route = vec![(e.socks, e.exit.clone())];
                    route.extend(e.alts.iter().cloned());
                    (e.orig, route, e.up.clone(), e.down.clone())
                })
            };
            let Some((orig, route, up, down)) = found else {
                debug!("tcp from {peer}: no redirect entry");
                return;
            };
            let mut inbound = inbound;
            let _ = inbound.set_nodelay(true);
            match open(&mut inbound, orig, &route).await {
                Ok((outbound, used)) => {
                    if used > 0 {
                        let name = &route[used].1;
                        info!("{orig}: {} did not answer, using {name}", route[0].1);
                        table.learn(orig.ip(), name);
                        if let Some(e) = table.map.lock().get_mut(&peer.port()) {
                            e.exit = name.clone();
                        }
                    }
                    pump(inbound, outbound, &up, &down).await
                }
                Err(e) => debug!("tcp {peer} -> {orig}: {e:#}"),
            }
            if let Some(e) = table.map.lock().get_mut(&peer.port()) {
                e.closed_at = Some(Instant::now());
            }
        });
        if let Some(e) = table.map.lock().get_mut(&peer.port())
            && e.closed_at.is_none()
        {
            e.task = Some(task.abort_handle());
        }
    }
}

/// Connects to `orig` through the first exit of `route` that answers. For TLS the
/// program's ClientHello is read first and replayed to the next exit when the server
/// stays silent; other protocols just use the first exit. Returns the outbound
/// stream (the server's first bytes already passed on to the program) and the index
/// of the exit used.
async fn open(
    inbound: &mut TcpStream,
    orig: SocketAddr,
    route: &[(SocketAddr, String)],
) -> Result<(TcpStream, usize)> {
    let hello = if route.len() > 1 {
        tokio::time::timeout(CLIENT_HELLO_WAIT, read_client_hello(inbound))
            .await
            .unwrap_or(Ok(Vec::new()))?
    } else {
        Vec::new()
    };
    let is_tls = hello.len() > 5 && hello[0] == 0x16 && hello[1] == 3;
    let mut last_err = None;
    let mut buf = vec![0u8; 16 * 1024];
    for (i, (socks_addr, name)) in route.iter().enumerate() {
        let last = i + 1 == route.len() || !is_tls;
        let mut out = match socks::connect(*socks_addr, orig).await {
            Ok(s) => s,
            Err(e) => {
                debug!("tcp -> {orig} via {name}: {e:#}");
                last_err = Some(e);
                if last {
                    break;
                }
                continue;
            }
        };
        if !hello.is_empty() {
            out.write_all(&hello).await?;
        }
        if last {
            return Ok((out, i));
        }
        match tokio::time::timeout(SERVER_HELLO_WAIT, out.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {
                inbound.write_all(&buf[..n]).await?;
                return Ok((out, i));
            }
            _ => debug!("tcp -> {orig}: no answer via {name}"),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no exit reached {orig}")))
}

/// Reads what the program sends first; for TLS, the whole first record (a
/// post-quantum ClientHello spans several TCP segments). A timeout around this
/// covers programs that wait for the server to speak first.
async fn read_client_hello(inbound: &mut TcpStream) -> Result<Vec<u8>> {
    let mut data = vec![0u8; 16 * 1024];
    let mut have = inbound.read(&mut data).await?;
    if have == 0 {
        anyhow::bail!("program closed the connection");
    }
    if have >= 5 && data[0] == 0x16 && data[1] == 3 {
        let want = (5 + u16::from_be_bytes([data[3], data[4]]) as usize).min(data.len());
        while have < want {
            let n = inbound.read(&mut data[have..want]).await?;
            if n == 0 {
                break;
            }
            have += n;
        }
    }
    data.truncate(have);
    Ok(data)
}

async fn pump(a: TcpStream, b: TcpStream, up: &AtomicU64, down: &AtomicU64) {
    let (mut ar, mut aw) = a.into_split();
    let (mut br, mut bw) = b.into_split();
    let upstream = async {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            let n = match ar.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if bw.write_all(&buf[..n]).await.is_err() {
                break;
            }
            up.fetch_add(n as u64, Ordering::Relaxed);
        }
        let _ = bw.shutdown().await;
    };
    let downstream = async {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            let n = match br.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if aw.write_all(&buf[..n]).await.is_err() {
                break;
            }
            down.fetch_add(n as u64, Ordering::Relaxed);
        }
        let _ = aw.shutdown().await;
    };
    tokio::join!(upstream, downstream);
}

#[cfg(test)]
mod revoke_tests {
    use super::*;

    #[tokio::test]
    async fn revoke_cuts_only_the_picked_connections() {
        let t = Table::default();
        let addr = |p: u16| SocketAddr::from(([10, 0, 0, 2], p));
        let server: SocketAddr = "1.2.3.4:443".parse().unwrap();
        let socks: SocketAddr = "127.0.0.1:1080".parse().unwrap();
        t.insert(addr(5000), server, socks, "x".into(), Vec::new());
        t.insert(addr(5001), server, socks, "x".into(), Vec::new());
        let long = tokio::spawn(std::future::pending::<()>());
        t.map.lock().get_mut(&5000).unwrap().task = Some(long.abort_handle());
        assert_eq!(t.revoke(|port| port == 5000), 1);
        assert!(long.await.unwrap_err().is_cancelled());
        let snap = t.snapshot();
        let open = |p: u16| snap.iter().find(|s| s.local.port() == p).unwrap().open;
        assert!(!open(5000));
        assert!(open(5001));
        // Already cut: not counted again.
        assert_eq!(t.revoke(|_| true), 1);
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;

    /// Minimal SOCKS5 server: accepts CONNECT, then answers the first bytes with
    /// `reply` (or stays silent when it is None).
    async fn fake_socks(reply: Option<&'static [u8]>) -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut b = [0u8; 512];
                    let _ = s.read(&mut b).await; // greeting
                    s.write_all(&[5, 0]).await.unwrap();
                    let _ = s.read(&mut b).await; // CONNECT
                    s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
                    let _ = s.read(&mut b).await; // ClientHello
                    match reply {
                        Some(r) => s.write_all(r).await.unwrap(),
                        None => std::future::pending::<()>().await,
                    }
                    std::future::pending::<()>().await;
                });
            }
        });
        addr
    }

    #[tokio::test]
    #[ignore = "fallback between servers is off: one server only"]
    async fn silent_exit_falls_back_to_the_next() {
        let dead = fake_socks(None).await;
        let good = fake_socks(Some(b"server-hello")).await;
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut app = TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
        let (mut inbound, _) = l.accept().await.unwrap();
        // TLS record header + 4-byte body.
        app.write_all(&[0x16, 3, 1, 0, 4, 1, 2, 3, 4]).await.unwrap();
        let route = vec![(dead, "dead".to_string()), (good, "good".to_string())];
        let orig: SocketAddr = "1.2.3.4:443".parse().unwrap();
        let (_out, used) = open(&mut inbound, orig, &route).await.unwrap();
        assert_eq!(used, 1);
        let mut got = [0u8; 12];
        app.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"server-hello");
    }

    #[tokio::test]
    async fn plain_protocols_stay_on_the_first_exit() {
        let first = fake_socks(None).await;
        let other = fake_socks(Some(b"x")).await;
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut app = TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
        let (mut inbound, _) = l.accept().await.unwrap();
        app.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        let route = vec![(first, "a".to_string()), (other, "b".to_string())];
        let (_out, used) = open(&mut inbound, "1.2.3.4:80".parse().unwrap(), &route)
            .await
            .unwrap();
        assert_eq!(used, 0);
    }
}
