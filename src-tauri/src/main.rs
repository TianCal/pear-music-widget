// No console window on Windows release builds — irrelevant on macOS, but the
// attribute costs nothing and keeps the crate portable if it ever moves.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod artwork;
mod commands;
mod doctor;
mod jyutping;
mod likes;
mod local;
mod lyrics;
mod lyrics_cache;
mod macos;
mod panel;
mod search;
mod sorter;
mod state;
mod store;
mod tray;
mod window;
mod ws;

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Manager, RunEvent, WindowEvent};
use tauri_plugin_autostart::MacosLauncher;

use crate::api::Api;
use crate::state::Core;
use crate::store::Store;
use crate::window::{WindowState, PANEL, WIDGET};
use crate::ws::Realtime;

/// Like state has no push channel, so poll it while we are connected.
const LIKE_POLL: Duration = Duration::from_secs(20);
/// PLAYER_STATE_CHANGED only fires on the player's own play/pause events, so a
/// missed one would leave the wrong glyph up indefinitely. Reconcile against the
/// player often enough that it is never wrong for long.
const PLAY_STATE_POLL: Duration = Duration::from_secs(5);

/// Whether anything is actually on screen to be wrong.
///
/// Both pollers exist to correct a display, so neither has any work to do while
/// both surfaces are hidden — which, for a menu-bar app, is nearly all the
/// time. Skipping the tick costs nothing in accuracy because showing either
/// surface resyncs it first; see `resync`.
///
/// Read from `Core`'s own record rather than asking each window: `is_visible`
/// is an AppKit round trip, and the pollers ask on every tick.
fn showing_anything(app: &AppHandle) -> bool {
    app.try_state::<Arc<Core>>().is_some_and(|core| core.showing())
}

/// Record that the surface `label` was just shown or hidden. Every `show` and
/// `hide` of the widget or the dropdown goes through here as well.
pub fn on_screen(app: &AppHandle, label: &str, shown: bool) {
    if let Some(core) = app.try_state::<Arc<Core>>() {
        core.set_on_screen(label, shown);
    }
}

/// Pull the player's ground truth now. Called whenever a surface comes back on
/// screen, since the pollers stand down while nothing is showing.
pub fn resync(app: &AppHandle) {
    let Some(core) = app.try_state::<Arc<Core>>() else {
        return;
    };
    let core = core.inner().clone();
    if core.status() != "connected" || core.is_local() {
        return;
    }
    tauri::async_runtime::spawn(async move { core.refresh_all().await });
}

/// Quit for real. Windows hide rather than close, so something has to say when
/// "hide" stops being the right answer.
pub fn quit(app: &AppHandle) {
    // Where the folder was left, so the next launch picks it back up.
    if let Some(core) = app.try_state::<Arc<Core>>() {
        core.local().remember(&core);
    }
    if let Some(state) = app.try_state::<Arc<WindowState>>() {
        state.begin_quit();
    }
    app.exit(0);
}

/// Pressing play when YouTube Music is not up should start it. Resetting the
/// backoff means the widget picks the app up as soon as it is listening, rather
/// than after however long the last failure had backed off to.
///
/// Only when there is something to wait for, though. A retry tears the socket
/// down and comes back through `offline`, which drops the queue — so raising an
/// app that is already running and connected, as double-clicking the artwork
/// does, would clear "Next tracks" for no reason.
pub fn launch_music_app(app: &AppHandle) {
    tray::open_music_app();

    let connected = app
        .try_state::<Arc<Core>>()
        .map(|core| core.status() == "connected")
        .unwrap_or(false);
    if !connected {
        ws::retry(app);
    }
}

fn spawn_pollers(core: Arc<Core>) {
    let like = Arc::clone(&core);
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(LIKE_POLL);
        loop {
            ticker.tick().await;
            let snapshot = like.snapshot();
            if snapshot.status != "connected"
                || like.is_local()
                || snapshot.song.is_none()
                || !showing_anything(&like.app)
            {
                continue;
            }
            if let Ok(Some(value)) = like.api.like_state().await {
                let state = value
                    .get("state")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                like.update_remote(|player| player.like = state);
            }
        }
    });

    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(PLAY_STATE_POLL);
        loop {
            ticker.tick().await;
            if core.status() != "connected" || core.is_local() || !showing_anything(&core.app) {
                continue;
            }
            if let Ok(Some(song)) = core.api.song().await {
                if let Some(paused) = song.get("isPaused").and_then(Value::as_bool) {
                    core.update_remote(|player| player.is_playing = !paused);
                }
            }
        }
    });
}

fn on_window_event(app: &AppHandle, label: &str, event: &WindowEvent) {
    match event {
        // Sort Songs is an ordinary window: closing it destroys it, and the
        // runtime behind it goes too, so it costs nothing while closed.
        WindowEvent::CloseRequested { .. } if label == sorter::LABEL => {}
        WindowEvent::Destroyed if label == sorter::LABEL => sorter::closed(app),

        // A widget should hide, not disappear: closing would leave the tray menu
        // holding a destroyed window and the app running with nothing to show.
        WindowEvent::CloseRequested { api, .. } => {
            let quitting = app
                .try_state::<Arc<WindowState>>()
                .map(|state| state.is_quitting())
                .unwrap_or(false);
            if quitting {
                return;
            }
            api.prevent_close();
            if let Some(window) = app.get_webview_window(label) {
                let _ = window.hide();
                on_screen(app, label, false);
            }
        }

        // A dropdown that stays up after you click elsewhere is not a dropdown.
        WindowEvent::Focused(false) if label == PANEL => {
            if let Some(window) = app.get_webview_window(PANEL) {
                panel::on_blur(app, &window);
            }
        }

        // Both carry the same question — did the user do this, or did we? —
        // so both go through one place that can tell.
        WindowEvent::Moved(_) | WindowEvent::Resized(_) if label == WIDGET => {
            if let Some(window) = app.get_webview_window(WIDGET) {
                window::on_geometry_changed(app, &window);
            }
        }

        _ => {}
    }
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    // An accessory app: no Dock icon, no menu bar.
    app.set_activation_policy(tauri::ActivationPolicy::Accessory);

    let handle = app.handle().clone();
    let store = Arc::new(Store::load());
    let api = Arc::new(Api::new(Arc::clone(&store)));
    let jyutping = Arc::new(jyutping::Jyutping::load(&handle));
    let core = Arc::new(Core::new(
        handle.clone(),
        Arc::clone(&store),
        Arc::clone(&api),
        jyutping,
    ));
    let realtime = Arc::new(Realtime::new());

    app.manage(Arc::clone(&store));
    app.manage(Arc::clone(&api));
    app.manage(Arc::clone(&core));
    app.manage(Arc::clone(&realtime));
    app.manage(Arc::new(WindowState::new()));
    app.manage(Arc::new(panel::PanelState::new()));
    app.manage(Arc::new(local::Local::new()));

    window::create(&handle)?;
    panel::create(&handle)?;
    tray::create(&handle)?;

    ws::spawn(Arc::clone(&core), realtime);
    local::Local::spawn_ticker(Arc::clone(&core));
    local::Local::restore(&core);
    spawn_pollers(core);

    // Off the setup path: taking stock of the cache means a directory listing,
    // and it may evict — neither belongs in front of the first paint.
    let cap = store.get(|s| s.lyrics_cache_mb);
    tauri::async_runtime::spawn_blocking(move || lyrics_cache::configure(cap));

    Ok(())
}

fn main() {
    // `--doctor` short-circuits the whole GUI: it is a connectivity check, not
    // a mode of the app.
    if std::env::args().any(|arg| arg == "--doctor") {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        std::process::exit(runtime.block_on(doctor::run()));
    }

    // Tauri's default runtime sizes itself to the machine — ten worker threads
    // on this one — for a workload that is one websocket, a poller and the
    // occasional fetch. Two is more than the app ever has in flight, and the
    // rest were pure stack and scheduler overhead. Set before anything can
    // spawn onto the default.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    tauri::async_runtime::set(runtime.handle().clone());

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // A second launch should surface the widget, not start a second one.
            if let Some(window) = app.get_webview_window(WIDGET) {
                let _ = window.show();
                on_screen(app, WIDGET, true);
                resync(app);
            }
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(setup)
        .on_menu_event(tray::handle_menu)
        .on_window_event(|window, event| {
            let app = window.app_handle().clone();
            on_window_event(&app, window.label(), event);
        })
        .invoke_handler(tauri::generate_handler![
            commands::widget_state,
            commands::command,
            commands::open_app,
            commands::search_tracks,
            commands::play_result,
            commands::play_queued,
            commands::play_queue_index,
            commands::queue_art,
            commands::set_panel,
            commands::context_menu,
            commands::hide_widget,
            commands::hide_panel,
            commands::retry,
            commands::quit,
            sorter::sorter_view,
            sorter::sorter_extras,
            sorter::sorter_choose_folder,
            sorter::sorter_rate,
            sorter::sorter_skip,
            sorter::sorter_undo,
            sorter::sorter_toggle_play,
            sorter::sorter_seek,
            sorter::sorter_replay,
            sorter::sorter_disliked,
            sorter::sorter_trash,
        ])
        .build(tauri::generate_context!())
        .expect("failed to start Perch Player");

    app.run(|app, event| {
        if let RunEvent::ExitRequested { .. } = event {
            // Cmd-Q and logout come this way rather than through `quit`.
            if let Some(core) = app.try_state::<Arc<Core>>() {
                core.local().remember(&core);
            }
            if let Some(state) = app.try_state::<Arc<WindowState>>() {
                state.begin_quit();
            }
        }
    });
}
