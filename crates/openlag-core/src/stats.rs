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
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub paths: Vec<PathSnapshot>,
    pub flows: Vec<FlowSnapshot>,
    pub dropped_upstream: u64,
}
