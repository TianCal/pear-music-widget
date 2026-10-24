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
/// The menu-bar dropdown's window. Its label is "panel" for history; in code
/// "panel" means the queue/lyrics/search section that opens inside a card.
pub const DROPDOWN: &str = "panel";

/// Natural size per skin. A flat table on purpose — there are no variants and no
/// automatic switching, so dragging an edge only ever scales the skin showing.
pub const BASE: [(&str, f64, f64); 1] = [("stack", 330.0, 284.0)];

pub const SKINS: [&str; 1] = ["stack"];

/// Whether a skin draws the queue as part of its own layout — Stack does, under
/// the transport. Declared here beside `BASE` so a new skin states its appetite
/// in one place; `refresh_queue` used to ask `skin == "stack"` in two spots.
///
/// This is about the *skin*. The queue panel is orthogonal: it can be open over
/// any skin, and it makes the queue wanted on its own.
pub fn skin_shows_queue(skin: &str) -> bool {
    matches!(skin, "stack")
}

const MIN_WIDTH: f64 = 240.0;
const MAX_WIDTH: f64 = 760.0;

/// The corner radius macOS applies to the vibrancy layer, matched by `--radius`
/// in the stylesheet.
const CORNER_RADIUS: f64 = 12.0;

/// Height each expanding panel adds, in CSS pixels. Must match `.search`,
/// `.lyrics` and `.queue` in styles.css — the window grows by exactly what the
/// panel occupies. Only one can be open at a time, which keeps the arithmetic
/// trivial. `Some(_)` is also what makes a name a panel at all: every caller
/// filters through this.
pub fn panel_height(which: &str) -> Option<f64> {
    match which {
        "search" => Some(216.0),
        "lyrics" => Some(192.0),
        "queue" => Some(260.0),
        _ => None,
    }
}

/// Panels worth reopening on the next launch. A search is a query you have
/// finished with; a queue or a lyric sheet is a mode you left open.
pub fn panel_is_restorable(which: &str) -> bool {
    matches!(which, "lyrics" | "queue")
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
    /// The last geometry we asked for ourselves, so a move or resize event can
    /// be told apart from one the user caused.
    pub last_set: Option<Rect>,
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
        "stack".to_string()
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

/// `y` moved up, if need be, so a window `height` tall stays inside `area`,
/// but never above its top.
pub fn fit_top(area: Rect, y: f64, height: f64) -> f64 {
    y.min((area.y + area.height - height).max(area.y)).round()
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

pub fn set_bounds(app: &AppHandle, window: &WebviewWindow, rect: Rect) {
    state(app).with_extras(window.label(), |extras| extras.last_set = Some(rect));
    let _ = window.set_position(LogicalPosition::new(rect.x, rect.y));
    let _ = window.set_size(LogicalSize::new(rect.width, rect.height));
}

/// Whether two rects are the same window position to within the rounding that
/// survives a logical → physical → logical round trip on a scaled display.
fn same_rect(a: Rect, b: Rect) -> bool {
    (a.x - b.x).abs() <= 1.0
        && (a.y - b.y).abs() <= 1.0
        && (a.width - b.width).abs() <= 1.0
        && (a.height - b.height).abs() <= 1.0
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
    let skin = if window.label() == DROPDOWN {
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
    // shrink as the page scales up or the two stop lining up. Sent with the
    // zoom so the renderer never keeps its own copy of the number.
    let _ = app.emit_to(
        window.label(),
        "zoom",
        serde_json::json!({ "zoom": zoom, "radius": CORNER_RADIUS }),
    );
}

// ------------------------------------------------------------------ chrome

/// Everything both windows share: glass, no shadow surprises, no Space that
/// hides them.
///
/// `level` is `None` for the widget: `always_on_top` already governs it, and a
/// second writer for the same property is how it ended up floating regardless
/// of the setting. The dropdown needs one, because Tauri cannot express the
/// pop-up-menu level a menu-bar dropdown has to sit at.
pub fn dress(
    window: &WebviewWindow,
    material: NSVisualEffectMaterial,
    level: Option<isize>,
    follow_everywhere: bool,
    liquid_glass: bool,
) {
    if !macos::install_liquid_glass(window, CORNER_RADIUS, liquid_glass) {
        if let Err(err) = apply_vibrancy(
            window,
            material,
            Some(NSVisualEffectState::Active),
            Some(CORNER_RADIUS),
        ) {
            eprintln!("[vibrancy] {}: {err}", window.label());
        }
    }
    macos::set_has_shadow(window, true);
    if let Some(level) = level {
        macos::set_level(window, level);
    }
    macos::follow_everywhere(window, follow_everywhere);
    macos::accept_mouse_moved(window);
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
        .title("Perch Player")
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
        // Clicking an inactive window normally just activates it and the click
        // is swallowed, which is why a "Next tracks" row needed clicking twice:
        // the first press only brought the widget forward. A widget is meant to
        // be operable without focusing it first.
        .accept_first_mouse(true)
        .build()?;

    // Following you across Spaces — and over a fullscreen app — is part of what
    // "always on top" means. Off, the widget is an ordinary window.
    dress(
        &window,
        NSVisualEffectMaterial::UnderWindowBackground,
        None,
        store.get(|s| s.always_on_top),
        store.get(|s| s.liquid_glass),
    );
    macos::set_alpha(&window, store.get(|s| s.opacity));
    let shape = shape_of(&skin, None);
    apply_size_limits(&window, shape);
    macos::set_aspect_ratio(&window, Some(shape));

    // Come back up showing whatever panel was left open. Done here, before the
    // window is ever on screen, rather than waiting for the renderer to ask:
    // the page boots well after `show`, so restoring it from there would put a
    // visible grow into every launch. The renderer's own `set_panel` then finds
    // the window already expanded and does nothing.
    if let Some(panel) = store.get(|s| s.panel.clone()) {
        set_expanded(app, &window, Some(&panel));
    }

    apply_zoom(app, &window);
    #[cfg(debug_assertions)]
    if std::env::var("PMW_DEVTOOLS").is_ok() {
        window.open_devtools();
    }
    let _ = window.show();
    crate::on_screen(app, WIDGET, true);

    Ok(window)
}

/// The shape the window should hold: the skin's, plus the height of whichever
/// panel is open.
///
/// A panel scales with the page, so its height is a fixed multiple of the width
/// and **an expanded window has a constant aspect ratio too** — the skin's, with
/// the panel added to the height. Expanding therefore does not have to abandon
/// the aspect lock, only widen it, which is what makes dragging an edge with the
/// lyrics open scale the card instead of stretching a window whose contents
/// ignore it.
fn shape_of(skin: &str, open: Option<&str>) -> (f64, f64) {
    let (width, height) = base_for(skin);
    (width, height + open.and_then(panel_height).unwrap_or(0.0))
}

/// The width range the window may be dragged through, as heights for `shape`.
///
/// Always applied *before* the resize it is meant to allow: `setFrame:` clamps
/// to the min and max, so limits still describing the old shape would swallow
/// the new size.
fn apply_size_limits(window: &WebviewWindow, shape: (f64, f64)) {
    let height = |width: f64| (width * shape.1 / shape.0).round();
    let _ = window.set_min_size(Some(LogicalSize::new(MIN_WIDTH, height(MIN_WIDTH))));
    let _ = window.set_max_size(Some(LogicalSize::new(MAX_WIDTH, height(MAX_WIDTH))));
}

/// Whether a move or resize event is the settling of something we asked for
/// rather than something the user did.
fn is_ours(state: &WindowState, label: &str, rect: Rect) -> bool {
    state
        .with_extras(label, |extras| extras.last_set)
        .is_some_and(|last| same_rect(last, rect))
}

/// A move or resize the user performed. While a panel is open the window
/// carries the panel's height, so the collapsed shape it must return to is
/// re-derived from the width rather than read off the window.
pub fn on_geometry_changed(app: &AppHandle, window: &WebviewWindow) {
    let state = state(app).inner().clone();
    let label = window.label().to_string();
    let rect = bounds_of(window);

    if !is_ours(&state, &label, rect) {
        let skin = skin_of(&app.state::<Arc<Store>>());
        state.with_extras(&label, |extras| {
            if extras.expanded_by.is_some() {
                extras.collapsed = Some(Rect {
                    height: height_for(rect.width, &skin),
                    ..rect
                });
            }
        });
    }

    apply_zoom(app, window);
    schedule_persist(app);
}

// ------------------------------------------------------------- persistence

/// Debounced write of position and per-skin width. Growing for a panel is our
/// own doing and must never be persisted as the user's preferred geometry:
/// while a panel is open the collapsed rectangle is what gets saved, since the
/// expanded window may have slid up to fit the screen.
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
        let collapsed =
            state.with_extras(WIDGET, |extras| extras.expanded_by.as_ref().and(extras.collapsed));
        let rect = collapsed.unwrap_or_else(|| bounds_of(&window));

        // One write, not one per field, and off this async worker.
        let skin = skin_of(&store);
        store.update_soon(|s| {
            s.bounds = Some(Position {
                x: rect.x.round(),
                y: rect.y.round(),
            });
            s.sizes.insert(skin, StoredSize { width: rect.width.round() });
        });
    });
}

// -------------------------------------------------------------- expansion

/// Grow the widget downwards to make room for a panel, and put it back
/// afterwards. The aspect ratio moves to the taller shape rather than being
/// released — a released lock let a drag stretch the window without the page
/// following — and the collapsed bounds are remembered rather than recomputed so
/// an odd size the user chose survives the round trip.
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
        let y = fit_top(work_area_matching(app, collapsed), collapsed.y, height);

        let shape = shape_of(&skin_of(&app.state::<Arc<Store>>()), Some(which));
        apply_size_limits(window, shape);
        set_bounds(
            app,
            window,
            Rect {
                x: collapsed.x,
                y,
                width: collapsed.width,
                height,
            },
        );
        macos::set_aspect_ratio(window, Some(shape));
        return;
    }

    let collapsed = state.with_extras(&label, |extras| {
        extras.expanded_by = None;
        extras.collapsed.take()
    });

    let shape = shape_of(&skin_of(&app.state::<Arc<Store>>()), None);
    apply_size_limits(window, shape);
    if let Some(collapsed) = collapsed {
        set_bounds(app, window, collapsed);
    }
    macos::set_aspect_ratio(window, Some(shape));
}

/// Set the window to a collapsed size, re-adding the open panel's height if one
/// is showing. Without this, changing skin or resetting while lyrics or search
/// are open would shrink the window *under* the panel and the two would overlap.
fn apply_collapsed_size(app: &AppHandle, window: &WebviewWindow, rect: Rect) {
    let state = state(app).inner().clone();
    let label = window.label().to_string();
    let open = state.with_extras(&label, |extras| extras.expanded_by.clone());

    let Some(extra) = open.as_deref().and_then(panel_height) else {
        set_bounds(app, window, rect);
        return;
    };

    state.with_extras(&label, |extras| extras.collapsed = Some(rect));
    let zoom = rect.width / base_for(&skin_of(&app.state::<Arc<Store>>())).0;
    set_bounds(
        app,
        window,
        Rect {
            height: rect.height + (extra * zoom).round(),
            ..rect
        },
    );
}

/// Back to the skin's natural size, in the default position, forgetting the size
/// the user had chosen for it.
pub fn reset_window(app: &AppHandle, window: &WebviewWindow) {
    let store = app.state::<Arc<Store>>().inner().clone();
    let skin = skin_of(&store);
    let size = base_for(&skin);
    let position = default_position(app, size);

    store.update_soon(|s| {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The claim that lets an expanded window keep an aspect lock at all: the
    /// height `apply_collapsed_size` computes for a given width is the height
    /// the lock would hold it to. If these disagree, dragging an edge with the
    /// lyrics open fights the layout.
    #[test]
    fn an_open_panel_scales_with_the_card() {
        for skin in SKINS {
            let (base_width, base_height) = base_for(skin);
            for panel in ["search", "lyrics", "queue"] {
                let extra = panel_height(panel).expect("a known panel");
                let shape = shape_of(skin, Some(panel));
                assert_eq!(shape, (base_width, base_height + extra));

                for width in [MIN_WIDTH, base_width, 500.0, MAX_WIDTH] {
                    let zoom = width / base_width;
                    let grown = height_for(width, skin) + (extra * zoom).round();
                    let locked = (width * shape.1 / shape.0).round();
                    assert!(
                        (grown - locked).abs() <= 1.0,
                        "{skin}/{panel} at {width}: grows to {grown}, locked to {locked}"
                    );
                }
            }
        }
    }
}
