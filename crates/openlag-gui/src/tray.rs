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

/// What the menu currently shows; the menu is only rebuilt when this changes.
#[derive(Default)]
struct Shown {
    signature: String,
    keys: Vec<String>,
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
            "Открыть OpenLag",
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

/// Rebuilds the menu and tooltip if anything visible changed.
pub fn update(app: &AppHandle, capturing: bool, preferred: Option<&str>, servers: &[TrayServer]) {
    let servers = &servers[..servers.len().min(MAX_SERVERS)];
    // Round pings so the menu is not rebuilt every second.
    let signature = format!(
        "{capturing}|{preferred:?}|{}",
        servers
            .iter()
            .map(|s| format!(
                "{}:{:?}",
                s.key,
                s.ping_ms.map(|p| (p / 10.0).round() as i64)
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut shown = SHOWN.lock();
    if shown.as_ref().is_some_and(|s| s.signature == signature) {
        return;
    }
    let Some(tray) = app.tray_by_id(TRAY) else {
        return;
    };
    match build_menu(app, capturing, preferred, servers) {
        Ok(menu) => {
            let _ = tray.set_menu(Some(menu));
        }
        Err(e) => {
            warn!("tray menu: {e}");
            return;
        }
    }
    let current = preferred
        .and_then(|p| servers.iter().find(|s| s.key == p))
        .or_else(|| servers.first())
        .map(|s| match s.ping_ms {
            Some(p) => format!("{} · {p:.0} мс", s.label),
            None => s.label.clone(),
        })
        .unwrap_or_default();
    let state = if capturing {
        "включено"
    } else {
        "выключено"
    };
    let _ = tray.set_tooltip(Some(if current.is_empty() {
        format!("OpenLag: {state}")
    } else {
        format!("OpenLag: {state}\n{current}")
    }));
    *shown = Some(Shown {
        signature,
        keys: servers.iter().map(|s| s.key.clone()).collect(),
    });
}

fn build_menu(
    app: &AppHandle,
    capturing: bool,
    preferred: Option<&str>,
    servers: &[TrayServer],
) -> tauri::Result<Menu<tauri::Wry>> {
    let open = MenuItem::with_id(app, "open", "Открыть OpenLag", true, None::<&str>)?;
    let toggle =
        CheckMenuItem::with_id(app, "toggle", "Оптимизация", true, capturing, None::<&str>)?;
    let srv = Submenu::with_id(app, "servers", "Сервер", true)?;
    srv.append(&CheckMenuItem::with_id(
        app,
        "srv:auto",
        "Авто (лучший пинг)",
        true,
        preferred.is_none(),
        None::<&str>,
    )?)?;
    if !servers.is_empty() {
        srv.append(&PredefinedMenuItem::separator(app)?)?;
    }
    for (i, s) in servers.iter().enumerate() {
        let label = match s.ping_ms {
            Some(p) => format!("{}   {p:.0} мс", s.label),
            None => format!("{}   нет ответа", s.label),
        };
        let checked = preferred == Some(s.key.as_str());
        srv.append(&CheckMenuItem::with_id(
            app,
            format!("srv:{i}"),
            label,
            true,
            checked,
            None::<&str>,
        )?)?;
    }
    let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
    Menu::with_items(
        app,
        &[
            &open,
            &PredefinedMenuItem::separator(app)?,
            &toggle,
            &srv,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )
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
                    let _ = app.emit("notify", format!("Не удалось включить: {e:#}"));
                }
                SHOWN.lock().take(); // force a menu refresh with the new state
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
                SHOWN.lock().take();
                let _ = app.emit("refresh", ());
            });
        }
        _ => {}
    }
}
