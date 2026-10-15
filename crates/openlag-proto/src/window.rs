/// Sliding window over sequence numbers that accepts each number at most once.
///
/// Multipath sends every packet over several routes, so the receiver sees the same
/// sequence number up to N times. Packets older than the window are treated as
/// duplicates: for real-time game traffic a packet that late is useless anyway.
#[derive(Debug, Clone)]
pub struct SeqWindow {
    highest: u64,
    bits: [u64; WORDS],
}

const WORDS: usize = 16;
const SIZE: u64 = (WORDS * 64) as u64;

impl Default for SeqWindow {
    fn default() -> Self {
        SeqWindow {
            highest: 0,
            bits: [0; WORDS],
        }
    }
}

impl SeqWindow {
    /// Returns true the first time `seq` is seen. Sequence numbers start at 1.
    pub fn accept(&mut self, seq: u64) -> bool {
        if seq == 0 {
            return false;
        }
        if seq > self.highest {
            if seq - self.highest >= SIZE {
                self.bits = [0; WORDS];
            } else {
                for s in self.highest + 1..seq {
                    self.clear(s);
                }
            }
            self.highest = seq;
            self.set(seq);
            return true;
        }
        if self.highest - seq >= SIZE || self.get(seq) {
            return false;
        }
        self.set(seq);
        true
    }

    pub fn highest(&self) -> u64 {
        self.highest
    }

    fn slot(seq: u64) -> (usize, u64) {
        let i = seq % SIZE;
        ((i / 64) as usize, 1 << (i % 64))
    }
    fn get(&self, seq: u64) -> bool {
        let (w, b) = Self::slot(seq);
        self.bits[w] & b != 0
    }
    fn set(&mut self, seq: u64) {
        let (w, b) = Self::slot(seq);
        self.bits[w] |= b;
    }
    fn clear(&mut self, seq: u64) {
        let (w, b) = Self::slot(seq);
        self.bits[w] &= !b;
    }
}
