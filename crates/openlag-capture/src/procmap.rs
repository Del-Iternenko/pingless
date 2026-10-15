//! Which process owns a local TCP or UDP port.
//!
//! Fed live by WinDivert SOCKET-layer events, with the system TCP/UDP tables as a
//! fallback: a program's first packet can reach the network layer before we have
//! processed the bind/connect event of its socket.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetExtendedUdpTable, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

pub const TCP: u8 = 6;
pub const UDP: u8 = 17;

/// Don't rescan the system tables more often than this on lookup misses.
const SNAPSHOT_MIN_INTERVAL: Duration = Duration::from_millis(100);
/// PIDs are reused by Windows; re-resolve names after this long.
const NAME_TTL: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct ProcessMap {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    owners: HashMap<(u8, u16), u32>,
    names: HashMap<u32, (Option<String>, Instant)>,
    last_snapshot: HashMap<u8, Instant>,
}

impl ProcessMap {
    pub fn on_bind(&self, proto: u8, port: u16, pid: u32) {
        let mut st = self.state.lock();
        if st.owners.insert((proto, port), pid) != Some(pid) {
            // New owner: make sure its name is resolved fresh.
            st.names.remove(&pid);
        }
    }

    pub fn on_close(&self, proto: u8, port: u16, pid: u32) {
        let mut st = self.state.lock();
        if st.owners.get(&(proto, port)) == Some(&pid) {
            st.owners.remove(&(proto, port));
        }
    }

    pub fn pid_for_port(&self, proto: u8, port: u16) -> Option<u32> {
        let mut st = self.state.lock();
        if let Some(pid) = st.owners.get(&(proto, port)) {
            return Some(*pid);
        }
        let now = Instant::now();
        if st
            .last_snapshot
            .get(&proto)
            .is_some_and(|t| now - *t < SNAPSHOT_MIN_INTERVAL)
        {
            return None;
        }
        st.last_snapshot.insert(proto, now);
        for (port, pid) in port_table(proto) {
            st.owners.insert((proto, port), pid);
        }
        st.owners.get(&(proto, port)).copied()
    }

    /// Lower-case executable file name, e.g. `cs2.exe`.
    pub fn name(&self, pid: u32) -> Option<String> {
        let now = Instant::now();
        let mut st = self.state.lock();
        if let Some((name, at)) = st.names.get(&pid)
            && now - *at < NAME_TTL
        {
            return name.clone();
        }
        let name = process_name(pid);
        st.names.insert(pid, (name.clone(), now));
        name
    }
}

/// All (local port, pid) pairs for TCP or UDP sockets, IPv4 and IPv6.
pub fn port_table(proto: u8) -> Vec<(u16, u32)> {
    // Row layouts (bytes): MIB_UDPROW_OWNER_PID addr,port,pid = 12;
    // MIB_UDP6ROW_OWNER_PID addr[16],scope,port,pid = 28;
    // MIB_TCPROW_OWNER_PID state,laddr,lport,raddr,rport,pid = 24;
    // MIB_TCP6ROW_OWNER_PID laddr[16],scope,lport,raddr[16],scope,rport,state,pid = 56.
    let layouts: [(u16, usize, usize, usize); 2] = if proto == TCP {
        [(AF_INET, 24, 8, 20), (AF_INET6, 56, 20, 52)]
    } else {
        [(AF_INET, 12, 4, 8), (AF_INET6, 28, 20, 24)]
    };
    let mut out = Vec::new();
    for (family, row, port_off, pid_off) in layouts {
        let Some(buf) = raw_table(proto, family as u32) else {
            continue;
        };
        let n = u32::from_ne_bytes(buf[0..4].try_into().unwrap()) as usize;
        for i in 0..n {
            let base = 4 + i * row;
            if base + row > buf.len() {
                break;
            }
            let port_raw = u32::from_ne_bytes(
                buf[base + port_off..base + port_off + 4]
                    .try_into()
                    .unwrap(),
            );
            let pid =
                u32::from_ne_bytes(buf[base + pid_off..base + pid_off + 4].try_into().unwrap());
            // The port is stored in network byte order in the low 16 bits.
            out.push(((port_raw as u16).swap_bytes(), pid));
        }
    }
    out
}

fn raw_table(proto: u8, family: u32) -> Option<Vec<u8>> {
    let mut size = 0u32;
    let mut buf: Vec<u8> = Vec::new();
    for _ in 0..4 {
        let ptr = if buf.is_empty() {
            std::ptr::null_mut()
        } else {
            buf.as_mut_ptr().cast()
        };
        // SAFETY: size describes buf's capacity; null is allowed to query the size.
        let rc = unsafe {
            if proto == TCP {
                GetExtendedTcpTable(ptr, &mut size, 0, family, TCP_TABLE_OWNER_PID_ALL, 0)
            } else {
                GetExtendedUdpTable(ptr, &mut size, 0, family, UDP_TABLE_OWNER_PID, 0)
            }
        };
        if rc == NO_ERROR {
            buf.truncate(size as usize);
            return Some(buf);
        }
        if rc != ERROR_INSUFFICIENT_BUFFER {
            return None;
        }
        // The table can grow between calls; leave some slack.
        buf = vec![0u8; size as usize + 4096];
        size = buf.len() as u32;
    }
    None
}

fn process_name(pid: u32) -> Option<String> {
    if pid == 0 || pid == 4 {
        return None; // idle and System
    }
    // SAFETY: plain Win32 calls; the handle is closed below.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        if ok == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Some(
            path.rsplit(['\\', '/'])
                .next()
                .unwrap_or(&path)
                .to_lowercase(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_own_sockets_and_name() {
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let map = ProcessMap::default();
        for (proto, port) in [
            (UDP, udp.local_addr().unwrap().port()),
            (TCP, tcp.local_addr().unwrap().port()),
        ] {
            let pid = map
                .pid_for_port(proto, port)
                .expect("own socket not in table");
            assert_eq!(pid, std::process::id());
        }
        let name = map.name(std::process::id()).unwrap();
        assert!(name.ends_with(".exe"), "{name}");
    }
}
