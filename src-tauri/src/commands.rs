//! Everything the renderer can ask for. The shapes here are the shapes
//! `src/bridge.js` hands to `app.js`, which was written against the Electron
//! preload and is unchanged.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager, WebviewWindow};

use crate::search;
use crate::state::{Core, PlayerState};
use crate::window::{self, PANEL};

/// Polling budget for a queued track to show up (see `play_result`).
const PLAY_POLL_ATTEMPTS: usize = 20;
const PLAY_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Pressing play when YouTube Music is not up should start it, not fail silently.
const PLAYBACK_COMMANDS: [&str; 3] = ["togglePlay", "next", "previous"];

fn core(app: &AppHandle) -> Arc<Core> {
    app.state::<Arc<Core>>().inner().clone()
}

#[tauri::command]
pub fn widget_state(app: AppHandle) -> PlayerState {
    core(&app).snapshot()
}

#[tauri::command]
pub fn open_app(app: AppHandle) -> Value {
    crate::launch_music_app(&app);
    json!({ "ok": true })
}

#[tauri::command]
pub async fn command(app: AppHandle, name: String, payload: Option<Value>) -> Value {
    let core = core(&app);

    if PLAYBACK_COMMANDS.contains(&name.as_str()) && core.status() == "offline" {
        crate::launch_music_app(&app);
        return json!({ "ok": true, "launched": true });
    }

    let payload = payload.unwrap_or(Value::Null);
    let number = |key: &str| payload.get(key).and_then(Value::as_f64).unwrap_or(0.0);

    let result = match name.as_str() {
        "togglePlay" => {
            // Flip our own copy first. The renderer updates optimistically for
            // instant feedback, and POSITION_CHANGED arrives about once a
            // second — without this the very next tick pushes the stale value
            // straight back and the icon snaps to the wrong glyph. The server
            // corrects us either way.
            let playing = core.snapshot().is_playing;
            core.update(|state| state.is_playing = !playing);
            core.api.toggle_play().await
        }
        "next" => core.api.next().await,
        "previous" => core.api.previous().await,
        "seek" => core.api.seek_to(number("seconds").max(0.0).round() as i64).await,
        "volume" => {
            let value = number("volume").round().clamp(0.0, 100.0);
            core.note_volume_send(value);
            core.update(|state| state.volume = value);
            // Always POSTs the raw slider value: calibration is display-only.
            core.api.set_volume(value as i64).await
        }
        "toggleMute" => core.api.toggle_mute().await,
        "shuffle" => core.api.shuffle().await,
        "like" | "dislike" => {
            let result = if name == "like" {
                core.api.like().await
            } else {
                core.api.dislike().await
            };
            if result.is_ok() {
                let like = core.api.like_state().await.ok().flatten().and_then(|value| {
                    value.get("state").and_then(Value::as_str).map(str::to_string)
                });
                core.update(|state| state.like = like);
            }
            result
        }
        other => return json!({ "ok": false, "error": format!("Unknown command {other}") }),
    };

    match result {
        Ok(()) => json!({ "ok": true }),
        Err(err) => {
            core.note_error(err.code, &err.message);
            json!({ "ok": false, "error": err.message })
        }
    }
}

#[tauri::command]
pub async fn search_tracks(app: AppHandle, query: String) -> Value {
    let query = query.trim().to_string();
    if query.is_empty() {
        return json!({ "ok": true, "results": [] });
    }

    let core = core(&app);
    let payload = match core.api.search(&query).await {
        Ok(payload) => payload,
        Err(err) => return json!({ "ok": false, "error": err.message }),
    };

    let results = search::parse_search_results(&payload);

    // Artwork goes through here for the same reason cover art does: the
    // renderer's CSP allows data: URLs only.
    let mut resolved = Vec::with_capacity(results.len());
    for item in results {
        let thumbnail = core.api.fetch_cover(item.thumbnail.as_deref()).await;
        resolved.push(search::SearchResult { thumbnail, ..item });
    }

    json!({ "ok": true, "results": resolved })
}

/// Queue a search result and jump to it.
///
/// Do not shortcut this to `next()`. The insert returns 204 *before* YouTube
/// Music has actually mutated its queue, so a `next()` fired straight after
/// skips onto whatever was already queued — you click A, an unrelated track
/// plays, and A only starts when you click something else.
#[tauri::command]
pub async fn play_result(app: AppHandle, video_id: String) -> Value {
    if video_id.is_empty() {
        return json!({ "ok": false });
    }
    let core = core(&app);

    let before = search::queue_entries(core.api.queue().await.ok().flatten().as_ref()).len();

    if let Err(err) = core.api.add_to_queue(&video_id).await {
        return json!({ "ok": false, "error": err.message });
    }

    for _ in 0..PLAY_POLL_ATTEMPTS {
        tokio::time::sleep(PLAY_POLL_INTERVAL).await;

        let entries = search::queue_entries(core.api.queue().await.ok().flatten().as_ref());
        let Some(current) = entries.iter().position(|entry| entry.selected) else {
            continue;
        };
        if current + 1 >= entries.len() {
            continue;
        }

        // INSERT_AFTER_CURRENT_VIDEO always lands in the slot after the playing
        // track. Accept it once that slot holds our id, or — since the id is
        // occasionally re-resolved on insert — once the queue has simply grown.
        let landed = entries[current + 1].video_id.as_deref() == Some(video_id.as_str())
            || entries.len() > before;
        if !landed {
            continue;
        }

        return match core.api.set_queue_index(current + 1).await {
            Ok(()) => json!({ "ok": true, "index": current + 1 }),
            Err(err) => json!({ "ok": false, "error": err.message }),
        };
    }

    json!({ "ok": false, "error": "Queued, but it did not appear in time" })
}

/// Open or close an expanding panel on the window that asked.
#[tauri::command]
pub fn set_panel(app: AppHandle, window: WebviewWindow, which: Option<String>) -> Value {
    let which = which.filter(|which| !which.is_empty());
    let core = core(&app);

    // Resizing a window is AppKit work; IPC arrives on a worker thread.
    let (handle, target, panel) = (app.clone(), window.clone(), which.clone());
    let _ = app.run_on_main_thread(move || {
        if target.label() == PANEL {
            crate::panel::set_expanded(&handle, &target, panel.as_deref());
        } else {
            window::set_expanded(&handle, &target, panel.as_deref());
        }
    });

    core.set_lyrics_wanted(window.label(), which.as_deref() == Some("lyrics"));

    let spawned = Arc::clone(&core);
    tauri::async_runtime::spawn(async move { spawned.refresh_lyrics().await });

    json!({ "ok": true })
}

/// Right-clicking either surface gets a focused subset of the tray menu — the
/// widget's own layout and chrome, or the dropdown's layout.
#[tauri::command]
pub fn context_menu(app: AppHandle, window: WebviewWindow) {
    let label = window.label().to_string();
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(target) = handle.get_webview_window(&label) else {
            return;
        };
        if label == PANEL {
            if let Ok(menu) = crate::tray::build_panel_menu(&handle) {
                crate::panel::popup_menu(&handle, &target, &menu);
            }
        } else if let Ok(menu) = crate::tray::build_widget_menu(&handle) {
            let _ = target.popup_menu(&menu);
        }
    });
}

#[tauri::command]
pub fn set_skin(app: AppHandle, skin: String) -> Value {
    if !window::SKINS.contains(&skin.as_str()) {
        return json!({ "ok": false });
    }
    crate::tray::set_skin(&app, &skin);
    json!({ "ok": true })
}

#[tauri::command]
pub fn set_panel_skin(app: AppHandle, skin: String) -> Value {
    if !window::SKINS.contains(&skin.as_str()) {
        return json!({ "ok": false });
    }
    crate::tray::set_panel_skin(&app, &skin);
    json!({ "ok": true })
}

/// Escape inside the dropdown closes it, like a real menu.
#[tauri::command]
pub fn hide_panel(app: AppHandle) {
    if let Some(panel) = app.get_webview_window(PANEL) {
        crate::panel::hide(&app, &panel);
    }
}

#[tauri::command]
pub fn retry(app: AppHandle) -> Value {
    crate::ws::retry(&app);
    json!({ "ok": true })
}

#[tauri::command]
pub fn quit(app: AppHandle) {
    crate::quit(&app);
}
