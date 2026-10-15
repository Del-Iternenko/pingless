//! Per-process UDP capture for Windows, built on WinDivert.
//!
//! Outbound UDP datagrams of selected executables are taken off the network layer and
//! handed to the engine; answers from relays are injected back as inbound packets that
//! look like they came from the game server itself. Because the injected packets carry
//! the original server address, Windows Firewall sees them as replies on the game's
//! existing flow and lets them in.
//!
//! TCP is not captured yet: game TCP connections go direct.

mod packet;

#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod procmap;
#[cfg(windows)]
mod windivert;

pub use packet::{build_udp, is_local_scope, parse_udp};

#[cfg(windows)]
pub use capture::{Capture, CaptureConfig, CaptureStats};
