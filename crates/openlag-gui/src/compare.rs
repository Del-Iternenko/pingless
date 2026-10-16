//! "Without OpenLag": direct ICMP ping from the physical network card to the same
//! game servers the tunnel carries, so the UI can show both side by side.
//!
//! The echo is sent with the physical adapter's address as source, so another VPN's
//! TUN (Happ...) is bypassed and the number is what the game would see without any
//! tunnel.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::Instant;

use serde::Serialize;

/// Direct path quality to one IP.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Direct {
    pub ms: Option<f64>,
    pub jitter_ms: f64,
    pub loss_pct: f64,
    #[serde(skip)]
    results: Vec<bool>,
    #[serde(skip)]
    last_seen: Option<Instant>,
}

impl Direct {
    fn record(&mut self, rtt: Option<f64>) {
        if let Some(r) = rtt {
            match self.ms {
                None => self.ms = Some(r),
                Some(s) => {
                    self.jitter_ms = 0.75 * self.jitter_ms + 0.25 * (s - r).abs();
                    self.ms = Some(0.8 * s + 0.2 * r);
                }
            }
        }
        self.results.push(rtt.is_some());
        if self.results.len() > 30 {
            self.results.remove(0);
        }
        let lost = self.results.iter().filter(|ok| !**ok).count();
        self.loss_pct = lost as f64 * 100.0 / self.results.len() as f64;
    }
}

#[derive(Default)]
pub struct Comparator {
    pub targets: HashMap<Ipv4Addr, Direct>,
}

/// At most this many game server IPs are pinged.
const MAX_TARGETS: usize = 12;

impl Comparator {
    /// Records one round of results and forgets IPs not seen for a minute. The pings
    /// themselves run outside the lock (see [`ping_all`]).
    pub fn record(&mut self, results: Vec<(Ipv4Addr, Option<f64>)>) {
        let now = Instant::now();
        for (ip, rtt) in results {
            let d = self.targets.entry(ip).or_default();
            d.last_seen = Some(now);
            d.record(rtt);
        }
        self.targets.retain(|_, d| {
            d.last_seen
                .is_some_and(|t| now - t < std::time::Duration::from_secs(60))
        });
    }

    pub fn view(&self) -> HashMap<String, Direct> {
        self.targets
            .iter()
            .map(|(ip, d)| (ip.to_string(), d.clone()))
            .collect()
    }
}

/// Pings up to [`MAX_TARGETS`] IPs in parallel (one thread each, 800 ms timeout).
pub fn ping_all(ips: &[Ipv4Addr], source: Option<Ipv4Addr>) -> Vec<(Ipv4Addr, Option<f64>)> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = ips
            .iter()
            .take(MAX_TARGETS)
            .map(|ip| scope.spawn(move || (*ip, icmp_ping(*ip, source, 800))))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

/// One ICMP echo; Some(rtt in ms) on reply.
fn icmp_ping(dst: Ipv4Addr, source: Option<Ipv4Addr>, timeout_ms: u32) -> Option<f64> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        ICMP_ECHO_REPLY, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho2Ex,
    };
    let data = *b"openlag-ping-32-bytes-of-payload";
    let mut reply = vec![0u8; std::mem::size_of::<ICMP_ECHO_REPLY>() + data.len() + 64];
    // SAFETY: plain IP Helper calls; buffers outlive them; the handle is closed below.
    unsafe {
        let h = IcmpCreateFile();
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let start = Instant::now();
        let n = IcmpSendEcho2Ex(
            h,
            std::ptr::null_mut(),
            None,
            std::ptr::null(),
            source.map(|s| u32::from_ne_bytes(s.octets())).unwrap_or(0),
            u32::from_ne_bytes(dst.octets()),
            data.as_ptr().cast(),
            data.len() as u16,
            std::ptr::null(),
            reply.as_mut_ptr().cast(),
            reply.len() as u32,
            timeout_ms,
        );
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        IcmpCloseHandle(h);
        if n == 0 {
            return None;
        }
        let r = &*(reply.as_ptr() as *const ICMP_ECHO_REPLY);
        // Status 0 = IP_SUCCESS. Prefer our own clock: the reply's field has 1 ms steps.
        (r.Status == 0).then_some(elapsed)
    }
}
