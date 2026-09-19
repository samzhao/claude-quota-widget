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
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, Rect, State, WebviewWindow, Window,
    WindowEvent,
};

const TRAY_ID: &str = "main";
const WINDOW: &str = "main";
const GAP_BELOW_MENUBAR: f64 = 6.0;
/// Clicking the menubar item while the dropdown is open blurs it first. Without
/// this grace window the click would hide it and immediately show it again.
const REOPEN_GRACE_MS: f64 = 350.0;
/// Breathing room kept between the window and the edges of the screen it is on.
const SCREEN_MARGIN: f64 = 16.0;
const MIN_HEIGHT: f64 = 200.0;
const MIN_WIDTH: f64 = 320.0;
const PERSIST_EVERY_MS: f64 = 400.0;

#[derive(Default)]
pub struct Ui {
    pub settings: Settings,
    /// Opened from the menubar item, so it should vanish on click-away.
    transient: bool,
    hidden_by_blur_at_ms: f64,
    /// Natural height of the page content, as last reported by the frontend.
    content_height: f64,
    /// The size we last asked for, so our own resizes are not mistaken for the user's.
    expected_size: Option<(f64, f64)>,
    unsaved_since_ms: f64,
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

/// Sizes the window: height follows the content unless the user picked one by
/// hand, and either way never taller than the screen the window is on.
/// `may_move` lifts the window when it would run off the bottom; it is off
/// while the user is dragging, so the window does not fight the drag.
fn fit_window(window: &WebviewWindow, ui: &mut Ui, may_move: bool) {
    let wanted = ui.settings.manual_height.unwrap_or(ui.content_height);
    if wanted <= 0.0 {
        return; // the page has not reported its height yet
    }
    let Ok(Some(monitor)) = window.current_monitor() else { return };
    let scale = monitor.scale_factor();
    let work = monitor.work_area();
    let tallest = (work.size.height as f64 / scale - 2.0 * SCREEN_MARGIN).max(MIN_HEIGHT);
    let height = wanted.clamp(MIN_HEIGHT, tallest).round();
    let width = ui.settings.width.max(MIN_WIDTH).round();

    let current = window
        .outer_size()
        .map(|size| (size.width as f64 / scale, size.height as f64 / scale))
        .unwrap_or((0.0, 0.0));
    if (current.0 - width).abs() > 1.0 || (current.1 - height).abs() > 1.0 {
        ui.expected_size = Some((width, height));
        let _ = window.set_size(LogicalSize::new(width, height));
    }

    if may_move {
        if let Ok(position) = window.outer_position() {
            let bottom_limit =
                work.position.y as f64 + work.size.height as f64 - SCREEN_MARGIN * scale;
            let overflow = position.y as f64 + height * scale - bottom_limit;
            if overflow > 0.0 {
                let lifted = (position.y as f64 - overflow).max(work.position.y as f64);
                let _ = window.set_position(PhysicalPosition::new(position.x, lifted.round() as i32));
            }
        }
    }
}

/// A resize we did not ask for is the user dragging an edge: remember it.
fn on_resized(window: &Window, physical: (u32, u32)) {
    let app = window.app_handle();
    let state = app.state::<UiState>();
    // try_lock, not lock: if our own sizing code holds the state, this resize
    // is its doing and blocking here would deadlock the main thread.
    let Ok(mut ui) = state.0.try_lock() else { return };
    if !window.is_visible().unwrap_or(false) {
        return;
    }
    let scale = window.scale_factor().unwrap_or(1.0);
    let (width, height) = (physical.0 as f64 / scale, physical.1 as f64 / scale);
    let close = |a: f64, b: f64| (a - b).abs() <= 2.0;
    if ui.expected_size.is_some_and(|(w, h)| close(w, width) && close(h, height)) {
        return;
    }
    // Moving between screens with different scale factors also lands here with
    // the same logical size; that is not a resize either.
    let shown_height = ui.settings.manual_height.unwrap_or(ui.content_height);
    let height_changed = !close(shown_height.max(MIN_HEIGHT), height)
        && !ui.expected_size.is_some_and(|(_, h)| close(h, height));
    let width_changed = !close(ui.settings.width, width);
    if !height_changed && !width_changed {
        return;
    }
    ui.settings.width = width;
    if height_changed {
        ui.settings.manual_height = Some(height);
    }
    ui.expected_size = Some((width, height));
    let settings = ui.settings;
    let now = now_ms();
    let save_now = now - ui.unsaved_since_ms > PERSIST_EVERY_MS;
    ui.unsaved_since_ms = now;
    drop(ui);
    if save_now {
        let _ = persist(app, &settings);
    }
    let _ = app.emit("settings-changed", settings);
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
    // It may be opening on a different, shorter screen than last time.
    fit_window(&window, &mut ui, true);
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
        WindowEvent::Resized(size) => on_resized(window, (size.width, size.height)),
        WindowEvent::Focused(false) => {
            let state = window.state::<UiState>();
            let Ok(mut ui) = state.0.try_lock() else { return };
            // Resize events are saved at most every 400ms; catch the last one here.
            let _ = persist(window.app_handle(), &ui.settings);
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
                if let Ok(mut ui) = window.state::<UiState>().0.try_lock() {
                    ui.transient = false;
                    // Dragged onto a shorter screen: shrink to fit, but never
                    // reposition mid-drag.
                    if let Some(webview) = window.app_handle().get_webview_window(WINDOW) {
                        fit_window(&webview, &mut ui, false);
                    }
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

/// The page tells us how tall its content naturally is, every time that changes.
#[tauri::command]
pub fn content_height(app: AppHandle, ui: State<'_, UiState>, height: f64) {
    let Ok(mut ui) = ui.0.lock() else { return };
    ui.content_height = height;
    if let Some(window) = main_window(&app) {
        fit_window(&window, &mut ui, true);
    }
}

/// Back to automatic height after a manual resize.
#[tauri::command]
pub fn fit_to_content(app: AppHandle, ui: State<'_, UiState>) -> Result<Settings, String> {
    let settings = {
        let mut ui = ui.0.lock().map_err(|e| e.to_string())?;
        ui.settings.manual_height = None;
        if let Some(window) = main_window(&app) {
            fit_window(&window, &mut ui, true);
        }
        ui.settings
    };
    persist(&app, &settings)?;
    Ok(settings)
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
pub fn quit_app(app: AppHandle, ui: State<'_, UiState>) {
    if let Ok(ui) = ui.0.lock() {
        let _ = persist(&app, &ui.settings);
    }
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
