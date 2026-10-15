//! Finding the physical network interface, so xray can bypass other VPNs' TUN adapters.
//!
//! With another VPN client in TUN mode (Happ, v2rayN, Clash...), all traffic including
//! ours is routed into its tunnel: our packets would cross two VPNs and pick up the
//! latency and jitter of both. Binding xray's outbound sockets to the real adapter
//! (`sockopt.interface`) sends them straight out.

/// Words in adapter descriptions or names that mark virtual / tunnel adapters.
const VIRTUAL: &[&str] = &[
    "tun",
    "tap",
    "wintun",
    "wireguard",
    "openvpn",
    "radmin",
    "hamachi",
    "zerotier",
    "tailscale",
    "hyper-v",
    "virtual",
    "vmware",
    "virtualbox",
    "vpn",
    "happ",
    "xray",
    "sing-box",
    "clash",
    "loopback",
    "bluetooth",
    "npcap",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetInterface {
    /// Friendly name, e.g. "Ethernet" or "Wi-Fi"; this is what xray expects.
    pub name: String,
    pub description: String,
    pub metric: u32,
}

/// The up, physical Ethernet or Wi-Fi adapter with a default gateway and the lowest
/// metric. None if there is none (then traffic follows the routing table as usual).
#[cfg(windows)]
pub fn physical_interface() -> Option<NetInterface> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_MULTICAST,
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET;

    const IF_TYPE_ETHERNET: u32 = 6;
    const IF_TYPE_WIFI: u32 = 71;
    const OPER_UP: i32 = 1;

    let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST;
    let mut size: u32 = 32 * 1024;
    let mut buf: Vec<u64> = Vec::new();
    for _ in 0..3 {
        buf = vec![0u64; size as usize / 8 + 1];
        // SAFETY: buf is large enough per `size`, aligned for the struct.
        let rc = unsafe {
            GetAdaptersAddresses(
                AF_INET as u32,
                flags,
                std::ptr::null(),
                buf.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if rc == 0 {
            break;
        }
        if rc != 111 {
            return None; // anything but ERROR_BUFFER_OVERFLOW
        }
    }
    let wide = |p: *const u16| -> String {
        if p.is_null() {
            return String::new();
        }
        // SAFETY: NUL-terminated wide string owned by the adapter list.
        unsafe {
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
        }
    };
    let mut best: Option<NetInterface> = None;
    let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !cur.is_null() {
        // SAFETY: walking the linked list returned by GetAdaptersAddresses.
        let a = unsafe { &*cur };
        cur = a.Next;
        if a.OperStatus != OPER_UP
            || !(a.IfType == IF_TYPE_ETHERNET || a.IfType == IF_TYPE_WIFI)
            || a.FirstGatewayAddress.is_null()
        {
            continue;
        }
        let name = wide(a.FriendlyName);
        let description = wide(a.Description);
        let text = format!("{name} {description}").to_lowercase();
        if VIRTUAL.iter().any(|v| text.contains(v)) {
            continue;
        }
        let iface = NetInterface {
            name,
            description,
            metric: a.Ipv4Metric,
        };
        if best.as_ref().is_none_or(|b| iface.metric < b.metric) {
            best = Some(iface);
        }
    }
    best
}

#[cfg(not(windows))]
pub fn physical_interface() -> Option<NetInterface> {
    None
}
