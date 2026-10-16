//! Fetching and decoding subscriptions from VPN providers (Happ, Incy, v2rayN-style
//! panels such as Marzban or Remnawave) and Amnezia `vpn://` keys.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::amnezia;
use crate::link::{ProxyServer, b64, parse_link};

/// What a subscription or a pasted text turned into.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Parsed {
    pub servers: Vec<ProxyServer>,
    /// Entries that could not be used, with the reason, for display.
    pub skipped: Vec<Skipped>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skipped {
    pub entry: String,
    pub reason: String,
}

/// Metadata providers send in response headers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubscriptionInfo {
    pub title: Option<String>,
    pub upload: Option<u64>,
    pub download: Option<u64>,
    pub total: Option<u64>,
    /// Unix seconds.
    pub expire: Option<u64>,
    pub update_interval_hours: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Many panels choose the response format by User-Agent.
    pub user_agent: String,
    /// Stable device id; panels with a device limit require it.
    pub hwid: Option<String>,
}

impl Default for FetchOptions {
    fn default() -> Self {
        FetchOptions {
            user_agent: "v2rayN/7.13.8".into(),
            hwid: None,
        }
    }
}

pub async fn fetch(url: &str, opts: &FetchOptions) -> Result<(Parsed, SubscriptionInfo)> {
    let url = url.trim();
    if url.starts_with("happ://crypt") {
        bail!(
            "encrypted happ://crypt links can only be opened by Happ; ask your provider for the plain https subscription link"
        );
    }
    // happ://add/<url> and similar wrappers just carry the real URL.
    let url = url.strip_prefix("happ://add/").unwrap_or(url);
    anyhow::ensure!(
        url.starts_with("http://") || url.starts_with("https://"),
        "subscription must be an http(s) link"
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(&opts.user_agent)
        .build()?;
    let mut req = client.get(url);
    if let Some(hwid) = &opts.hwid {
        req = req
            .header("x-hwid", hwid)
            .header("x-device-os", "Windows")
            .header("x-device-model", "PC");
    }
    let resp = req
        .send()
        .await
        .context("cannot reach the subscription server")?;
    let status = resp.status();
    let info = info_from_headers(resp.headers());
    let body = resp.text().await?;
    anyhow::ensure!(
        status.is_success(),
        "subscription server answered HTTP {status}"
    );
    let trimmed = body.trim_start();
    if trimmed.starts_with("<!") || trimmed.to_ascii_lowercase().starts_with("<html") {
        bail!(
            "the server returned a web page instead of a subscription; the link may be wrong or need another User-Agent"
        );
    }
    Ok((parse_text(&body), info))
}

/// Parses anything a user may paste: links, base64 lists, JSON configs, Amnezia keys.
pub fn parse_text(text: &str) -> Parsed {
    let text = text.trim().trim_start_matches('\u{feff}');
    let mut out = Parsed::default();
    if text.is_empty() {
        return out;
    }
    if crate::wireguard::is_conf(text) {
        match crate::wireguard::parse_conf(text, "") {
            Ok(s) => out.servers.push(s),
            Err(e) => out.skipped.push(Skipped {
                entry: "WireGuard".into(),
                reason: format!("{e:#}"),
            }),
        }
        return out;
    }
    if text.starts_with('{') || text.starts_with('[') {
        match serde_json::from_str::<Value>(text) {
            Ok(v) => {
                parse_json(&v, &mut out);
                return out;
            }
            Err(e) => {
                out.skipped.push(Skipped {
                    entry: "JSON".into(),
                    reason: e.to_string(),
                });
                return out;
            }
        }
    }
    // A whole-body base64 blob that decodes to links.
    if !text.contains("://")
        && let Ok(decoded) = b64(text)
        && let Ok(decoded) = String::from_utf8(decoded)
        && decoded.contains("://")
    {
        return parse_text(&decoded);
    }
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("//"))
    {
        let result = if line.starts_with("vpn://") {
            amnezia::parse_key(line)
        } else {
            parse_link(line)
        };
        match result {
            Ok(s) => out.servers.push(s),
            Err(e) => out.skipped.push(Skipped {
                entry: short(line),
                reason: format!("{e:#}"),
            }),
        }
    }
    out
}

/// Xray JSON: a single config with outbounds, or an array of them (one per server,
/// as Happ- and Remnawave-style JSON subscriptions send).
fn parse_json(v: &Value, out: &mut Parsed) {
    match v {
        Value::Array(items) => {
            for item in items {
                parse_json(item, out);
            }
        }
        Value::Object(obj) if obj.contains_key("outbounds") => {
            let name = obj
                .get("remarks")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            match crate::xray::server_from_config(v, &name) {
                Ok(s) => out.servers.push(s),
                Err(e) => out.skipped.push(Skipped {
                    entry: if name.is_empty() {
                        "JSON config".into()
                    } else {
                        name
                    },
                    reason: format!("{e:#}"),
                }),
            }
        }
        _ => out.skipped.push(Skipped {
            entry: "JSON".into(),
            reason: "not an Xray config".into(),
        }),
    }
}

fn short(s: &str) -> String {
    // Never show full credentials in the UI.
    let scheme = s.split("://").next().unwrap_or("");
    let name = s.rsplit_once('#').map(|(_, n)| {
        percent_encoding::percent_decode_str(n)
            .decode_utf8_lossy()
            .into_owned()
    });
    match name {
        Some(n) if !n.is_empty() => format!("{scheme}://… {n}"),
        _ => format!("{scheme}://…"),
    }
}

fn info_from_headers(h: &reqwest::header::HeaderMap) -> SubscriptionInfo {
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
    let mut info = SubscriptionInfo::default();
    if let Some(title) = get("profile-title") {
        info.title = Some(match title.strip_prefix("base64:") {
            Some(b) => b64(b)
                .ok()
                .and_then(|d| String::from_utf8(d).ok())
                .unwrap_or(title),
            None => title,
        });
    }
    if let Some(u) = get("subscription-userinfo") {
        for part in u.split(';') {
            if let Some((k, v)) = part.trim().split_once('=') {
                let n = v.trim().parse::<f64>().ok().map(|f| f as u64);
                match k.trim() {
                    "upload" => info.upload = n,
                    "download" => info.download = n,
                    "total" => info.total = n,
                    "expire" => info.expire = n.filter(|&e| e > 0),
                    _ => {}
                }
            }
        }
    }
    info.update_interval_hours = get("profile-update-interval").and_then(|v| v.trim().parse().ok());
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn base64_list_with_bad_entries() {
        let list = "vless://id@1.1.1.1:443?security=none#A\ntuic://x@h:1#H\n\ntrojan://pw@t.example:443#T\n";
        let p = parse_text(&base64::engine::general_purpose::STANDARD.encode(list));
        assert_eq!(p.servers.len(), 2);
        assert_eq!(p.skipped.len(), 1);
        assert!(p.skipped[0].entry.contains('H'));
        assert!(
            !p.skipped[0].entry.contains("x@h"),
            "credentials leaked into UI text"
        );
    }

    #[test]
    fn json_array_subscription() {
        let cfg = r#"[{"remarks":"NL","outbounds":[
            {"tag":"proxy","protocol":"vless","settings":{"vnext":[{"address":"nl.example","port":443,"users":[{"id":"u","encryption":"none"}]}]}},
            {"tag":"direct","protocol":"freedom"}]}]"#;
        let p = parse_text(cfg);
        assert_eq!(p.servers.len(), 1, "{:?}", p.skipped);
        assert_eq!(p.servers[0].name, "NL");
        assert_eq!(p.servers[0].address, "nl.example");
        assert!(p.servers[0].outbound.get("tag").is_none());
    }
}
