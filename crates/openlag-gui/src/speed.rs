//! Speed test through one server, like v2rayN's "real delay" and download test: HTTPS
//! requests go through the server's local SOCKS port, so they measure the whole path.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::Serialize;

#[derive(Serialize)]
pub struct Speed {
    /// Time of an HTTPS request on an open connection, ms.
    pub delay_ms: f64,
    /// Download speed, megabits per second.
    pub mbps: f64,
    pub bytes: u64,
}

const DELAY_URL: &str = "https://www.gstatic.com/generate_204";
const DOWN_URL: &str = "https://speed.cloudflare.com/__down?bytes=25000000";
/// The download stops after this long; enough to get past TCP slow start.
const DOWN_TIME: Duration = Duration::from_secs(7);

pub async fn run(socks: SocketAddr) -> Result<Speed> {
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("socks5h://{socks}"))?)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(25))
        .build()?;
    // The first request pays for the TLS handshake through the tunnel; the second one
    // reuses the connection and shows the bare round trip.
    client
        .get(DELAY_URL)
        .send()
        .await
        .context("server does not answer")?;
    let t = Instant::now();
    client.get(DELAY_URL).send().await?;
    let delay_ms = t.elapsed().as_secs_f64() * 1000.0;

    // Cloudflare serves at most 25 MB per request: fast links get several in a row.
    let start = Instant::now();
    let mut bytes = 0u64;
    'outer: while start.elapsed() < DOWN_TIME {
        let resp = client.get(DOWN_URL).send().await?.error_for_status()?;
        let mut stream = resp.bytes_stream();
        loop {
            let left = DOWN_TIME.saturating_sub(start.elapsed());
            match tokio::time::timeout(left, stream.next()).await {
                Ok(Some(chunk)) => bytes += chunk?.len() as u64,
                Ok(None) => break,
                Err(_) => break 'outer,
            }
        }
    }
    let secs = start.elapsed().as_secs_f64().max(0.001);
    Ok(Speed {
        delay_ms,
        mbps: bytes as f64 * 8.0 / secs / 1e6,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    /// Live test: OPENLAG_SPEED_CONF=<WireGuard .conf or links file> cargo test -- --ignored
    #[tokio::test]
    #[ignore]
    async fn speed_through_a_real_server() {
        let path = std::env::var("OPENLAG_SPEED_CONF").expect("OPENLAG_SPEED_CONF");
        let parsed = openlag_proxy::parse_text(&std::fs::read_to_string(path).unwrap());
        let xray =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/xray/xray.exe");
        let dir = std::env::temp_dir().join("openlag-speed-test");
        let x = openlag_proxy::Xray::start(&xray, &dir, &parsed.servers[..1])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let r = super::run(x.socks[0].unwrap()).await.unwrap();
        eprintln!(
            "delay {:.0} ms, {:.1} Mbit/s, {} bytes",
            r.delay_ms, r.mbps, r.bytes
        );
        assert!(r.bytes > 100_000);
    }
}

#[cfg(test)]
mod priority_tests {
    #[test]
    fn priority_can_be_raised_and_restored() {
        let mut child = std::process::Command::new("ping")
            .args(["-n", "20", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        assert_eq!(
            crate::sys::set_priority(pid, crate::sys::HIGH_PRIORITY),
            crate::sys::NORMAL_PRIORITY
        );
        assert_eq!(
            crate::sys::set_priority(pid, crate::sys::NORMAL_PRIORITY),
            crate::sys::HIGH_PRIORITY
        );
        assert!(
            crate::sys::processes()
                .iter()
                .any(|(p, n)| *p == pid && n == "ping.exe")
        );
        let _ = child.kill();
    }
}
