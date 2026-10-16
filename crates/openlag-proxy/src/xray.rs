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
/// Options for running xray.
#[derive(Debug, Clone, Default)]
pub struct XrayOptions {
    /// Send traffic to the servers through this interface (friendly name), bypassing
    /// TUN adapters of other VPN clients.
    pub bind_interface: Option<String>,
    /// uTLS fingerprint for TLS and REALITY ("chrome", "firefox", "randomized"...);
    /// None keeps what the subscription says.
    pub fingerprint: Option<String>,
    /// Split the TLS ClientHello into small TCP pieces to slip past DPI that blocks
    /// by SNI.
    pub fragment: Option<Fragment>,
    /// Multiplex TCP connections over one (mux.cool); 0 = off. Never applied to
    /// XTLS Vision, which does not support it.
    pub mux_concurrency: u32,
    /// DNS used to resolve server addresses, e.g. "https://1.1.1.1/dns-query";
    /// None = system resolver.
    pub dns: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Fragment {
    /// "tlshello" or a packet range such as "1-3".
    pub packets: String,
    /// Piece size range in bytes, e.g. "100-200".
    pub length: String,
    /// Pause between pieces in ms, e.g. "10-20".
    pub interval: String,
}

/// Xray config with one SOCKS inbound per server, routed to that server only.
pub fn build_config(servers: &[(ProxyServer, u16)]) -> Value {
    build_config_with(servers, &XrayOptions::default())
}

pub fn build_config_with(servers: &[(ProxyServer, u16)], opts: &XrayOptions) -> Value {
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
        if !out.get("streamSettings").is_some_and(Value::is_object) {
            out["streamSettings"] = json!({});
        }
        let st = &mut out["streamSettings"];
        if let Some(fp) = &opts.fingerprint {
            for key in ["tlsSettings", "realitySettings"] {
                if st.get(key).is_some_and(Value::is_object) {
                    st[key]["fingerprint"] = json!(fp);
                }
            }
        }
        if opts.fragment.is_some() {
            // The fragment outbound dials for us (and carries the interface binding).
            st["sockopt"]["dialerProxy"] = json!("fragment");
        } else if let Some(iface) = &opts.bind_interface {
            st["sockopt"]["interface"] = json!(iface);
        }
        if opts.dns.is_some() {
            st["sockopt"]["domainStrategy"] = json!("UseIPv4");
        }
        let vision = out
            .pointer("/settings/vnext/0/users/0/flow")
            .and_then(Value::as_str)
            .is_some_and(|f| !f.is_empty());
        if opts.mux_concurrency > 0 && !vision {
            out["mux"] = json!({ "enabled": true, "concurrency": opts.mux_concurrency, "xudpConcurrency": 16, "xudpProxyUDP443": "skip" });
        }
        outbounds.push(out);
        rules.push(json!({ "type": "field", "inboundTag": [format!("in-{i}")], "outboundTag": format!("out-{i}") }));
    }
    if let Some(f) = &opts.fragment {
        let mut frag = json!({
            "tag": "fragment",
            "protocol": "freedom",
            "settings": { "fragment": { "packets": f.packets, "length": f.length, "interval": f.interval } },
            "streamSettings": { "sockopt": { "tcpNoDelay": true } },
        });
        if let Some(iface) = &opts.bind_interface {
            frag["streamSettings"]["sockopt"]["interface"] = json!(iface);
        }
        if opts.dns.is_some() {
            frag["settings"]["domainStrategy"] = json!("UseIPv4");
        }
        outbounds.push(frag);
    }
    outbounds.push(json!({ "tag": "block", "protocol": "blackhole" }));
    let mut cfg = json!({
        "log": { "loglevel": "warning" },
        "inbounds": inbounds,
        "outbounds": outbounds,
        "routing": { "rules": rules },
    });
    if let Some(dns) = &opts.dns {
        cfg["dns"] = json!({ "servers": [dns], "queryStrategy": "UseIPv4" });
    }
    cfg
}

/// A running xray process. Killed when dropped, and by Windows if we crash.
pub struct Xray {
    child: Child,
    #[cfg(windows)]
    _job: Option<job::Job>,
    /// SOCKS5 address for each server that passed validation, same order as input.
    pub socks: Vec<Option<SocketAddr>>,
    /// Why a server was left out.
    pub errors: Vec<Option<String>>,
}

impl Xray {
    /// Validates every server on its own (a broken entry must not take down the
    /// rest), then starts one xray with all valid ones.
    pub async fn start(xray: &Path, work_dir: &Path, servers: &[ProxyServer]) -> Result<Xray> {
        Self::start_with(xray, work_dir, servers, &XrayOptions::default()).await
    }

    pub async fn start_with(
        xray: &Path,
        work_dir: &Path,
        servers: &[ProxyServer],
        opts: &XrayOptions,
    ) -> Result<Xray> {
        anyhow::ensure!(xray.exists(), "xray.exe not found at {}", xray.display());
        std::fs::create_dir_all(work_dir)?;

        // Check all servers in parallel; `xray -test` takes ~100 ms each.
        let mut set = tokio::task::JoinSet::new();
        for (i, s) in servers.iter().enumerate() {
            let path = work_dir.join(format!("check-{i}.json"));
            let cfg = build_config_with(&[(s.clone(), 0)], opts);
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
            serde_json::to_vec_pretty(&build_config_with(&chosen, opts))?,
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
        #[cfg(windows)]
        let job = job::Job::kill_on_close(&child);
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
                    #[cfg(windows)]
                    _job: job,
                    socks,
                    errors,
                });
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("xray did not open its SOCKS ports in 5 s")
    }

    /// Process id, so capture can keep xray's own traffic out of the tunnel.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
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

#[cfg(windows)]
mod job {
    use tokio::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    /// Job object that kills its processes when the last handle closes, which also
    /// happens when our process dies. Without it a crash would leave xray running.
    pub(super) struct Job(HANDLE);

    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub(super) fn kill_on_close(child: &Child) -> Option<Job> {
            let process = child.raw_handle()? as HANDLE;
            // SAFETY: plain Win32 calls with valid handles and a zeroed info struct.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) != 0
                    && AssignProcessToJobObject(job, process) != 0;
                if !ok {
                    CloseHandle(job);
                    return None;
                }
                Some(Job(job))
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closed once.
            unsafe { CloseHandle(self.0) };
        }
    }
}
