//! Runs the real xray binary (vendor/xray/xray.exe) against generated configs.
//! Skipped when xray is not present.

use std::path::PathBuf;

use openlag_proxy::{Xray, parse_link};

fn xray_path() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/xray/xray.exe");
    p.exists().then_some(p)
}

const LINKS: &[&str] = &[
    "vless://11111111-2222-4333-8444-555555555555@203.0.113.7:443?type=tcp&security=reality&pbk=SbVKOEMjK0sIlbwg4akyBg5mL5KZwwB-ed4eEE7YnRc&fp=chrome&sni=www.example.com&sid=ab12&spx=%2F&flow=xtls-rprx-vision#reality",
    "vless://11111111-2222-4333-8444-555555555555@cdn.example.com:443?type=ws&security=tls&path=%2Fws&host=cdn.example.com#ws",
    "vless://11111111-2222-4333-8444-555555555555@x.example.com:443?type=xhttp&security=tls&path=%2Fx&mode=auto#xhttp",
    "vless://11111111-2222-4333-8444-555555555555@g.example.com:443?type=grpc&security=tls&serviceName=svc#grpc",
    "trojan://password@t.example.com:443?sni=t.example.com#trojan",
    "ss://Y2hhY2hhMjAtaWV0Zi1wb2x5MTMwNTpzZWNyZXQ@1.2.3.4:8388#ss",
    "wireguard://yAnz5TF%2BlXXJte14tji3zlMNq%2Bhd2rYUIgJBgB3fBmk%3D@203.0.113.5:51820?publickey=xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg%3D&address=10.0.0.2%2F32,fd00::2%2F128&mtu=1420#wg",
];

#[tokio::test]
async fn xray_accepts_generated_configs() {
    let Some(xray) = xray_path() else {
        eprintln!("xray.exe not found, skipping");
        return;
    };
    let servers: Vec<_> = LINKS.iter().map(|l| parse_link(l).unwrap()).collect();
    let dir = std::env::temp_dir().join(format!("openlag-xray-test-{}", std::process::id()));
    let x = Xray::start(&xray, &dir, &servers).await.unwrap();
    for (s, e) in servers.iter().zip(&x.errors) {
        assert!(e.is_none(), "{}: {}", s.name, e.as_deref().unwrap_or(""));
    }
    assert!(x.socks.iter().all(Option::is_some));
    drop(x);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn broken_server_does_not_block_others() {
    let Some(xray) = xray_path() else { return };
    let good = parse_link(LINKS[4]).unwrap();
    let mut bad = good.clone();
    bad.name = "bad".into();
    bad.outbound["streamSettings"]["network"] = serde_json::json!("nonsense-transport");
    let dir = std::env::temp_dir().join(format!("openlag-xray-test2-{}", std::process::id()));
    let x = Xray::start(&xray, &dir, &[bad, good]).await.unwrap();
    assert!(x.errors[0].is_some());
    assert!(x.socks[1].is_some());
    drop(x);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn xray_accepts_advanced_options() {
    let Some(xray) = xray_path() else { return };
    let servers: Vec<_> = LINKS.iter().map(|l| parse_link(l).unwrap()).collect();
    let opts = openlag_proxy::XrayOptions {
        bind_interface: None,
        fingerprint: Some("firefox".into()),
        fragment: Some(openlag_proxy::Fragment {
            packets: "tlshello".into(),
            length: "100-200".into(),
            interval: "10-20".into(),
        }),
        mux_concurrency: 8,
        dns: Some("https://1.1.1.1/dns-query".into()),
    };
    let dir = std::env::temp_dir().join(format!("openlag-xray-test3-{}", std::process::id()));
    let x = Xray::start_with(&xray, &dir, &servers, &opts)
        .await
        .unwrap();
    for (s, e) in servers.iter().zip(&x.errors) {
        assert!(e.is_none(), "{}: {}", s.name, e.as_deref().unwrap_or(""));
    }
    drop(x);
    let _ = std::fs::remove_dir_all(dir);
}
