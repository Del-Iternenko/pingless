use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use openlag_core::{Engine, EngineConfig, Snapshot};
use serde::Deserialize;

#[derive(Parser)]
#[command(
    name = "openlag",
    version,
    about = "Open-source game traffic optimizer"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Tunnel the configured games (needs Administrator and WinDivert).
    Run {
        #[arg(short, long, default_value = "config.toml")]
        config: PathBuf,
    },
    /// Only measure the routes to your relays; captures nothing.
    Probe {
        #[arg(short, long, default_value = "config.toml")]
        config: PathBuf,
    },
    /// Check that this machine can capture traffic.
    Check,
}

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    capture: CaptureSection,
    #[serde(flatten)]
    engine: EngineConfig,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CaptureSection {
    #[serde(default)]
    processes: Vec<String>,
    #[serde(default)]
    tunnel_existing: bool,
    /// `only` (default) or `all_except`.
    #[serde(default)]
    mode: String,
}

fn load(path: &PathBuf) -> Result<Config> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    cfg.engine.validate()?;
    Ok(cfg)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();
    match Cli::parse().cmd {
        Cmd::Run { config } => run(load(&config)?).await,
        Cmd::Probe { config } => probe(load(&config)?).await,
        Cmd::Check => check(),
    }
}

async fn probe(cfg: Config) -> Result<()> {
    let engine = Engine::start(cfg.engine, Arc::new(|_| {})).await?;
    dashboard(&engine, String::new).await
}

#[cfg(windows)]
async fn run(cfg: Config) -> Result<()> {
    use openlag_capture::{Capture, CaptureConfig, Selection};

    let mode = match cfg.capture.mode.as_str() {
        "" | "only" => Selection::Only,
        "all_except" => Selection::AllExcept,
        other => anyhow::bail!("[capture] mode must be \"only\" or \"all_except\", not {other:?}"),
    };
    anyhow::ensure!(
        mode == Selection::AllExcept || !cfg.capture.processes.is_empty(),
        "[capture] processes is empty: nothing to tunnel"
    );
    let mut capture = Capture::open(&CaptureConfig {
        processes: cfg.capture.processes.clone(),
        mode,
        tunnel_existing: cfg.capture.tunnel_existing,
        exclude_pids: vec![],
        ..Default::default()
    })?;
    let engine = Arc::new(Engine::start(cfg.engine, capture.downstream()).await?);

    // Give probes a moment so the first game packets already have a route.
    for _ in 0..20 {
        if engine.any_route_alive() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !engine.any_route_alive() {
        tracing::warn!("no relay answers yet; new game flows go direct until one does");
    }
    capture.start(engine.clone());
    let result = dashboard(&engine, || {
        let s = capture.stats();
        format!(
            "capture: udp tunneled {}  direct {}  injected {}  tcp redirected {}  inject errors {}",
            s.tunneled, s.direct, s.injected, s.tcp_redirected, s.inject_errors
        )
    })
    .await;
    capture.stop();
    result
}

#[cfg(not(windows))]
async fn run(_cfg: Config) -> Result<()> {
    anyhow::bail!("capture is only implemented on Windows; use `probe` to test relays")
}

/// Redraws a status table every second until Ctrl+C.
async fn dashboard(engine: &Engine, extra: impl Fn() -> String) -> Result<()> {
    let mut history: HashMap<u16, Vec<Option<f64>>> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
        let snap = engine.snapshot();
        for p in &snap.paths {
            let h = history.entry(p.id).or_default();
            h.push(if p.alive { p.last_rtt_ms } else { None });
            if h.len() > 40 {
                h.remove(0);
            }
        }
        print!("\x1b[2J\x1b[H{}", render(&snap, &history));
        let line = extra();
        if !line.is_empty() {
            println!("{line}");
        }
        println!("Ctrl+C to stop");
    }
}

fn render(snap: &Snapshot, history: &HashMap<u16, Vec<Option<f64>>>) -> String {
    let mut out = String::from("Pingless\n\nROUTES\n");
    out += &format!(
        "  {:<1} {:<14} {:<18} {:>8} {:>8} {:>7}  {}\n",
        "", "exit", "route", "ping", "jitter", "loss", "last 40 s"
    );
    for p in &snap.paths {
        let mark = if p.active {
            "*"
        } else if p.alive {
            " "
        } else {
            "x"
        };
        let ping = p
            .srtt_ms
            .filter(|_| p.alive)
            .map(|v| format!("{v:.1} ms"))
            .unwrap_or_else(|| "-".into());
        out += &format!(
            "  {mark} {:<14} {:<18} {:>8} {:>5.1} ms {:>5.1} %  {}\n",
            p.exit,
            p.route,
            ping,
            p.jitter_ms,
            p.loss_pct,
            sparkline(history.get(&p.id).map(Vec::as_slice).unwrap_or(&[]))
        );
    }
    out += "  (* carries traffic, x down)\n\nFLOWS\n";
    if snap.flows.is_empty() {
        out += "  none yet\n";
    }
    for f in &snap.flows {
        out += &format!(
            "  #{:<4} {:<22} -> {:<22} {:<10} up {:>7} down {:>7} dup {:>6}\n",
            f.id,
            f.local.to_string(),
            f.remote.to_string(),
            f.exit.as_deref().unwrap_or("direct"),
            f.packets_up,
            f.packets_down,
            f.duplicates
        );
    }
    if snap.dropped_upstream > 0 {
        out += &format!("  dropped upstream packets: {}\n", snap.dropped_upstream);
    }
    out + "\n"
}

fn sparkline(values: &[Option<f64>]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let known: Vec<f64> = values.iter().flatten().copied().collect();
    let (lo, hi) = known
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    values
        .iter()
        .map(|v| match v {
            None => '·',
            Some(v) if hi - lo < 0.5 => BARS[1],
            Some(v) => BARS[(((v - lo) / (hi - lo)) * 7.0).round() as usize],
        })
        .collect()
}

#[cfg(windows)]
fn check() -> Result<()> {
    let exe_dir = std::env::current_exe()?
        .parent()
        .map(PathBuf::from)
        .unwrap_or_default();
    for f in ["WinDivert.dll", "WinDivert64.sys"] {
        let ok = exe_dir.join(f).exists();
        println!(
            "{} {f} next to openlag.exe",
            if ok { "ok  " } else { "MISS" }
        );
    }
    match openlag_capture::Capture::open(&openlag_capture::CaptureConfig::default()) {
        Ok(mut c) => {
            c.stop();
            println!("ok   WinDivert opened: capture works on this machine");
        }
        Err(e) => println!("FAIL {e:#}"),
    }
    Ok(())
}

#[cfg(not(windows))]
fn check() -> Result<()> {
    println!("capture is only implemented on Windows");
    Ok(())
}
