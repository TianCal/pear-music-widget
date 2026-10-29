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

use std::ffi::{CStr, CString};

use objc2::runtime::{AnyClass, AnyObject};
use objc2::{class, msg_send};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use tauri::{AppHandle, WebviewWindow};

/// Above everything, including other apps' floating windows: a dropdown from
/// the menu bar has to sit over whatever it drops onto.
pub const LEVEL_POPUP_MENU: isize = 101;

const CAN_JOIN_ALL_SPACES: usize = 1 << 0;
const FULL_SCREEN_AUXILIARY: usize = 1 << 8;
const VIEW_WIDTH_SIZABLE: usize = 1 << 1;
const VIEW_HEIGHT_SIZABLE: usize = 1 << 4;

pub fn supports_liquid_glass() -> bool {
    AnyClass::get(c"NSGlassEffectView").is_some()
}

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

/// How long after a click a second one still counts as a double click, in
/// seconds — the interval set in Mouse settings, which is an accessibility
/// control as much as a preference and so is not ours to hardcode.
///
/// Read rather than posted, unlike everything else in this file: there is a
/// value to return, and `NSEvent` is being asked a question rather than a
/// window being told something, so nothing is queued behind a resize.
///
/// Clamped because the setting reaches 1s at its slowest, and the tray icon
/// cannot afford a window that long — the first click of a pair opens the
/// dropdown, so every unclosed second of it is a second in which an ordinary
/// open-then-close would be read as a double click.
pub fn double_click_interval() -> f64 {
    let interval: f64 = unsafe { msg_send![class!(NSEvent), doubleClickInterval] };
    interval.clamp(0.2, 0.6)
}

/// Whether Option is down right now. `NSEvent.modifierFlags` is the live state,
/// which during a click handler is the state the click was made with.
pub fn option_held() -> bool {
    const OPTION: usize = 1 << 19; // NSEventModifierFlagOption
    let flags: usize = unsafe { msg_send![class!(NSEvent), modifierFlags] };
    flags & OPTION != 0
}

pub fn set_alpha(window: &WebviewWindow, alpha: f64) {
    let alpha = alpha.clamp(0.0, 1.0);
    with_ns_window(window, move |handle| unsafe {
        let _: () = msg_send![handle, setAlphaValue: alpha];
    });
}

/// Ask for an exact widget opacity without turning the menu into a long list of
/// percentages. The alert is dispatched onto AppKit's thread and returns its
/// result through the callback, so menu handling never reaches across threads.
pub fn prompt_opacity(app: &AppHandle, current: f64, apply: impl FnOnce(f64) + Send + 'static) {
    prompt_field(
        app,
        "Custom Widget Opacity".into(),
        "Enter a whole number from 1 to 100%.".into(),
        (current.clamp(0.01, 1.0) * 100.0).round().to_string(),
        "Apply",
        220.0,
        move |text| {
            if let Ok(percent) = text.trim().parse::<u32>() {
                if (1..=100).contains(&percent) {
                    apply(f64::from(percent) / 100.0);
                }
            }
        },
    );
}

pub fn prompt_song_time(app: &AppHandle, current: f64, apply: impl FnOnce(f64) + Send + 'static) {
    prompt_field(
        app,
        "Custom Song Time".into(),
        "Enter seconds from -20 to 20. Positive moves lyrics earlier; negative moves them later. Decimals are allowed.".into(),
        current.to_string(),
        "Apply",
        220.0,
        move |text| {
            if let Ok(seconds) = text.trim().parse::<f64>() {
                if seconds.is_finite() && (-20.0..=20.0).contains(&seconds) { apply(seconds); }
            }
        },
    );
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

/// Use macOS's native Liquid Glass container when the runtime provides it.
///
/// Looking the class up dynamically keeps the existing vibrancy path available
/// on older macOS releases. The renderer's cover-derived ambient colours remain
/// above the native material, so the glass does not need its own tint colour.
pub fn install_liquid_glass(window: &WebviewWindow, corner_radius: f64, enabled: bool) -> bool {
    let Some(glass_class) = AnyClass::get(c"NSGlassEffectView") else {
        return false;
    };

    with_ns_window(window, move |handle| unsafe {
        let content: *mut AnyObject = msg_send![handle, contentView];
        if content.is_null() {
            return;
        }

        let frame: NSRect = msg_send![content, frame];
        let root: *mut AnyObject = msg_send![class!(NSView), alloc];
        let root: *mut AnyObject = msg_send![root, initWithFrame: frame];
        let glass: *mut AnyObject = msg_send![glass_class, alloc];
        let glass: *mut AnyObject = msg_send![glass, initWithFrame: frame];
        let autoresize = VIEW_WIDTH_SIZABLE | VIEW_HEIGHT_SIZABLE;

        // The replacement content view must carry the same rounded shape as
        // the glass. Without a mask AppKit draws the focused-window rim around
        // this root's rectangular bounds, leaving dark L-shaped corner marks
        // outside the rounded material.
        let _: () = msg_send![root, setWantsLayer: true];
        let layer: *mut AnyObject = msg_send![root, layer];
        let _: () = msg_send![layer, setCornerRadius: corner_radius];
        let _: () = msg_send![layer, setMasksToBounds: true];

        // Regular glass keeps substantially more of its tone when the window
        // resigns key. Clear glass swings from almost transparent to a dense
        // inactive fill, which is too large a state change for a persistent
        // desktop widget.
        let _: () = msg_send![glass, setStyle: 0isize];
        let _: () = msg_send![glass, setCornerRadius: corner_radius];
        let _: () = msg_send![glass, setAutoresizingMask: autoresize];
        let _: () = msg_send![glass, setHidden: !enabled];
        let _: () = msg_send![content, retain];
        let _: () = msg_send![content, setFrame: frame];
        let _: () = msg_send![content, setAutoresizingMask: autoresize];
        let _: () = msg_send![root, addSubview: glass];
        let _: () = msg_send![root, addSubview: content];
        let _: () = msg_send![handle, setContentView: root];
        let _: () = msg_send![content, release];
        let _: () = msg_send![glass, release];
        let _: () = msg_send![root, release];
    });
    true
}

/// Showing or hiding the material is constant work and does not reconstruct the
/// WKWebView. The glass view was installed once by `install_liquid_glass` and
/// remains the root container's first subview.
pub fn set_liquid_glass(window: &WebviewWindow, enabled: bool) {
    let Some(glass_class) = AnyClass::get(c"NSGlassEffectView") else {
        return;
    };
    with_ns_window(window, move |handle| unsafe {
        let root: *mut AnyObject = msg_send![handle, contentView];
        let subviews: *mut AnyObject = msg_send![root, subviews];
        let glass: *mut AnyObject = msg_send![subviews, firstObject];
        let is_glass: bool = msg_send![glass, isKindOfClass: glass_class];
        if is_glass {
            let _: () = msg_send![glass, setHidden: !enabled];
        }
    });
}

unsafe fn ns_string(text: &str) -> *mut AnyObject {
    let text = CString::new(text.replace('\0', "")).expect("NUL removed");
    msg_send![class!(NSString), stringWithUTF8String: text.as_ptr()]
}

/// Free-text entry, for names rather than numbers. `apply` gets the trimmed
/// text, possibly empty, only when the first button is chosen.
pub fn prompt_text(
    app: &AppHandle,
    title: String,
    detail: String,
    current: String,
    apply: impl FnOnce(String) + Send + 'static,
) {
    prompt_field(app, title, detail, current, "Save", 260.0, apply);
}

/// An alert with one text field, brought to the front. `apply` gets the
/// trimmed text only when `button` (the first) is chosen; callers validate
/// it, and invalid input leaves the setting unchanged.
fn prompt_field(
    app: &AppHandle,
    title: String,
    detail: String,
    current: String,
    button: &'static str,
    width: f64,
    apply: impl FnOnce(String) + Send + 'static,
) {
    let _ = app.run_on_main_thread(move || unsafe {
        let alert: *mut AnyObject = msg_send![class!(NSAlert), new];
        let field: *mut AnyObject = msg_send![class!(NSTextField), alloc];
        let field: *mut AnyObject = msg_send![field, initWithFrame: NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(width, 24.0),
        )];
        let _: () = msg_send![alert, setMessageText: ns_string(&title)];
        let _: () = msg_send![alert, setInformativeText: ns_string(&detail)];
        let _: *mut AnyObject = msg_send![alert, addButtonWithTitle: ns_string(button)];
        let _: *mut AnyObject = msg_send![alert, addButtonWithTitle: ns_string("Cancel")];
        let _: () = msg_send![field, setStringValue: ns_string(&current)];
        let _: () = msg_send![alert, setAccessoryView: field];
        let _: () = msg_send![field, release];

        activate_app();
        let response: isize = msg_send![alert, runModal];
        let text: *mut AnyObject = msg_send![field, stringValue];
        let utf8: *const std::ffi::c_char = msg_send![text, UTF8String];
        let value = if utf8.is_null() {
            String::new()
        } else {
            CStr::from_ptr(utf8).to_string_lossy().trim().to_string()
        };
        let _: () = msg_send![alert, release];
        // NSAlertFirstButtonReturn.
        if response == 1000 {
            apply(value);
        }
    });
}

/// A destructive yes/no. The confirming button is first so Return is not the
/// safe choice by accident — macOS gives the first button the default key, so
/// it is labelled with the action rather than "OK".
pub fn confirm(
    app: &AppHandle,
    title: String,
    detail: String,
    action: &'static str,
    apply: impl FnOnce() + Send + 'static,
) {
    let _ = app.run_on_main_thread(move || unsafe {
        let alert: *mut AnyObject = msg_send![class!(NSAlert), new];
        // NSAlertStyleCritical: the caution badge over the app icon.
        let _: () = msg_send![alert, setAlertStyle: 2usize];
        let _: () = msg_send![alert, setMessageText: ns_string(&title)];
        let _: () = msg_send![alert, setInformativeText: ns_string(&detail)];
        let _: *mut AnyObject = msg_send![alert, addButtonWithTitle: ns_string(action)];
        let _: *mut AnyObject = msg_send![alert, addButtonWithTitle: ns_string("Cancel")];
        activate_app();
        let response: isize = msg_send![alert, runModal];
        let _: () = msg_send![alert, release];
        if response == 1000 {
            apply();
        }
    });
}

/// An accessory app's alert otherwise opens behind whatever is frontmost.
unsafe fn activate_app() {
    let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
    let _: () = msg_send![app, activateIgnoringOtherApps: true];
}

/// Move a file to the Trash of the volume it is on, recoverably — never a
/// delete. On an external drive that is the drive's own `.Trashes`.
pub fn trash(path: &std::path::Path) -> Result<(), String> {
    unsafe {
        let manager: *mut AnyObject = msg_send![class!(NSFileManager), defaultManager];
        let url: *mut AnyObject =
            msg_send![class!(NSURL), fileURLWithPath: ns_string(&path.to_string_lossy())];
        let mut error: *mut AnyObject = std::ptr::null_mut();
        let ok: bool = msg_send![
            manager,
            trashItemAtURL: url,
            resultingItemURL: std::ptr::null_mut::<*mut AnyObject>(),
            error: &mut error
        ];
        if ok {
            return Ok(());
        }
        if error.is_null() {
            return Err("The file could not be moved to the Trash".into());
        }
        let description: *mut AnyObject = msg_send![error, localizedDescription];
        let utf8: *const std::ffi::c_char = msg_send![description, UTF8String];
        Err(if utf8.is_null() {
            "The file could not be moved to the Trash".into()
        } else {
            CStr::from_ptr(utf8).to_string_lossy().into_owned()
        })
    }
}

/// A one-button notice, for failures the user asked to happen.
pub fn inform(app: &AppHandle, title: String, detail: String) {
    let _ = app.run_on_main_thread(move || unsafe {
        let alert: *mut AnyObject = msg_send![class!(NSAlert), new];
        let _: () = msg_send![alert, setMessageText: ns_string(&title)];
        let _: () = msg_send![alert, setInformativeText: ns_string(&detail)];
        activate_app();
        let _: isize = msg_send![alert, runModal];
        let _: () = msg_send![alert, release];
    });
}

#[cfg(test)]
mod tests {
    /// `PMW_TRASH_DIR=/Volumes/X cargo test trash_moves -- --ignored`
    #[test]
    #[ignore]
    fn trash_moves_a_file_out_of_place() {
        let dir = std::env::var("PMW_TRASH_DIR").unwrap_or_else(|_| std::env::temp_dir().display().to_string());
        let path = std::path::Path::new(&dir).join("pmw-trash-test.txt");
        std::fs::write(&path, b"x").unwrap();
        super::trash(&path).unwrap();
        assert!(!path.exists());
    }
}

/// The menu bar's height and the camera housing's width on the display that
/// has the menu bar, in points; the width is 0 on a display without a notch.
///
/// Read rather than posted, like `double_click_interval`: a question for
/// `NSScreen`, not an edit queued behind a resize. Call on the main thread.
pub fn menu_bar_shape() -> (f64, f64) {
    unsafe {
        let screens: *mut AnyObject = msg_send![class!(NSScreen), screens];
        let count: usize = msg_send![screens, count];
        if count == 0 {
            return (24.0, 0.0);
        }
        let screen: *mut AnyObject = msg_send![screens, objectAtIndex: 0usize];
        let frame: NSRect = msg_send![screen, frame];
        let visible: NSRect = msg_send![screen, visibleFrame];
        let bar = (frame.origin.y + frame.size.height) - (visible.origin.y + visible.size.height);
        // An auto-hiding menu bar leaves no inset; draw at the usual height.
        let bar = if bar >= 20.0 { bar } else { 24.0 };

        // The two strips either side of the notch, macOS 12 and later.
        let responds: bool =
            msg_send![screen, respondsToSelector: objc2::sel!(auxiliaryTopLeftArea)];
        if !responds {
            return (bar, 0.0);
        }
        let left: NSRect = msg_send![screen, auxiliaryTopLeftArea];
        let right: NSRect = msg_send![screen, auxiliaryTopRightArea];
        if left.size.width <= 0.0 || right.size.width <= 0.0 {
            return (bar, 0.0);
        }
        (bar, (frame.size.width - left.size.width - right.size.width).max(0.0))
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreate(source: *const std::ffi::c_void) -> *mut std::ffi::c_void;
    fn CGEventGetLocation(event: *mut std::ffi::c_void) -> NSPoint;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> *mut AnyObject;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> NSRect;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: *const std::ffi::c_void);
}

/// Where the pointer is, in points from the top-left of the primary display:
/// the same space as Tauri's logical positions.
///
/// Read through Quartz rather than `NSEvent`, so any thread can ask and a
/// poller needs no main-thread hop. For a window in an app that is never
/// active: WebKit sends hover only to the active app.
pub fn pointer_location() -> Option<(f64, f64)> {
    unsafe {
        let event = CGEventCreate(std::ptr::null());
        if event.is_null() {
            return None;
        }
        let at = CGEventGetLocation(event);
        CFRelease(event);
        Some((at.x, at.y))
    }
}

/// On every desktop Space, but not over another app's fullscreen window.
pub fn join_all_desktops(window: &WebviewWindow) {
    with_ns_window(window, move |handle| unsafe {
        let _: () = msg_send![handle, setCollectionBehavior: CAN_JOIN_ALL_SPACES];
    });
}

/// Let clicks fall through to whatever is underneath, or take them.
pub fn set_ignores_mouse(window: &WebviewWindow, ignore: bool) {
    with_ns_window(window, move |handle| unsafe {
        let _: () = msg_send![handle, setIgnoresMouseEvents: ignore];
    });
}

/// Move and size a window in one AppKit call, queued like everything else
/// here. `x` and `y` are top-left, in the logical coordinates Tauri uses,
/// whose origin is the top-left of the primary display.
pub fn set_frame(window: &WebviewWindow, x: f64, y: f64, width: f64, height: f64) {
    with_ns_window(window, move |handle| unsafe {
        let screens: *mut AnyObject = msg_send![class!(NSScreen), screens];
        let count: usize = msg_send![screens, count];
        if count == 0 {
            return;
        }
        let primary: *mut AnyObject = msg_send![screens, objectAtIndex: 0usize];
        let screen: NSRect = msg_send![primary, frame];
        // AppKit counts up from the primary display's bottom edge.
        let frame = NSRect::new(
            NSPoint::new(x, screen.size.height - y - height),
            NSSize::new(width, height),
        );
        let _: () = msg_send![handle, setFrame: frame, display: true];
    });
}

/// What sits at the top of the menu-bar display.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TopOfDisplay {
    /// A fullscreen Space is showing: another app's ordinary window reaches
    /// the display's top edge, where in a normal Space only the menu bar can
    /// be. That holds for one app alone, for Split View, where each app takes
    /// part of the width (Chrome measured at 1200 of 1920 points), and for a
    /// borderless full-screen game. (`visibleFrame` is no help: it kept the
    /// menu bar's height in fullscreen.)
    pub fullscreen: bool,
    /// Where the menu bar's own window (the main menu level, 24) is, as the
    /// top of a window lined up with this display: 0 when it is down, below
    /// zero while it slides, `None` when it is off screen. In fullscreen it is
    /// away until the pointer reaches the top, then slides in (measured: y
    /// -136, -13, 0) and back out.
    pub menu_bar: Option<f64>,
}

/// The dictionary keys `top_of_main_display` reads, as retained `NSString`s
/// built once: it runs every second, or every 100 ms in fullscreen, and was
/// making the eight strings each time. Kept as addresses because a raw
/// pointer cannot sit in a static; an immutable string is safe to share.
fn window_keys() -> [usize; 8] {
    static KEYS: std::sync::OnceLock<[usize; 8]> = std::sync::OnceLock::new();
    *KEYS.get_or_init(|| {
        [
            c"kCGWindowLayer",
            c"kCGWindowOwnerPID",
            c"kCGWindowAlpha",
            c"kCGWindowBounds",
            c"X",
            c"Y",
            c"Width",
            c"Height",
        ]
        .map(|name: &CStr| unsafe {
            let string: *mut AnyObject = msg_send![class!(NSString), alloc];
            let string: *mut AnyObject = msg_send![string, initWithUTF8String: name.as_ptr()];
            string as usize
        })
    })
}

/// Read through Quartz's window list, which is safe off the main thread; the
/// array it returns is toll-free bridged to `NSArray`, so it is read with
/// plain messages.
pub fn top_of_main_display() -> TopOfDisplay {
    const MAIN_MENU_LEVEL: f64 = 24.0;
    const ON_SCREEN_ONLY: u32 = 1 << 0;
    const EXCLUDE_DESKTOP: u32 = 1 << 4;
    let ours = std::process::id() as i64;
    objc2::rc::autoreleasepool(|_| unsafe {
        let screen = CGDisplayBounds(CGMainDisplayID());
        let list = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
        if list.is_null() {
            return TopOfDisplay::default();
        }
        let [layer_key, owner_key, alpha_key, bounds_key, x_key, y_key, w_key, h_key] =
            window_keys().map(|key| key as *mut AnyObject);
        let number = |dict: *mut AnyObject, k: *mut AnyObject| -> Option<f64> {
            let value: *mut AnyObject = msg_send![dict, objectForKey: k];
            (!value.is_null()).then(|| msg_send![value, doubleValue])
        };

        let count: usize = msg_send![list, count];
        let mut top = TopOfDisplay::default();
        for i in 0..count {
            let window: *mut AnyObject = msg_send![list, objectAtIndex: i];
            let layer = number(window, layer_key);
            // Ordinary app windows and the menu bar; the Dock and overlays
            // live on other layers.
            if (layer != Some(0.0) && layer != Some(MAIN_MENU_LEVEL))
                || number(window, owner_key).map(|pid| pid as i64) == Some(ours)
                || number(window, alpha_key).unwrap_or(1.0) <= 0.0
            {
                continue;
            }
            let bounds: *mut AnyObject = msg_send![window, objectForKey: bounds_key];
            if bounds.is_null() {
                continue;
            }
            let (Some(x), Some(y), Some(w), Some(h)) = (
                number(bounds, x_key),
                number(bounds, y_key),
                number(bounds, w_key),
                number(bounds, h_key),
            ) else {
                continue;
            };
            let at_top = (y - screen.origin.y).abs() < 1.0;
            if layer == Some(MAIN_MENU_LEVEL) {
                // Lined up with this display, not moving sideways with a
                // Space switch; how far down it has come is the answer.
                if (x - screen.origin.x).abs() < 1.0 && (w - screen.size.width).abs() < 1.0 && y <= screen.origin.y + 1.0 {
                    top.menu_bar = Some(y - screen.origin.y);
                }
                continue;
            }
            // At the top edge, over this display, and more than a sliver
            // (fullscreen apps split their chrome into thin strips too).
            let over = x < screen.origin.x + screen.size.width && x + w > screen.origin.x;
            if at_top && over && w >= 200.0 && h >= 100.0 {
                top.fullscreen = true;
            }
        }
        CFRelease(list as *const std::ffi::c_void);
        top
    })
}
