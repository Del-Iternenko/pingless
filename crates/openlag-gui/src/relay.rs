//! One-click install of our own relay (exit node) on the user's Linux server over SSH:
//! uploads the static Linux build that ships with OpenLag, runs it as a systemd
//! service and opens the UDP port in ufw/firewalld. Progress goes to a terminal tab.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use russh::ChannelMsg;
use tauri::AppHandle;

use crate::term::{Client, SshHost, connect, emit_data};

type Session = russh::client::Handle<Client>;

/// Relay binaries next to the app (`relay/openlag-relay-<arch>`), or in the source
/// tree for dev builds.
pub fn binary(dirs: &[PathBuf], arch: &str) -> Option<PathBuf> {
    let name = format!("openlag-relay-{arch}");
    let mut candidates: Vec<PathBuf> = dirs.iter().map(|d| d.join("relay").join(&name)).collect();
    if cfg!(debug_assertions) {
        let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/dist");
        candidates.push(target.join(format!("{arch}-unknown-linux-musl/release/openlag-relay")));
    }
    candidates.into_iter().find(|p| p.exists())
}

fn say(app: &AppHandle, id: u32, text: &str) {
    emit_data(app, id, format!("\x1b[36m▸ {text}\x1b[0m\r\n").as_bytes());
}

/// Runs `cmd`, feeding `stdin` if given. Output is echoed to the tab when `echo`.
/// Returns the exit status and stdout.
async fn exec(
    app: &AppHandle,
    id: u32,
    session: &Session,
    cmd: &str,
    stdin: Option<&[u8]>,
    echo: bool,
) -> Result<(u32, String)> {
    let mut ch = session.channel_open_session().await?;
    ch.exec(true, cmd).await?;
    if let Some(data) = stdin {
        ch.data(data).await?;
        ch.eof().await?;
    }
    let (mut status, mut out) = (None, Vec::new());
    while let Some(msg) = ch.wait().await {
        match msg {
            ChannelMsg::Data { data } => {
                if echo {
                    emit_data(app, id, &crlf(&data));
                }
                out.extend_from_slice(&data);
            }
            ChannelMsg::ExtendedData { data, .. } => emit_data(
                app,
                id,
                format!("\x1b[33m{}\x1b[0m", String::from_utf8_lossy(&crlf(&data))).as_bytes(),
            ),
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok((
        status.unwrap_or(255),
        String::from_utf8_lossy(&out).into_owned(),
    ))
}

/// Exec output has bare `\n`; xterm needs `\r\n`.
fn crlf(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    for &b in data {
        if b == b'\n' {
            out.push(b'\r');
        }
        out.push(b);
    }
    out
}

pub struct Deployed {
    /// Where clients send to: the server's IPv4 and the relay port.
    pub via: SocketAddr,
}

#[allow(clippy::too_many_arguments)]
pub async fn deploy(
    app: &AppHandle,
    id: u32,
    host: &SshHost,
    password: &str,
    known_hosts: &Path,
    bin_dirs: &[PathBuf],
    port: u16,
    psk: &str,
) -> Result<Deployed> {
    anyhow::ensure!(port >= 1024, "порт должен быть от 1024");
    anyhow::ensure!(
        !psk.is_empty()
            && psk
                .chars()
                .all(|c| c.is_ascii_graphic() && !"'\"\\%$`".contains(c)),
        "ключ psk в настройках relay содержит кавычки или спецсимволы: поменяйте его на буквы и цифры"
    );
    let session = connect(app, id, host, password, known_hosts).await?;

    say(app, id, "Проверяю сервер");
    let (_, info) = exec(
        app,
        id,
        &session,
        "uname -sm; id -u; command -v systemctl",
        None,
        false,
    )
    .await?;
    let mut lines = info.lines();
    let uname = lines.next().unwrap_or("").to_string();
    let uid = lines.next().unwrap_or("").trim().to_string();
    let systemd = lines.next().is_some_and(|l| !l.trim().is_empty());
    emit_data(app, id, format!("  {uname}, uid {uid}\r\n").as_bytes());
    if !uname.starts_with("Linux") {
        bail!("нужен Linux-сервер, а это: {uname}");
    }
    if !systemd {
        bail!(
            "на сервере нет systemd: поставьте relay вручную (openlag-relay exit --listen 0.0.0.0:{port})"
        );
    }
    let arch = match uname.split_whitespace().nth(1).unwrap_or("") {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => bail!("архитектура {other} пока не поддерживается (есть x86_64 и aarch64)"),
    };
    let sudo = if uid == "0" {
        ""
    } else {
        let (code, _) = exec(app, id, &session, "sudo -n true", None, false).await?;
        if code != 0 {
            bail!(
                "нужен вход под root или sudo без пароля для пользователя {}",
                host.user
            );
        }
        "sudo -n "
    };

    let bin = binary(bin_dirs, arch)
        .with_context(|| format!("нет сборки relay для {arch} рядом с OpenLag"))?;
    let data =
        std::fs::read(&bin).with_context(|| format!("не удалось прочитать {}", bin.display()))?;
    say(
        app,
        id,
        &format!(
            "Загружаю relay ({arch}, {:.1} МБ)",
            data.len() as f64 / 1_048_576.0
        ),
    );
    let (code, _) = exec(
        app,
        id,
        &session,
        "cat > /tmp/openlag-relay.upload && chmod 755 /tmp/openlag-relay.upload",
        Some(&data),
        true,
    )
    .await?;
    if code != 0 {
        bail!("не удалось загрузить файл (код {code})");
    }
    let (_, ver) = exec(
        app,
        id,
        &session,
        "/tmp/openlag-relay.upload --version",
        None,
        false,
    )
    .await?;
    if !ver.contains("openlag-relay") {
        bail!(
            "загруженный relay не запускается на сервере: {}",
            ver.trim()
        );
    }

    say(app, id, "Ставлю службу openlag-relay");
    let script = install_script(port, psk);
    let (code, out) = exec(
        app,
        id,
        &session,
        &format!("{sudo}sh -s"),
        Some(script.as_bytes()),
        true,
    )
    .await?;
    if code != 0 || !out.contains("active") {
        bail!(
            "служба не запустилась (код {code}); смотрите вывод выше или `journalctl -u openlag-relay`"
        );
    }

    let ip = resolve_v4(&host.host).await?;
    let _ = session
        .disconnect(russh::Disconnect::ByApplication, "", "")
        .await;
    say(app, id, &format!("Готово: relay слушает {ip}:{port}/udp"));
    Ok(Deployed {
        via: SocketAddr::new(ip.into(), port),
    })
}

pub async fn remove(
    app: &AppHandle,
    id: u32,
    host: &SshHost,
    password: &str,
    known_hosts: &Path,
) -> Result<std::net::Ipv4Addr> {
    let session = connect(app, id, host, password, known_hosts).await?;
    let (_, uid) = exec(app, id, &session, "id -u", None, false).await?;
    let sudo = if uid.trim() == "0" { "" } else { "sudo -n " };
    say(app, id, "Удаляю службу openlag-relay");
    let script = "systemctl disable --now openlag-relay 2>/dev/null; \
        rm -f /etc/systemd/system/openlag-relay.service /usr/local/bin/openlag-relay; \
        rm -rf /etc/openlag; systemctl daemon-reload; echo removed\n";
    let (code, out) = exec(
        app,
        id,
        &session,
        &format!("{sudo}sh -s"),
        Some(script.as_bytes()),
        true,
    )
    .await?;
    let _ = session
        .disconnect(russh::Disconnect::ByApplication, "", "")
        .await;
    if code != 0 || !out.contains("removed") {
        bail!("не удалось удалить (код {code})");
    }
    say(app, id, "Relay удалён с сервера");
    resolve_v4(&host.host).await
}

fn install_script(port: u16, psk: &str) -> String {
    format!(
        r#"set -e
install -m 755 /tmp/openlag-relay.upload /usr/local/bin/openlag-relay
rm -f /tmp/openlag-relay.upload
mkdir -p /etc/openlag
( umask 077; printf 'OPENLAG_PSK=%s\n' '{psk}' > /etc/openlag/relay.env )
cat > /etc/systemd/system/openlag-relay.service <<'EOF'
[Unit]
Description=OpenLag relay (exit node)
After=network-online.target
Wants=network-online.target

[Service]
EnvironmentFile=/etc/openlag/relay.env
ExecStart=/usr/local/bin/openlag-relay exit --listen 0.0.0.0:{port}
Restart=always
RestartSec=2
DynamicUser=yes
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable openlag-relay >/dev/null 2>&1
systemctl restart openlag-relay
if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q 'Status: active'; then
  ufw allow {port}/udp >/dev/null && echo "ufw: открыт {port}/udp"
fi
if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
  firewall-cmd --permanent --add-port={port}/udp >/dev/null && firewall-cmd --reload >/dev/null && echo "firewalld: открыт {port}/udp"
fi
sleep 1
systemctl is-active openlag-relay
"#
    )
}

async fn resolve_v4(host: &str) -> Result<std::net::Ipv4Addr> {
    if let Ok(ip) = host.parse() {
        return Ok(ip);
    }
    tokio::net::lookup_host((host, 0))
        .await
        .with_context(|| format!("не удалось узнать IP {host}"))?
        .find_map(|a| match a.ip() {
            std::net::IpAddr::V4(v4) => Some(v4),
            _ => None,
        })
        .with_context(|| format!("у {host} нет IPv4-адреса"))
}

/// Adds (or replaces, by name) an exit in the relays TOML, keeping the PSK.
pub fn add_exit(toml_text: &str, psk: &str, name: &str, via: SocketAddr) -> Result<String> {
    let mut t: toml::Table = if toml_text.trim().is_empty() {
        toml::Table::new()
    } else {
        toml::from_str(toml_text).context("relay settings")?
    };
    t.insert("psk".into(), psk.into());
    let exits = t
        .entry("exits")
        .or_insert_with(|| toml::Value::Array(vec![]))
        .as_array_mut()
        .context("exits must be a list")?;
    exits.retain(|e| e.get("name").and_then(|n| n.as_str()) != Some(name));
    let mut route = toml::Table::new();
    route.insert("name".into(), "direct".into());
    route.insert("via".into(), via.to_string().into());
    let mut exit = toml::Table::new();
    exit.insert("name".into(), name.into());
    exit.insert("routes".into(), toml::Value::Array(vec![route.into()]));
    exits.push(exit.into());
    Ok(toml::to_string(&t)?)
}

pub fn remove_exit(toml_text: &str, via_host: &str) -> Result<String> {
    if toml_text.trim().is_empty() {
        return Ok(String::new());
    }
    let mut t: toml::Table = toml::from_str(toml_text).context("relay settings")?;
    if let Some(exits) = t.get_mut("exits").and_then(|e| e.as_array_mut()) {
        exits.retain(|e| {
            !e.get("routes")
                .and_then(|r| r.as_array())
                .is_some_and(|routes| {
                    routes.iter().any(|r| {
                        r.get("via")
                            .and_then(|v| v.as_str())
                            .and_then(|v| v.parse::<SocketAddr>().ok())
                            .is_some_and(|a| a.ip().to_string() == via_host)
                    })
                })
        });
    }
    Ok(toml::to_string(&t)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exits_are_added_replaced_and_removed() {
        let a = add_exit("", "abc", "Франкфурт", "203.0.113.10:7000".parse().unwrap()).unwrap();
        let b = add_exit(&a, "abc", "Варшава", "198.51.100.2:7000".parse().unwrap()).unwrap();
        let b = add_exit(&b, "abc", "Франкфурт", "203.0.113.11:7100".parse().unwrap()).unwrap();
        let r = crate::settings::parse_relays(&b).unwrap();
        assert_eq!(r.psk, "abc");
        assert_eq!(r.exits.len(), 2);
        assert_eq!(
            r.exits[1].routes[0].via,
            "203.0.113.11:7100".parse().unwrap()
        );
        let c = remove_exit(&b, "198.51.100.2").unwrap();
        let r = crate::settings::parse_relays(&c).unwrap();
        assert_eq!(r.exits.len(), 1);
        assert_eq!(r.exits[0].name, "Франкфурт");
    }
}
