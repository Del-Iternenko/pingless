//! OpenLag client engine.
//!
//! The engine knows nothing about how packets are captured. The capture layer hands it
//! UDP payloads of game flows with [`Engine::send_upstream`] and gets answers back
//! through the [`Downstream`] callback. Everything about relays lives here: path
//! probing, route selection, multipath sending and de-duplication.

mod config;
mod engine;
mod socks;
mod stats;

pub use config::{EngineConfig, ExitSpec, ProxySpec, RouteSpec};
pub use engine::{Downstream, DownstreamFn, Engine, FlowKey, Upstream};
pub use stats::{FlowSnapshot, PathSnapshot, Snapshot};
