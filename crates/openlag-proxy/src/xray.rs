//! Running xray-core as a sidecar: one local SOCKS5 inbound (with UDP) per server.

use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

use crate::link::ProxyServer;

/// Turns a full Xray client config into a server entry, taking its proxy outbound.
pub fn server_from_config(cfg: &Value, name: &str) -> Result<ProxyServer> {
    let outbounds = cfg
        .get("outbounds")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("config has no outbounds"))?;
    let usable = |o: &&Value| {
        matches!(
            o.get("protocol").and_then(Value::as_str),
            Some("vless" | "vmess" | "trojan" | "shadowsocks")
        )
    };
    let outbound = outbounds
        .iter()
        .filter(usable)
        .find(|o| o.get("tag").and_then(Value::as_str) == Some("proxy"))
        .or_else(|| outbounds.iter().find(usable))
        .ok_or_else(|| anyhow!("no vless/vmess/trojan/shadowsocks outbound in config"))?;
    let protocol = outbound["protocol"].as_str().unwrap().to_string();
    let target = outbound
        .pointer("/settings/vnext/0")
        .or_else(|| outbound.pointer("/settings/servers/0"))
        .or_else(|| outbound.get("settings"))
        .ok_or_else(|| anyhow!("outbound has no server"))?;
    let address = target
        .get("address")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("outbound has no address"))?
        .to_string();
    let port = target
        .get("port")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("outbound has no port"))? as u16;
    let mut outbound = outbound.clone();
    if let Some(o) = outbound.as_object_mut() {
        o.remove("tag");
        // Mux would merge game UDP into one stream and add head-of-line blocking.
        o.remove("mux");
    }
    let name = if name.trim().is_empty() {
        format!("{address}:{port}")
    } else {
        name.trim().to_string()
    };
    Ok(ProxyServer {
        name,
        protocol,
        address,
        port,
        outbound,
    })
}

/// Xray config with one SOCKS inbound per server, routed to that server only.
pub fn build_config(servers: &[(ProxyServer, u16)]) -> Value {
    let mut inbounds = Vec::new();
    let mut outbounds = Vec::new();
    let mut rules = Vec::new();
    for (i, (server, port)) in servers.iter().enumerate() {
        inbounds.push(json!({
            "tag": format!("in-{i}"),
            "listen": "127.0.0.1",
            "port": port,
            "protocol": "socks",
            "settings": { "auth": "noauth", "udp": true, "ip": "127.0.0.1" },
        }));
        let mut out = server.outbound.clone();
        out["tag"] = json!(format!("out-{i}"));
        outbounds.push(out);
        rules.push(json!({ "type": "field", "inboundTag": [format!("in-{i}")], "outboundTag": format!("out-{i}") }));
    }
    outbounds.push(json!({ "tag": "block", "protocol": "blackhole" }));
    json!({
        "log": { "loglevel": "warning" },
        "inbounds": inbounds,
        "outbounds": outbounds,
        "routing": { "rules": rules },
    })
}

/// A running xray process. Killed when dropped.
pub struct Xray {
    child: Child,
    /// SOCKS5 address for each server that passed validation, same order as input.
    pub socks: Vec<Option<SocketAddr>>,
    /// Why a server was left out.
    pub errors: Vec<Option<String>>,
}

impl Xray {
    /// Validates every server on its own (a broken entry must not take down the
    /// rest), then starts one xray with all valid ones.
    pub async fn start(xray: &Path, work_dir: &Path, servers: &[ProxyServer]) -> Result<Xray> {
        anyhow::ensure!(xray.exists(), "xray.exe not found at {}", xray.display());
        std::fs::create_dir_all(work_dir)?;

        // Check all servers in parallel; `xray -test` takes ~100 ms each.
        let mut set = tokio::task::JoinSet::new();
        for (i, s) in servers.iter().enumerate() {
            let path = work_dir.join(format!("check-{i}.json"));
            let cfg = build_config(&[(s.clone(), 0)]);
            let xray = xray.to_path_buf();
            set.spawn(async move {
                let r = test_config(&xray, &path, &cfg).await;
                let _ = std::fs::remove_file(&path);
                (i, r)
            });
        }
        let mut results: Vec<Result<()>> = (0..servers.len())
            .map(|_| Err(anyhow!("not checked")))
            .collect();
        while let Some(joined) = set.join_next().await {
            let (i, r) = joined?;
            results[i] = r;
        }

        let mut chosen = Vec::new();
        let mut socks = vec![None; servers.len()];
        let mut errors = vec![None; servers.len()];
        for (i, r) in results.into_iter().enumerate() {
            match r {
                Ok(()) => {
                    let port = free_port()?;
                    socks[i] = Some(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
                    chosen.push((servers[i].clone(), port));
                }
                Err(e) => {
                    warn!("server {} rejected by xray: {e:#}", servers[i].name);
                    errors[i] = Some(format!("{e:#}"));
                }
            }
        }
        if chosen.is_empty() {
            bail!("no server config was accepted by xray");
        }
        let cfg_path = work_dir.join("xray.json");
        std::fs::write(
            &cfg_path,
            serde_json::to_vec_pretty(&build_config(&chosen))?,
        )?;
        let mut child = command(xray)
            .arg("run")
            .arg("-c")
            .arg(&cfg_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("cannot start xray")?;
        let last_line = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        for stream in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
            child.stderr.take().map(|s| Box::new(s) as _),
        ]
        .into_iter()
        .flatten()
        {
            let last = last_line.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!(target: "xray", "{line}");
                    if !line.trim().is_empty() {
                        *last.lock().unwrap() = line;
                    }
                }
            });
        }
        // Wait until the first inbound accepts connections, or xray exits.
        let first = socks.iter().flatten().next().copied().unwrap();
        for _ in 0..50 {
            if let Some(status) = child.try_wait()? {
                tokio::time::sleep(Duration::from_millis(100)).await; // let the reader catch up
                bail!(
                    "xray exited at start ({status}): {}",
                    last_line.lock().unwrap()
                );
            }
            if tokio::net::TcpStream::connect(first).await.is_ok() {
                info!("xray started with {} server(s)", chosen.len());
                return Ok(Xray {
                    child,
                    socks,
                    errors,
                });
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("xray did not open its SOCKS ports in 5 s")
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

async fn test_config(xray: &Path, path: &Path, cfg: &Value) -> Result<()> {
    std::fs::write(path, serde_json::to_vec(cfg)?)?;
    let out = command(xray)
        .arg("run")
        .arg("-test")
        .arg("-c")
        .arg(path)
        .output()
        .await?;
    if out.status.success() {
        return Ok(());
    }
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    let reason = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("invalid config")
        .trim()
        .to_string();
    bail!("{reason}")
}

fn command(xray: &Path) -> Command {
    let mut c = Command::new(xray);
    #[cfg(windows)]
    c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    c
}

/// A port free for both TCP and UDP.
///
/// Hyper-V/WSL exclude whole blocks of ports, sometimes for UDP only, and Windows
/// hands out ephemeral ports sequentially, so asking the OS for port 0 can return
/// dozens of ports in a row that UDP cannot bind. Random ports below the dynamic
/// range avoid that.
fn free_port() -> Result<u16> {
    use rand::Rng;
    let mut rng = rand::rng();
    for _ in 0..200 {
        let port = rng.random_range(20_000..40_000);
        let Ok(_tcp) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) else {
            continue;
        };
        if std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).is_ok() {
            return Ok(port);
        }
    }
    bail!("no local port is free for both TCP and UDP")
}
