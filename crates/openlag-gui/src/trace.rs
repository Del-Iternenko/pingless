//! Route diagnostics like MTR/WinMTR: ICMP echoes with growing TTL, straight from the
//! physical network card, a few rounds, with loss and latency per hop. Shows where on
//! the way to a game server the ping or the loss comes from (home network, provider,
//! a backbone hop...). No admin rights needed.

use std::net::Ipv4Addr;
use std::time::Instant;

use serde::Serialize;

const MAX_HOPS: u8 = 30;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Hop {
    pub ttl: u8,
    /// Router that answered, if any did.
    pub ip: Option<String>,
    pub sent: u32,
    pub lost: u32,
    pub best_ms: Option<f64>,
    pub avg_ms: Option<f64>,
    pub worst_ms: Option<f64>,
    #[serde(skip)]
    sum: f64,
}

impl Hop {
    fn record(&mut self, r: Option<Probe>) {
        self.sent += 1;
        let Some(p) = r else {
            self.lost += 1;
            return;
        };
        self.ip.get_or_insert_with(|| p.from.to_string());
        self.sum += p.ms;
        let got = (self.sent - self.lost) as f64;
        self.avg_ms = Some(self.sum / got);
        self.best_ms = Some(self.best_ms.map_or(p.ms, |b| b.min(p.ms)));
        self.worst_ms = Some(self.worst_ms.map_or(p.ms, |w| w.max(p.ms)));
    }
}

#[derive(Debug, Clone, Copy)]
struct Probe {
    from: Ipv4Addr,
    ms: f64,
    /// The destination itself answered (not a router on the way).
    reached: bool,
}

/// Runs `rounds` rounds over all TTLs in parallel; stops the list at the destination.
pub fn trace(dst: Ipv4Addr, source: Option<Ipv4Addr>, rounds: u32) -> Vec<Hop> {
    let mut hops: Vec<Hop> = (1..=MAX_HOPS)
        .map(|ttl| Hop {
            ttl,
            ..Default::default()
        })
        .collect();
    let mut last = MAX_HOPS;
    for _ in 0..rounds {
        let results: Vec<Option<Probe>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (1..=last)
                .map(|ttl| scope.spawn(move || probe(dst, source, ttl, 1500)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().ok().flatten())
                .collect()
        });
        if let Some(i) = results.iter().position(|r| r.is_some_and(|p| p.reached)) {
            last = last.min(i as u8 + 1);
        }
        for (i, r) in results.into_iter().enumerate().take(last as usize) {
            hops[i].record(r);
        }
    }
    hops.truncate(last as usize);
    // Trailing silent hops when the destination never answered: keep a few, not 30.
    while hops.len() > 1
        && hops.last().is_some_and(|h| h.ip.is_none())
        && hops[hops.len() - 2].ip.is_none()
    {
        hops.pop();
    }
    hops
}

/// One echo with the given TTL; the answer comes from the router where it expired, or
/// from the destination.
fn probe(dst: Ipv4Addr, source: Option<Ipv4Addr>, ttl: u8, timeout_ms: u32) -> Option<Probe> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        ICMP_ECHO_REPLY, IP_OPTION_INFORMATION, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho2Ex,
    };
    const IP_SUCCESS: u32 = 0;
    const IP_TTL_EXPIRED_TRANSIT: u32 = 11013;
    let data = *b"openlag-trace-32-bytes-payload!!";
    let mut reply = vec![0u8; std::mem::size_of::<ICMP_ECHO_REPLY>() + data.len() + 64];
    let opts = IP_OPTION_INFORMATION {
        Ttl: ttl,
        Tos: 0,
        Flags: 0,
        OptionsSize: 0,
        OptionsData: std::ptr::null_mut(),
    };
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
            &opts,
            reply.as_mut_ptr().cast(),
            reply.len() as u32,
            timeout_ms,
        );
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        IcmpCloseHandle(h);
        if n == 0 {
            return None;
        }
        let r = &*(reply.as_ptr() as *const ICMP_ECHO_REPLY);
        let from = Ipv4Addr::from(r.Address.to_ne_bytes());
        match r.Status {
            IP_SUCCESS => Some(Probe {
                from,
                ms,
                reached: true,
            }),
            IP_TTL_EXPIRED_TRANSIT => Some(Probe {
                from,
                ms,
                reached: false,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    /// Live: traces to 1.1.1.1 from this PC.
    #[test]
    #[ignore]
    fn trace_cloudflare() {
        let hops = super::trace("1.1.1.1".parse().unwrap(), None, 2);
        for h in &hops {
            eprintln!(
                "{:>2} {:<16} loss {}/{} avg {:?}",
                h.ttl,
                h.ip.as_deref().unwrap_or("*"),
                h.lost,
                h.sent,
                h.avg_ms.map(|v| v.round())
            );
        }
        assert!(hops.last().unwrap().ip.as_deref() == Some("1.1.1.1"));
    }
}
