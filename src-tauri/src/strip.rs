//! The menu-bar strip: the playing song at the top centre of the display with
//! the menu bar, either side of the camera housing where there is one.
//!
//! Its own page (`strip/index.html`), turned on from the tray menu
//! (`menuBarStrip`). The window exists only while the strip is on: built by
//! `show`, destroyed by `hide`, so a strip that is off costs no webview.
//!
//! The window never changes size: resizing a webview shows its old picture
//! for a frame, pinned to the corner, which reads as a jump or a flash.
//! Instead it is one fixed transparent box, centred on the top edge, that lets
//! clicks through except while the sheet is open and the pointer is on it.
//! The page reports which part of it is drawn (`strip_hot`), and only that
//! part counts as hovering.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::Thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::macos;
use crate::state::Core;
use crate::store::Store;

pub const LABEL: &str = "strip";

/// The colour of the folded bar's words (`stripInk`): `auto` (follows the
/// appearance), `cover` (the cover's accent), or a `#rrggbb` picked in the
/// system Colours panel. The menu bar's own text follows the wallpaper rather
/// than the appearance, so Automatic can come out unreadable over it.
pub fn valid_ink(ink: &str) -> bool {
    matches!(ink, "auto" | "cover")
        || (ink.len() == 7
            && ink.starts_with('#')
            && ink[1..].chars().all(|c| c.is_ascii_hexdigit()))
}

/// The stored ink, or Automatic for anything else: the file is hand-editable.
pub fn ink_of(store: &Store) -> String {
    store.get(|s| match s.strip_ink.as_deref() {
        Some(ink) if valid_ink(ink) => ink.to_ascii_lowercase(),
        _ => "auto".to_string(),
    })
}

pub fn set_ink(app: &AppHandle, ink: &str) {
    if !valid_ink(ink) {
        return;
    }
    let ink = ink.to_ascii_lowercase();
    app.state::<Arc<Store>>().update_soon(|s| s.strip_ink = Some(ink.clone()));
    // The strip paints it, so it travels as state, like `tint`.
    app.state::<Arc<Core>>().update(|state| state.strip_ink = ink.clone());
}

/// Text colour → Choose colour…: the system Colours panel, starting from the
/// current custom colour. Each change applies at once, so the bar follows
/// the wheel while it is dragged.
pub fn choose_ink(app: &AppHandle) {
    let current = ink_of(&app.state::<Arc<Store>>());
    let start = current.starts_with('#').then_some(current);
    let handle = app.clone();
    macos::pick_colour(app, start, move |hex| set_ink(&handle, &hex));
}

/// `NSStatusWindowLevel`: over the menu bar (24), under the menus it opens.
const LEVEL_STATUS: isize = 25;

/// Room for the widest bar (a long title either side of a notch) and for the
/// open sheet with its shadow, so the window never has to grow.
const WINDOW: (f64, f64) = (840.0, 300.0);

/// While the pointer moves, at most how often it is looked at near the top
/// of the screen, and further away. A still pointer is not looked at: the
/// watcher sleeps until macOS says it moved (`macos::watch_pointer`).
const HOVER_NEAR: Duration = Duration::from_millis(40);
const HOVER_FAR: Duration = Duration::from_millis(150);

/// After a Space change, another app coming forward or a display change
/// (`macos::watch_screens`), look for a fullscreen app this often, for this
/// long, while the switch animates; after a display change, re-measure the
/// menu bar too.
const SETTLE_CHECK: Duration = Duration::from_millis(250);
const SETTLE: Duration = Duration::from_millis(1500);
/// Otherwise this seldom, for whatever changes unannounced: an app hiding
/// the menu bar without a Space of its own.
const FALLBACK_CHECK: Duration = Duration::from_secs(5);
/// In fullscreen with the pointer near the top, or the menu bar down: it
/// slides with no event to say so, and the strip comes and goes with it.
const MENU_BAR_CHECK: Duration = Duration::from_millis(100);
/// How far down the pointer counts as "near the top" for that.
const NEAR_TOP: f64 = 60.0;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Shape {
    /// Menu bar height, points.
    pub bar: f64,
    /// Camera housing width, points; 0 without one.
    pub notch: f64,
    /// `PMW_STRIP_OPEN=1` keeps the sheet open, for screenshots.
    pub pinned: bool,
}

/// The drawn part of the window, in points from its top-left corner.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct Hot {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Hot {
    fn contains(&self, (x, y): (f64, f64)) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y <= self.y + self.height
    }
}

/// Top-left and width of the window, in Tauri's logical coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Frame {
    x: f64,
    y: f64,
    width: f64,
}

pub struct StripState {
    shape: Mutex<Shape>,
    frame: Mutex<Frame>,
    hot: Mutex<Hot>,
    /// Whether the pointer was over the drawn part at the last look.
    hovered: AtomicBool,
    /// Whether the page has the sheet open (`strip_open`).
    open: AtomicBool,
    /// Set while the strip's right-click menu is up: the pointer is in the
    /// menu, off the sheet, and that must not fold the sheet under it.
    menu_open: AtomicBool,
    /// Set while a fullscreen app is up and its menu bar is not down: the
    /// strip is hidden, with the setting still on.
    covered: AtomicBool,
    /// `menuBarStrip`, for the watcher: it can look every 40 ms, and reading
    /// the setting itself took the settings lock each time.
    on: AtomicBool,
    /// Something to look at: the pointer moved, or the drawn part changed.
    look: AtomicBool,
    /// A Space, app or display changed (`macos::watch_screens`).
    screens_changed: AtomicBool,
    /// A display changed.
    displays_changed: AtomicBool,
    watcher: OnceLock<Thread>,
}

impl StripState {
    /// Wake the watcher for a look. Called for every pointer event, so it
    /// unparks only the first time before the watcher has looked.
    fn nudge(&self) {
        if !self.look.swap(true, Ordering::SeqCst) {
            if let Some(watcher) = self.watcher.get() {
                watcher.unpark();
            }
        }
    }
}

fn state(app: &AppHandle) -> Arc<StripState> {
    app.state::<Arc<StripState>>().inner().clone()
}

/// Main thread only: it asks `NSScreen`. `PMW_NOTCH=<points>` pretends the
/// display has a notch that wide, to see the wrapped layout on a flat one.
/// Debug aids, both.
fn measure() -> Shape {
    let (bar, notch) = macos::menu_bar_shape();
    let notch = std::env::var("PMW_NOTCH")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(notch);
    let pinned = std::env::var("PMW_STRIP_OPEN").is_ok_and(|value| value == "1");
    Shape { bar, notch, pinned }
}

/// The window centred on the top edge of the menu-bar display.
fn frame(app: &AppHandle) -> Frame {
    let Ok(Some(monitor)) = app.primary_monitor() else {
        return Frame { x: 0.0, y: 0.0, width: WINDOW.0 };
    };
    let scale = monitor.scale_factor();
    let position = monitor.position().to_logical::<f64>(scale);
    let size = monitor.size().to_logical::<f64>(scale);
    let width = WINDOW.0.min(size.width);
    Frame {
        x: (position.x + (size.width - width) / 2.0).round(),
        y: position.y,
        width,
    }
}

/// From `setup`, on the main thread.
pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    app.manage(Arc::new(StripState {
        shape: Mutex::new(measure()),
        frame: Mutex::new(Frame::default()),
        hot: Mutex::new(Hot::default()),
        hovered: AtomicBool::new(false),
        open: AtomicBool::new(false),
        menu_open: AtomicBool::new(false),
        covered: AtomicBool::new(false),
        on: AtomicBool::new(app.state::<Arc<Store>>().get(|s| s.menu_bar_strip)),
        look: AtomicBool::new(false),
        screens_changed: AtomicBool::new(false),
        displays_changed: AtomicBool::new(false),
        watcher: OnceLock::new(),
    }));
    let strip = state(app);
    let _ = strip.watcher.set(watch_pointer(app.clone()));
    macos::watch_screens(move |display| {
        if display {
            strip.displays_changed.store(true, Ordering::SeqCst);
        }
        strip.screens_changed.store(true, Ordering::SeqCst);
        strip.look.store(true, Ordering::SeqCst);
        if let Some(watcher) = strip.watcher.get() {
            watcher.unpark();
        }
    });
    if state(app).on.load(Ordering::SeqCst) {
        show(app)?;
    }
    Ok(())
}

/// From the tray menu, on the main thread.
pub fn set_shown(app: &AppHandle, shown: bool) {
    app.state::<Arc<Store>>().update_soon(|s| s.menu_bar_strip = shown);
    state(app).on.store(shown, Ordering::SeqCst);
    if shown {
        // The menu bar may have moved to another display since launch.
        *state(app).shape.lock().expect("strip lock") = measure();
        if let Err(err) = show(app) {
            eprintln!("[strip] {err}");
        }
    } else {
        hide(app);
    }
}

fn show(app: &AppHandle) -> tauri::Result<()> {
    let strip = state(app);
    // Built visible; if a fullscreen app is up with its menu bar away, the
    // watcher's first look hides it again.
    strip.covered.store(false, Ordering::SeqCst);
    strip.screens_changed.store(true, Ordering::SeqCst);
    strip.displays_changed.store(true, Ordering::SeqCst);
    if app.get_webview_window(LABEL).is_none() {
        let frame = frame(app);
        *strip.frame.lock().expect("strip lock") = frame;
        let window = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("strip/index.html".into()))
            .title("Perch Player")
            .inner_size(frame.width, WINDOW.1)
            // Here rather than a `set_frame` after building, which the
            // builder's own position would overrule.
            .position(frame.x, frame.y)
            .decorations(false)
            .transparent(true)
            .resizable(false)
            .maximizable(false)
            .minimizable(false)
            .skip_taskbar(true)
            .shadow(false)
            .focused(false)
            .theme(crate::window::forced_theme())
            .accept_first_mouse(true)
            .build()?;
        macos::set_level(&window, LEVEL_STATUS);
        // Every desktop, but not over a fullscreen app, whose menu bar is
        // hidden: the strip would float over its content.
        macos::join_all_desktops(&window);
        macos::set_ignores_mouse(&window, true);
        macos::accept_mouse_moved(&window);
    }
    // Folded until the page says otherwise (`strip_open`).
    app.state::<Arc<Core>>().set_slow_ticks(LABEL, true);
    crate::on_screen(app, LABEL, true);
    let moved = strip.clone();
    macos::watch_pointer(move || moved.nudge());
    strip.look.store(false, Ordering::SeqCst);
    strip.nudge();
    Ok(())
}

fn hide(app: &AppHandle) {
    macos::unwatch_pointer();
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.destroy();
    }
    let strip = state(app);
    strip.covered.store(false, Ordering::SeqCst);
    strip.hovered.store(false, Ordering::SeqCst);
    strip.open.store(false, Ordering::SeqCst);
    *strip.hot.lock().expect("strip lock") = Hot::default();
    let core = app.state::<Arc<Core>>();
    core.set_queue_wanted(LABEL, false);
    core.set_slow_ticks(LABEL, false);
    crate::on_screen(app, LABEL, false);
}

/// The strip never belongs to the active app, so its page hears no hover;
/// look where the pointer is instead, and say when it enters or leaves the
/// drawn part (`strip-hover`). The pointer is read off the main thread
/// (`macos::pointer_location`), so a look costs no hop.
///
/// Looks happen when there is something to see, not on a clock: the pointer
/// moved or the drawn part changed (`StripState::nudge`), at most every
/// `HOVER_NEAR` or `HOVER_FAR`; a Space, app or display changed; or one of
/// the timed checks below is due. In between the thread parks, so a still
/// pointer costs nothing. Parked while off.
fn watch_pointer(app: AppHandle) -> Thread {
    std::thread::spawn(move || {
        let strip = state(&app);
        let mut display_checked: Option<Instant> = None;
        let mut top_checked: Option<Instant> = None;
        let mut settle_until: Option<Instant> = None;
        let mut display_settle_until: Option<Instant> = None;
        let mut fullscreen = false;
        let mut menu_bar_was: Option<f64> = None;
        let mut pointed: Option<(f64, f64)> = None;
        loop {
            if !strip.on.load(Ordering::SeqCst) {
                std::thread::park();
                continue;
            }
            if strip.menu_open.load(Ordering::SeqCst) {
                std::thread::sleep(HOVER_FAR);
                continue;
            }
            let moved = strip.look.swap(false, Ordering::SeqCst);
            if strip.screens_changed.swap(false, Ordering::SeqCst) {
                settle_until = Some(Instant::now() + SETTLE);
                top_checked = None;
            }
            if strip.displays_changed.swap(false, Ordering::SeqCst) {
                display_settle_until = Some(Instant::now() + SETTLE);
                display_checked = None;
            }
            let settling = settle_until.is_some_and(|until| Instant::now() < until);
            let display_settling = display_settle_until.is_some_and(|until| Instant::now() < until);
            let frame = *strip.frame.lock().expect("strip lock");
            let pointer = macos::pointer_location();
            let near_top = pointer.is_some_and(|(_, y)| y - frame.y < NEAR_TOP);
            // In fullscreen the menu bar comes down when the pointer reaches
            // the top, and the strip comes with it; look often then, and while
            // it is down, so the strip leaves with it too.
            let top_every = |fullscreen: bool| {
                if fullscreen && (near_top || !strip.covered.load(Ordering::SeqCst)) {
                    MENU_BAR_CHECK
                } else if settling {
                    SETTLE_CHECK
                } else {
                    FALLBACK_CHECK
                }
            };
            if top_checked.is_none_or(|at| at.elapsed() >= top_every(fullscreen)) {
                top_checked = Some(Instant::now());
                let top = macos::top_of_main_display();
                fullscreen = top.fullscreen;
                // Moving down into the open sheet makes macOS put its menu bar
                // away; the strip stays while its sheet is in use.
                let in_use = strip.open.load(Ordering::SeqCst) && strip.hovered.load(Ordering::SeqCst);
                // Down, or on its way down: come with it. On its way up: go
                // with it, rather than after it has gone.
                let menu_bar = match (top.menu_bar, menu_bar_was) {
                    (None, _) => false,
                    (Some(y), _) if y >= -0.5 => true,
                    (Some(y), Some(was)) => y > was,
                    (Some(_), None) => true,
                };
                menu_bar_was = top.menu_bar;
                let covered = top.fullscreen && !menu_bar && !in_use;
                if strip.covered.swap(covered, Ordering::SeqCst) != covered {
                    let handle = app.clone();
                    let _ = app.run_on_main_thread(move || set_covered(&handle, covered));
                }
            }
            let display_every = if display_settling { SETTLE_CHECK } else { FALLBACK_CHECK };
            if display_checked.is_none_or(|at| at.elapsed() >= display_every) {
                display_checked = Some(Instant::now());
                let handle = app.clone();
                let _ = app.run_on_main_thread(move || follow_display(&handle));
            }
            let mut near = near_top;
            if strip.covered.load(Ordering::SeqCst) {
                pointed = None;
            } else {
                let hot = *strip.hot.lock().expect("strip lock");
                let at = pointer.map(|(x, y)| (x - frame.x, y - frame.y));
                let inside = at.is_some_and(|at| hot.contains(at));
                if strip.hovered.swap(inside, Ordering::SeqCst) != inside {
                    if let Some(window) = app.get_webview_window(LABEL) {
                        // Take clicks only while the pointer is on the open
                        // sheet, so the menu bar beside it never loses one to
                        // the fold delay.
                        macos::set_ignores_mouse(&window, !(inside && strip.open.load(Ordering::SeqCst)));
                    }
                    let _ = app.emit_to(LABEL, "strip-hover", inside);
                }
                // Over the open sheet, say where: WebKit's own hover reaches a
                // never-active app's page late, so the page highlights from this.
                let over = at.filter(|_| inside && strip.open.load(Ordering::SeqCst));
                if over != pointed {
                    pointed = over;
                    let (x, y) = over.unwrap_or((-1.0, -1.0));
                    let _ = app.emit_to(LABEL, "strip-pointer", serde_json::json!({ "x": x, "y": y }));
                }
                near = at.is_some_and(|(_, y)| y < WINDOW.1 + 40.0);
            }
            // A moving pointer is looked at no more often than this.
            if moved {
                std::thread::sleep(if near { HOVER_NEAR } else { HOVER_FAR });
            }
            // Then sleep until it moves again or a timed check is due.
            let due = [
                top_checked.map(|at| at + top_every(fullscreen)),
                display_checked.map(|at| at + display_every),
            ]
            .into_iter()
            .flatten()
            .min();
            let wait = due.map_or(Duration::ZERO, |due| due.saturating_duration_since(Instant::now()));
            if !wait.is_zero() && !strip.look.load(Ordering::SeqCst) {
                std::thread::park_timeout(wait);
            }
        }
    })
    .thread()
    .clone()
}

/// Main thread. Step aside for a fullscreen app, and come back with its menu
/// bar or after it. The setting stays on, and so does the window: the page
/// slides the bar up out of sight and back down (`strip-away`), the way the
/// menu bar itself moves. Hiding and showing the window instead drew a stale
/// frame on the way back, a visible twitch.
fn set_covered(app: &AppHandle, covered: bool) {
    let Some(window) = app.get_webview_window(LABEL) else {
        return;
    };
    let strip = state(app);
    // The setting may have changed, or the state flipped back, since the
    // watcher asked; act on what holds now.
    if strip.covered.load(Ordering::SeqCst) != covered {
        return;
    }
    if covered {
        strip.hovered.store(false, Ordering::SeqCst);
        let _ = app.emit_to(LABEL, "strip-hover", false);
        macos::set_ignores_mouse(&window, true);
    }
    let _ = app.emit_to(LABEL, "strip-away", covered);
    crate::on_screen(app, LABEL, !covered);
}

/// Main thread. Re-centre and re-shape after a display change.
fn follow_display(app: &AppHandle) {
    let Some(window) = app.get_webview_window(LABEL) else {
        return;
    };
    let strip = state(app);
    let shape = measure();
    let changed = {
        let mut held = strip.shape.lock().expect("strip lock");
        let changed = *held != shape;
        *held = shape.clone();
        changed
    };
    if changed {
        let _ = app.emit_to(LABEL, "strip-shape", &shape);
    }
    let frame = frame(app);
    let moved = {
        let mut held = strip.frame.lock().expect("strip lock");
        let moved = *held != frame;
        *held = frame;
        moved
    };
    if moved {
        macos::set_frame(&window, frame.x, frame.y, frame.width, WINDOW.1);
    }
}

#[tauri::command]
pub fn strip_shape(app: AppHandle) -> Shape {
    state(&app).shape.lock().expect("strip lock").clone()
}

/// The page's drawn part changed: the bar folded, or the sheet open.
#[tauri::command]
pub fn strip_hot(app: AppHandle, hot: Hot) {
    let strip = state(&app);
    *strip.hot.lock().expect("strip lock") = hot;
    // The pointer may be still over what just became drawn.
    strip.nudge();
}

/// The sheet opened or folded. Open, it takes clicks while the pointer is on
/// it, and shows the tracks either side of the playing one, so it wants the
/// queue.
#[tauri::command]
pub fn strip_open(app: AppHandle, open: bool) {
    let strip = state(&app);
    strip.open.store(open, Ordering::SeqCst);
    if let Some(window) = app.get_webview_window(LABEL) {
        let hovered = strip.hovered.load(Ordering::SeqCst);
        macos::set_ignores_mouse(&window, !(open && (hovered || strip.shape.lock().expect("strip lock").pinned)));
    }
    let core = app.state::<Arc<Core>>();
    core.want_queue(LABEL, open);
    // Folded, only the hairline shows the position, every `HAIRLINE_MS`.
    core.set_slow_ticks(LABEL, !open);
    strip.nudge();
}

/// Right-click on the open sheet: the strip's menu, under the pointer. The
/// popup is modal; the watcher stands down until it closes, then the next
/// look folds the sheet if the pointer has gone.
#[tauri::command]
pub fn strip_menu(app: AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(window) = handle.get_webview_window(LABEL) else {
            return;
        };
        let Ok(menu) = crate::tray::build_strip_menu(&handle) else {
            return;
        };
        let strip = state(&handle);
        strip.menu_open.store(true, Ordering::SeqCst);
        let _ = window.popup_menu(&menu);
        strip.menu_open.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::valid_ink;

    #[test]
    fn inks_are_auto_cover_or_a_hex_colour() {
        for ink in ["auto", "cover", "#ffffff", "#1A2b3C"] {
            assert!(valid_ink(ink), "{ink}");
        }
        for ink in ["", "white", "#fff", "#gggggg", "ffffff", "#1234567", "#ééé"] {
            assert!(!valid_ink(ink), "{ink}");
        }
    }
}
