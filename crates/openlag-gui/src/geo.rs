//! Where a server really exits: ask Cloudflare through the server itself.
//!
//! `http://1.1.1.1/cdn-cgi/trace` answers with the client IP it sees and that IP's
//! country (`loc=`), plus the Cloudflare data centre that served it (`colo=`). Going
//! through the server's SOCKS port, that is the server's exit, whatever its name says.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Re-check a server's exit after this long (auto servers may move).
pub const STALE_SECS: u64 = 6 * 3600;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Geo {
    pub ip: String,
    /// ISO 3166 alpha-2, upper case (e.g. "PL").
    pub country: String,
    /// Cloudflare data centre (IATA code) nearest to the exit, e.g. "WAW".
    pub colo: String,
    pub checked_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GeoCache {
    pub servers: HashMap<String, Geo>,
}

impl GeoCache {
    pub fn load(path: &Path) -> GeoCache {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &PathBuf) {
        if let Ok(b) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(path, b);
        }
    }
}

pub async fn probe(socks: SocketAddr) -> Result<Geo> {
    tokio::time::timeout(Duration::from_secs(10), probe_inner(socks))
        .await
        .context("timed out")?
}

async fn probe_inner(socks: SocketAddr) -> Result<Geo> {
    let mut s = TcpStream::connect(socks).await?;
    s.write_all(&[5, 1, 0]).await?;
    let mut hello = [0u8; 2];
    s.read_exact(&mut hello).await?;
    anyhow::ensure!(hello == [5, 0], "SOCKS login refused");
    // By domain name, resolved at the exit. 1.1.1.1 itself only redirects to HTTPS;
    // cloudflare.com still answers /cdn-cgi/trace over plain HTTP.
    const HOST: &str = "cloudflare.com";
    let mut req = vec![5, 1, 0, 3, HOST.len() as u8];
    req.extend_from_slice(HOST.as_bytes());
    req.extend_from_slice(&80u16.to_be_bytes());
    s.write_all(&req).await?;
    let mut head = [0u8; 4];
    s.read_exact(&mut head).await?;
    anyhow::ensure!(head[1] == 0, "SOCKS connect failed ({})", head[1]);
    let skip = match head[3] {
        1 => 4,
        4 => 16,
        3 => s.read_u8().await? as usize,
        t => bail!("bad SOCKS address type {t}"),
    };
    let mut rest = vec![0u8; skip + 2];
    s.read_exact(&mut rest).await?;
    let get = format!(
        "GET /cdn-cgi/trace HTTP/1.1\r\nHost: {HOST}\r\nUser-Agent: Pingless\r\nConnection: close\r\n\r\n"
    );
    s.write_all(get.as_bytes()).await?;
    let mut body = Vec::new();
    s.read_to_end(&mut body).await?;
    let text = String::from_utf8_lossy(&body);
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name).and_then(|v| v.strip_prefix('=')))
            .map(|v| v.trim().to_string())
    };
    let (Some(ip), Some(country)) = (field("ip"), field("loc")) else {
        bail!("unexpected trace answer")
    };
    Ok(Geo {
        ip,
        country: country.to_uppercase(),
        colo: field("colo").unwrap_or_default(),
        checked_at: crate::settings::now(),
    })
}
