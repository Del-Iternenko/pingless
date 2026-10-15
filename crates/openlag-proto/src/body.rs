use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::{Error, Kind};

pub(crate) const DATA_FIXED_LEN: usize = 4 + 8;
pub(crate) const MAX_ADDR_LEN: usize = 1 + 16 + 2;
const PING_LEN: usize = 2 + 4 + 8;

/// Game payload of one flow.
///
/// `peer` is the game server: the destination when the client sends, the source when
/// the relay answers. Carrying it in every packet keeps the relay stateless about flow
/// setup, so a lost "open" message can never strand a flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataBody<'a> {
    pub flow: u32,
    pub seq: u64,
    pub peer: SocketAddr,
    pub payload: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PingBody {
    pub path: u16,
    pub probe: u32,
    /// Client clock in microseconds, echoed back by the relay.
    pub sent_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body<'a> {
    Data(DataBody<'a>),
    Ping(PingBody),
    Pong(PingBody),
    Close { flow: u32 },
}

impl Body<'_> {
    pub fn kind(&self) -> Kind {
        match self {
            Body::Data(_) => Kind::Data,
            Body::Ping(_) => Kind::Ping,
            Body::Pong(_) => Kind::Pong,
            Body::Close { .. } => Kind::Close,
        }
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Body::Data(d) => {
                out.extend_from_slice(&d.flow.to_le_bytes());
                out.extend_from_slice(&d.seq.to_le_bytes());
                encode_addr(d.peer, out);
                out.extend_from_slice(d.payload);
            }
            Body::Ping(p) | Body::Pong(p) => {
                out.extend_from_slice(&p.path.to_le_bytes());
                out.extend_from_slice(&p.probe.to_le_bytes());
                out.extend_from_slice(&p.sent_us.to_le_bytes());
            }
            Body::Close { flow } => out.extend_from_slice(&flow.to_le_bytes()),
        }
    }

    pub(crate) fn decode(kind: Kind, buf: &[u8]) -> Result<Body<'_>, Error> {
        match kind {
            Kind::Data => {
                if buf.len() < DATA_FIXED_LEN {
                    return Err(Error::Truncated);
                }
                let flow = u32::from_le_bytes(buf[0..4].try_into().unwrap());
                let seq = u64::from_le_bytes(buf[4..12].try_into().unwrap());
                let (peer, used) = decode_addr(&buf[12..])?;
                Ok(Body::Data(DataBody {
                    flow,
                    seq,
                    peer,
                    payload: &buf[12 + used..],
                }))
            }
            Kind::Ping | Kind::Pong => {
                if buf.len() != PING_LEN {
                    return Err(Error::Truncated);
                }
                let p = PingBody {
                    path: u16::from_le_bytes(buf[0..2].try_into().unwrap()),
                    probe: u32::from_le_bytes(buf[2..6].try_into().unwrap()),
                    sent_us: u64::from_le_bytes(buf[6..14].try_into().unwrap()),
                };
                Ok(if kind == Kind::Ping {
                    Body::Ping(p)
                } else {
                    Body::Pong(p)
                })
            }
            Kind::Close => {
                let bytes: [u8; 4] = buf.try_into().map_err(|_| Error::Truncated)?;
                Ok(Body::Close {
                    flow: u32::from_le_bytes(bytes),
                })
            }
        }
    }
}

fn encode_addr(addr: SocketAddr, out: &mut Vec<u8>) {
    match addr.ip() {
        IpAddr::V4(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            out.push(6);
            out.extend_from_slice(&ip.octets());
        }
    }
    out.extend_from_slice(&addr.port().to_be_bytes());
}

fn decode_addr(buf: &[u8]) -> Result<(SocketAddr, usize), Error> {
    let (ip, len): (IpAddr, usize) = match buf.first() {
        Some(4) if buf.len() >= 7 => (
            Ipv4Addr::from(<[u8; 4]>::try_from(&buf[1..5]).unwrap()).into(),
            5,
        ),
        Some(6) if buf.len() >= 19 => (
            Ipv6Addr::from(<[u8; 16]>::try_from(&buf[1..17]).unwrap()).into(),
            17,
        ),
        _ => return Err(Error::Address),
    };
    let port = u16::from_be_bytes([buf[len], buf[len + 1]]);
    Ok((SocketAddr::new(ip, port), len + 2))
}
