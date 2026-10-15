//! Which process owns a local UDP port.
//!
//! Fed live by WinDivert SOCKET-layer events, with the system UDP table as a fallback:
//! a game's first datagram can reach the network layer before we have processed the
//! BIND event of its socket.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{GetExtendedUdpTable, UDP_TABLE_OWNER_PID};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

/// Don't rescan the UDP table more often than this on lookup misses.
const SNAPSHOT_MIN_INTERVAL: Duration = Duration::from_millis(100);
/// PIDs are reused by Windows; re-resolve names after this long.
const NAME_TTL: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct ProcessMap {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    port_pid: HashMap<u16, u32>,
    names: HashMap<u32, (Option<String>, Instant)>,
    last_snapshot: Option<Instant>,
}

impl ProcessMap {
    pub fn on_bind(&self, port: u16, pid: u32) {
        let mut st = self.state.lock();
        if st.port_pid.insert(port, pid) != Some(pid) {
            // New owner: make sure its name is resolved fresh.
            st.names.remove(&pid);
        }
    }

    pub fn on_close(&self, port: u16, pid: u32) {
        let mut st = self.state.lock();
        if st.port_pid.get(&port) == Some(&pid) {
            st.port_pid.remove(&port);
        }
    }

    pub fn pid_for_port(&self, port: u16) -> Option<u32> {
        let mut st = self.state.lock();
        if let Some(pid) = st.port_pid.get(&port) {
            return Some(*pid);
        }
        let now = Instant::now();
        if st
            .last_snapshot
            .is_some_and(|t| now - t < SNAPSHOT_MIN_INTERVAL)
        {
            return None;
        }
        st.last_snapshot = Some(now);
        for (port, pid) in udp_table() {
            st.port_pid.insert(port, pid);
        }
        st.port_pid.get(&port).copied()
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

/// All (local port, pid) pairs of UDP sockets, IPv4 and IPv6.
pub fn udp_table() -> Vec<(u16, u32)> {
    let mut out = Vec::new();
    // Row layouts: MIB_UDPROW_OWNER_PID is addr, port, pid (12 bytes);
    // MIB_UDP6ROW_OWNER_PID is addr[16], scope, port, pid (28 bytes).
    for (family, row, port_off, pid_off) in
        [(AF_INET, 12usize, 4usize, 8usize), (AF_INET6, 28, 20, 24)]
    {
        let Some(buf) = raw_table(family as u32) else {
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

fn raw_table(family: u32) -> Option<Vec<u8>> {
    let mut size = 0u32;
    let mut buf: Vec<u8> = Vec::new();
    for _ in 0..4 {
        // SAFETY: size describes buf's capacity; null is allowed to query the size.
        let rc = unsafe {
            GetExtendedUdpTable(
                if buf.is_empty() {
                    std::ptr::null_mut()
                } else {
                    buf.as_mut_ptr().cast()
                },
                &mut size,
                0,
                family,
                UDP_TABLE_OWNER_PID,
                0,
            )
        };
        if rc == NO_ERROR {
            buf.truncate(size as usize);
            return Some(buf);
        }
        if rc != ERROR_INSUFFICIENT_BUFFER {
            return None;
        }
        // The table can grow between calls; leave some slack.
        buf = vec![0u8; size as usize + 1024];
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
    fn finds_own_udp_socket_and_name() {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = s.local_addr().unwrap().port();
        let map = ProcessMap::default();
        let pid = map.pid_for_port(port).expect("own socket not in UDP table");
        assert_eq!(pid, std::process::id());
        let name = map.name(pid).unwrap();
        assert!(name.ends_with(".exe"), "{name}");
    }
}
