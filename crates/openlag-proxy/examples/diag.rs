//! Latency diagnostics for subscription servers.
//!
//! cargo run --release -p openlag-proxy --example diag -- <settings.json> [name filter...]
//!
//! For each server: UDP round trip through the server (DNS to 1.1.1.1, like a game
//! packet), and TCP "real delay" (connect + HTTP HEAD through the server, like a browser).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use openlag_proxy::{ProxyServer, Xray, XrayOptions, physical_interface};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let raw = std::fs::read(&args[0])?;
    let filters: Vec<String> = args[1..].iter().map(|s| s.to_lowercase()).collect();
    let mut servers: Vec<ProxyServer> = Vec::new();
    // Not OpenLag settings: links, a WireGuard .conf, an Amnezia key...
    let settings: serde_json::Value = match serde_json::from_slice::<serde_json::Value>(&raw) {
        Ok(v) if v.get("subscriptions").is_some() => v,
        _ => {
            let parsed = openlag_proxy::parse_text(&String::from_utf8_lossy(&raw));
            for s in &parsed.skipped {
                println!("skipped {}: {}", s.entry, s.reason);
            }
            servers = parsed.servers;
            serde_json::json!({ "subscriptions": [] })
        }
    };
    for sub in settings["subscriptions"].as_array().unwrap() {
        for s in sub["servers"].as_array().unwrap() {
            let s: ProxyServer = serde_json::from_value(s.clone())?;
            if s.address.starts_with("127.") {
                continue;
            }
            if filters.is_empty() || filters.iter().any(|f| s.name.to_lowercase().contains(f)) {
                servers.push(s);
            }
        }
    }
    let xray_exe = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/xray/xray.exe");
    let bind = if std::env::var("OPENLAG_NO_BIND").is_ok() {
        None
    } else {
        physical_interface()
    };
    println!("bind interface: {bind:?}");
    let opts = XrayOptions {
        bind_interface: bind.map(|i| i.name),
        ..Default::default()
    };
    let x = Xray::start_with(
        &xray_exe,
        &std::env::temp_dir().join("openlag-diag"),
        &servers,
        &opts,
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    println!(
        "{:<44} {:>18} {:>22} {:>14}",
        "server", "UDP rtt min/avg/max", "UDP jitter, loss", "TCP real delay"
    );
    for (i, s) in servers.iter().enumerate() {
        let Some(socks) = x.socks[i] else {
            println!(
                "{:<44} rejected: {}",
                s.name,
                x.errors[i].as_deref().unwrap_or("")
            );
            continue;
        };
        let udp = udp_rtts(socks, 12).await;
        let tcp = tcp_delay(socks).await;
        let ok: Vec<f64> = udp.iter().flatten().copied().collect();
        let loss = 100.0 * (udp.len() - ok.len()) as f64 / udp.len() as f64;
        let udp_txt = if ok.is_empty() {
            "no answer".to_string()
        } else {
            let avg = ok.iter().sum::<f64>() / ok.len() as f64;
            let min = ok.iter().cloned().fold(f64::MAX, f64::min);
            let max = ok.iter().cloned().fold(0.0, f64::max);
            format!("{min:.0}/{avg:.0}/{max:.0}")
        };
        let jitter = if ok.len() > 1 {
            ok.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (ok.len() - 1) as f64
        } else {
            0.0
        };
        let tcp_txt = match tcp {
            Ok(v) => format!("{v:.0} ms"),
            Err(e) => format!("err {e}"),
        };
        let name: String = s.name.chars().take(42).collect();
        println!(
            "{name:<44} {udp_txt:>18} {:>15.1} {loss:>4.0}% {tcp_txt:>14}",
            jitter
        );
    }
    // Which local address does xray use to reach the servers? (direct NIC vs a TUN)
    if let Some(pid) = x.pid() {
        println!(
            "\nxray pid {pid}: see its connections with Get-NetTCPConnection -OwningProcess {pid}"
        );
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
    Ok(())
}

/// DNS queries to 1.1.1.1 through a SOCKS5 UDP association, one every 150 ms.
async fn udp_rtts(socks: SocketAddr, n: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; n];
    let Ok(mut ctrl) = TcpStream::connect(socks).await else {
        return out;
    };
    let mut b = [0u8; 2];
    if ctrl.write_all(&[5, 1, 0]).await.is_err() || ctrl.read_exact(&mut b).await.is_err() {
        return out;
    }
    let _ = ctrl.write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0]).await;
    let mut r = [0u8; 10];
    if ctrl.read_exact(&mut r).await.is_err() || r[1] != 0 {
        return out;
    }
    let relay = SocketAddr::from(([r[4], r[5], r[6], r[7]], u16::from_be_bytes([r[8], r[9]])));
    let relay = if relay.ip().is_unspecified() {
        SocketAddr::new(socks.ip(), relay.port())
    } else {
        relay
    };
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    udp.connect(relay).await.unwrap();
    let mut buf = [0u8; 2048];
    for (i, slot) in out.iter_mut().enumerate() {
        let id = 0x4000 + i as u16;
        let mut pkt = vec![0, 0, 0, 1, 1, 1, 1, 1, 0, 53];
        pkt.extend_from_slice(&id.to_be_bytes());
        pkt.extend_from_slice(&[
            1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 3, b'o', b'n', b'e', 3, b'o', b'n', b'e', 3, b'o', b'n',
            b'e', 3, b'o', b'n', b'e', 0, 0, 1, 0, 1,
        ]);
        let t = Instant::now();
        let _ = udp.send(&pkt).await;
        let deadline = t + Duration::from_secs(2);
        while let Ok(Ok(n)) = tokio::time::timeout_at(deadline.into(), udp.recv(&mut buf)).await {
            if n > 12 && buf[10..12] == id.to_be_bytes() {
                *slot = Some(t.elapsed().as_secs_f64() * 1000.0);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    out
}

/// SOCKS CONNECT to 1.1.1.1:80 and time until the HTTP response starts.
async fn tcp_delay(socks: SocketAddr) -> anyhow::Result<f64> {
    let t = Instant::now();
    let mut s = TcpStream::connect(socks).await?;
    let mut b = [0u8; 2];
    s.write_all(&[5, 1, 0]).await?;
    s.read_exact(&mut b).await?;
    s.write_all(&[5, 1, 0, 1, 1, 1, 1, 1, 0, 80]).await?;
    let mut r = [0u8; 10];
    s.read_exact(&mut r).await?;
    anyhow::ensure!(r[1] == 0, "connect failed");
    s.write_all(b"HEAD / HTTP/1.1\r\nHost: 1.1.1.1\r\nConnection: close\r\n\r\n")
        .await?;
    let mut first = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut first)).await??;
    Ok(t.elapsed().as_secs_f64() * 1000.0)
}
