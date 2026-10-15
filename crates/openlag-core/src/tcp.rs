//! TCP through proxy exits.
//!
//! The capture layer redirects a captured TCP connection to a local listener by
//! rewriting packets (the WinDivert "streamdump" trick): the app's packets arrive at
//! the listener looking like they come from `server_ip:client_port`. The listener looks
//! up where the connection was really going and opens it through the proxy's SOCKS5
//! port, then copies bytes both ways.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use parking_lot::Mutex;
use serde::Serialize;
use tokio::net::{TcpListener, TcpStream};
use tracing::debug;

use crate::socks;

/// Unaccepted redirects (lost SYN, app gave up) are forgotten after this long.
const PENDING_TTL: Duration = Duration::from_secs(60);
/// After a connection ends its mapping is kept a little for late FIN/ACK packets.
const CLOSED_TTL: Duration = Duration::from_secs(30);

pub(crate) struct Entry {
    pub(crate) orig: SocketAddr,
    pub(crate) client: SocketAddr,
    pub(crate) socks: SocketAddr,
    pub(crate) exit: String,
    created: Instant,
    accepted: bool,
    closed_at: Option<Instant>,
    up: Arc<AtomicU64>,
    down: Arc<AtomicU64>,
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
}

impl Table {
    pub(crate) fn insert(
        &self,
        client: SocketAddr,
        orig: SocketAddr,
        socks: SocketAddr,
        exit: String,
    ) {
        self.map.lock().insert(
            client.port(),
            Entry {
                orig,
                client,
                socks,
                exit,
                created: Instant::now(),
                accepted: false,
                closed_at: None,
                up: Arc::default(),
                down: Arc::default(),
            },
        );
    }

    pub(crate) fn original(&self, client_port: u16) -> Option<SocketAddr> {
        self.map.lock().get(&client_port).map(|e| e.orig)
    }

    pub(crate) fn sweep(&self) {
        let now = Instant::now();
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
        let table = table.clone();
        tokio::spawn(async move {
            let found = {
                let mut map = table.map.lock();
                map.get_mut(&peer.port()).map(|e| {
                    e.accepted = true;
                    (e.orig, e.socks, e.up.clone(), e.down.clone())
                })
            };
            let Some((orig, socks_addr, up, down)) = found else {
                debug!("tcp from {peer}: no redirect entry");
                return;
            };
            let _ = inbound.set_nodelay(true);
            match socks::connect(socks_addr, orig).await {
                Ok(outbound) => pump(inbound, outbound, &up, &down).await,
                Err(e) => debug!("tcp {peer} -> {orig}: {e:#}"),
            }
            if let Some(e) = table.map.lock().get_mut(&peer.port()) {
                e.closed_at = Some(Instant::now());
            }
        });
    }
}

async fn pump(a: TcpStream, b: TcpStream, up: &AtomicU64, down: &AtomicU64) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
