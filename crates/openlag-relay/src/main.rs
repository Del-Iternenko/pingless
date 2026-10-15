use std::net::SocketAddr;

use anyhow::Result;
use clap::{Parser, Subcommand};
use openlag_relay::{BridgeConfig, ExitConfig, run_bridge, run_exit};

#[derive(Parser)]
#[command(name = "openlag-relay", version, about = "OpenLag relay node")]
struct Cli {
    #[command(subcommand)]
    role: Role,
}

#[derive(Subcommand)]
enum Role {
    /// Exit node: decrypts client traffic and talks to game servers.
    Exit {
        /// Listen address, can be repeated (e.g. a normal and a high port).
        #[arg(long, default_value = "0.0.0.0:7000")]
        listen: Vec<SocketAddr>,
        /// Pre-shared key; prefer the OPENLAG_PSK environment variable.
        #[arg(long, env = "OPENLAG_PSK", hide_env_values = true)]
        psk: String,
        /// Allow flows to private and loopback addresses (for local testing only).
        #[arg(long)]
        allow_private: bool,
    },
    /// Bridge node: forwards encrypted datagrams to one exit.
    Bridge {
        #[arg(long, default_value = "0.0.0.0:7001")]
        listen: Vec<SocketAddr>,
        #[arg(long)]
        exit: SocketAddr,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match Cli::parse().role {
        Role::Exit {
            listen,
            psk,
            allow_private,
        } => {
            run_exit(ExitConfig {
                listen,
                psk,
                allow_private,
            })
            .await
        }
        Role::Bridge { listen, exit } => run_bridge(BridgeConfig { listen, exit }).await,
    }
}
