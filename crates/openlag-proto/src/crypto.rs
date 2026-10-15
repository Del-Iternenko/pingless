use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use rand::Rng;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Body, Error, HEADER_LEN, Header, MAX_DATAGRAM, TAG_LEN};

const RELAY_BIT: u64 = 1 << 63;

/// Long-lived secret both sides derive from the pre-shared key.
#[derive(Clone)]
pub struct SessionKeys {
    master: [u8; 32],
}

impl SessionKeys {
    pub fn from_psk(psk: &str) -> SessionKeys {
        SessionKeys {
            master: blake3::derive_key("openlag v1 master key", psk.as_bytes()),
        }
    }

    /// Per-session cipher. `relay_side` picks which half of the counter space we send in.
    pub fn session(&self, session: u64, relay_side: bool) -> PacketCipher {
        let key = blake3::keyed_hash(&self.master, &session.to_le_bytes());
        let start: u64 = rand::rng().random_range(0..1u64 << 62);
        PacketCipher {
            aead: ChaCha20Poly1305::new(Key::from_slice(key.as_bytes())),
            session,
            counter: AtomicU64::new(if relay_side { start | RELAY_BIT } else { start }),
            relay_side,
        }
    }
}

/// Seals and opens packets of one session.
pub struct PacketCipher {
    aead: ChaCha20Poly1305,
    session: u64,
    counter: AtomicU64,
    relay_side: bool,
}

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    *Nonce::from_slice(&n)
}

impl PacketCipher {
    pub fn session_id(&self) -> u64 {
        self.session
    }

    /// Encodes and encrypts `body` into `out` (cleared first).
    pub fn seal(&self, body: &Body<'_>, out: &mut Vec<u8>) -> Result<(), Error> {
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        out.clear();
        Header {
            kind: body.kind(),
            session: self.session,
            counter,
        }
        .encode(out);
        body.encode(out);
        if out.len() + TAG_LEN > MAX_DATAGRAM {
            return Err(Error::TooLarge);
        }
        let (aad, plain) = out.split_at_mut(HEADER_LEN);
        let tag = self
            .aead
            .encrypt_in_place_detached(&nonce(counter), aad, plain)
            .map_err(|_| Error::Auth)?;
        out.extend_from_slice(&tag);
        Ok(())
    }

    /// Decrypts `buf` in place and parses the body. The returned body borrows `buf`.
    pub fn open<'a>(&self, buf: &'a mut [u8]) -> Result<(Header, Body<'a>), Error> {
        let header = Header::decode(buf)?;
        if header.session != self.session {
            return Err(Error::Auth);
        }
        // Accept only counters from the other side's half of the space.
        if (header.counter & RELAY_BIT != 0) == self.relay_side {
            return Err(Error::Auth);
        }
        let split = buf.len() - TAG_LEN;
        let (msg, tag) = buf.split_at_mut(split);
        let (aad, cipher) = msg.split_at_mut(HEADER_LEN);
        self.aead
            .decrypt_in_place_detached(&nonce(header.counter), aad, cipher, Tag::from_slice(tag))
            .map_err(|_| Error::Auth)?;
        let body = Body::decode(header.kind, &buf[HEADER_LEN..split])?;
        Ok((header, body))
    }
}
