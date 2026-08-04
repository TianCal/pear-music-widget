//! The floating widget: natural sizes per skin, zoom, panel expansion and
//! position persistence.
//!
//! The window is exactly the card. The vibrancy layer is rounded to the same
//! radius as the content and macOS draws the shadow around it, which is the only
//! way to get blur clipped to the card's corners rather than a square behind it.
//!
//! Every skin is a self-contained layout with its own natural size and aspect
//! ratio. Dragging an edge scales whichever skin is showing; it never switches
//! between them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};
use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial, NSVisualEffectState};

use crate::macos;
use crate::store::{Position, StoredSize, Store};

pub const WIDGET: &str = "widget";
pub const PANEL: &str = "panel";

/// Natural size per skin. A flat table on purpose — there are no variants and no
/// automatic switching, so dragging an edge only ever scales the skin showing.
pub const BASE: [(&str, f64, f64); 2] = [("classic", 300.0, 110.0), ("stack", 330.0, 284.0)];

pub const SKINS: [&str; 2] = ["classic", "stack"];

const MIN_WIDTH: f64 = 240.0;
const MAX_WIDTH: f64 = 760.0;

/// The corner radius macOS applies to the vibrancy layer, matched by `--radius`
/// in the stylesheet.
const CORNER_RADIUS: f64 = 12.0;

/// Height each expanding panel adds, in CSS pixels. Must match `.search` and
/// `.lyrics` in styles.css — the window grows by exactly what the panel
/// occupies. Only one can be open at a time, which keeps the arithmetic trivial.
pub fn panel_height(which: &str) -> Option<f64> {
    match which {
        "search" => Some(216.0),
        "lyrics" => Some(240.0),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Per-window runtime state. Electron hung these off the BrowserWindow; here
/// they are keyed by window label.
#[derive(Default)]
pub struct WindowExtras {
    /// Which panel the window has grown for, if any.
    pub expanded_by: Option<String>,
    /// The size to come back to. Remembered rather than recomputed so an odd
    /// size the user chose survives the round trip.
    pub collapsed: Option<Rect>,
}

pub struct WindowState {
    extras: Mutex<HashMap<String, WindowExtras>>,
    /// Bumped on every move/resize; a persist task only writes if it is still
    /// the latest.
    persist_seq: AtomicU64,
    quitting: AtomicBool,
}

impl WindowState {
    pub fn new() -> Self {
        Self {
            extras: Mutex::new(HashMap::new()),
            persist_seq: AtomicU64::new(0),
            quitting: AtomicBool::new(false),
        }
    }

    pub fn begin_quit(&self) {
        self.quitting.store(true, Ordering::SeqCst);
    }

    pub fn is_quitting(&self) -> bool {
        self.quitting.load(Ordering::SeqCst)
    }

    pub fn with_extras<T>(&self, label: &str, edit: impl FnOnce(&mut WindowExtras) -> T) -> T {
        let mut extras = self.extras.lock().expect("window extras");
        edit(extras.entry(label.to_string()).or_default())
    }
}

fn state(app: &AppHandle) -> tauri::State<'_, Arc<WindowState>> {
    app.state::<Arc<WindowState>>()
}

// ------------------------------------------------------------------- skins

/// The settings file is hand-editable, so never trust the value blindly.
pub fn valid_skin(skin: &str) -> String {
    if SKINS.contains(&skin) {
        skin.to_string()
    } else {
        "classic".to_string()
    }
}

pub fn skin_of(store: &Store) -> String {
    store.get(|s| valid_skin(&s.skin))
}

pub fn panel_skin_of(store: &Store) -> String {
    store.get(|s| valid_skin(&s.panel_skin))
}

pub fn base_for(skin: &str) -> (f64, f64) {
    let skin = valid_skin(skin);
    BASE.iter()
        .find(|(name, _, _)| *name == skin)
        .map(|(_, width, height)| (*width, *height))
        .expect("valid_skin always names a row")
}

fn aspect_of(skin: &str) -> f64 {
    let (width, height) = base_for(skin);
    width / height
}

fn height_for(width: f64, skin: &str) -> f64 {
    (width / aspect_of(skin)).round()
}

/// The size the user last left this skin at, falling back to its natural size.
/// Only the width is stored back — the height is always re-derived, so a size
/// can never drift off the skin's aspect ratio.
fn size_for_skin(store: &Store, skin: &str) -> (f64, f64) {
    match store.get(|s| s.sizes.get(skin).copied()) {
        Some(StoredSize { width }) if width > 0.0 => {
            let width = width.clamp(MIN_WIDTH, MAX_WIDTH);
            (width, height_for(width, skin))
        }
        _ => base_for(skin),
    }
}

/// Remember the current width against the skin that is showing.
fn remember_size(store: &Store, skin: &str, width: f64) {
    store.update(|s| {
        s.sizes.insert(skin.to_string(), StoredSize { width });
    });
}

// ----------------------------------------------------------------- geometry

/// Work areas in logical points, which is what the store and every size in this
/// module are expressed in.
fn work_areas(app: &AppHandle) -> Vec<Rect> {
    app.available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|monitor| {
            let scale = monitor.scale_factor();
            let area = monitor.work_area();
            Rect {
                x: area.position.x as f64 / scale,
                y: area.position.y as f64 / scale,
                width: area.size.width as f64 / scale,
                height: area.size.height as f64 / scale,
            }
        })
        .collect()
}

fn primary_work_area(app: &AppHandle) -> Rect {
    app.primary_monitor()
        .ok()
        .flatten()
        .map(|monitor| {
            let scale = monitor.scale_factor();
            let area = monitor.work_area();
            Rect {
                x: area.position.x as f64 / scale,
                y: area.position.y as f64 / scale,
                width: area.size.width as f64 / scale,
                height: area.size.height as f64 / scale,
            }
        })
        .unwrap_or(Rect {
            x: 0.0,
            y: 0.0,
            width: 1440.0,
            height: 900.0,
        })
}

/// The work area of the display holding most of `rect`, falling back to the
/// primary one.
pub fn work_area_matching(app: &AppHandle, rect: Rect) -> Rect {
    let centre_x = rect.x + rect.width / 2.0;
    let centre_y = rect.y + rect.height / 2.0;

    work_areas(app)
        .into_iter()
        .find(|area| {
            centre_x >= area.x
                && centre_x < area.x + area.width
                && centre_y >= area.y
                && centre_y < area.y + area.height
        })
        .unwrap_or_else(|| primary_work_area(app))
}

/// The work area of the display nearest a point, for anchoring the dropdown.
pub fn work_area_near(app: &AppHandle, x: f64, y: f64) -> Rect {
    work_area_matching(
        app,
        Rect {
            x,
            y,
            width: 0.0,
            height: 0.0,
        },
    )
}

/// Top right, inset from the work area — which already starts below the menu
/// bar, so the widget clears it without knowing how tall it is.
fn default_position(app: &AppHandle, size: (f64, f64)) -> Position {
    let area = primary_work_area(app);
    Position {
        x: (area.x + area.width - size.0 - 16.0).round(),
        y: (area.y + 16.0).round(),
    }
}

/// Keep the saved position usable if the display layout changed since last run.
fn is_on_screen(app: &AppHandle, bounds: Position, size: (f64, f64)) -> bool {
    work_areas(app).into_iter().any(|area| {
        bounds.x < area.x + area.width - 40.0
            && bounds.x + size.0 > area.x + 40.0
            && bounds.y < area.y + area.height - 40.0
            && bounds.y + size.1 > area.y
    })
}

pub fn bounds_of(window: &WebviewWindow) -> Rect {
    let scale = window.scale_factor().unwrap_or(1.0);
    let position = window
        .outer_position()
        .map(|p| p.to_logical::<f64>(scale))
        .unwrap_or(LogicalPosition::new(0.0, 0.0));
    let size = window
        .inner_size()
        .map(|s| s.to_logical::<f64>(scale))
        .unwrap_or(LogicalSize::new(0.0, 0.0));
    Rect {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    }
}

pub fn set_bounds(window: &WebviewWindow, rect: Rect) {
    let _ = window.set_position(LogicalPosition::new(rect.x, rect.y));
    let _ = window.set_size(LogicalSize::new(rect.width, rect.height));
}

// --------------------------------------------------------------------- zoom

/// Scale the page so the layout always fills the window. The renderer is
/// authored once at the base size; everything else is zoom.
///
/// Measures the window, so this is only correct when nothing has just resized
/// it — use `apply_zoom_to` after a programmatic resize.
pub fn apply_zoom(app: &AppHandle, window: &WebviewWindow) {
    apply_zoom_to(app, window, bounds_of(window));
}

/// Zoom for a size we have just asked for rather than one we can read back.
///
/// Tauri's `set_size`/`set_position` are messages on the event loop, so
/// `bounds_of` immediately afterwards still reports the *old* geometry. Zooming
/// off that measurement is what left a resized window rendering its layout at
/// the previous skin's scale, stranded in a corner of the new one.
pub fn apply_zoom_to(app: &AppHandle, window: &WebviewWindow, rect: Rect) {
    let store = app.state::<Arc<Store>>();
    let skin = if window.label() == PANEL {
        panel_skin_of(&store)
    } else {
        skin_of(&store)
    };
    let (base_width, base_height) = base_for(&skin);

    // Take the tighter of the two axes. Rounding the window height to whole
    // pixels can leave it a fraction short of the ratio, and scaling on width
    // alone would then clip the last row by a pixel.
    let zoom = (rect.width / base_width)
        .min(rect.height / base_height)
        .clamp(0.4, 3.0);

    let _ = window.set_zoom(zoom);
    // The corner radius is fixed in device pixels, so the CSS radius has to
    // shrink as the page scales up or the two stop lining up.
    let _ = app.emit_to(window.label(), "zoom", zoom);
}

// ------------------------------------------------------------------ chrome

/// Everything both windows share: glass, no shadow surprises, no Space that
/// hides them.
pub fn dress(window: &WebviewWindow, material: NSVisualEffectMaterial, level: isize) {
    if let Err(err) = apply_vibrancy(
        window,
        material,
        Some(NSVisualEffectState::Active),
        Some(CORNER_RADIUS),
    ) {
        eprintln!("[vibrancy] {}: {err}", window.label());
    }
    macos::set_has_shadow(window, true);
    macos::set_level(window, level);
    macos::join_all_spaces(window);
}

// ------------------------------------------------------------------ create

/// `PMW_THEME=dark|light` pins the webview's colour scheme. Debug-only, and the
/// only way to see both appearances without changing the machine's setting.
pub fn forced_theme() -> Option<tauri::Theme> {
    match std::env::var("PMW_THEME").ok()?.as_str() {
        "dark" => Some(tauri::Theme::Dark),
        "light" => Some(tauri::Theme::Light),
        _ => None,
    }
}

pub fn create(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let store = app.state::<Arc<Store>>().inner().clone();
    let skin = skin_of(&store);
    let size = size_for_skin(&store, &skin);

    let saved = store.get(|s| s.bounds);
    let position = saved
        .filter(|bounds| is_on_screen(app, *bounds, size))
        .unwrap_or_else(|| default_position(app, size));

    let window = WebviewWindowBuilder::new(app, WIDGET, WebviewUrl::App("index.html".into()))
        .title("Pear Music Widget")
        .inner_size(size.0, size.1)
        .position(position.x, position.y)
        .decorations(false)
        .transparent(true)
        .resizable(true)
        .maximizable(false)
        .minimizable(false)
        .always_on_top(store.get(|s| s.always_on_top))
        .skip_taskbar(true)
        .shadow(true)
        .visible(false)
        .theme(forced_theme())
        .build()?;

    dress(&window, NSVisualEffectMaterial::UnderWindowBackground, macos::LEVEL_FLOATING);
    macos::set_alpha(&window, store.get(|s| s.opacity));
    apply_size_limits(&window, &skin);
    macos::set_aspect_ratio(&window, Some(base_for(&skin)));

    apply_zoom(app, &window);
    #[cfg(debug_assertions)]
    if std::env::var("PMW_DEVTOOLS").is_ok() {
        window.open_devtools();
    }
    let _ = window.show();

    Ok(window)
}

fn apply_size_limits(window: &WebviewWindow, skin: &str) {
    let _ = window.set_min_size(Some(LogicalSize::new(
        MIN_WIDTH,
        height_for(MIN_WIDTH, skin),
    )));
    let _ = window.set_max_size(Some(LogicalSize::new(
        MAX_WIDTH,
        height_for(MAX_WIDTH, skin),
    )));
}

/// While a panel is open the aspect lock is off and the window is taller than
/// any skin, so the limits have to open up with it.
fn release_size_limits(window: &WebviewWindow) {
    let _ = window.set_min_size(Some(LogicalSize::new(MIN_WIDTH, 80.0)));
    let _ = window.set_max_size(Some(LogicalSize::new(MAX_WIDTH, 4000.0)));
}

// ------------------------------------------------------------- persistence

/// Debounced write of position and per-skin width. Growing for a panel is our
/// own doing and must never be persisted as the user's preferred size, so the
/// caller checks `expanded_by` first.
pub fn schedule_persist(app: &AppHandle) {
    let state = state(app).inner().clone();
    let seq = state.persist_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();

    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        if state.persist_seq.load(Ordering::SeqCst) != seq {
            return;
        }
        let Some(window) = app.get_webview_window(WIDGET) else {
            return;
        };
        let store = app.state::<Arc<Store>>().inner().clone();
        let rect = bounds_of(&window);

        store.update(|s| {
            s.bounds = Some(Position {
                x: rect.x.round(),
                y: rect.y.round(),
            })
        });
        remember_size(&store, &skin_of(&store), rect.width.round());
    });
}

// -------------------------------------------------------------- expansion

/// Grow the widget downwards to make room for a panel, and put it back
/// afterwards. The aspect ratio has to be released while expanded or a drag
/// would snap the panel away, and the collapsed bounds are remembered rather
/// than recomputed so an odd size the user chose survives the round trip.
pub fn set_expanded(app: &AppHandle, window: &WebviewWindow, panel: Option<&str>) {
    let state = state(app).inner().clone();
    let label = window.label().to_string();
    let panel = panel.filter(|which| panel_height(which).is_some());

    let already = state.with_extras(&label, |extras| extras.expanded_by.clone());
    if already.as_deref() == panel {
        return;
    }

    if let Some(which) = panel {
        let extra = panel_height(which).unwrap_or(0.0);

        // Switching straight from one panel to another: measure from the
        // collapsed size we already remembered, not from the expanded window.
        let collapsed = state
            .with_extras(&label, |extras| extras.collapsed)
            .unwrap_or_else(|| bounds_of(window));
        state.with_extras(&label, |extras| {
            extras.collapsed = Some(collapsed);
            extras.expanded_by = Some(which.to_string());
        });

        let zoom = collapsed.width / base_for(&skin_of(&app.state::<Arc<Store>>())).0;
        let height = collapsed.height + (extra * zoom).round();

        // Slide up if the taller window would hang off the bottom of the screen.
        let area = work_area_matching(app, collapsed);
        let max_y = area.y + area.height - height;
        let y = collapsed.y.min(max_y.max(area.y)).round();

        macos::set_aspect_ratio(window, None);
        release_size_limits(window);
        set_bounds(
            window,
            Rect {
                x: collapsed.x,
                y,
                width: collapsed.width,
                height,
            },
        );
        return;
    }

    let collapsed = state.with_extras(&label, |extras| {
        extras.expanded_by = None;
        extras.collapsed.take()
    });

    let skin = skin_of(&app.state::<Arc<Store>>());
    apply_size_limits(window, &skin);
    if let Some(collapsed) = collapsed {
        set_bounds(window, collapsed);
    }
    macos::set_aspect_ratio(window, Some(base_for(&skin)));
}

/// Set the window to a collapsed size, re-adding the open panel's height if one
/// is showing. Without this, changing skin or resetting while lyrics or search
/// are open would shrink the window *under* the panel and the two would overlap.
fn apply_collapsed_size(app: &AppHandle, window: &WebviewWindow, rect: Rect) {
    let state = state(app).inner().clone();
    let label = window.label().to_string();
    let open = state.with_extras(&label, |extras| extras.expanded_by.clone());

    let Some(extra) = open.as_deref().and_then(panel_height) else {
        set_bounds(window, rect);
        return;
    };

    state.with_extras(&label, |extras| extras.collapsed = Some(rect));
    let zoom = rect.width / base_for(&skin_of(&app.state::<Arc<Store>>())).0;
    set_bounds(
        window,
        Rect {
            height: rect.height + (extra * zoom).round(),
            ..rect
        },
    );
}

/// Resize to `next`, pinning whichever corner of the window is nearest a screen
/// corner so a widget parked bottom-right stays there. Measures against the
/// collapsed bounds when a panel is open, so the panel's extra height does not
/// skew which corner looks nearest.
fn resize_keeping_corner(app: &AppHandle, window: &WebviewWindow, next: (f64, f64)) -> Position {
    let state = state(app).inner().clone();
    let current = state
        .with_extras(window.label(), |extras| extras.collapsed)
        .unwrap_or_else(|| bounds_of(window));
    let area = work_area_matching(app, current);

    let dist_left = (current.x - area.x).abs();
    let dist_right = (area.x + area.width - (current.x + current.width)).abs();
    let dist_top = (current.y - area.y).abs();
    let dist_bottom = (area.y + area.height - (current.y + current.height)).abs();

    let x = if dist_right < dist_left {
        current.x + current.width - next.0
    } else {
        current.x
    }
    .round();
    let y = if dist_bottom < dist_top {
        current.y + current.height - next.1
    } else {
        current.y
    }
    .round();

    apply_collapsed_size(
        app,
        window,
        Rect {
            x,
            y,
            width: next.0,
            height: next.1,
        },
    );
    Position { x, y }
}

// --------------------------------------------------------------- skin swap

/// Switch layout wholesale. Each skin has its own aspect ratio, so the lock has
/// to be re-set or the next drag would snap the window back to the old shape.
pub fn apply_skin(app: &AppHandle, window: &WebviewWindow, skin: &str) {
    if !SKINS.contains(&skin) {
        return;
    }
    let store = app.state::<Arc<Store>>().inner().clone();
    store.update(|s| s.skin = skin.to_string());

    macos::set_aspect_ratio(window, None);
    release_size_limits(window);

    // Come back to whatever size the user last left this skin at.
    let size = size_for_skin(&store, skin);
    let position = resize_keeping_corner(app, window, size);
    store.update(|s| s.bounds = Some(position));

    apply_size_limits(window, skin);
    macos::set_aspect_ratio(window, Some(base_for(skin)));
    apply_zoom_to(
        app,
        window,
        Rect {
            x: position.x,
            y: position.y,
            width: size.0,
            height: size.1,
        },
    );
}

/// Back to the skin's natural size, in the default corner, forgetting the size
/// the user had chosen for it.
pub fn reset_window(app: &AppHandle, window: &WebviewWindow) {
    let store = app.state::<Arc<Store>>().inner().clone();
    let skin = skin_of(&store);
    let size = base_for(&skin);
    let position = default_position(app, size);

    store.update(|s| {
        s.sizes.remove(&skin);
        s.bounds = Some(position);
    });

    let rect = Rect {
        x: position.x,
        y: position.y,
        width: size.0,
        height: size.1,
    };
    apply_collapsed_size(app, window, rect);
    apply_zoom_to(app, window, rect);
}
