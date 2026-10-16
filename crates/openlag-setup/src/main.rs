#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Pingless installer and uninstaller.
//!
//! The payload (`core.zip`, `extras.zip`) is embedded at build time. A copy of this exe
//! is placed in the install folder as `uninstall.exe`; started with `--uninstall` it
//! shows the removal flow instead.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

const CORE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/core.zip"));
const EXTRAS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/extras.zip"));
const CORE_SIZE: u64 = parse_u64(env!("OPENLAG_CORE_SIZE"));
const EXTRAS_SIZE: u64 = parse_u64(env!("OPENLAG_EXTRAS_SIZE"));
const VERSION: &str = env!("CARGO_PKG_VERSION");
const APP_EXE: &str = "Pingless.exe";
const UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Pingless";
pub const GITHUB: &str = "https://github.com/Del-Iternenko/pingless";

const fn parse_u64(s: &str) -> u64 {
    let b = s.as_bytes();
    let mut i = 0;
    let mut v = 0u64;
    while i < b.len() {
        v = v * 10 + (b[i] - b'0') as u64;
        i += 1;
    }
    v
}

#[derive(Serialize)]
struct Info {
    mode: &'static str,
    version: &'static str,
    default_dir: String,
    installed_dir: Option<String>,
    core_mb: f64,
    extras_mb: f64,
    github: &'static str,
    /// For `--update`: what the existing installation has, to keep it as it was.
    keep_desktop: bool,
    keep_start_menu: bool,
    keep_autostart: bool,
}

#[derive(Deserialize)]
struct InstallOptions {
    dir: String,
    desktop: bool,
    start_menu: bool,
    extras: bool,
    autostart: bool,
}

#[derive(Serialize, Clone)]
struct Progress {
    pct: f64,
    text: String,
}

fn uninstall_mode() -> bool {
    std::env::args().any(|a| a == "--uninstall")
}

/// Started by Pingless's updater: reinstall in place without asking anything.
fn update_mode() -> bool {
    std::env::args().any(|a| a == "--update")
}

fn program_files() -> PathBuf {
    std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
}

fn install_location(key: &str) -> Option<PathBuf> {
    let key = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(key)
        .ok()?;
    let dir: String = key.get_value("InstallLocation").ok()?;
    Some(PathBuf::from(dir))
}

/// The app was called OpenLag up to 1.0; an install of that is replaced in place.
const LEGACY_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\OpenLag";

fn legacy_dir() -> Option<PathBuf> {
    install_location(LEGACY_KEY)
}

/// Where Pingless is (or, after an OpenLag install, will be) installed.
fn installed_dir() -> Option<PathBuf> {
    install_location(UNINSTALL_KEY).or_else(|| {
        legacy_dir().map(|d| match d.parent() {
            Some(p) if d.ends_with("OpenLag") => p.join("Pingless"),
            _ => d,
        })
    })
}

/// Removes an OpenLag install (program, shortcuts, task, firewall rule, uninstall
/// entry). Settings stay: Pingless keeps the same data folder.
fn remove_legacy(new_dir: &Path) {
    let Some(old) = legacy_dir() else { return };
    stop_running(&old);
    let _ = hidden("schtasks", &["/Delete", "/TN", "OpenLag", "/F"]);
    let _ = hidden(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            "name=OpenLag local proxy",
        ],
    );
    let _ = hidden("reg", &["delete", r"HKCU\Software\Classes\openlag", "/f"]);
    let _ = std::fs::remove_file(start_menu_dir().join("OpenLag.lnk"));
    let _ = std::fs::remove_file(public_desktop().join("OpenLag.lnk"));
    // Only a folder that is clearly ours, and never the one we install into.
    if old.ends_with("OpenLag") && old != new_dir && old.join("OpenLag.exe").exists() {
        let _ = std::fs::remove_dir_all(&old);
    }
    let _ = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE).delete_subkey_all(LEGACY_KEY);
}

fn had_legacy_autostart() -> bool {
    hidden("schtasks", &["/Query", "/TN", "OpenLag"]).is_ok_and(|o| o.status.success())
}

#[tauri::command]
fn info() -> Info {
    let installed = installed_dir();
    Info {
        mode: if uninstall_mode() {
            "uninstall"
        } else if update_mode() && installed.is_some() {
            "update"
        } else {
            "install"
        },
        keep_desktop: public_desktop().join("Pingless.lnk").exists()
            || public_desktop().join("OpenLag.lnk").exists(),
        keep_start_menu: start_menu_dir().join("Pingless.lnk").exists()
            || start_menu_dir().join("OpenLag.lnk").exists(),
        keep_autostart: hidden("schtasks", &["/Query", "/TN", "Pingless"])
            .is_ok_and(|o| o.status.success())
            || had_legacy_autostart(),
        version: VERSION,
        default_dir: installed
            .clone()
            .unwrap_or_else(|| program_files().join("Pingless"))
            .display()
            .to_string(),
        installed_dir: installed.map(|d| d.display().to_string()),
        core_mb: CORE_SIZE as f64 / 1_048_576.0,
        extras_mb: EXTRAS_SIZE as f64 / 1_048_576.0,
        github: GITHUB,
    }
}

#[tauri::command]
async fn pick_dir(app: AppHandle, current: String) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_title("Папка установки")
            .set_directory(Path::new(&current).parent().unwrap_or(Path::new("C:\\")))
            .blocking_pick_folder()
            .and_then(|f| f.into_path().ok())
            // Always install into a dedicated folder.
            .map(|p| {
                if p.ends_with("Pingless") {
                    p
                } else {
                    p.join("Pingless")
                }
            })
            .map(|p| p.display().to_string())
    })
    .await
    .ok()
    .flatten()
}

#[tauri::command]
fn open_url(url: String) {
    if url.starts_with("https://") {
        shell_open(&url);
    }
}

#[tauri::command]
fn launch(dir: String) -> Result<(), String> {
    std::process::Command::new(Path::new(&dir).join(APP_EXE))
        .current_dir(&dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn install(app: AppHandle, opts: InstallOptions) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        run_install(&app, &opts).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn uninstall(app: AppHandle, remove_settings: bool) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        run_uninstall(&app, remove_settings).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn emit(app: &AppHandle, pct: f64, text: impl Into<String>) {
    let _ = app.emit(
        "progress",
        Progress {
            pct,
            text: text.into(),
        },
    );
}

fn run_install(app: &AppHandle, o: &InstallOptions) -> Result<()> {
    let dir = PathBuf::from(o.dir.trim());
    anyhow::ensure!(dir.is_absolute(), "укажите полный путь к папке установки");
    emit(app, 1.0, "Закрываю запущенный Pingless…");
    stop_running(&dir);
    remove_legacy(&dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("не удалось создать {}", dir.display()))?;

    let total = CORE_SIZE + if o.extras { EXTRAS_SIZE } else { 0 };
    let mut done = 0u64;
    extract(app, CORE, &dir, total.max(1), &mut done)?;
    if o.extras {
        extract(app, EXTRAS, &dir, total.max(1), &mut done)?;
        let wv2 = dir.join("MicrosoftEdgeWebview2Setup.exe");
        if wv2.exists() {
            if !webview2_installed() {
                emit(app, 91.0, "Устанавливаю WebView2 Runtime…");
                let _ = std::process::Command::new(&wv2)
                    .args(["/silent", "/install"])
                    .status();
            }
            let _ = std::fs::remove_file(&wv2);
        }
    }

    emit(app, 93.0, "Регистрирую программу…");
    let me = std::env::current_exe()?;
    let uninstaller = dir.join("uninstall.exe");
    if me != uninstaller {
        std::fs::copy(&me, &uninstaller).context("не удалось записать uninstall.exe")?;
    }
    register(&dir, total)?;

    emit(app, 96.0, "Создаю ярлыки…");
    let target = dir.join(APP_EXE);
    if o.start_menu {
        shortcut(&start_menu_dir().join("Pingless.lnk"), &target)?;
    }
    if o.desktop {
        shortcut(&public_desktop().join("Pingless.lnk"), &target)?;
    }
    if o.autostart {
        let run = format!("\"{}\" --minimized", target.display());
        let _ = hidden(
            "schtasks",
            &[
                "/Create", "/TN", "Pingless", "/TR", &run, "/SC", "ONLOGON", "/RL", "HIGHEST", "/F",
            ],
        );
    }
    emit(app, 100.0, "Готово");
    Ok(())
}

fn extract(app: &AppHandle, data: &[u8], dir: &Path, total: u64, done: &mut u64) -> Result<()> {
    let mut zip = zip::ZipArchive::new(Cursor::new(data))?;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        let Some(rel) = f.enclosed_name() else {
            continue;
        };
        let path = dir.join(&rel);
        emit(
            app,
            2.0 + 88.0 * *done as f64 / total as f64,
            rel.display().to_string(),
        );
        if f.is_dir() {
            std::fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut buf = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut buf)?;
        if let Err(e) = std::fs::write(&path, &buf) {
            // A loaded WinDivert driver keeps its .sys locked; same bytes are fine.
            let same = std::fs::read(&path).map(|old| old == buf).unwrap_or(false);
            anyhow::ensure!(
                same,
                "не удалось записать {}: {e}. Закройте Pingless или перезагрузите ПК.",
                path.display()
            );
        }
        *done += buf.len() as u64;
    }
    Ok(())
}

fn run_uninstall(app: &AppHandle, remove_settings: bool) -> Result<()> {
    let dir = installed_dir()
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
        })
        .context("Pingless не найден")?;
    emit(app, 10.0, "Закрываю Pingless…");
    stop_running(&dir);
    emit(app, 30.0, "Убираю автозапуск и правило брандмауэра…");
    let _ = hidden("schtasks", &["/Delete", "/TN", "Pingless", "/F"]);
    // openlag:// links, registered by the app itself.
    let _ = hidden("reg", &["delete", r"HKCU\Software\Classes\openlag", "/f"]);
    let _ = hidden(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            "name=Pingless local proxy",
        ],
    );
    emit(app, 50.0, "Удаляю ярлыки…");
    let _ = std::fs::remove_file(start_menu_dir().join("Pingless.lnk"));
    let _ = std::fs::remove_file(public_desktop().join("Pingless.lnk"));
    let _ =
        winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE).delete_subkey_all(UNINSTALL_KEY);
    emit(app, 70.0, "Удаляю файлы…");
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if e.file_name() == "uninstall.exe" {
                continue;
            }
            let p = e.path();
            let _ = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
        }
    }
    if remove_settings && let Some(appdata) = std::env::var_os("APPDATA") {
        let _ = std::fs::remove_dir_all(PathBuf::from(appdata).join("dev.openlag.app"));
    }
    // uninstall.exe is running from this folder; let cmd remove it after we exit.
    schedule_dir_removal(&dir);
    emit(app, 100.0, "Готово");
    Ok(())
}

fn stop_running(dir: &Path) {
    let script = format!(
        "Get-Process | Where-Object {{ $_.Path -and $_.Path.StartsWith('{}', 'OrdinalIgnoreCase') -and $_.Id -ne {} }} | Stop-Process -Force",
        dir.display().to_string().replace('\'', "''"),
        std::process::id()
    );
    let _ = powershell(&script);
    std::thread::sleep(std::time::Duration::from_millis(800));
}

fn register(dir: &Path, size: u64) -> Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;
    let (key, _) = RegKey::predef(HKEY_LOCAL_MACHINE).create_subkey(UNINSTALL_KEY)?;
    let d = dir.display().to_string();
    key.set_value("DisplayName", &"Pingless")?;
    key.set_value("DisplayVersion", &VERSION)?;
    key.set_value("Publisher", &"Del-Iternenko")?;
    key.set_value("URLInfoAbout", &GITHUB)?;
    key.set_value("InstallLocation", &d)?;
    key.set_value("DisplayIcon", &format!("{d}\\{APP_EXE}"))?;
    key.set_value(
        "UninstallString",
        &format!("\"{d}\\uninstall.exe\" --uninstall"),
    )?;
    key.set_value("EstimatedSize", &((size / 1024) as u32))?;
    key.set_value("NoModify", &1u32)?;
    key.set_value("NoRepair", &1u32)?;
    Ok(())
}

fn webview2_installed() -> bool {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    const ID: &str = r"Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";
    [
        (HKEY_LOCAL_MACHINE, format!(r"SOFTWARE\WOW6432Node\{ID}")),
        (HKEY_LOCAL_MACHINE, format!(r"SOFTWARE\{ID}")),
        (HKEY_CURRENT_USER, format!(r"SOFTWARE\{ID}")),
    ]
    .iter()
    .any(|(root, path)| {
        RegKey::predef(*root)
            .open_subkey(path)
            .and_then(|k| k.get_value::<String, _>("pv"))
            .is_ok_and(|v| !v.is_empty() && v != "0.0.0.0")
    })
}

fn start_menu_dir() -> PathBuf {
    let pd = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    pd.join(r"Microsoft\Windows\Start Menu\Programs")
}

fn public_desktop() -> PathBuf {
    let p = std::env::var_os("PUBLIC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Users\Public"));
    p.join("Desktop")
}

fn shortcut(lnk: &Path, target: &Path) -> Result<()> {
    let q = |p: &Path| p.display().to_string().replace('\'', "''");
    let wd = target.parent().unwrap_or(Path::new("."));
    let script = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}'); $s.TargetPath='{}'; $s.WorkingDirectory='{}'; \
         $s.IconLocation='{},0'; $s.Description='Pingless'; $s.Save()",
        q(lnk),
        q(target),
        q(wd),
        q(target)
    );
    let out = powershell(&script)?;
    anyhow::ensure!(
        out.status.success(),
        "ярлык {}: {}",
        lnk.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

const NO_WINDOW: u32 = 0x0800_0000;

fn hidden(exe: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new(exe)
        .args(args)
        .creation_flags(NO_WINDOW)
        .output()
}

/// Runs a script via -EncodedCommand, so paths with Cyrillic or quotes survive.
fn powershell(script: &str) -> std::io::Result<std::process::Output> {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    hidden(
        "powershell",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &base64(&utf16),
        ],
    )
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

fn schedule_dir_removal(dir: &Path) {
    use std::os::windows::process::CommandExt;
    const DETACHED: u32 = 0x0000_0008;
    let cmd = format!(
        "ping 127.0.0.1 -n 4 >nul & rmdir /s /q \"{}\"",
        dir.display()
    );
    let _ = std::process::Command::new("cmd")
        .args(["/c", &cmd])
        .creation_flags(NO_WINDOW | DETACHED)
        .spawn();
}

fn shell_open(url: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (verb, file) = (w("open"), w(url));
    // SAFETY: NUL-terminated strings that outlive the call.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// `music.mp3` next to the installer, if any: the public build has no music inside,
/// but whoever has a track can drop it beside the exe.
#[tauri::command]
fn external_music() -> tauri::ipc::Response {
    let data = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("music.mp3")))
        .and_then(|p| std::fs::read(p).ok())
        .unwrap_or_default();
    tauri::ipc::Response::new(data)
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            info,
            pick_dir,
            open_url,
            launch,
            install,
            uninstall,
            external_music
        ])
        .run(tauri::generate_context!())
        .expect("failed to start the installer");
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_reference() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
    }
}
