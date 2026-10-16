use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde::Serialize;

/// A probe counts as lost if no answer arrives within this time.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Loss is computed over this many finished probes.
const LOSS_WINDOW: usize = 50;
/// A route with no answer for this long is considered down.
const DEAD_AFTER: Duration = Duration::from_secs(3);
/// Score penalty per percent of loss, in ms.
const LOSS_PENALTY_MS: f64 = 10.0;

/// Latency, jitter and loss of one route, fed by Ping/Pong probes.
#[derive(Debug)]
pub(crate) struct PathStats {
    outstanding: VecDeque<(u32, Instant)>,
    results: VecDeque<bool>,
    srtt: Option<f64>,
    rttvar: f64,
    last_rtt: Option<f64>,
    last_answer: Option<Instant>,
    pub(crate) bytes_up: u64,
    pub(crate) bytes_down: u64,
}

impl PathStats {
    pub(crate) fn new() -> PathStats {
        PathStats {
            outstanding: VecDeque::new(),
            results: VecDeque::new(),
            srtt: None,
            rttvar: 0.0,
            last_rtt: None,
            last_answer: None,
            bytes_up: 0,
            bytes_down: 0,
        }
    }

    pub(crate) fn on_probe_sent(&mut self, probe: u32, now: Instant) {
        self.expire(now);
        self.outstanding.push_back((probe, now));
    }

    /// Records an answer. The RTT comes from our own send time, not from the echoed
    /// timestamp, so a forged Pong cannot fake low latency.
    pub(crate) fn on_pong(&mut self, probe: u32, now: Instant) -> Option<f64> {
        let pos = self.outstanding.iter().position(|(p, _)| *p == probe)?;
        let (_, sent) = self.outstanding.remove(pos)?;
        let rtt = (now - sent).as_secs_f64() * 1000.0;
        // RFC 6298 style smoothing.
        match self.srtt {
            None => {
                self.srtt = Some(rtt);
                self.rttvar = rtt / 2.0;
            }
            Some(s) => {
                self.rttvar = 0.75 * self.rttvar + 0.25 * (s - rtt).abs();
                self.srtt = Some(0.875 * s + 0.125 * rtt);
            }
        }
        self.last_rtt = Some(rtt);
        self.last_answer = Some(now);
        self.push_result(true);
        Some(rtt)
    }

    fn expire(&mut self, now: Instant) {
        while let Some(&(_, sent)) = self.outstanding.front() {
            if now - sent < PROBE_TIMEOUT {
                break;
            }
            self.outstanding.pop_front();
            self.push_result(false);
        }
    }

    fn push_result(&mut self, ok: bool) {
        self.results.push_back(ok);
        if self.results.len() > LOSS_WINDOW {
            self.results.pop_front();
        }
    }

    pub(crate) fn loss_pct(&self) -> f64 {
        if self.results.is_empty() {
            return 0.0;
        }
        let lost = self.results.iter().filter(|ok| !**ok).count();
        lost as f64 * 100.0 / self.results.len() as f64
    }

    pub(crate) fn alive(&self, now: Instant) -> bool {
        self.last_answer.is_some_and(|t| now - t < DEAD_AFTER)
    }

    /// Lower is better. None while the route has never answered or is down.
    pub(crate) fn score(&self, now: Instant) -> Option<f64> {
        if !self.alive(now) {
            return None;
        }
        Some(self.srtt? + 2.0 * self.rttvar + self.loss_pct() * LOSS_PENALTY_MS)
    }

    pub(crate) fn snapshot(&self, now: Instant) -> (Option<f64>, Option<f64>, f64, f64, bool) {
        (
            self.srtt,
            self.last_rtt,
            self.rttvar,
            self.loss_pct(),
            self.alive(now),
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PathSnapshot {
    pub id: u16,
    pub exit: String,
    pub route: String,
    /// `relay` for our own relays, `proxy` for VPN servers through xray.
    pub kind: String,
    pub via: SocketAddr,
    pub srtt_ms: Option<f64>,
    pub last_rtt_ms: Option<f64>,
    pub jitter_ms: f64,
    pub loss_pct: f64,
    pub alive: bool,
    pub active: bool,
    pub bytes_up: u64,
    pub bytes_down: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FlowSnapshot {
    pub id: u32,
    pub local: SocketAddr,
    pub remote: SocketAddr,
    pub exit: Option<String>,
    pub packets_up: u64,
    pub packets_down: u64,
    /// Copies of downstream packets dropped because another route delivered first.
    pub duplicates: u64,
    pub idle_ms: u64,
    /// Round trip through the tunnel measured on the game's own request/response
    /// packets; None for streaming flows.
    pub rtt_ms: Option<f64>,
    pub rtt_jitter_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub paths: Vec<PathSnapshot>,
    pub flows: Vec<FlowSnapshot>,
    pub dropped_upstream: u64,
    pub preferred_exit: Option<String>,
    /// TCP connections redirected through proxies (largest first, up to 200).
    pub tcp: Vec<crate::tcp::TcpSnapshot>,
}

/// Passive round-trip time of a flow, from the game's own packets.
///
/// Games ping their servers and relays with small request/response packets (Valve's
/// SDR relay pings, matchmaking pings). For such flows the time from a request to the
/// next answer is the real RTT through the tunnel. Streaming flows (match traffic at
/// tick rate, both sides sending on their own) give meaningless pairs, so the RTT is
/// only reported while the flow looks request/response: few packets and about one
/// answer per request.
#[derive(Debug)]
pub(crate) struct FlowRtt {
    sent: VecDeque<Instant>,
    window_start: Instant,
    window_up: u32,
    window_down: u32,
    pingish: bool,
    srtt: Option<f64>,
    jitter: f64,
    samples: u32,
}

const RTT_WINDOW: Duration = Duration::from_secs(5);
const RTT_MAX_AGE: Duration = Duration::from_secs(2);

impl FlowRtt {
    pub(crate) fn new(now: Instant) -> FlowRtt {
        FlowRtt {
            sent: VecDeque::new(),
            window_start: now,
            window_up: 0,
            window_down: 0,
            pingish: false,
            srtt: None,
            jitter: 0.0,
            samples: 0,
        }
    }

    fn roll(&mut self, now: Instant) {
        if now - self.window_start < RTT_WINDOW {
            return;
        }
        let (up, down) = (self.window_up as f64, self.window_down as f64);
        // At most ~10 requests per second, and roughly one answer each.
        let pingish = (1.0..=50.0).contains(&up) && (0.6..=1.6).contains(&(down / up));
        if !pingish {
            self.srtt = None;
            self.samples = 0;
        }
        self.pingish = pingish;
        self.window_start = now;
        self.window_up = 0;
        self.window_down = 0;
    }

    pub(crate) fn on_up(&mut self, now: Instant) {
        self.roll(now);
        self.window_up += 1;
        while self.sent.front().is_some_and(|t| now - *t > RTT_MAX_AGE) {
            self.sent.pop_front();
        }
        if self.sent.len() >= 32 {
            self.sent.pop_front();
        }
        self.sent.push_back(now);
    }

    pub(crate) fn on_down(&mut self, now: Instant) {
        self.roll(now);
        self.window_down += 1;
        while self.sent.front().is_some_and(|t| now - *t > RTT_MAX_AGE) {
            self.sent.pop_front();
        }
        let Some(sent) = self.sent.pop_front() else {
            return;
        };
        let rtt = (now - sent).as_secs_f64() * 1000.0;
        match self.srtt {
            None => self.srtt = Some(rtt),
            Some(s) => {
                self.jitter = 0.75 * self.jitter + 0.25 * (s - rtt).abs();
                self.srtt = Some(0.8 * s + 0.2 * rtt);
            }
        }
        self.samples += 1;
    }

    /// (rtt, jitter) in ms while the flow looks request/response.
    pub(crate) fn get(&self) -> Option<(f64, f64)> {
        if self.pingish && self.samples >= 2 {
            self.srtt.map(|s| (s, self.jitter))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod rtt_tests {
    use super::*;

    #[test]
    fn ping_flow_reports_rtt_streaming_does_not() {
        let t0 = Instant::now();
        let mut ping = FlowRtt::new(t0);
        // One request per 500 ms, answer after 40 ms, for 12 s.
        for i in 0..24 {
            let t = t0 + Duration::from_millis(500 * i);
            ping.on_up(t);
            ping.on_down(t + Duration::from_millis(40));
        }
        let (rtt, _) = ping.get().expect("ping flow must report");
        assert!((rtt - 40.0).abs() < 1.0, "{rtt}");

        let mut stream = FlowRtt::new(t0);
        // 64 packets per second each way, independent.
        for i in 0..64 * 12 {
            let t = t0 + Duration::from_micros(15_625 * i);
            stream.on_up(t);
            stream.on_down(t + Duration::from_millis(3));
        }
        assert!(stream.get().is_none(), "streaming flow must not report");
    }
}
