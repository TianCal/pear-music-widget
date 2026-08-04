// No console window on Windows release builds — irrelevant on macOS, but the
// attribute costs nothing and keeps the crate portable if it ever moves.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod commands;
mod doctor;
mod lyrics;
mod macos;
mod panel;
mod search;
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

/// Quit for real. Windows hide rather than close, so something has to say when
/// "hide" stops being the right answer.
pub fn quit(app: &AppHandle) {
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
            if snapshot.status != "connected" || snapshot.song.is_none() {
                continue;
            }
            if let Ok(Some(value)) = like.api.like_state().await {
                let state = value
                    .get("state")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                like.update(|player| player.like = state);
            }
        }
    });

    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(PLAY_STATE_POLL);
        loop {
            ticker.tick().await;
            if core.status() != "connected" {
                continue;
            }
            if let Ok(Some(song)) = core.api.song().await {
                if let Some(paused) = song.get("isPaused").and_then(Value::as_bool) {
                    core.update(|player| player.is_playing = !paused);
                }
            }
        }
    });
}

fn on_window_event(app: &AppHandle, label: &str, event: &WindowEvent) {
    match event {
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
    let core = Arc::new(Core::new(
        handle.clone(),
        Arc::clone(&store),
        Arc::clone(&api),
    ));
    let realtime = Arc::new(Realtime::new());

    app.manage(Arc::clone(&store));
    app.manage(Arc::clone(&api));
    app.manage(Arc::clone(&core));
    app.manage(Arc::clone(&realtime));
    app.manage(Arc::new(WindowState::new()));
    app.manage(Arc::new(panel::PanelState::new()));

    window::create(&handle)?;
    panel::create(&handle)?;
    tray::create(&handle)?;

    ws::spawn(Arc::clone(&core), realtime);
    spawn_pollers(core);

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

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // A second launch should surface the widget, not start a second one.
            if let Some(window) = app.get_webview_window(WIDGET) {
                let _ = window.show();
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
            commands::set_panel,
            commands::context_menu,
            commands::set_skin,
            commands::set_panel_skin,
            commands::hide_panel,
            commands::retry,
            commands::quit,
        ])
        .build(tauri::generate_context!())
        .expect("failed to start Pear Music Widget");

    app.run(|app, event| {
        if let RunEvent::ExitRequested { .. } = event {
            if let Some(state) = app.try_state::<Arc<WindowState>>() {
                state.begin_quit();
            }
        }
    });
}
