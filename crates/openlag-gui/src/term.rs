//! Terminal sessions for the UI (xterm.js): local shells through Windows ConPTY and
//! SSH sessions to the user's servers.
//!
//! Output goes to the UI as `term-data` events ({id, data: base64}); the session's end
//! as `term-exit` ({id, reason}).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;

#[derive(Clone, Serialize)]
struct Data {
    id: u32,
    data: String,
}

#[derive(Clone, Serialize)]
struct Exit {
    id: u32,
    reason: String,
}

enum Session {
    Local {
        master: Box<dyn MasterPty + Send>,
        writer: Box<dyn Write + Send>,
        child: Box<dyn portable_pty::Child + Send + Sync>,
    },
    Ssh {
        tx: mpsc::UnboundedSender<SshCmd>,
    },
    /// Output-only session of a background job.
    Task,
}

enum SshCmd {
    Write(Vec<u8>),
    Resize(u32, u32),
    Close,
}

#[derive(Default)]
pub struct Terminals {
    next: AtomicU32,
    sessions: Mutex<HashMap<u32, Session>>,
}

/// A saved SSH server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshHost {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// `password` or `key`.
    pub auth: String,
    #[serde(default)]
    pub key_path: String,
    /// DPAPI-encrypted password or key passphrase (see `secret`).
    #[serde(default)]
    pub secret: String,
}

pub(crate) fn emit_data(app: &AppHandle, id: u32, bytes: &[u8]) {
    let _ = app.emit(
        "term-data",
        Data {
            id,
            data: STANDARD.encode(bytes),
        },
    );
}

pub(crate) fn emit_exit(app: &AppHandle, id: u32, reason: impl Into<String>) {
    let _ = app.emit(
        "term-exit",
        Exit {
            id,
            reason: reason.into(),
        },
    );
}

impl Terminals {
    fn new_id(&self) -> u32 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Starts `shell` (powershell, pwsh, cmd, wsl) in a pseudo console.
    pub fn open_local(
        self: &Arc<Self>,
        app: &AppHandle,
        shell: &str,
        cols: u16,
        rows: u16,
    ) -> Result<u32> {
        let program = match shell {
            "cmd" => "cmd.exe",
            "pwsh" => "pwsh.exe",
            "wsl" => "wsl.exe",
            _ => "powershell.exe",
        };
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: rows.max(2),
                cols: cols.max(10),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("cannot create a pseudo console")?;
        let mut cmd = CommandBuilder::new(program);
        if program == "powershell.exe" {
            cmd.arg("-NoLogo");
        }
        if let Some(home) = std::env::var_os("USERPROFILE") {
            cmd.cwd(home);
        }
        cmd.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("cannot start {program}"))?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let id = self.new_id();
        self.sessions.lock().insert(
            id,
            Session::Local {
                master: pair.master,
                writer,
                child,
            },
        );

        let (app, me) = (app.clone(), self.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => emit_data(&app, id, &buf[..n]),
                }
            }
            me.sessions.lock().remove(&id);
            emit_exit(&app, id, "процесс завершён");
        });
        Ok(id)
    }

    /// Connects to a saved server and opens an interactive shell.
    pub fn open_ssh(
        self: &Arc<Self>,
        app: &AppHandle,
        host: SshHost,
        password: String,
        known_hosts: PathBuf,
        cols: u16,
        rows: u16,
    ) -> Result<u32> {
        let id = self.new_id();
        let (tx, rx) = mpsc::unbounded_channel();
        self.sessions.lock().insert(id, Session::Ssh { tx });
        let (app, me) = (app.clone(), self.clone());
        tauri::async_runtime::spawn(async move {
            let reason =
                match ssh_session(&app, id, &host, &password, &known_hosts, cols, rows, rx).await {
                    Ok(()) => "соединение закрыто".to_string(),
                    Err(e) => {
                        emit_data(&app, id, format!("\r\n\x1b[31m{e:#}\x1b[0m\r\n").as_bytes());
                        format!("{e:#}")
                    }
                };
            me.sessions.lock().remove(&id);
            emit_exit(&app, id, reason);
        });
        Ok(id)
    }

    /// Registers an output-only tab for a background job; the caller prints with
    /// `emit_data` and ends it with `finish_task`.
    pub fn open_task(&self) -> u32 {
        let id = self.new_id();
        self.sessions.lock().insert(id, Session::Task);
        id
    }

    pub fn finish_task(&self, app: &AppHandle, id: u32, reason: impl Into<String>) {
        self.sessions.lock().remove(&id);
        emit_exit(app, id, reason);
    }

    pub fn write(&self, id: u32, data: &[u8]) -> Result<()> {
        let mut sessions = self.sessions.lock();
        match sessions.get_mut(&id).context("terminal is closed")? {
            Session::Local { writer, .. } => {
                writer.write_all(data)?;
                writer.flush()?;
            }
            Session::Ssh { tx } => {
                let _ = tx.send(SshCmd::Write(data.to_vec()));
            }
            Session::Task => {}
        }
        Ok(())
    }

    pub fn resize(&self, id: u32, cols: u16, rows: u16) -> Result<()> {
        let sessions = self.sessions.lock();
        match sessions.get(&id).context("terminal is closed")? {
            Session::Local { master, .. } => master.resize(PtySize {
                rows: rows.max(2),
                cols: cols.max(10),
                pixel_width: 0,
                pixel_height: 0,
            })?,
            Session::Ssh { tx } => {
                let _ = tx.send(SshCmd::Resize(cols as u32, rows as u32));
            }
            Session::Task => {}
        }
        Ok(())
    }

    pub fn close(&self, id: u32) {
        if let Some(s) = self.sessions.lock().remove(&id) {
            match s {
                Session::Local { mut child, .. } => {
                    let _ = child.kill();
                }
                Session::Ssh { tx } => {
                    let _ = tx.send(SshCmd::Close);
                }
                Session::Task => {}
            }
        }
    }

    pub fn close_all(&self) {
        let ids: Vec<u32> = self.sessions.lock().keys().copied().collect();
        for id in ids {
            self.close(id);
        }
    }
}

// ---------------------------------------------------------------------------- SSH

/// Trust on first use, like OpenSSH's known_hosts: the first key seen for host:port
/// is saved; a different key later refuses the connection (possible interception).
pub(crate) struct Client {
    host_id: String,
    known_hosts: PathBuf,
    app: AppHandle,
    term: u32,
}

fn load_known(path: &PathBuf) -> HashMap<String, String> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

impl russh::client::Handler for Client {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fp = key
            .public_key()
            .fingerprint(russh::keys::HashAlg::Sha256)
            .to_string();
        let mut known = load_known(&self.known_hosts);
        match known.get(&self.host_id) {
            Some(saved) if *saved == fp => Ok(true),
            Some(saved) => bail!(
                "ключ сервера {} изменился!\r\n  был:   {saved}\r\n  стал:  {fp}\r\nВозможен перехват соединения. Если вы переустановили сервер, удалите его ключ в списке серверов и подключитесь снова.",
                self.host_id
            ),
            None => {
                emit_data(
                    &self.app,
                    self.term,
                    format!(
                        "\x1b[33mНовый сервер {}. Ключ запомнен: {fp}\x1b[0m\r\n",
                        self.host_id
                    )
                    .as_bytes(),
                );
                known.insert(self.host_id.clone(), fp);
                if let Ok(b) = serde_json::to_vec_pretty(&known) {
                    let _ = std::fs::write(&self.known_hosts, b);
                }
                Ok(true)
            }
        }
    }
}

/// Connects and logs in to a saved server; progress and the host key notice go to
/// terminal `id`.
pub(crate) async fn connect(
    app: &AppHandle,
    id: u32,
    host: &SshHost,
    password: &str,
    known_hosts: &std::path::Path,
) -> Result<russh::client::Handle<Client>> {
    emit_data(
        app,
        id,
        format!(
            "Подключение к {}@{}:{}…\r\n",
            host.user, host.host, host.port
        )
        .as_bytes(),
    );
    let config = Arc::new(russh::client::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(600)),
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        ..Default::default()
    });
    let handler = Client {
        host_id: format!("{}:{}", host.host, host.port),
        known_hosts: known_hosts.to_path_buf(),
        app: app.clone(),
        term: id,
    };
    let mut session = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        russh::client::connect(config, (host.host.as_str(), host.port), handler),
    )
    .await
    .context("сервер не отвечает (15 с)")??;

    let ok = if host.auth == "key" {
        let key = russh::keys::load_secret_key(
            &host.key_path,
            (!password.is_empty()).then_some(password),
        )
        .with_context(|| format!("не удалось прочитать ключ {}", host.key_path))?;
        let hash = session.best_supported_rsa_hash().await?.flatten();
        session
            .authenticate_publickey(
                &host.user,
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await?
            .success()
    } else {
        session
            .authenticate_password(&host.user, password)
            .await?
            .success()
    };
    if !ok {
        bail!("сервер отклонил вход: проверьте пользователя и пароль или ключ");
    }
    Ok(session)
}

#[allow(clippy::too_many_arguments)]
async fn ssh_session(
    app: &AppHandle,
    id: u32,
    host: &SshHost,
    password: &str,
    known_hosts: &std::path::Path,
    cols: u16,
    rows: u16,
    mut rx: mpsc::UnboundedReceiver<SshCmd>,
) -> Result<()> {
    use russh::ChannelMsg;
    let session = connect(app, id, host, password, known_hosts).await?;

    let mut channel = session.channel_open_session().await?;
    channel
        .request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
        .await?;
    channel.request_shell(false).await?;
    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(SshCmd::Write(data)) => channel.data(&data[..]).await?,
                Some(SshCmd::Resize(c, r)) => channel.window_change(c, r, 0, 0).await?,
                Some(SshCmd::Close) | None => {
                    let _ = channel.close().await;
                    break;
                }
            },
            msg = channel.wait() => match msg {
                Some(ChannelMsg::Data { data }) => emit_data(app, id, &data),
                Some(ChannelMsg::ExtendedData { data, .. }) => emit_data(app, id, &data),
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    emit_data(app, id, format!("\r\n[код выхода {exit_status}]\r\n").as_bytes());
                }
                Some(ChannelMsg::Eof | ChannelMsg::Close) | None => break,
                _ => {}
            },
        }
    }
    let _ = session
        .disconnect(russh::Disconnect::ByApplication, "", "")
        .await;
    Ok(())
}
