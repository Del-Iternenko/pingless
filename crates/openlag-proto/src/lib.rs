//! Wire protocol shared by the OpenLag client and relay.
//!
//! Every datagram between a client and a relay looks like this:
//!
//! ```text
//! +-----+------+------------+------------+---------------------------+
//! | ver | kind | session u64| counter u64| AEAD(body) + 16-byte tag  |
//! +-----+------+------------+------------+---------------------------+
//! ```
//!
//! The 18-byte header is authenticated as associated data. Each session has its own
//! key derived from the pre-shared key, so counters of different sessions never share
//! a key. Inside a session the client counter has the top bit clear and the relay
//! counter has it set, and both start at a random offset, so a relay restart does not
//! reuse nonces.

mod body;
mod crypto;
mod window;

pub use body::{Body, DataBody, PingBody};
pub use crypto::{PacketCipher, SessionKeys};
pub use window::SeqWindow;

use thiserror::Error;

pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 18;
pub const TAG_LEN: usize = 16;
/// Bytes added on top of the game payload for a Data packet carrying an IPv6 address.
pub const MAX_OVERHEAD: usize = HEADER_LEN + TAG_LEN + body::DATA_FIXED_LEN + body::MAX_ADDR_LEN;
/// Largest datagram either side will send or accept.
pub const MAX_DATAGRAM: usize = 65_507;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("datagram too short")]
    Truncated,
    #[error("unsupported protocol version {0}")]
    Version(u8),
    #[error("unknown packet kind {0}")]
    Kind(u8),
    #[error("authentication failed")]
    Auth,
    #[error("malformed address")]
    Address,
    #[error("payload too large")]
    TooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Game payload, client to relay or relay to client.
    Data = 1,
    /// Latency probe sent by the client on one path.
    Ping = 2,
    /// Relay's answer to a Ping, echoed unchanged.
    Pong = 3,
    /// The client is done with a flow; the relay may free it at once.
    Close = 4,
}

impl TryFrom<u8> for Kind {
    type Error = Error;
    fn try_from(v: u8) -> Result<Self, Error> {
        Ok(match v {
            1 => Kind::Data,
            2 => Kind::Ping,
            3 => Kind::Pong,
            4 => Kind::Close,
            other => return Err(Error::Kind(other)),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub session: u64,
    pub counter: u64,
}

impl Header {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(VERSION);
        out.push(self.kind as u8);
        out.extend_from_slice(&self.session.to_le_bytes());
        out.extend_from_slice(&self.counter.to_le_bytes());
    }

    /// Reads the cleartext header. Relays in bridge mode only need this much.
    pub fn decode(buf: &[u8]) -> Result<Header, Error> {
        if buf.len() < HEADER_LEN + TAG_LEN {
            return Err(Error::Truncated);
        }
        if buf[0] != VERSION {
            return Err(Error::Version(buf[0]));
        }
        Ok(Header {
            kind: Kind::try_from(buf[1])?,
            session: u64::from_le_bytes(buf[2..10].try_into().unwrap()),
            counter: u64::from_le_bytes(buf[10..18].try_into().unwrap()),
        })
    }
}

#[cfg(test)]
mod tests;
