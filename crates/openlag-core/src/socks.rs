//! SOCKS5 UDP ASSOCIATE client (RFC 1928), used to reach proxy exits through xray.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{Result, anyhow, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

/// One UDP association. It lives as long as the TCP control connection.
pub(crate) struct Association {
    pub(crate) ctrl: TcpStream,
    pub(crate) udp: UdpSocket,
}

impl Association {
    pub(crate) async fn open(socks: SocketAddr) -> Result<Association> {
        let mut ctrl = TcpStream::connect(socks).await?;
        ctrl.set_nodelay(true)?;
        ctrl.write_all(&[5, 1, 0]).await?;
        let mut hello = [0u8; 2];
        ctrl.read_exact(&mut hello).await?;
        if hello != [5, 0] {
            bail!("SOCKS server refused no-auth login");
        }
        // UDP ASSOCIATE; 0.0.0.0:0 means "I'll tell you my address with the packets".
        ctrl.write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        let mut head = [0u8; 4];
        ctrl.read_exact(&mut head).await?;
        if head[1] != 0 {
            bail!("SOCKS UDP ASSOCIATE failed with code {}", head[1]);
        }
        let ip: IpAddr = match head[3] {
            1 => {
                let mut a = [0u8; 4];
                ctrl.read_exact(&mut a).await?;
                Ipv4Addr::from(a).into()
            }
            4 => {
                let mut a = [0u8; 16];
                ctrl.read_exact(&mut a).await?;
                Ipv6Addr::from(a).into()
            }
            3 => {
                let len = ctrl.read_u8().await? as usize;
                let mut skip = vec![0u8; len];
                ctrl.read_exact(&mut skip).await?;
                socks.ip()
            }
            t => bail!("bad SOCKS address type {t}"),
        };
        let port = ctrl.read_u16().await?;
        // A server bound to 0.0.0.0 means "same host as the TCP endpoint".
        let relay = SocketAddr::new(if ip.is_unspecified() { socks.ip() } else { ip }, port);
        let bind: SocketAddr = if relay.is_ipv4() {
            (Ipv4Addr::LOCALHOST, 0).into()
        } else {
            (Ipv6Addr::LOCALHOST, 0).into()
        };
        let udp = UdpSocket::bind(bind).await?;
        udp.connect(relay).await?;
        Ok(Association { ctrl, udp })
    }
}

/// Opens a TCP connection to `dst` through the SOCKS5 server.
pub(crate) async fn connect(socks: SocketAddr, dst: SocketAddr) -> Result<TcpStream> {
    let mut s = TcpStream::connect(socks).await?;
    s.set_nodelay(true)?;
    s.write_all(&[5, 1, 0]).await?;
    let mut hello = [0u8; 2];
    s.read_exact(&mut hello).await?;
    if hello != [5, 0] {
        bail!("SOCKS server refused no-auth login");
    }
    let mut req = vec![5, 1, 0];
    match dst.ip() {
        IpAddr::V4(ip) => {
            req.push(1);
            req.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            req.push(4);
            req.extend_from_slice(&ip.octets());
        }
    }
    req.extend_from_slice(&dst.port().to_be_bytes());
    s.write_all(&req).await?;
    let mut head = [0u8; 4];
    s.read_exact(&mut head).await?;
    if head[1] != 0 {
        bail!("SOCKS CONNECT to {dst} failed with code {}", head[1]);
    }
    let skip = match head[3] {
        1 => 4,
        4 => 16,
        3 => s.read_u8().await? as usize,
        t => bail!("bad SOCKS address type {t}"),
    };
    let mut rest = vec![0u8; skip + 2];
    s.read_exact(&mut rest).await?;
    Ok(s)
}

/// `RSV RSV FRAG ATYP ADDR PORT DATA`
pub(crate) fn encode(dst: SocketAddr, payload: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(&[0, 0, 0]);
    match dst.ip() {
        IpAddr::V4(ip) => {
            out.push(1);
            out.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
        }
    }
    out.extend_from_slice(&dst.port().to_be_bytes());
    out.extend_from_slice(payload);
}

pub(crate) fn decode(buf: &[u8]) -> Result<(SocketAddr, &[u8])> {
    if buf.len() < 4 || buf[2] != 0 {
        bail!("short or fragmented SOCKS datagram");
    }
    let (ip, rest): (IpAddr, &[u8]) = match buf[3] {
        1 if buf.len() >= 10 => (
            Ipv4Addr::from(<[u8; 4]>::try_from(&buf[4..8]).unwrap()).into(),
            &buf[8..],
        ),
        4 if buf.len() >= 22 => (
            Ipv6Addr::from(<[u8; 16]>::try_from(&buf[4..20]).unwrap()).into(),
            &buf[20..],
        ),
        _ => return Err(anyhow!("unsupported SOCKS address in reply")),
    };
    let port = u16::from_be_bytes([rest[0], rest[1]]);
    Ok((SocketAddr::new(ip, port), &rest[2..]))
}

/// A DNS query used as a UDP latency probe through a proxy. The probe id is the
/// DNS transaction id.
pub(crate) fn dns_probe(id: u16) -> Vec<u8> {
    let mut q = Vec::with_capacity(32);
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[1, 0, 0, 1, 0, 0, 0, 0, 0, 0]); // RD, 1 question
    for label in ["one", "one", "one", "one"] {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]); // A, IN
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let mut out = Vec::new();
        for dst in ["1.2.3.4:27015", "[2001:db8::1]:443"] {
            let dst: SocketAddr = dst.parse().unwrap();
            encode(dst, b"payload", &mut out);
            assert_eq!(decode(&out).unwrap(), (dst, &b"payload"[..]));
        }
        assert!(
            decode(&[0, 0, 1, 1, 0, 0, 0, 0, 0, 0]).is_err(),
            "fragments are not supported"
        );
    }
}
