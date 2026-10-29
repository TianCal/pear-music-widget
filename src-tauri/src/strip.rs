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
use std::sync::{Arc, Mutex};
use std::thread::Thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::macos;
use crate::state::Core;
use crate::store::Store;

pub const LABEL: &str = "strip";

/// `NSStatusWindowLevel`: over the menu bar (24), under the menus it opens.
const LEVEL_STATUS: isize = 25;

/// Room for the widest bar (a long title either side of a notch) and for the
/// open sheet with its shadow, so the window never has to grow.
const WINDOW: (f64, f64) = (840.0, 300.0);

/// How often the pointer is checked while it is near the top of the screen,
/// and while it is further away. Off, the watcher parks.
const HOVER_NEAR: Duration = Duration::from_millis(40);
const HOVER_FAR: Duration = Duration::from_millis(150);

/// How often to look for a display change: a dock, an unplug, a new
/// resolution, which move the menu bar or its notch.
const DISPLAY_CHECK: Duration = Duration::from_secs(2);

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
    watcher: Mutex<Option<Thread>>,
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
        watcher: Mutex::new(None),
    }));
    let watcher = watch_pointer(app.clone());
    *state(app).watcher.lock().expect("strip lock") = Some(watcher);
    if app.state::<Arc<Store>>().get(|s| s.menu_bar_strip) {
        show(app)?;
    }
    Ok(())
}

/// From the tray menu, on the main thread.
pub fn set_shown(app: &AppHandle, shown: bool) {
    app.state::<Arc<Store>>().update(|s| s.menu_bar_strip = shown);
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
    crate::on_screen(app, LABEL, true);
    if let Some(watcher) = strip.watcher.lock().expect("strip lock").as_ref() {
        watcher.unpark();
    }
    Ok(())
}

fn hide(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.destroy();
    }
    let strip = state(app);
    strip.hovered.store(false, Ordering::SeqCst);
    strip.open.store(false, Ordering::SeqCst);
    *strip.hot.lock().expect("strip lock") = Hot::default();
    let core = app.state::<Arc<Core>>();
    core.set_queue_wanted(LABEL, false);
    crate::on_screen(app, LABEL, false);
}

/// The strip never belongs to the active app, so its page hears no hover;
/// look where the pointer is instead, and say when it enters or leaves the
/// drawn part (`strip-hover`). The pointer is read off the main thread
/// (`macos::pointer_location`), so a look costs no hop. Parked while off.
fn watch_pointer(app: AppHandle) -> Thread {
    std::thread::spawn(move || {
        let mut checked = Instant::now();
        let mut pointed: Option<(f64, f64)> = None;
        loop {
            if !app.state::<Arc<Store>>().get(|s| s.menu_bar_strip) {
                std::thread::park();
                continue;
            }
            let strip = state(&app);
            if strip.menu_open.load(Ordering::SeqCst) {
                std::thread::sleep(HOVER_FAR);
                continue;
            }
            let frame = *strip.frame.lock().expect("strip lock");
            let hot = *strip.hot.lock().expect("strip lock");
            let at = macos::pointer_location().map(|(x, y)| (x - frame.x, y - frame.y));
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
            if checked.elapsed() >= DISPLAY_CHECK {
                checked = Instant::now();
                let handle = app.clone();
                let _ = app.run_on_main_thread(move || follow_display(&handle));
            }
            let near = at.is_some_and(|(_, y)| y < WINDOW.1 + 40.0);
            std::thread::sleep(if near { HOVER_NEAR } else { HOVER_FAR });
        }
    })
    .thread()
    .clone()
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
    *state(&app).hot.lock().expect("strip lock") = hot;
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
    app.state::<Arc<Core>>().want_queue(LABEL, open);
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
