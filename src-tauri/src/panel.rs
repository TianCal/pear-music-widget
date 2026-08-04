//! The menu-bar dropdown.
//!
//! It renders whichever skin is configured for it, independently of the floating
//! widget, so its size follows `panelSkin` rather than being fixed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use window_vibrancy::NSVisualEffectMaterial;

use crate::macos;
use crate::store::Store;
use crate::window::{self, Rect, PANEL};

const EDGE_MARGIN: f64 = 8.0;
const GAP_BELOW_MENU_BAR: f64 = 6.0;

/// Clicking the tray icon while the panel has focus fires blur (which hides it)
/// and then click (which would immediately reopen it). Ignore a toggle that
/// arrives right after a blur-close so the icon behaves like a real toggle.
const REOPEN_GUARD: Duration = Duration::from_millis(250);

pub struct PanelState {
    hidden_at: Mutex<Option<Instant>>,
    expanded_by: Mutex<Option<String>>,
    visible: AtomicBool,
    /// Set while the dropdown's own context menu is up. A menu takes focus, and
    /// blur is what closes the dropdown — without this the menu would dismiss
    /// the window it was opened from, which is why the Electron build never
    /// gave the dropdown a menu at all.
    menu_open: AtomicBool,
}

impl PanelState {
    pub fn new() -> Self {
        Self {
            hidden_at: Mutex::new(None),
            expanded_by: Mutex::new(None),
            visible: AtomicBool::new(false),
            menu_open: AtomicBool::new(false),
        }
    }
}

fn state(app: &AppHandle) -> Arc<PanelState> {
    app.state::<Arc<PanelState>>().inner().clone()
}

fn size_of(app: &AppHandle) -> (f64, f64) {
    let store = app.state::<Arc<Store>>();
    window::base_for(&window::panel_skin_of(&store))
}

pub fn create(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let size = size_of(app);

    // The same document as the widget, with `?mode=panel` so the renderer knows
    // which skin to follow and drops the resize edges.
    let panel = WebviewWindowBuilder::new(
        app,
        PANEL,
        WebviewUrl::App("index.html?mode=panel".into()),
    )
    .title("Pear Music Widget")
    .inner_size(size.0, size.1)
    .decorations(false)
    .transparent(true)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .skip_taskbar(true)
    .shadow(true)
    .visible(false)
    .theme(window::forced_theme())
    .build()?;

    // `Menu` rather than `UnderWindowBackground`: a dropdown should read as part
    // of the menu bar it hangs from.
    window::dress(&panel, NSVisualEffectMaterial::Menu, Some(macos::LEVEL_POPUP_MENU));
    window::apply_zoom(app, &panel);

    Ok(panel)
}

/// Grow downwards for whichever panel is open, clamped to the screen.
/// Returns the rect it asked for — the caller cannot read it back yet.
fn resize(app: &AppHandle, panel: &WebviewWindow) -> Rect {
    let size = size_of(app);
    let bounds = window::bounds_of(panel);
    let extra = state(app)
        .expanded_by
        .lock()
        .expect("panel lock")
        .as_deref()
        .and_then(window::panel_height)
        .unwrap_or(0.0);

    let height = size.1 + extra;
    let area = window::work_area_matching(app, bounds);
    let max_y = area.y + area.height - height;
    let y = bounds.y.min(max_y.max(area.y)).round();

    let rect = Rect {
        x: bounds.x,
        y,
        width: size.0,
        height,
    };
    window::set_bounds(panel, rect);
    rect
}

pub fn set_expanded(app: &AppHandle, panel: &WebviewWindow, which: Option<&str>) {
    let next = which
        .filter(|which| window::panel_height(which).is_some())
        .map(str::to_string);
    {
        let state = state(app);
        let mut expanded = state.expanded_by.lock().expect("panel lock");
        if *expanded == next {
            return;
        }
        *expanded = next;
    }
    resize(app, panel);
}

/// Switching the dropdown's skin changes its natural size.
pub fn apply_panel_skin(app: &AppHandle, skin: &str) {
    let store = app.state::<Arc<Store>>().inner().clone();
    store.update(|s| s.panel_skin = skin.to_string());

    if let Some(panel) = app.get_webview_window(PANEL) {
        // The natural size changed, so the collapsed rect is what the zoom
        // follows — never the geometry the window still reports.
        let rect = resize(app, &panel);
        let size = size_of(app);
        window::apply_zoom_to(app, &panel, Rect { width: size.0, height: size.1, ..rect });
    }
}

/// A dropdown that reopened mid-search would be showing a stale query, so the
/// renderer is told to tear its panel down too.
fn collapse(app: &AppHandle, panel: &WebviewWindow) {
    let had_panel = state(app)
        .expanded_by
        .lock()
        .expect("panel lock")
        .is_some();
    if !had_panel {
        return;
    }
    set_expanded(app, panel, None);
    let _ = app.emit_to(PANEL, "panel-collapsed", ());
}

pub fn hide(app: &AppHandle, panel: &WebviewWindow) {
    let _ = panel.hide();
    collapse(app, panel);
    let state = state(app);
    state.visible.store(false, Ordering::SeqCst);
    *state.hidden_at.lock().expect("panel lock") = Some(Instant::now());
}

/// Called when the dropdown loses focus — a menu that stays up after you click
/// elsewhere is not a menu.
pub fn on_blur(app: &AppHandle, panel: &WebviewWindow) {
    if !panel.is_visible().unwrap_or(false) {
        return;
    }
    if state(app).menu_open.load(Ordering::SeqCst) {
        return; // it lost focus to its own context menu
    }
    hide(app, panel);
}

/// Pop the dropdown's context menu, holding it open for the duration.
/// `popup_menu` runs macOS's modal menu loop, so this returns once the menu has
/// been dismissed.
pub fn popup_menu(app: &AppHandle, panel: &WebviewWindow, menu: &tauri::menu::Menu<tauri::Wry>) {
    let state = state(app);
    state.menu_open.store(true, Ordering::SeqCst);
    let _ = panel.popup_menu(menu);
    state.menu_open.store(false, Ordering::SeqCst);
    // Take focus back, or the next click outside would have nothing to blur.
    let _ = panel.set_focus();
}

/// Park the panel under the tray icon, clamped to the screen it sits on.
/// Returns the rect it asked for.
fn position(app: &AppHandle, panel: &WebviewWindow, icon: Rect) -> Rect {
    let size = size_of(app);
    let anchor_x = icon.x + icon.width / 2.0;
    let area = window::work_area_near(app, anchor_x, icon.y);

    let min_x = area.x + EDGE_MARGIN;
    let max_x = area.x + area.width - size.0 - EDGE_MARGIN;
    let x = (anchor_x - size.0 / 2.0).clamp(min_x.min(max_x), max_x.max(min_x)).round();
    let y = (icon.y + icon.height + GAP_BELOW_MENU_BAR).round();

    let rect = Rect {
        x,
        y,
        width: size.0,
        height: size.1,
    };
    window::set_bounds(panel, rect);
    rect
}

pub fn toggle(app: &AppHandle, icon: Rect) {
    let Some(panel) = app.get_webview_window(PANEL) else {
        return;
    };
    let state = state(app);

    if state.visible.load(Ordering::SeqCst) || panel.is_visible().unwrap_or(false) {
        hide(app, &panel);
        return;
    }

    let guarded = state
        .hidden_at
        .lock()
        .expect("panel lock")
        .map(|at| at.elapsed() < REOPEN_GUARD)
        .unwrap_or(false);
    if guarded {
        return;
    }

    let rect = position(app, &panel, icon);
    window::apply_zoom_to(app, &panel, rect);
    state.visible.store(true, Ordering::SeqCst);
    let _ = panel.show();
    let _ = panel.set_focus();
}
