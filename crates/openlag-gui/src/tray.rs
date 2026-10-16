//! System tray icon: open the window, switch optimisation on/off, pick a server.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};
use tracing::warn;

use crate::AppState;

const TRAY: &str = "main";
/// Servers shown in the tray submenu, best ping first.
const MAX_SERVERS: usize = 12;

/// Set when the user chose "Exit": the close button then really closes.
pub static QUITTING: AtomicBool = AtomicBool::new(false);

/// The live menu. It is rebuilt only when the set of servers changes; pings, checks
/// and the tooltip are updated in place, so a menu the user has open stays open.
struct Shown {
    /// Server keys in menu order ("srv:{i}" ids index into it).
    keys: Vec<String>,
    toggle: CheckMenuItem<tauri::Wry>,
    auto: CheckMenuItem<tauri::Wry>,
    items: Vec<CheckMenuItem<tauri::Wry>>,
    labels: Vec<String>,
    /// Check marks as last set; None after a click toggled one natively.
    checks: Option<(bool, Option<String>)>,
    tooltip: String,
}

static SHOWN: Mutex<Option<Shown>> = Mutex::new(None);

/// One server line for the tray menu.
pub struct TrayServer {
    pub key: String,
    pub label: String,
    pub ping_ms: Option<f64>,
}

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let menu = Menu::with_items(
        app,
        &[&MenuItem::with_id(
            app,
            "open",
            &tr!("Открыть OpenLag", "Open OpenLag"),
            true,
            None::<&str>,
        )?],
    )?;
    let mut builder = TrayIconBuilder::with_id(TRAY)
        .tooltip("OpenLag")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| on_menu(app, event.id().as_ref()))
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

pub fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn server_label(s: &TrayServer) -> String {
    match s.ping_ms {
        Some(p) => tr!("{}   {p:.0} мс", "{}   {p:.0} ms", s.label),
        None => tr!("{}   нет ответа", "{}   no reply", s.label),
    }
}

/// Brings the menu and tooltip up to date, touching only what changed.
pub fn update(app: &AppHandle, capturing: bool, preferred: Option<&str>, servers: &[TrayServer]) {
    let all = servers;
    let servers = &servers[..servers.len().min(MAX_SERVERS)];
    let Some(tray) = app.tray_by_id(TRAY) else {
        return;
    };
    let mut shown = SHOWN.lock();

    // Keep the listed servers while they all still exist and no hidden one is clearly
    // faster than the slowest listed: ranks near the cut flip every second, and each
    // rebuild would close a menu the user has open.
    let keep = shown.as_ref().is_some_and(|sh| {
        let ping = |k: &str| all.iter().find(|s| s.key == k).map(|s| s.ping_ms);
        if sh.keys.len() != servers.len() || sh.keys.iter().any(|k| ping(k).is_none()) {
            return false;
        }
        let worst = sh
            .keys
            .iter()
            .map(|k| ping(k).flatten().unwrap_or(f64::INFINITY))
            .fold(0.0, f64::max);
        !all.iter()
            .filter(|s| !sh.keys.contains(&s.key))
            .any(|s| s.ping_ms.is_some_and(|p| p + 30.0 < worst))
    });
    if !keep {
        match build_menu(app, servers) {
            Ok((menu, toggle, auto, items)) => {
                let _ = tray.set_menu(Some(menu));
                *shown = Some(Shown {
                    keys: servers.iter().map(|s| s.key.clone()).collect(),
                    toggle,
                    auto,
                    labels: servers.iter().map(server_label).collect(),
                    items,
                    checks: None,
                    tooltip: String::new(),
                });
            }
            Err(e) => {
                warn!("tray menu: {e}");
                return;
            }
        }
    }
    let Some(sh) = shown.as_mut() else {
        return;
    };

    // Pings change every second: update the labels, keep the order.
    for (i, key) in sh.keys.iter().enumerate() {
        if let Some(s) = servers.iter().find(|s| &s.key == key) {
            let label = server_label(s);
            if sh.labels[i] != label {
                let _ = sh.items[i].set_text(&label);
                sh.labels[i] = label;
            }
        }
    }
    let checks = (capturing, preferred.map(String::from));
    if sh.checks.as_ref() != Some(&checks) {
        let _ = sh.toggle.set_checked(capturing);
        let _ = sh.auto.set_checked(preferred.is_none());
        for (i, key) in sh.keys.iter().enumerate() {
            let _ = sh.items[i].set_checked(preferred == Some(key.as_str()));
        }
        sh.checks = Some(checks);
    }

    let current = preferred
        .and_then(|p| servers.iter().find(|s| s.key == p))
        .or_else(|| servers.first())
        .map(|s| match s.ping_ms {
            Some(p) => tr!(
                "{} · {:.0} мс",
                "{} · {:.0} ms",
                s.label,
                (p / 5.0).round() * 5.0
            ),
            None => s.label.clone(),
        })
        .unwrap_or_default();
    let state = if capturing {
        tr!("включено", "on")
    } else {
        tr!("выключено", "off")
    };
    let tooltip = if current.is_empty() {
        format!("OpenLag: {state}")
    } else {
        format!("OpenLag: {state}\n{current}")
    };
    if sh.tooltip != tooltip {
        let _ = tray.set_tooltip(Some(&tooltip));
        sh.tooltip = tooltip;
    }
}

type Built = (
    Menu<tauri::Wry>,
    CheckMenuItem<tauri::Wry>,
    CheckMenuItem<tauri::Wry>,
    Vec<CheckMenuItem<tauri::Wry>>,
);

fn build_menu(app: &AppHandle, servers: &[TrayServer]) -> tauri::Result<Built> {
    let open = MenuItem::with_id(
        app,
        "open",
        &tr!("Открыть OpenLag", "Open OpenLag"),
        true,
        None::<&str>,
    )?;
    let toggle = CheckMenuItem::with_id(
        app,
        "toggle",
        tr!("Оптимизация", "Optimization"),
        true,
        false,
        None::<&str>,
    )?;
    let srv = Submenu::with_id(app, "servers", tr!("Сервер", "Server"), true)?;
    let auto = CheckMenuItem::with_id(
        app,
        "srv:auto",
        tr!("Авто (лучший пинг)", "Auto (best ping)"),
        true,
        false,
        None::<&str>,
    )?;
    srv.append(&auto)?;
    if !servers.is_empty() {
        srv.append(&PredefinedMenuItem::separator(app)?)?;
    }
    let mut items = Vec::with_capacity(servers.len());
    for (i, s) in servers.iter().enumerate() {
        let item = CheckMenuItem::with_id(
            app,
            format!("srv:{i}"),
            server_label(s),
            true,
            false,
            None::<&str>,
        )?;
        srv.append(&item)?;
        items.push(item);
    }
    let quit = MenuItem::with_id(app, "quit", tr!("Выход", "Quit"), true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &PredefinedMenuItem::separator(app)?,
            &toggle,
            &srv,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    Ok((menu, toggle, auto, items))
}

/// A click flips a check mark natively; make the next update set them all again.
fn recheck() {
    if let Some(sh) = SHOWN.lock().as_mut() {
        sh.checks = None;
    }
}

fn on_menu(app: &AppHandle, id: &str) {
    let st = app.state::<Arc<AppState>>().inner().clone();
    let app = app.clone();
    match id {
        "open" => show_window(&app),
        "quit" => {
            QUITTING.store(true, Ordering::SeqCst);
            app.exit(0);
        }
        "toggle" => {
            tauri::async_runtime::spawn(async move {
                if let Err(e) = st.toggle_capture().await {
                    show_window(&app);
                    let _ = app.emit(
                        "notify",
                        tr!("Не удалось включить: {e:#}", "Could not turn on: {e:#}"),
                    );
                }
                recheck();
                let _ = app.emit("refresh", ());
            });
        }
        _ if id.starts_with("srv:") => {
            let key = match &id[4..] {
                "auto" => None,
                idx => {
                    let Ok(i) = idx.parse::<usize>() else { return };
                    let Some(k) = SHOWN.lock().as_ref().and_then(|s| s.keys.get(i).cloned()) else {
                        return;
                    };
                    Some(k)
                }
            };
            tauri::async_runtime::spawn(async move {
                st.set_preferred_exit(key).await;
                recheck();
                let _ = app.emit("refresh", ());
            });
        }
        _ => {}
    }
}
