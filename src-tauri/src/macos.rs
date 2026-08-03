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

/// Above normal windows but below menus — a floating widget.
pub const LEVEL_FLOATING: isize = 3;
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

/// Follow the user onto every Space, and stay visible over a fullscreen app
/// rather than being hidden with the desktop.
pub fn join_all_spaces(window: &WebviewWindow) {
    with_ns_window(window, |handle| unsafe {
        let behavior: usize = CAN_JOIN_ALL_SPACES | FULL_SCREEN_AUXILIARY;
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
