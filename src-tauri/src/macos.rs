//! The AppKit calls Tauri does not wrap.
//!
//! Electron gave us `setAspectRatio`, `setOpacity` and a window-level argument
//! to `setAlwaysOnTop` for free. Tauri does not, so these go straight to
//! NSWindow.
//!
//! **Every call here is posted to the event loop rather than made inline**, and
//! that is load-bearing. Tauri's own window setters (`set_size`, `set_position`,
//! `set_min_size`) are messages on that same queue, so a direct `msg_send!`
//! would execute *before* a resize that was requested earlier. Re-applying an
//! aspect ratio ahead of the resize it was meant to follow leaves the window at
//! a size that matches neither skin — which is precisely what it did.
//!
//! Posting keeps everything in FIFO order with the resize, at the cost of these
//! being fire-and-forget. Nothing here needs a return value.

use objc2::msg_send;
use objc2::runtime::AnyObject;
use objc2_foundation::NSSize;
use tauri::WebviewWindow;

/// Above everything, including other apps' floating windows: a dropdown from
/// the menu bar has to sit over whatever it drops onto.
pub const LEVEL_POPUP_MENU: isize = 101;

const CAN_JOIN_ALL_SPACES: usize = 1 << 0;
const FULL_SCREEN_AUXILIARY: usize = 1 << 8;

/// Run `edit` against the NSWindow, on the main thread, behind anything already
/// queued for this window. A no-op once the window is gone.
fn with_ns_window(window: &WebviewWindow, edit: impl FnOnce(*mut AnyObject) + Send + 'static) {
    let window = window.clone();
    let _ = window.clone().run_on_main_thread(move || {
        if let Ok(handle) = window.ns_window() {
            edit(handle as *mut AnyObject);
        }
    });
}

/// Lock the window to a skin's aspect ratio, or release the lock.
///
/// Each skin has its own ratio, so the lock must be released before a resize
/// and re-applied afterwards, or the next drag snaps the window back to the old
/// shape. AppKit has no "clear" call: setting the resize increments is the
/// documented way to drop an aspect ratio, since the two are mutually exclusive.
pub fn set_aspect_ratio(window: &WebviewWindow, ratio: Option<(f64, f64)>) {
    with_ns_window(window, move |handle| unsafe {
        match ratio {
            Some((width, height)) => {
                let _: () = msg_send![handle, setContentAspectRatio: NSSize::new(width, height)];
            }
            None => {
                let _: () = msg_send![handle, setContentResizeIncrements: NSSize::new(1.0, 1.0)];
            }
        }
    });
}

/// Deliver `mouseMoved` to the page while this window is key.
///
/// Off by default. It only covers the case where the widget *has* been clicked:
/// AppKit routes `mouseMoved` to the key window alone, so a widget sitting in
/// the background still sees none. What carries that case is hover — tracking
/// areas fire `mouseover` in a background window — which is why the renderer
/// wakes the corner buttons on both.
pub fn accept_mouse_moved(window: &WebviewWindow) {
    with_ns_window(window, move |handle| unsafe {
        let _: () = msg_send![handle, setAcceptsMouseMovedEvents: true];
    });
}

pub fn set_alpha(window: &WebviewWindow, alpha: f64) {
    let alpha = alpha.clamp(0.0, 1.0);
    with_ns_window(window, move |handle| unsafe {
        let _: () = msg_send![handle, setAlphaValue: alpha];
    });
}

pub fn set_level(window: &WebviewWindow, level: isize) {
    with_ns_window(window, move |handle| unsafe {
        let _: () = msg_send![handle, setLevel: level];
    });
}

/// Whether the window follows the user onto every Space and shows over a
/// fullscreen app, or behaves like an ordinary window that belongs to one Space.
///
/// `FULL_SCREEN_AUXILIARY` is exactly the flag that lets a window sit over
/// another app's fullscreen window, and `CAN_JOIN_ALL_SPACES` puts it on the
/// fullscreen Space in the first place — so both have to go when the widget is
/// meant to be ordinary. Setting the level alone was not enough: with these
/// still set, a widget with "always on top" off stayed out of the way on the
/// desktop but appeared over a fullscreen app on another display.
pub fn follow_everywhere(window: &WebviewWindow, follow: bool) {
    with_ns_window(window, move |handle| unsafe {
        let behavior: usize = if follow {
            CAN_JOIN_ALL_SPACES | FULL_SCREEN_AUXILIARY
        } else {
            0 // NSWindowCollectionBehaviorDefault
        };
        let _: () = msg_send![handle, setCollectionBehavior: behavior];
    });
}

/// A transparent window gets its shadow from whatever opaque content it draws —
/// here, the vibrancy layer.
pub fn set_has_shadow(window: &WebviewWindow, shadow: bool) {
    with_ns_window(window, move |handle| unsafe {
        let _: () = msg_send![handle, setHasShadow: shadow];
    });
}
