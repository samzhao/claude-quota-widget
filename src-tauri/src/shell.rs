//! How the app sits in macOS: the menubar item, the window dropping down under
//! it, pinning on top, close-to-hide, and the Dock icon setting.

use crate::settings::{self, Settings};
use crate::tray::{self, Summary};
use std::{
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{
    tray::{MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, PhysicalPosition, Rect, State, WebviewWindow, Window, WindowEvent,
};

const TRAY_ID: &str = "main";
const WINDOW: &str = "main";
const GAP_BELOW_MENUBAR: f64 = 6.0;
/// Clicking the menubar item while the dropdown is open blurs it first. Without
/// this grace window the click would hide it and immediately show it again.
const REOPEN_GRACE_MS: f64 = 350.0;

#[derive(Default)]
pub struct Ui {
    pub settings: Settings,
    /// Opened from the menubar item, so it should vanish on click-away.
    transient: bool,
    hidden_by_blur_at_ms: f64,
}

#[derive(Default)]
pub struct UiState(pub Mutex<Ui>);

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn main_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(WINDOW)
}

fn apply_dock_icon(app: &AppHandle, show: bool) {
    #[cfg(target_os = "macos")]
    {
        let policy = if show {
            tauri::ActivationPolicy::Regular
        } else {
            tauri::ActivationPolicy::Accessory
        };
        let _ = app.set_activation_policy(policy);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (app, show);
}

fn apply_pinned(window: &WebviewWindow, pinned: bool) {
    let _ = window.set_always_on_top(pinned);
    let _ = window.set_visible_on_all_workspaces(pinned);
}

fn show_window(window: &WebviewWindow) {
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
}

/// Centers the window under the menubar item, kept inside that screen.
fn place_under(app: &AppHandle, window: &WebviewWindow, icon: &Rect) {
    let scale = window.scale_factor().unwrap_or(1.0);
    let icon_pos = icon.position.to_physical::<f64>(scale);
    let icon_size = icon.size.to_physical::<f64>(scale);
    let Ok(window_size) = window.outer_size() else { return };
    let width = window_size.width as f64;

    let mut x = icon_pos.x + icon_size.width / 2.0 - width / 2.0;
    let y = icon_pos.y + icon_size.height + GAP_BELOW_MENUBAR * scale;
    if let Ok(Some(monitor)) = app.monitor_from_point(icon_pos.x, icon_pos.y) {
        let left = monitor.position().x as f64;
        let right = left + monitor.size().width as f64;
        let margin = 8.0 * scale;
        x = x.clamp(left + margin, (right - width - margin).max(left + margin));
    }
    let _ = window.set_position(PhysicalPosition::new(x.round() as i32, y.round() as i32));
}

fn on_tray_click(app: &AppHandle, icon: &Rect) {
    let Some(window) = main_window(app) else { return };
    let state = app.state::<UiState>();
    let Ok(mut ui) = state.0.lock() else { return };

    let visible = window.is_visible().unwrap_or(false);
    let just_blurred = now_ms() - ui.hidden_by_blur_at_ms < REOPEN_GRACE_MS;
    if visible || just_blurred {
        ui.transient = false;
        ui.hidden_by_blur_at_ms = 0.0;
        let _ = window.hide();
        return;
    }
    // A pinned window stays where it was dragged; otherwise drop it under the item.
    if !ui.settings.pinned {
        place_under(app, &window, icon);
        ui.transient = true;
    }
    drop(ui);
    show_window(&window);
}

pub fn on_window_event(window: &Window, event: &WindowEvent) {
    if window.label() != WINDOW {
        return;
    }
    match event {
        // The app lives in the menubar: closing the window only hides it.
        WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let _ = window.hide();
        }
        WindowEvent::Focused(false) => {
            let state = window.state::<UiState>();
            let Ok(mut ui) = state.0.lock() else { return };
            if ui.transient && !ui.settings.pinned {
                ui.transient = false;
                ui.hidden_by_blur_at_ms = now_ms();
                let _ = window.hide();
            }
        }
        // Dragging the dropdown somewhere means "keep this open here".
        WindowEvent::Moved(_) => {
            // Programmatic placement also fires Moved, right before show(), when
            // the window is still hidden; only a visible window is a user drag.
            if window.is_visible().unwrap_or(false) {
                if let Ok(mut ui) = window.state::<UiState>().0.lock() {
                    ui.transient = false;
                }
            }
        }
        _ => {}
    }
}

pub fn update_tray(app: &AppHandle, summary: &Summary) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else { return };
    let _ = tray.set_icon(Some(tray::gauge_icon(summary.percent, summary.level)));
    let _ = tray.set_icon_as_template(tray::is_template(summary.level));
    let _ = tray.set_title(Some(&summary.title));
    let _ = tray.set_tooltip(Some(&summary.tooltip));
}

pub fn init(app: &AppHandle) -> tauri::Result<()> {
    let loaded = app
        .path()
        .app_data_dir()
        .map(|dir| settings::load(&dir))
        .unwrap_or_default();
    if let Ok(mut ui) = app.state::<UiState>().0.lock() {
        ui.settings = loaded;
    }
    apply_dock_icon(app, loaded.show_dock_icon);
    if let Some(window) = main_window(app) {
        apply_pinned(&window, loaded.pinned);
    }

    let placeholder = tray::summarize(&[]);
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(tray::gauge_icon(placeholder.percent, placeholder.level))
        .icon_as_template(true)
        .title(&placeholder.title)
        .tooltip(&placeholder.tooltip)
        // Deliberately no menu. A menu attached to the status item is opened by
        // macOS itself, before (or instead of) our click handler, so the click
        // could never reliably open the app. Quit lives in the window.
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = event
            {
                on_tray_click(tray.app_handle(), &rect);
            }
        })
        .build(app)?;
    Ok(())
}

fn persist(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    settings::save(&dir, settings)
}

#[tauri::command]
pub fn hide_window(app: AppHandle, ui: State<'_, UiState>) {
    if let Ok(mut ui) = ui.0.lock() {
        ui.transient = false;
    }
    if let Some(window) = main_window(&app) {
        let _ = window.hide();
    }
}

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
pub fn get_settings(ui: State<'_, UiState>) -> Result<Settings, String> {
    Ok(ui.0.lock().map_err(|e| e.to_string())?.settings)
}

#[tauri::command]
pub fn set_pinned(app: AppHandle, ui: State<'_, UiState>, pinned: bool) -> Result<Settings, String> {
    let settings = {
        let mut ui = ui.0.lock().map_err(|e| e.to_string())?;
        ui.settings.pinned = pinned;
        // Pinning a dropdown keeps it; unpinning leaves a normal window behind.
        ui.transient = false;
        ui.settings
    };
    if let Some(window) = main_window(&app) {
        apply_pinned(&window, pinned);
    }
    persist(&app, &settings)?;
    Ok(settings)
}

#[tauri::command]
pub fn set_show_dock_icon(
    app: AppHandle,
    ui: State<'_, UiState>,
    show: bool,
) -> Result<Settings, String> {
    let settings = {
        let mut ui = ui.0.lock().map_err(|e| e.to_string())?;
        ui.settings.show_dock_icon = show;
        ui.transient = false;
        ui.settings
    };
    apply_dock_icon(&app, show);
    // Switching activation policy can drop the window behind others.
    if let Some(window) = main_window(&app) {
        if window.is_visible().unwrap_or(false) {
            show_window(&window);
        }
    }
    persist(&app, &settings)?;
    Ok(settings)
}
