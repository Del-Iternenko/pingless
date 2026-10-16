//! Amnezia `vpn://` keys: URL-safe base64 of a Qt `qCompress` blob (4-byte big-endian
//! length followed by a zlib stream) holding a JSON description of the server.

use std::io::Read;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::link::{ProxyServer, b64};

pub fn parse_key(key: &str) -> Result<ProxyServer> {
    let json = decode_key(key)?;
    let name = json
        .get("description")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| json.get("hostName").and_then(Value::as_str))
        .unwrap_or("Amnezia")
        .to_string();
    let containers = json
        .get("containers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut kinds = Vec::new();
    for c in &containers {
        let kind = c.get("container").and_then(Value::as_str).unwrap_or("");
        kinds.push(kind.to_string());
        if kind == "amnezia-xray" {
            let last = c
                .pointer("/xray/last_config")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("Xray container has no config"))?;
            let cfg: Value =
                serde_json::from_str(last).context("Xray container config is not JSON")?;
            return crate::xray::server_from_config(&cfg, &name);
        }
    }
    if let Some(c) = containers.iter().find(|c| {
        matches!(
            c.get("container").and_then(Value::as_str),
            Some("amnezia-awg" | "amnezia-wireguard")
        )
    }) {
        return crate::wireguard::from_amnezia(c, &name);
    }
    bail!(
        "no supported protocol in this key (found: {})",
        kinds.join(", ")
    )
}

pub fn decode_key(key: &str) -> Result<Value> {
    let body = key
        .trim()
        .strip_prefix("vpn://")
        .ok_or_else(|| anyhow!("not a vpn:// key"))?;
    let raw = b64(body).context("vpn:// key is not base64")?;
    // Old keys are plain JSON; current ones are qCompress'ed.
    if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
        return Ok(v);
    }
    anyhow::ensure!(raw.len() > 4, "vpn:// key is too short");
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(&raw[4..])
        .read_to_end(&mut out)
        .context("vpn:// key: bad compression")?;
    serde_json::from_slice(&out).context("vpn:// key: not JSON inside")
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use std::io::Write;

    fn make_key(json: &str) -> String {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(json.as_bytes()).unwrap();
        let mut blob = (json.len() as u32).to_be_bytes().to_vec();
        blob.extend(z.finish().unwrap());
        format!(
            "vpn://{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(blob)
        )
    }

    #[test]
    fn xray_container() {
        let inner = r#"{"outbounds":[{"protocol":"vless","settings":{"vnext":[{"address":"5.5.5.5","port":443,"users":[{"id":"u","flow":"xtls-rprx-vision","encryption":"none"}]}]},"streamSettings":{"network":"tcp","security":"reality"}}]}"#;
        let json = serde_json::json!({
            "description": "My Amnezia",
            "hostName": "5.5.5.5",
            "containers": [{ "container": "amnezia-xray", "xray": { "last_config": inner } }]
        });
        let s = parse_key(&make_key(&json.to_string())).unwrap();
        assert_eq!(s.name, "My Amnezia");
        assert_eq!((s.address.as_str(), s.port), ("5.5.5.5", 443));
    }

    #[test]
    fn wireguard_container() {
        let conf = "[Interface]\nPrivateKey = $WIREGUARD_CLIENT_PRIVATE_KEY\nAddress = 10.8.1.2/32\nS1 = 0\nS2 = 0\nH1 = 1\nH2 = 2\nH3 = 3\nH4 = 4\n[Peer]\nPublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=\nEndpoint = $SERVER_IP_ADDRESS:$SERVER_PORT\n";
        let last = serde_json::json!({
            "config": conf,
            "hostName": "198.51.100.9",
            "port": 32123,
            "client_priv_key": "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=",
        });
        let json = serde_json::json!({
            "description": "AWG",
            "containers": [{ "container": "amnezia-awg", "awg": { "last_config": last.to_string() } }]
        });
        let s = parse_key(&make_key(&json.to_string())).unwrap();
        assert_eq!((s.address.as_str(), s.port), ("198.51.100.9", 32123));
        assert_eq!(s.protocol, "wireguard");
    }
}
