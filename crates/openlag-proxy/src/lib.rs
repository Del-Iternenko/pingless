//! VPN subscriptions as OpenLag exits.
//!
//! Servers from Happ, Incy, Amnezia and similar providers speak Xray protocols
//! (VLESS, VMess, Trojan, Shadowsocks). OpenLag runs xray-core next to the client with
//! one local SOCKS5 port per server and sends game UDP through it, so no TUN adapter
//! and no system-wide VPN is involved.

mod amnezia;
mod link;
mod netif;
mod subscription;
mod xray;

pub use amnezia::decode_key as decode_amnezia_key;
pub use link::{ProxyServer, parse_link};
pub use netif::{NetInterface, physical_interface};
pub use subscription::{FetchOptions, Parsed, Skipped, SubscriptionInfo, fetch, parse_text};
pub use xray::{Fragment, Xray, XrayOptions, build_config, build_config_with, server_from_config};
