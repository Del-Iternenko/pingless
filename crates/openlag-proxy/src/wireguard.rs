//! WireGuard servers through xray's userspace WireGuard outbound: no TUN adapter and
//! no driver, the same local SOCKS5 port as every other server.
//!
//! Accepts `.conf` files (`[Interface]` / `[Peer]`), `wireguard://` links (v2rayN
//! format) and AmneziaWG configs whose obfuscation is off (S1 = S2 = 0, H1..H4 = 1..4):
//! those servers also accept plain WireGuard clients. Real AmneziaWG obfuscation is
//! not something xray speaks.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::link::{ProxyServer, b64};

#[derive(Default)]
struct Conf {
    private_key: String,
    addresses: Vec<String>,
    mtu: Option<u32>,
    public_key: String,
    preshared_key: Option<String>,
    endpoint: String,
    keepalive: Option<u32>,
    reserved: Option<Vec<u8>>,
    /// AmneziaWG fields seen in [Interface], lowercase name -> value.
    awg: Vec<(String, String)>,
}

/// True for text that looks like a WireGuard config file.
pub fn is_conf(text: &str) -> bool {
    text.contains("[Interface]") && text.contains("[Peer]")
}

pub fn parse_conf(text: &str, name: &str) -> Result<ProxyServer> {
    let mut c = Conf::default();
    let mut section = String::new();
    let mut peers = 0;
    for line in text.lines() {
        let line = line.split(['#', ';']).next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            section = line.trim_matches(['[', ']']).to_ascii_lowercase();
            if section == "peer" {
                peers += 1;
            }
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
        match (section.as_str(), k.as_str()) {
            ("interface", "privatekey") => c.private_key = v,
            ("interface", "address") => c.addresses.extend(list(&v)),
            ("interface", "mtu") => c.mtu = v.parse().ok(),
            ("interface", "jc" | "jmin" | "jmax" | "s1" | "s2" | "h1" | "h2" | "h3" | "h4") => {
                c.awg.push((k, v))
            }
            // Only the first peer: a client config for one server has exactly one.
            ("peer", "publickey") if peers == 1 => c.public_key = v,
            ("peer", "presharedkey") if peers == 1 => c.preshared_key = Some(v),
            ("peer", "endpoint") if peers == 1 => c.endpoint = v,
            ("peer", "persistentkeepalive") if peers == 1 => c.keepalive = v.parse().ok(),
            _ => {}
        }
    }
    check_awg(&c.awg)?;
    build(c, name)
}

/// `wireguard://<private key>@host:port?publickey=..&address=..&mtu=..&reserved=..#name`
pub fn parse_link(link: &str) -> Result<ProxyServer> {
    let p = crate::link::split(link)?;
    let q = |k: &str| p.query.get(k).map(|v| v.replace(' ', "+"));
    let c = Conf {
        // Keys are base64; a raw '+' in the link was decoded to a space.
        private_key: p.user.replace(' ', "+"),
        addresses: q("address").map(|a| list(&a)).unwrap_or_default(),
        mtu: q("mtu").and_then(|m| m.parse().ok()),
        public_key: q("publickey").or_else(|| q("peer")).unwrap_or_default(),
        preshared_key: q("presharedkey").or_else(|| q("psk")),
        endpoint: format!("{}:{}", bracket(&p.host), p.port),
        keepalive: q("keepalive").and_then(|k| k.parse().ok()),
        reserved: q("reserved").and_then(|r| {
            r.split(',')
                .map(|b| b.trim().parse::<u8>().ok())
                .collect::<Option<Vec<_>>>()
        }),
        awg: Vec::new(),
    };
    let name = crate::link::display_name(&p.name, &p.host, p.port);
    build(c, &name)
}

fn bracket(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

fn list(v: &str) -> Vec<String> {
    v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Plain WireGuard can talk to an AmneziaWG server only when its packet headers and
/// paddings are the WireGuard ones; junk packets (Jc...) are client-side only.
fn check_awg(awg: &[(String, String)]) -> Result<()> {
    for (k, v) in awg {
        let plain = match k.as_str() {
            "s1" | "s2" => v == "0",
            "h1" => v == "1",
            "h2" => v == "2",
            "h3" => v == "3",
            "h4" => v == "4",
            _ => true,
        };
        if !plain {
            bail!(
                "это AmneziaWG с обфускацией ({}={v}): такой протокол xray не поддерживает. Возьмите у провайдера обычный WireGuard или Xray-ключ",
                k.to_uppercase()
            );
        }
    }
    Ok(())
}

fn build(c: Conf, name: &str) -> Result<ProxyServer> {
    key32(&c.private_key).context("PrivateKey")?;
    key32(&c.public_key).context("PublicKey of [Peer]")?;
    if let Some(psk) = &c.preshared_key {
        key32(psk).context("PresharedKey")?;
    }
    anyhow::ensure!(!c.endpoint.is_empty(), "no Endpoint in [Peer]");
    anyhow::ensure!(
        !c.endpoint.contains('$'),
        "Endpoint is a template ({}), not an address",
        c.endpoint
    );
    let (address, port) = split_endpoint(&c.endpoint)?;
    let addresses = if c.addresses.is_empty() {
        bail!("no Address in [Interface]");
    } else {
        // xray wants CIDR; a bare address means a single host.
        c.addresses
            .iter()
            .map(|a| {
                if a.contains('/') {
                    a.clone()
                } else if a.contains(':') {
                    format!("{a}/128")
                } else {
                    format!("{a}/32")
                }
            })
            .collect::<Vec<_>>()
    };
    let has_v6 = addresses.iter().any(|a| a.contains(':'));
    let mut peer = json!({
        "endpoint": c.endpoint,
        "publicKey": c.public_key,
        "allowedIPs": ["0.0.0.0/0", "::/0"],
        "keepAlive": c.keepalive.unwrap_or(25),
    });
    if let Some(psk) = &c.preshared_key {
        peer["preSharedKey"] = json!(psk);
    }
    let mut settings = json!({
        "secretKey": c.private_key,
        "address": addresses,
        "peers": [peer],
        "mtu": c.mtu.unwrap_or(1420),
        "noKernelTun": true,
        "domainStrategy": if has_v6 { "ForceIP" } else { "ForceIPv4" },
    });
    if let Some(r) = c.reserved {
        settings["reserved"] = json!(r);
    }
    Ok(ProxyServer {
        name: if name.trim().is_empty() {
            format!("WireGuard {address}")
        } else {
            name.trim().to_string()
        },
        protocol: "wireguard".into(),
        address,
        port,
        outbound: json!({ "protocol": "wireguard", "settings": settings }),
    })
}

fn key32(k: &str) -> Result<()> {
    let raw = b64(k.trim()).map_err(|_| anyhow!("not a base64 key"))?;
    anyhow::ensure!(raw.len() == 32, "key must be 32 bytes, got {}", raw.len());
    Ok(())
}

fn split_endpoint(e: &str) -> Result<(String, u16)> {
    let (h, p) = if let Some(rest) = e.strip_prefix('[') {
        rest.split_once("]:")
            .ok_or_else(|| anyhow!("bad Endpoint {e}"))?
    } else {
        e.rsplit_once(':')
            .ok_or_else(|| anyhow!("no port in Endpoint {e}"))?
    };
    Ok((
        h.to_string(),
        p.parse()
            .with_context(|| format!("bad port in Endpoint {e}"))?,
    ))
}

/// An Amnezia `amnezia-awg` / `amnezia-wireguard` container: its `last_config` holds
/// the client config text.
pub fn from_amnezia(container: &Value, name: &str) -> Result<ProxyServer> {
    let inner = ["awg", "wireguard"]
        .iter()
        .find_map(|k| container.get(*k))
        .ok_or_else(|| anyhow!("WireGuard container has no settings"))?;
    let last = inner
        .get("last_config")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("WireGuard container has no config"))?;
    let cfg: Value = serde_json::from_str(last).context("WireGuard config is not JSON")?;
    let text = cfg
        .get("config")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("WireGuard container has no config text"))?;
    // The text may still carry placeholders that the JSON next to it fills in.
    let mut text = text.to_string();
    for (field, var) in [
        ("hostName", "$SERVER_IP_ADDRESS"),
        ("port", "$SERVER_PORT"),
        ("client_priv_key", "$WIREGUARD_CLIENT_PRIVATE_KEY"),
        ("server_pub_key", "$WIREGUARD_SERVER_PUBLIC_KEY"),
        ("psk_key", "$WIREGUARD_PSK"),
        ("client_ip", "$WIREGUARD_CLIENT_IP"),
    ] {
        if let Some(v) = cfg.get(field) {
            let v = v
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| v.to_string());
            text = text.replace(var, &v);
        }
    }
    parse_conf(&text, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIV: &str = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";
    const PUB: &str = "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=";

    fn conf(extra: &str) -> String {
        format!(
            "[Interface]\nPrivateKey = {PRIV}\nAddress = 10.8.0.2/32, fd00::2/128\nDNS = 1.1.1.1\n{extra}\n\n[Peer]\nPublicKey = {PUB}\nAllowedIPs = 0.0.0.0/0\nEndpoint = 203.0.113.5:51820\nPersistentKeepalive = 15\n"
        )
    }

    #[test]
    fn conf_file() {
        assert!(is_conf(&conf("")));
        let s = parse_conf(&conf("MTU = 1380"), "Home").unwrap();
        assert_eq!((s.address.as_str(), s.port), ("203.0.113.5", 51820));
        assert_eq!(s.protocol, "wireguard");
        let st = &s.outbound["settings"];
        assert_eq!(st["secretKey"], PRIV);
        assert_eq!(st["mtu"], 1380);
        assert_eq!(st["peers"][0]["keepAlive"], 15);
        assert_eq!(st["domainStrategy"], "ForceIP");
    }

    #[test]
    fn amnezia_without_obfuscation_is_plain_wireguard() {
        let ok =
            conf("Jc = 4\nJmin = 40\nJmax = 70\nS1 = 0\nS2 = 0\nH1 = 1\nH2 = 2\nH3 = 3\nH4 = 4");
        assert!(parse_conf(&ok, "").is_ok());
        let awg = conf("Jc = 4\nS1 = 86\nH1 = 1234567");
        let err = parse_conf(&awg, "").unwrap_err().to_string();
        assert!(err.contains("AmneziaWG"), "{err}");
    }

    #[test]
    fn link() {
        let l = format!(
            "wireguard://{}@[2001:db8::1]:51820?publickey={}&address=10.0.0.2%2F32&reserved=1,2,3&mtu=1280#WG%20test",
            PRIV.replace('+', "%2B").replace('=', "%3D"),
            PUB
        );
        let s = parse_link(&l).unwrap();
        assert_eq!(s.name, "WG test");
        assert_eq!(s.address, "2001:db8::1");
        assert_eq!(
            s.outbound["settings"]["peers"][0]["endpoint"],
            "[2001:db8::1]:51820"
        );
        assert_eq!(s.outbound["settings"]["peers"][0]["publicKey"], PUB);
        assert_eq!(s.outbound["settings"]["reserved"], json!([1, 2, 3]));
    }

    #[test]
    fn bad_keys_are_reported() {
        let bad = conf("").replace(PUB, "nope");
        assert!(parse_conf(&bad, "").is_err());
    }
}
