//! End-to-end tests on loopback: fake game server, exit, bridges, and lossy links.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use openlag_core::{Downstream, Engine, EngineConfig, ExitSpec, FlowKey, RouteSpec, Upstream};
use openlag_relay::{BridgeConfig, BridgeServer, ExitConfig, ExitServer};
use parking_lot::Mutex;
use rand::Rng;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

const PSK: &str = "test-psk";

fn local() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

/// Game server stand-in: echoes every datagram back to the sender.
async fn echo_server() -> SocketAddr {
    let s = UdpSocket::bind(local()).await.unwrap();
    let addr = s.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            if let Ok((n, from)) = s.recv_from(&mut buf).await {
                let _ = s.send_to(&buf[..n], from).await;
            }
        }
    });
    addr
}

async fn exit() -> SocketAddr {
    let server = ExitServer::bind(ExitConfig {
        listen: vec![local()],
        psk: PSK.into(),
        allow_private: true,
    })
    .await
    .unwrap();
    let addr = server.local_addrs()[0];
    tokio::spawn(server.run());
    addr
}

async fn bridge(exit: SocketAddr) -> (SocketAddr, JoinHandle<anyhow::Result<()>>) {
    let server = BridgeServer::bind(BridgeConfig {
        listen: vec![local()],
        exit,
    })
    .await
    .unwrap();
    let addr = server.local_addrs()[0];
    (addr, tokio::spawn(server.run()))
}

/// UDP forwarder for a single client that drops `loss` of packets in each direction.
async fn lossy_link(target: SocketAddr, loss: f64) -> SocketAddr {
    let front = Arc::new(UdpSocket::bind(local()).await.unwrap());
    let back = Arc::new(UdpSocket::bind(local()).await.unwrap());
    let addr = front.local_addr().unwrap();
    let client: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
    {
        let (front, back, client) = (front.clone(), back.clone(), client.clone());
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            loop {
                let Ok((n, from)) = front.recv_from(&mut buf).await else {
                    continue;
                };
                *client.lock() = Some(from);
                if !rand::rng().random_bool(loss) {
                    let _ = back.send_to(&buf[..n], target).await;
                }
            }
        });
    }
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            let Ok((n, _)) = back.recv_from(&mut buf).await else {
                continue;
            };
            let to = *client.lock();
            if let Some(to) = to
                && !rand::rng().random_bool(loss)
            {
                let _ = front.send_to(&buf[..n], to).await;
            }
        }
    });
    addr
}

fn route(name: &str, via: SocketAddr) -> RouteSpec {
    RouteSpec {
        name: name.into(),
        via,
        bind: None,
    }
}

fn config(routes: Vec<RouteSpec>, multipath: usize) -> EngineConfig {
    EngineConfig {
        psk: PSK.into(),
        exits: vec![ExitSpec {
            name: "test-exit".into(),
            routes,
        }],
        multipath,
        probe_interval_ms: 50,
        switch_margin_ms: 5.0,
        flow_idle_secs: 120,
    }
}

type Received = Arc<Mutex<HashMap<u32, usize>>>;

async fn start(cfg: EngineConfig) -> (Engine, Received) {
    let received: Received = Arc::default();
    let sink = received.clone();
    let engine = Engine::start(
        cfg,
        Arc::new(move |d: Downstream<'_>| {
            let n = u32::from_le_bytes(d.payload[..4].try_into().unwrap());
            *sink.lock().entry(n).or_default() += 1;
        }),
    )
    .await
    .unwrap();
    (engine, received)
}

async fn wait_alive(engine: &Engine, routes: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while engine.snapshot().paths.iter().filter(|p| p.active).count() < routes {
        assert!(
            Instant::now() < deadline,
            "routes did not come up: {:?}",
            engine.snapshot().paths
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn game_flow(server: SocketAddr) -> FlowKey {
    FlowKey {
        local: "10.0.0.2:50000".parse().unwrap(),
        remote: server,
    }
}

async fn send_numbered(engine: &Engine, key: FlowKey, range: std::ops::Range<u32>) {
    for n in range {
        let mut payload = n.to_le_bytes().to_vec();
        payload.extend_from_slice(&[0xAB; 60]);
        assert_eq!(engine.send_upstream(key, &payload), Upstream::Tunneled);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn multipath_survives_heavy_loss_and_never_duplicates() {
    let game = echo_server().await;
    let exit = exit().await;
    // Two independent routes, each losing 30% per direction (~51% round trip).
    let a = lossy_link(exit, 0.3).await;
    let (b_bridge, _b) = bridge(exit).await;
    let b = lossy_link(b_bridge, 0.3).await;

    let (engine, received) = start(config(vec![route("lossy-a", a), route("lossy-b", b)], 2)).await;
    wait_alive(&engine, 2).await;
    send_numbered(&engine, game_flow(game), 0..400).await;

    let got = received.lock();
    assert!(
        got.values().all(|&c| c == 1),
        "a packet was delivered twice"
    );
    // Single route round trip success is ~49%; with two routes it is ~74%.
    let ratio = got.len() as f64 / 400.0;
    assert!(
        ratio > 0.62,
        "delivery ratio {ratio:.2} is not better than a single lossy route"
    );
    let snap = engine.snapshot();
    assert!(
        snap.flows[0].duplicates > 0,
        "expected copies from both routes to be de-duplicated"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn traffic_moves_to_the_remaining_route_when_one_dies() {
    let game = echo_server().await;
    let exit = exit().await;
    let (a, a_task) = bridge(exit).await;
    let (b, _b_task) = bridge(exit).await;

    let (engine, received) =
        start(config(vec![route("bridge-a", a), route("bridge-b", b)], 1)).await;
    wait_alive(&engine, 1).await;
    let key = game_flow(game);
    send_numbered(&engine, key, 0..50).await;
    assert_eq!(received.lock().len(), 50);

    // Kill whichever bridge is active.
    let active = engine
        .snapshot()
        .paths
        .into_iter()
        .find(|p| p.active)
        .unwrap();
    if active.route == "bridge-a" {
        a_task.abort();
    } else {
        _b_task.abort();
    }
    // Dead after 3 s without answers, then the other route takes over.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let now_active = engine
        .snapshot()
        .paths
        .into_iter()
        .find(|p| p.active)
        .unwrap();
    assert_ne!(now_active.route, active.route);

    received.lock().clear();
    send_numbered(&engine, key, 1000..1050).await;
    assert_eq!(
        received.lock().len(),
        50,
        "flow did not recover on the other route"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn flow_goes_direct_when_no_relay_answers() {
    // Nothing listens here.
    let dead: SocketAddr = {
        let s = std::net::UdpSocket::bind(local()).unwrap();
        s.local_addr().unwrap()
    };
    let (engine, _) = start(config(vec![route("dead", dead)], 2)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let key = game_flow("1.1.1.1:27015".parse().unwrap());
    assert_eq!(engine.send_upstream(key, b"x"), Upstream::Direct);
    // And it stays direct: switching later would change the IP the game server sees.
    assert_eq!(engine.send_upstream(key, b"x"), Upstream::Direct);
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_key_is_rejected_by_exit() {
    let game = echo_server().await;
    let exit = exit().await;
    let mut cfg = config(vec![route("direct", exit)], 1);
    cfg.psk = "not-the-psk".into();
    let (engine, received) = start(cfg).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!engine.any_route_alive());
    // Flow goes direct, nothing reaches the game through the exit.
    assert_eq!(
        engine.send_upstream(game_flow(game), &0u32.to_le_bytes()),
        Upstream::Direct
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(received.lock().is_empty());
}
