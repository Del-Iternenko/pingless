//! Share links (`vless://`, `vmess://`, `trojan://`, `ss://`) to Xray outbounds.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// One server from a subscription, ready to be used as an Xray outbound.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProxyServer {
    pub name: String,
    /// `vless`, `vmess`, `trojan` or `shadowsocks`.
    pub protocol: String,
    pub address: String,
    pub port: u16,
    /// Xray outbound object without a tag.
    pub outbound: Value,
}

impl ProxyServer {
    /// Stable identifier, used to remember which server the user picked.
    pub fn key(&self) -> String {
        format!(
            "{}://{}:{}#{}",
            self.protocol, self.address, self.port, self.name
        )
    }
}

/// Parses one share link.
pub fn parse_link(link: &str) -> Result<ProxyServer> {
    let link = link.trim();
    let (scheme, _) = link
        .split_once("://")
        .ok_or_else(|| anyhow!("not a link"))?;
    match scheme.to_ascii_lowercase().as_str() {
        "vless" => parse_vless_trojan(link, "vless"),
        "trojan" => parse_vless_trojan(link, "trojan"),
        "vmess" => parse_vmess(link),
        "ss" => parse_ss(link),
        "hysteria2" | "hy2" | "tuic" | "wireguard" | "socks" | "http" | "https" => {
            bail!("{scheme}:// is not supported yet")
        }
        _ => bail!("unknown link type {scheme}://"),
    }
}

/// Splits `scheme://userinfo@host:port?query#fragment` without assuming a URL crate's
/// opinion on unusual hosts.
struct Parts {
    user: String,
    host: String,
    port: u16,
    query: HashMap<String, String>,
    name: String,
}

fn split(link: &str) -> Result<Parts> {
    let rest = link.split_once("://").map(|(_, r)| r).unwrap_or(link);
    let (rest, name) = match rest.split_once('#') {
        Some((r, f)) => (r, decode(f)),
        None => (rest, String::new()),
    };
    let (rest, query) = match rest.split_once('?') {
        Some((r, q)) => (r, parse_query(q)),
        None => (rest, HashMap::new()),
    };
    let rest = rest.trim_end_matches('/');
    let (user, hostport) = match rest.rsplit_once('@') {
        Some((u, h)) => (decode(u), h),
        None => (String::new(), rest),
    };
    let (host, port) = split_host_port(hostport)?;
    Ok(Parts {
        user,
        host,
        port,
        query,
        name,
    })
}

fn split_host_port(s: &str) -> Result<(String, u16)> {
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (h, p) = rest
            .split_once("]:")
            .ok_or_else(|| anyhow!("bad IPv6 address {s}"))?;
        (h.to_string(), p)
    } else {
        let (h, p) = s
            .rsplit_once(':')
            .ok_or_else(|| anyhow!("no port in {s}"))?;
        (h.to_string(), p)
    };
    anyhow::ensure!(!host.is_empty(), "empty host");
    Ok((
        host,
        port.parse().with_context(|| format!("bad port {port}"))?,
    ))
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(kv), String::new()),
        })
        .collect()
}

fn decode(s: &str) -> String {
    percent_decode_str(&s.replace('+', "%20"))
        .decode_utf8_lossy()
        .into_owned()
}

/// Decodes standard or URL-safe base64, with or without padding.
pub(crate) fn b64(s: &str) -> Result<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
        if let Ok(v) = engine.decode(&s) {
            return Ok(v);
        }
    }
    bail!("invalid base64")
}

fn display_name(name: &str, host: &str, port: u16) -> String {
    if name.trim().is_empty() {
        format!("{host}:{port}")
    } else {
        name.trim().to_string()
    }
}

fn parse_vless_trojan(link: &str, protocol: &str) -> Result<ProxyServer> {
    let p = split(link)?;
    anyhow::ensure!(
        !p.user.is_empty(),
        "{protocol} link has no {}",
        if protocol == "vless" {
            "id"
        } else {
            "password"
        }
    );
    let q = &p.query;
    let settings = if protocol == "vless" {
        let mut user = json!({ "id": p.user, "encryption": q.get("encryption").map(String::as_str).unwrap_or("none") });
        if let Some(flow) = q.get("flow").filter(|f| !f.is_empty()) {
            user["flow"] = json!(flow);
        }
        json!({ "vnext": [{ "address": p.host, "port": p.port, "users": [user] }] })
    } else {
        json!({ "servers": [{ "address": p.host, "port": p.port, "password": p.user }] })
    };
    let default_security = if protocol == "trojan" { "tls" } else { "none" };
    let stream = stream_settings(
        q.get("type").map(String::as_str).unwrap_or("tcp"),
        q.get("security")
            .map(String::as_str)
            .unwrap_or(default_security),
        q,
        &p.host,
    )?;
    Ok(ProxyServer {
        name: display_name(&p.name, &p.host, p.port),
        protocol: protocol.into(),
        address: p.host,
        port: p.port,
        outbound: json!({ "protocol": protocol, "settings": settings, "streamSettings": stream }),
    })
}

fn parse_vmess(link: &str) -> Result<ProxyServer> {
    let body = link.split_once("://").map(|(_, r)| r).unwrap_or_default();
    // Some clients append a fragment to the base64 blob.
    let body = body.split('#').next().unwrap_or_default();
    let raw = b64(body).context("vmess link is not base64 JSON")?;
    let v: Value = serde_json::from_slice(&raw).context("vmess link is not base64 JSON")?;
    let s = |k: &str| match &v[k] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    let host = s("add");
    let port: u16 = s("port").parse().context("vmess: bad port")?;
    anyhow::ensure!(
        !host.is_empty() && !s("id").is_empty(),
        "vmess: missing address or id"
    );
    let mut q: HashMap<String, String> = HashMap::new();
    for (from, to) in [
        ("host", "host"),
        ("path", "path"),
        ("sni", "sni"),
        ("alpn", "alpn"),
        ("fp", "fp"),
        ("type", "headerType"),
    ] {
        let val = s(from);
        if !val.is_empty() {
            q.insert(to.into(), val);
        }
    }
    let net = match s("net").as_str() {
        "" => "tcp".to_string(),
        n => n.to_string(),
    };
    if net == "grpc" {
        q.insert("serviceName".into(), s("path"));
        if s("type") == "multi" {
            q.insert("mode".into(), "multi".into());
        }
    }
    let security = if s("tls") == "tls" { "tls" } else { "none" };
    let stream = stream_settings(&net, security, &q, &host)?;
    let aid: u32 = s("aid").parse().unwrap_or(0);
    let scy = match s("scy").as_str() {
        "" => "auto".to_string(),
        x => x.to_string(),
    };
    Ok(ProxyServer {
        name: display_name(&s("ps"), &host, port),
        protocol: "vmess".into(),
        address: host.clone(),
        port,
        outbound: json!({
            "protocol": "vmess",
            "settings": { "vnext": [{ "address": host, "port": port, "users": [{ "id": s("id"), "alterId": aid, "security": scy }] }] },
            "streamSettings": stream,
        }),
    })
}

fn parse_ss(link: &str) -> Result<ProxyServer> {
    let body = link.split_once("://").map(|(_, r)| r).unwrap_or_default();
    let (body, name) = match body.split_once('#') {
        Some((b, f)) => (b, decode(f)),
        None => (body, String::new()),
    };
    let (body, query) = match body.split_once('?') {
        Some((b, q)) => (b, parse_query(q)),
        None => (body, HashMap::new()),
    };
    if query.contains_key("plugin") {
        bail!("ss:// with plugins is not supported yet");
    }
    let body = body.trim_end_matches('/');
    // SIP002: base64(method:password)@host:port, or legacy base64(method:password@host:port).
    let (userinfo, hostport) = match body.rsplit_once('@') {
        Some((u, h)) => {
            let u = decode(u);
            let creds = match b64(&u) {
                Ok(d) if String::from_utf8(d.clone()).is_ok_and(|s| s.contains(':')) => {
                    String::from_utf8(d).unwrap()
                }
                _ => u, // SIP022 keys are sent in clear text
            };
            (creds, h.to_string())
        }
        None => {
            let decoded = String::from_utf8(b64(body)?).context("ss: bad base64")?;
            let (u, h) = decoded
                .rsplit_once('@')
                .ok_or_else(|| anyhow!("ss: no server"))?;
            (u.to_string(), h.to_string())
        }
    };
    let (method, password) = userinfo
        .split_once(':')
        .ok_or_else(|| anyhow!("ss: no method"))?;
    let (host, port) = split_host_port(&hostport)?;
    Ok(ProxyServer {
        name: display_name(&name, &host, port),
        protocol: "shadowsocks".into(),
        address: host.clone(),
        port,
        outbound: json!({
            "protocol": "shadowsocks",
            "settings": { "servers": [{ "address": host, "port": port, "method": method, "password": password }] },
        }),
    })
}

fn stream_settings(
    network: &str,
    security: &str,
    q: &HashMap<String, String>,
    host: &str,
) -> Result<Value> {
    let get = |k: &str| q.get(k).filter(|v| !v.is_empty()).cloned();
    let mut s = Map::new();
    let network = match network {
        "" | "tcp" | "raw" => "raw",
        "splithttp" => "xhttp",
        "h2" => "http",
        other => other,
    };
    s.insert("network".into(), json!(network));
    match network {
        "raw" => {
            if get("headerType").as_deref() == Some("http") {
                let path: Vec<String> = get("path")
                    .unwrap_or("/".into())
                    .split(',')
                    .map(String::from)
                    .collect();
                let mut request = json!({ "path": path });
                if let Some(h) = get("host") {
                    request["headers"] = json!({ "Host": h.split(',').collect::<Vec<_>>() });
                }
                s.insert(
                    "rawSettings".into(),
                    json!({ "header": { "type": "http", "request": request } }),
                );
            }
        }
        "ws" => {
            let mut ws = json!({ "path": get("path").unwrap_or("/".into()) });
            if let Some(h) = get("host") {
                ws["host"] = json!(h);
            }
            s.insert("wsSettings".into(), ws);
        }
        "httpupgrade" => {
            let mut hu = json!({ "path": get("path").unwrap_or("/".into()) });
            if let Some(h) = get("host") {
                hu["host"] = json!(h);
            }
            s.insert("httpupgradeSettings".into(), hu);
        }
        "grpc" => {
            let mut g = json!({ "serviceName": get("serviceName").unwrap_or_default(), "multiMode": get("mode").as_deref() == Some("multi") });
            if let Some(a) = get("authority") {
                g["authority"] = json!(a);
            }
            s.insert("grpcSettings".into(), g);
        }
        "xhttp" => {
            let mut x = json!({ "path": get("path").unwrap_or("/".into()), "mode": get("mode").unwrap_or("auto".into()) });
            if let Some(h) = get("host") {
                x["host"] = json!(h);
            }
            if let Some(extra) = get("extra")
                && let Ok(v) = serde_json::from_str::<Value>(&extra)
            {
                x["extra"] = v;
            }
            s.insert("xhttpSettings".into(), x);
        }
        "kcp" | "mkcp" => {
            let mut k = json!({ "header": { "type": get("headerType").unwrap_or("none".into()) } });
            if let Some(seed) = get("seed") {
                k["seed"] = json!(seed);
            }
            s.insert("network".into(), json!("kcp"));
            s.insert("kcpSettings".into(), k);
        }
        "http" => {
            let mut h = json!({ "path": get("path").unwrap_or("/".into()) });
            if let Some(hh) = get("host") {
                h["host"] = json!(hh.split(',').collect::<Vec<_>>());
            }
            s.insert("httpSettings".into(), h);
        }
        "quic" => bail!("QUIC transport is not supported by current Xray"),
        other => bail!("unknown transport {other}"),
    }

    let sni = get("sni")
        .or_else(|| get("peer"))
        .or_else(|| get("host").filter(|_| security == "tls"));
    let alpn: Option<Vec<String>> = get("alpn").map(|a| a.split(',').map(String::from).collect());
    match security {
        "" | "none" => {}
        "tls" | "xtls" => {
            let mut t = json!({ "serverName": sni.unwrap_or_else(|| host.to_string()) });
            if let Some(fp) = get("fp") {
                t["fingerprint"] = json!(fp);
            }
            if let Some(alpn) = alpn {
                t["alpn"] = json!(alpn);
            }
            if matches!(
                get("allowInsecure").or_else(|| get("insecure")).as_deref(),
                Some("1" | "true")
            ) {
                t["allowInsecure"] = json!(true);
            }
            s.insert("security".into(), json!("tls"));
            s.insert("tlsSettings".into(), t);
        }
        "reality" => {
            let pbk = get("pbk").ok_or_else(|| anyhow!("reality link has no public key (pbk)"))?;
            let mut r = json!({
                "serverName": sni.unwrap_or_default(),
                "fingerprint": get("fp").unwrap_or("chrome".into()),
                "publicKey": pbk,
                "shortId": get("sid").unwrap_or_default(),
                "spiderX": get("spx").unwrap_or_default(),
            });
            if let Some(pqv) = get("pqv") {
                r["mldsa65Verify"] = json!(pqv);
            }
            s.insert("security".into(), json!("reality"));
            s.insert("realitySettings".into(), r);
        }
        other => bail!("unknown security {other}"),
    }
    Ok(Value::Object(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vless_reality() {
        let s = parse_link(
            "vless://0b1c2d3e-0000-4000-8000-000000000001@203.0.113.7:443?type=tcp&security=reality\
             &pbk=PUBKEY&fp=chrome&sni=www.example.com&sid=ab12&spx=%2F&flow=xtls-rprx-vision#%F0%9F%87%A9%F0%9F%87%AA%20Germany",
        )
        .unwrap();
        assert_eq!(s.name, "🇩🇪 Germany");
        assert_eq!((s.address.as_str(), s.port), ("203.0.113.7", 443));
        let o = &s.outbound;
        assert_eq!(
            o["settings"]["vnext"][0]["users"][0]["flow"],
            "xtls-rprx-vision"
        );
        assert_eq!(o["streamSettings"]["network"], "raw");
        assert_eq!(
            o["streamSettings"]["realitySettings"]["publicKey"],
            "PUBKEY"
        );
        assert_eq!(o["streamSettings"]["realitySettings"]["spiderX"], "/");
    }

    #[test]
    fn vless_ws_tls_ipv6() {
        let s = parse_link(
            "vless://id@[2001:db8::1]:8443?type=ws&security=tls&path=%2Fws&host=cdn.example.com#x",
        )
        .unwrap();
        assert_eq!(s.address, "2001:db8::1");
        let st = &s.outbound["streamSettings"];
        assert_eq!(st["wsSettings"]["path"], "/ws");
        assert_eq!(st["tlsSettings"]["serverName"], "cdn.example.com");
    }

    #[test]
    fn vmess_grpc() {
        let json = r#"{"v":"2","ps":"JP","add":"vm.example.com","port":"443","id":"uuid","aid":"0","net":"grpc","path":"svc","tls":"tls","sni":"vm.example.com"}"#;
        let link = format!(
            "vmess://{}",
            base64::engine::general_purpose::STANDARD.encode(json)
        );
        let s = parse_link(&link).unwrap();
        assert_eq!(s.name, "JP");
        assert_eq!(
            s.outbound["streamSettings"]["grpcSettings"]["serviceName"],
            "svc"
        );
        assert_eq!(s.outbound["streamSettings"]["security"], "tls");
    }

    #[test]
    fn trojan_defaults_to_tls() {
        let s = parse_link("trojan://pass%40word@tr.example.com:443#T").unwrap();
        assert_eq!(
            s.outbound["settings"]["servers"][0]["password"],
            "pass@word"
        );
        assert_eq!(s.outbound["streamSettings"]["security"], "tls");
    }

    #[test]
    fn shadowsocks_sip002_and_legacy() {
        let creds = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode("chacha20-ietf-poly1305:secret");
        let s = parse_link(&format!("ss://{creds}@1.2.3.4:8388#SS")).unwrap();
        assert_eq!(
            s.outbound["settings"]["servers"][0]["method"],
            "chacha20-ietf-poly1305"
        );
        let legacy = base64::engine::general_purpose::STANDARD.encode("aes-256-gcm:pw@5.6.7.8:443");
        let s = parse_link(&format!("ss://{legacy}#L")).unwrap();
        assert_eq!((s.address.as_str(), s.port), ("5.6.7.8", 443));
        // SIP022 keys are not base64-wrapped.
        let s =
            parse_link("ss://2022-blake3-aes-128-gcm:a2V5a2V5a2V5a2V5a2V5aw%3D%3D@9.9.9.9:443#n")
                .unwrap();
        assert_eq!(
            s.outbound["settings"]["servers"][0]["method"],
            "2022-blake3-aes-128-gcm"
        );
    }

    #[test]
    fn unsupported_is_reported() {
        assert!(
            parse_link("hysteria2://x@h:1")
                .unwrap_err()
                .to_string()
                .contains("not supported")
        );
        assert!(parse_link("ss://YWVzOnB3@h:1?plugin=obfs").is_err());
    }
}
