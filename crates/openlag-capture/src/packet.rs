//! Minimal IPv4/IPv6 + UDP parsing and building for captured and injected packets.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const UDP: u8 = 17;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpPacket<'a> {
    pub src: SocketAddr,
    pub dst: SocketAddr,
    pub payload: &'a [u8],
}

/// Parses an IP packet carrying UDP. Returns None for anything else, including IPv4
/// fragments and IPv6 packets with extension headers, which callers pass through.
pub fn parse_udp(pkt: &[u8]) -> Option<UdpPacket<'_>> {
    let (src_ip, dst_ip, l4): (IpAddr, IpAddr, &[u8]) = match pkt.first()? >> 4 {
        4 => {
            let ihl = (pkt[0] & 0x0f) as usize * 4;
            if ihl < 20 || pkt.len() < ihl || pkt[9] != UDP {
                return None;
            }
            let total = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
            let frag = u16::from_be_bytes([pkt[6], pkt[7]]);
            // More-fragments flag or non-zero offset.
            if frag & 0x3fff != 0 || total > pkt.len() || total < ihl {
                return None;
            }
            let src = Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]);
            let dst = Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]);
            (src.into(), dst.into(), &pkt[ihl..total])
        }
        6 => {
            if pkt.len() < 40 || pkt[6] != UDP {
                return None;
            }
            let plen = u16::from_be_bytes([pkt[4], pkt[5]]) as usize;
            if 40 + plen > pkt.len() {
                return None;
            }
            let src = Ipv6Addr::from(<[u8; 16]>::try_from(&pkt[8..24]).unwrap());
            let dst = Ipv6Addr::from(<[u8; 16]>::try_from(&pkt[24..40]).unwrap());
            (src.into(), dst.into(), &pkt[40..40 + plen])
        }
        _ => return None,
    };
    if l4.len() < 8 {
        return None;
    }
    let ulen = u16::from_be_bytes([l4[4], l4[5]]) as usize;
    if ulen < 8 || ulen > l4.len() {
        return None;
    }
    Some(UdpPacket {
        src: SocketAddr::new(src_ip, u16::from_be_bytes([l4[0], l4[1]])),
        dst: SocketAddr::new(dst_ip, u16::from_be_bytes([l4[2], l4[3]])),
        payload: &l4[8..ulen],
    })
}

/// Builds a complete IP+UDP packet with valid checksums. None if the address
/// families differ or the payload does not fit.
pub fn build_udp(
    src: SocketAddr,
    dst: SocketAddr,
    payload: &[u8],
    out: &mut Vec<u8>,
) -> Option<()> {
    out.clear();
    let ulen = 8 + payload.len();
    match (src.ip(), dst.ip()) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            let total = 20 + ulen;
            if total > u16::MAX as usize {
                return None;
            }
            out.extend_from_slice(&[0x45, 0]);
            out.extend_from_slice(&(total as u16).to_be_bytes());
            out.extend_from_slice(&[0, 0, 0x40, 0, 64, UDP, 0, 0]); // id 0, DF, TTL 64
            out.extend_from_slice(&s.octets());
            out.extend_from_slice(&d.octets());
            let csum = fold(sum(&out[..20], 0));
            out[10..12].copy_from_slice(&csum.to_be_bytes());
            push_udp(src.port(), dst.port(), payload, out);
            let mut pseudo = sum(&s.octets(), 0);
            pseudo = sum(&d.octets(), pseudo);
            pseudo += UDP as u32 + ulen as u32;
            finish_udp_checksum(out, 20, pseudo);
        }
        (IpAddr::V6(s), IpAddr::V6(d)) => {
            if ulen > u16::MAX as usize {
                return None;
            }
            out.extend_from_slice(&[0x60, 0, 0, 0]);
            out.extend_from_slice(&(ulen as u16).to_be_bytes());
            out.extend_from_slice(&[UDP, 64]);
            out.extend_from_slice(&s.octets());
            out.extend_from_slice(&d.octets());
            push_udp(src.port(), dst.port(), payload, out);
            let mut pseudo = sum(&s.octets(), 0);
            pseudo = sum(&d.octets(), pseudo);
            pseudo += UDP as u32 + ulen as u32;
            finish_udp_checksum(out, 40, pseudo);
        }
        _ => return None,
    }
    Some(())
}

fn push_udp(sport: u16, dport: u16, payload: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&sport.to_be_bytes());
    out.extend_from_slice(&dport.to_be_bytes());
    out.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(payload);
}

fn finish_udp_checksum(out: &mut [u8], l4: usize, pseudo: u32) {
    let mut c = fold(sum(&out[l4..], pseudo));
    if c == 0 {
        c = 0xffff; // zero means "no checksum" in UDP
    }
    out[l4 + 6..l4 + 8].copy_from_slice(&c.to_be_bytes());
}

fn sum(data: &[u8], mut acc: u32) -> u32 {
    let (pairs, rest) = data.as_chunks::<2>();
    for &pair in pairs {
        acc += u16::from_be_bytes(pair) as u32;
        acc = (acc & 0xffff) + (acc >> 16);
    }
    if let [last] = rest {
        acc += (*last as u32) << 8;
    }
    acc
}

fn fold(mut acc: u32) -> u16 {
    while acc >> 16 != 0 {
        acc = (acc & 0xffff) + (acc >> 16);
    }
    !(acc as u16)
}

/// True for addresses that should never be tunnelled: LAN, loopback, multicast, etc.
pub fn is_local_scope(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_unspecified()
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies a packet the way a receiving stack would: checksums over the whole
    /// header or segment must fold to zero.
    fn checksums_ok(pkt: &[u8]) -> bool {
        let v4 = pkt[0] >> 4 == 4;
        let l4 = if v4 { 20 } else { 40 };
        let (s, d) = if v4 {
            (&pkt[12..16], &pkt[16..20])
        } else {
            (&pkt[8..24], &pkt[24..40])
        };
        let ulen = pkt.len() - l4;
        let pseudo = sum(d, sum(s, 0)) + UDP as u32 + ulen as u32;
        let udp_ok = fold(sum(&pkt[l4..], pseudo)) == 0;
        let ip_ok = !v4 || fold(sum(&pkt[..20], 0)) == 0;
        udp_ok && ip_ok
    }

    #[test]
    fn build_then_parse_v4_and_v6() {
        for (src, dst) in [
            ("203.0.113.5:27015", "192.168.1.10:50000"),
            ("[2001:db8::5]:27015", "[2001:db8::10]:50000"),
        ] {
            let (src, dst): (SocketAddr, SocketAddr) = (src.parse().unwrap(), dst.parse().unwrap());
            let mut out = Vec::new();
            // Odd length exercises the checksum padding.
            build_udp(src, dst, b"hello!!", &mut out).unwrap();
            assert!(checksums_ok(&out));
            let p = parse_udp(&out).unwrap();
            assert_eq!((p.src, p.dst, p.payload), (src, dst, &b"hello!!"[..]));
        }
    }

    #[test]
    fn rejects_fragments_mixed_families_and_garbage() {
        let mut out = Vec::new();
        build_udp(
            "1.2.3.4:1".parse().unwrap(),
            "5.6.7.8:2".parse().unwrap(),
            b"x",
            &mut out,
        )
        .unwrap();
        out[6] |= 0x20; // more fragments
        assert!(parse_udp(&out).is_none());
        assert!(
            build_udp(
                "1.2.3.4:1".parse().unwrap(),
                "[::1]:2".parse().unwrap(),
                b"x",
                &mut out
            )
            .is_none()
        );
        assert!(parse_udp(&[0x45, 0, 0]).is_none());
        assert!(parse_udp(&[]).is_none());
    }

    #[test]
    fn local_scope() {
        for ip in [
            "192.168.0.1",
            "10.1.2.3",
            "127.0.0.1",
            "224.0.0.251",
            "100.64.0.1",
            "fe80::1",
            "ff02::1",
        ] {
            assert!(is_local_scope(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "2001:4860::8888"] {
            assert!(!is_local_scope(ip.parse().unwrap()), "{ip}");
        }
    }
}
