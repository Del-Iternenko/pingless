//! Game UDP through a real xray chain on loopback:
//! engine -> SOCKS5 UDP -> xray client -> VLESS -> xray "VPN server" -> echo server.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use openlag_core::{Downstream, Engine, EngineConfig, FlowKey, ProxySpec, Upstream};
use openlag_proxy::{Xray, parse_link};
use parking_lot::Mutex;
use tokio::net::UdpSocket;
use tokio::process::Command;

type Received = Arc<Mutex<Vec<(SocketAddr, Vec<u8>)>>>;

fn xray_path() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/xray/xray.exe");
    p.exists().then_some(p)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_through_vless_server() {
    let Some(xray) = xray_path() else {
        eprintln!("xray.exe not found, skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("openlag-vless-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // The "VPN provider": VLESS inbound, direct outbound.
    let uuid = "0b1c2d3e-0000-4000-8000-000000000001";
    let server_port = free_port();
    let server_cfg = serde_json::json!({
        "log": { "loglevel": "warning" },
        "inbounds": [{
            "listen": "127.0.0.1", "port": server_port, "protocol": "vless",
            "settings": { "clients": [{ "id": uuid }], "decryption": "none" },
        }],
        "outbounds": [{ "protocol": "freedom" }],
    });
    let server_path = dir.join("server.json");
    std::fs::write(&server_path, server_cfg.to_string()).unwrap();
    let _server = Command::new(&xray)
        .arg("run")
        .arg("-c")
        .arg(&server_path)
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    // Game server stand-in.
    let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        while let Ok((n, from)) = echo.recv_from(&mut buf).await {
            let _ = echo.send_to(&buf[..n], from).await;
        }
    });

    // The subscription link the user would paste.
    let link = format!("vless://{uuid}@127.0.0.1:{server_port}?type=tcp&security=none#Local VLESS");
    let server = parse_link(&link).unwrap();
    let client = Xray::start(&xray, &dir.join("client"), std::slice::from_ref(&server))
        .await
        .unwrap();
    let socks = client.socks[0].unwrap();

    let received: Received = Arc::default();
    let sink = received.clone();
    let engine = Engine::start(
        EngineConfig {
            psk: String::new(),
            exits: vec![],
            proxies: vec![ProxySpec {
                name: server.name.clone(),
                socks,
            }],
            preferred_exit: None,
            proxy_probe_target: None,
            multipath: 1,
            probe_interval_ms: 250,
            switch_margin_ms: 5.0,
            flow_idle_secs: 120,
        },
        Arc::new(move |d: Downstream<'_>| sink.lock().push((d.from, d.payload.to_vec()))),
    )
    .await
    .unwrap();

    // Flows only use an exit that answers probes, and probes go to 1.1.1.1 through
    // the proxy, so this part needs Internet access.
    let deadline = Instant::now() + Duration::from_secs(8);
    while !engine.any_route_alive() {
        if Instant::now() > deadline {
            eprintln!("proxy probe got no answer from 1.1.1.1 (offline?), skipping");
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let key = FlowKey {
        local: "10.0.0.2:50000".parse().unwrap(),
        remote: echo_addr,
    };
    for i in 0u32..20 {
        assert_eq!(
            engine.send_upstream(key, &i.to_le_bytes()),
            Upstream::Tunneled
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let got = received.lock();
    assert!(got.len() >= 19, "only {} of 20 echoes came back", got.len());
    assert!(
        got.iter().all(|(from, _)| *from == echo_addr),
        "answers must look like they come from the game server"
    );
    let snap = engine.snapshot();
    assert_eq!(snap.flows[0].exit.as_deref(), Some("Local VLESS"));
    drop(client);
    let _ = std::fs::remove_dir_all(dir);
}
