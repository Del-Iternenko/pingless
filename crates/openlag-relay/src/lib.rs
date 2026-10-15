//! Relay roles.
//!
//! * **exit**: terminates client sessions and talks to game servers. The game sees the
//!   exit's IP, so every path of one flow must end at the same exit.
//! * **bridge**: forwards datagrams to one exit without decrypting them. Clients reach
//!   the exit through several bridges at once to get different routes (multipath).

mod bridge;
mod exit;

pub use bridge::{BridgeConfig, BridgeServer, run_bridge};
pub use exit::{ExitConfig, ExitServer, run_exit};
