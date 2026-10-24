//! Everything the renderer can ask for. The shapes here are the shapes
//! `src/bridge.js` hands to `app.js`, which was written against the Electron
//! preload and is unchanged.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{future, StreamExt};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, WebviewWindow};

use crate::api::{COVER_PX, THUMB_PX};
use crate::search;
use crate::state::{Bootstrap, Core, Repeat};
use crate::window::{self, DROPDOWN, WIDGET};

/// Backoff for waiting on the player after an insert or a jump: a handful of
/// checks, early ones close together, rather than a fixed-rate burst. Each
/// `/queue` is a full round trip into the YouTube Music renderer.
const POLL_BACKOFF_MS: [u64; 5] = [150, 300, 600, 1200, 2400];

/// How long after a jump lands the queue is read once more, for the selection.
const JUMP_CONFIRM: Duration = Duration::from_millis(600);

/// Search thumbnails fetched at once.
const SEARCH_ART_CONCURRENCY: usize = 4;

/// The latest search each surface started, so a superseded one stops before
/// parsing its reply or fetching artwork. Per window: the widget and the
/// dropdown each type their own queries.
static SEARCH_SEQ: AtomicU64 = AtomicU64::new(0);
static SEARCH_LATEST: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

fn search_begin(label: &str) -> u64 {
    let seq = SEARCH_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    SEARCH_LATEST
        .lock()
        .expect("search lock")
        .get_or_insert_with(HashMap::new)
        .insert(label.to_string(), seq);
    seq
}

fn search_current(label: &str, seq: u64) -> bool {
    SEARCH_LATEST
        .lock()
        .expect("search lock")
        .as_ref()
        .and_then(|latest| latest.get(label))
        == Some(&seq)
}

/// Ceiling on one `queue_art` request. A surface asks for what it is showing
/// plus a screenful either side; anything past that is a runaway loop, not a
/// viewport.
const QUEUE_ART_MAX: usize = 48;

/// Pressing play when YouTube Music is not up should start it, not fail silently.
const PLAYBACK_COMMANDS: [&str; 3] = ["togglePlay", "next", "previous"];

fn core(app: &AppHandle) -> Arc<Core> {
    app.state::<Arc<Core>>().inner().clone()
}

/// Refresh "Next tracks" once the player has actually landed on `index`.
///
/// Usually the jump changes the track, and then VIDEO_CHANGED → `apply_song`
/// refreshes the queue on its own; this only watches the cheap `/song` (after
/// checking whether the socket already delivered) in case that message never
/// comes. The queue can also change without the *track* changing — jumping
/// between two copies of the same song is the obvious case — and then no
/// VIDEO_CHANGED is coming, so the queue itself is polled until the slot goes
/// selected: read before the player has switched, it is the list we already
/// had. Either way the queue is published once.
fn refresh_queue_after_jump(
    core: Arc<Core>,
    index: usize,
    target: Option<String>,
    from: Option<String>,
) {
    tauri::async_runtime::spawn(async move {
        let moves = from.is_none() || target != from;
        for delay in POLL_BACKOFF_MS {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            if core.is_local() {
                return;
            }
            if moves {
                let playing = || core.snapshot().song.map(|song| song.video_id);
                // Either the socket delivered, or `/song` shows the switch.
                let landed = playing() != from
                    || match core.api.song().await {
                        Ok(Some(song)) => {
                            let id = song.get("videoId").and_then(Value::as_str).map(str::to_string);
                            let switched = id.is_some() && id != from;
                            // Skip a reply the socket has overtaken: a second
                            // quick jump may already be showing.
                            if switched && playing() == from {
                                core.apply_song(Some(&song));
                            }
                            switched
                        }
                        _ => false,
                    };
                if landed {
                    // `apply_song` refreshed the queue on the track change, but
                    // that read can land before the queue marks the new slot
                    // selected. Confirm once.
                    tokio::time::sleep(JUMP_CONFIRM).await;
                    core.refresh_queue().await;
                    return;
                }
            } else if let Ok(Some(queue)) = core.api.queue().await {
                let entries = search::queue_entries(Some(&queue));
                if entries.get(index).is_some_and(|entry| entry.selected) {
                    core.adopt_queue(&queue);
                    return;
                }
            }
        }
        // Never saw it land; show whatever the player has now.
        core.refresh_queue().await;
    });
}

/// The playing video id, for `refresh_queue_after_jump` to watch it change.
fn playing_id(core: &Core) -> Option<String> {
    core.snapshot().song.map(|song| song.video_id)
}

/// The state, the artwork, the queue and the lyrics in one reply. They travel
/// on four separate events afterwards, but a window loading from cold needs all
/// four before it can draw anything.
#[tauri::command]
pub fn widget_state(app: AppHandle) -> Bootstrap {
    core(&app).bootstrap()
}

#[tauri::command]
pub fn open_app(app: AppHandle) -> Value {
    crate::launch_music_app(&app);
    json!({ "ok": true })
}

#[tauri::command]
pub async fn command(app: AppHandle, name: String, payload: Option<Value>) -> Value {
    let core = core(&app);

    if core.is_local() {
        // Loads and seeks open files: a blocking thread, not this async worker.
        let payload = payload.unwrap_or(Value::Null);
        return tauri::async_runtime::spawn_blocking(move || local_command(&core, &name, payload))
            .await
            .unwrap_or_else(|err| json!({ "ok": false, "error": err.to_string() }));
    }

    if PLAYBACK_COMMANDS.contains(&name.as_str()) && core.status() == crate::state::Status::Offline {
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
        // The player exposes one control — a button cycling NONE → ALL → ONE →
        // NONE — so a target mode has to be expressed as a number of presses.
        //
        // The widget offers only the two modes worth having from a widget:
        // pressing means "the other one". NONE is still *shown* truthfully when
        // it is set inside YouTube Music, and a press from there turns repeat
        // on rather than cycling through it, which would make the button take
        // two presses to do anything visible.
        "repeat" => {
            let known = core.snapshot().repeat;
            let (target, presses) = match known {
                Some(Repeat::All) => (Repeat::One, 1),
                Some(Repeat::One) => (Repeat::All, 2),
                _ => (Repeat::All, 1),
            };
            // Optimistic, exactly as `togglePlay` is — but only from a mode we
            // actually knew. From an unknown one the press lands somewhere we
            // cannot predict, so the read-back below is the first honest answer.
            if known.is_some() {
                core.update(|state| state.repeat = Some(target));
            }

            let result = core.api.switch_repeat(presses).await;
            if result.is_ok() {
                // REPEAT_CHANGED covers this too, but only once the player's own
                // observer fires; reading back is what makes the button correct
                // itself when it does not.
                let mode = crate::state::repeat_of(core.api.repeat_mode().await);
                if mode.is_some() {
                    // A folder may have taken over while this was out.
                    core.update_remote(|state| state.repeat = mode);
                }
            }
            result
        }
        "like" | "dislike" => {
            let result = if name == "like" {
                core.api.like().await
            } else {
                core.api.dislike().await
            };
            if result.is_ok() {
                // A failed read says nothing about the heart: keep it.
                let like = crate::state::like_of(core.api.like_state().await);
                if like.is_some() {
                    core.update_remote(|state| state.like = like);
                }
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

/// The same command names, answered by the folder player.
fn local_command(core: &Arc<Core>, name: &str, payload: Value) -> Value {
    let local = core.local();
    let number = |key: &str| payload.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    match name {
        "togglePlay" => local.toggle_play(core),
        "next" => local.next(core),
        "previous" => local.previous(core),
        "seek" => local.seek(core, number("seconds")),
        "volume" => local.set_volume(core, number("volume").round().clamp(0.0, 100.0)),
        "toggleMute" => local.toggle_mute(core),
        "shuffle" => local.toggle_shuffle(core),
        "repeat" => local.cycle_repeat(core),
        "like" => local.rate(core, crate::likes::Rating::Like),
        "dislike" => local.rate(core, crate::likes::Rating::Dislike),
        "trash" => confirm_trash(core),
        other => return json!({ "ok": false, "error": format!("Unknown command {other}") }),
    }
    json!({ "ok": true })
}

/// Ask, natively, before moving the playing file to the Trash. The track is
/// captured now, so an answer given after playback moved on trashes the file
/// the dialog named — never whatever is playing by then.
fn confirm_trash(core: &Arc<Core>) {
    let Some(track) = core.local().current() else { return };
    let artist = core.snapshot().song.map(|song| song.artist).unwrap_or_default();
    let title = format!("Move “{}” to the Trash?", track.title);
    let detail = format!(
        "{}{}\n\n{}\n\nYou can put it back from the Trash on that drive.",
        artist,
        if track.album.is_empty() { String::new() } else { format!(" — {}", track.album) },
        track.path.display()
    );
    let handle = Arc::clone(core);
    crate::macos::confirm(&core.app, title, detail, "Move to Trash", move || {
        // The callback runs on the main thread; moving files and loading the
        // next track do not belong there.
        tauri::async_runtime::spawn_blocking(move || {
            if let Err(err) = handle.local().trash(&handle, &track.id) {
                crate::macos::inform(&handle.app, "Could not move it to the Trash".into(), err);
            }
        });
    });
}

async fn local_jump(core: &Arc<Core>, index: Option<usize>, id: &str) -> Value {
    let (core, id) = (Arc::clone(core), id.to_string());
    let ok = tauri::async_runtime::spawn_blocking(move || core.local().play_id(&core, index, &id))
        .await
        .unwrap_or(false);
    json!({ "ok": ok })
}

#[tauri::command]
pub async fn search_tracks(app: AppHandle, window: WebviewWindow, query: String) -> Value {
    let label = window.label().to_string();
    let seq = search_begin(&label);
    let current = || search_current(&label, seq);
    // The renderer ignores a superseded reply anyway; this only stops the work.
    let superseded = || json!({ "ok": true, "results": [], "superseded": true });

    let query = query.trim().to_string();
    if query.is_empty() {
        return json!({ "ok": true, "results": [] });
    }

    let core = core(&app);
    if core.is_local() {
        let local = core.local();
        let names = core.store.get(|s| s.artist_names.clone());
        let results = tauri::async_runtime::spawn_blocking(move || local.search(&query, &names))
            .await
            .unwrap_or_default();
        return json!({ "ok": true, "results": results });
    }
    // Serialised with every other `/search` (see `Api::search_if`); a query
    // typed over while it waited for its turn is never sent.
    let payload = match core.api.search_if(&query, current).await {
        None => return superseded(),
        Some(Ok(payload)) => payload,
        Some(Err(err)) => return json!({ "ok": false, "error": err.message }),
    };
    if !current() {
        return superseded();
    }

    let results = search::parse_search_results(&payload);

    // Artwork goes through here for the same reason cover art does: the
    // renderer's CSP allows data: URLs only. Ten of them one after another put
    // ten round trips between typing and seeing the list; all ten at once
    // compete with the playing cover. A few at a time, in order.
    let resolved: Vec<search::SearchResult> = futures_util::stream::iter(results)
        .map(|item| {
            let core = Arc::clone(&core);
            let wanted = current();
            async move {
                let thumbnail = if wanted {
                    core.api.fetch_cover(item.thumbnail.as_deref(), THUMB_PX).await
                } else {
                    None
                };
                search::SearchResult { thumbnail, ..item }
            }
        })
        .buffered(SEARCH_ART_CONCURRENCY)
        .collect()
        .await;
    if !current() {
        return superseded();
    }

    json!({ "ok": true, "results": resolved })
}

/// Everything the playlist picker offers, from both sources at once: picking
/// a row switches source as well as playlist, so YouTube Music playlists are
/// listed while a folder plays and folders while YouTube Music does. The
/// YouTube half fails on its own (YouTube Music not running, signed out)
/// without taking the folders with it.
///
/// Rows carry their cover's source URL, not the cover: the picker asks for
/// the rows it shows through `playlist_art`, so a large library neither holds
/// the reply up nor floods the image cache.
#[tauri::command]
pub async fn list_playlists(app: AppHandle) -> Value {
    let core = core(&app);
    let local = core.is_local();

    let (saved, current_folder, current_playlist) = core.store.get(|s| {
        (s.saved_folders(), s.local_folder.clone(), s.youtube_playlist.clone())
    });
    let player = core.local();
    let folders: Vec<Value> = saved
        .iter()
        .map(|folder| {
            let path = std::path::Path::new(&folder.path);
            let name = |p: Option<&std::path::Path>| {
                p.and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned())
            };
            json!({
                "path": folder.path,
                "name": name(Some(path)).unwrap_or_else(|| folder.path.clone()),
                "parent": name(path.parent()).unwrap_or_default(),
                "present": player.folder_present(&folder.path),
                "current": local && current_folder.as_deref() == Some(folder.path.as_str()),
            })
        })
        .collect();

    let youtube = match core.api.library_playlists().await {
        Ok(payload) => {
            let playlists: Vec<Value> = search::parse_library_playlists(&payload)
                .into_iter()
                .map(|item| {
                    json!({
                        "id": item.id,
                        "title": item.title,
                        "subtitle": item.subtitle,
                        "art": item.thumbnail,
                    })
                })
                .collect();
            json!({ "ok": true, "playlists": playlists })
        }
        Err(err) => json!({ "ok": false, "error": err.message }),
    };

    json!({
        "ok": true,
        "local": local,
        "youtube": youtube,
        "currentPlaylist": if local { None } else { current_playlist },
        "folders": folders,
    })
}

/// Most covers one `playlist_art` call resolves: a screenful and then some.
const PLAYLIST_ART_MAX: usize = 24;

/// Where YouTube Music serves playlist covers from. Auto playlists such as
/// Liked Music use static artwork on gstatic.com.
fn art_host(host: &str) -> bool {
    ["googleusercontent.com", "ytimg.com", "ggpht.com", "gstatic.com"]
        .iter()
        .any(|domain| host.ends_with(&format!(".{domain}")))
}

/// Covers for the picker rows on screen, as `data:` URLs keyed by the source
/// URL `list_playlists` gave. Only YouTube's image hosts (`art_host`) are
/// fetched, so the renderer cannot point this at anything else.
#[tauri::command]
pub async fn playlist_art(app: AppHandle, urls: Vec<String>) -> Value {
    let core = core(&app);
    let allowed = |url: &str| reqwest::Url::parse(url).ok().is_some_and(|url| {
        url.scheme() == "https" && url.host_str().is_some_and(art_host)
    });
    let art: serde_json::Map<String, Value> = futures_util::stream::iter(
        urls.into_iter().filter(|url| allowed(url)).take(PLAYLIST_ART_MAX),
    )
    .map(|url| {
        let core = Arc::clone(&core);
        async move {
            let cover = core.api.fetch_cover(Some(&url), THUMB_PX).await;
            (url, cover.map(|data| Value::String(data.to_string())).unwrap_or(Value::Null))
        }
    })
    .buffer_unordered(SEARCH_ART_CONCURRENCY)
    .collect()
    .await;
    json!({ "ok": true, "art": art })
}

/// An artist's Top songs for the artist popup, by the browse id on the
/// playing `Song`'s `artists`, or by name when it has none. YouTube Music
/// only: a local file's credit has no page, and the renderer does not offer
/// the popup there.
///
/// A typed reply rather than `json!`: the page's covers are `data:` URLs, and
/// building a `Value` would copy them once more before Tauri serialises it.
#[tauri::command]
pub async fn artist_page(app: AppHandle, id: String, query: String) -> ArtistReply {
    let core = core(&app);
    if core.is_local() {
        return ArtistReply::failed("Artist pages are a YouTube Music feature".into());
    }
    match crate::artist::page(&core.api, &id, &query).await {
        Ok(page) => ArtistReply { ok: true, page: Some(page), error: None },
        Err(failed) => ArtistReply::failed(failed.message),
    }
}

#[derive(serde::Serialize)]
pub struct ArtistReply {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<Arc<search::ArtistPage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl ArtistReply {
    fn failed(error: String) -> Self {
        Self { ok: false, page: None, error: Some(error) }
    }
}

/// Bumped by every `play_playlist`, so a slow pick (a folder scanning off an
/// external drive) cannot start playing after a newer one has.
static PLAY_PLAYLIST_GEN: AtomicU64 = AtomicU64::new(0);

/// Play a picker row: `kind` is `youtube` (a playlist browse id) or `folder`
/// (a saved folder's path). Either one starts playing, switching source first
/// when the row belongs to the other one.
#[tauri::command]
pub async fn play_playlist(app: AppHandle, kind: String, id: String) -> Value {
    let core = core(&app);
    let generation = PLAY_PLAYLIST_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let superseded = || PLAY_PLAYLIST_GEN.load(Ordering::SeqCst) != generation;
    match kind.as_str() {
        "youtube" => {
            let from = playing_id(&core);
            // Start it before leaving a folder, so a playlist YouTube Music
            // cannot play leaves the folder playing rather than silence.
            if let Err(err) = core.api.play_playlist(&id).await {
                return json!({ "ok": false, "error": err.message });
            }
            if core.is_local() {
                // `close` waits out a load in progress: not on this worker.
                let handle = Arc::clone(&core);
                let _ = tauri::async_runtime::spawn_blocking(move || {
                    crate::local::Local::close(&handle)
                })
                .await;
            }
            core.store.update_soon(move |s| s.youtube_playlist = Some(id));
            refresh_queue_after_jump(Arc::clone(&core), 0, None, from);
            json!({ "ok": true })
        }
        "folder" => {
            // Only folders the user has chosen before: the path comes from the
            // renderer, and opening one also scans and remembers it.
            let saved = core.store.get(|s| s.saved_folders());
            if !saved.iter().any(|folder| folder.path == id) {
                return json!({ "ok": false, "error": "That folder is not in your list" });
            }
            let player = core.local();
            if !player.folder_present(&id) {
                return json!({ "ok": false, "error": "That folder's drive is not connected" });
            }
            let current = core.store.get(|s| s.local_folder.clone());
            if !(core.is_local() && current.as_deref() == Some(id.as_str())) {
                if let Err(err) =
                    crate::local::Local::open(&core, std::path::PathBuf::from(&id)).await
                {
                    return json!({ "ok": false, "error": err });
                }
            }
            if superseded() {
                return json!({ "ok": false, "error": "Another pick took over" });
            }
            // A folder opens paused where it was left; picking it means play.
            let handle = Arc::clone(&core);
            let _ = tauri::async_runtime::spawn_blocking(move || {
                if handle.is_local() && !handle.snapshot().is_playing {
                    handle.local().toggle_play(&handle);
                }
            })
            .await;
            json!({ "ok": true })
        }
        other => json!({ "ok": false, "error": format!("Unknown playlist kind {other}") }),
    }
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
    if core.is_local() {
        return local_jump(&core, None, &video_id).await;
    }

    let before = search::queue_entries(core.api.queue().await.ok().flatten().as_ref()).len();

    if let Err(err) = core.api.add_to_queue(&video_id).await {
        return json!({ "ok": false, "error": err.message });
    }

    // Only the queue can show the insert (`/song` does not change until the
    // jump), so this one does poll it — with backoff, not at a fixed rate.
    for delay in POLL_BACKOFF_MS {
        tokio::time::sleep(Duration::from_millis(delay)).await;

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

        let (target, from) = (entries[current + 1].video_id.clone(), playing_id(&core));
        return match core.api.set_queue_index(current + 1).await {
            Ok(()) => {
                refresh_queue_after_jump(Arc::clone(&core), current + 1, target, from);
                json!({ "ok": true, "index": current + 1 })
            }
            Err(err) => json!({ "ok": false, "error": err.message }),
        };
    }

    json!({ "ok": false, "error": "Queued, but it did not appear in time" })
}

/// Play a track that is already in the queue — every "Next tracks" row is one.
///
/// Jumping to it is the whole job. Queueing it, as `play_result` does, inserts a
/// *second copy* after the playing track and jumps to that, which leaves the
/// original where it was: the queue behind the new position is untouched, so the
/// list you just clicked comes back looking identical and the track you picked
/// is still sitting in it.
#[tauri::command]
pub async fn play_queued(app: AppHandle, video_id: String) -> Value {
    if video_id.is_empty() {
        return json!({ "ok": false });
    }
    let core = core(&app);
    if core.is_local() {
        return local_jump(&core, None, &video_id).await;
    }
    let entries = search::queue_entries(core.api.queue().await.ok().flatten().as_ref());

    // Search forward from the playing track: the same id can appear earlier in
    // the queue, and jumping to that would rewind rather than skip ahead.
    let start = entries
        .iter()
        .position(|entry| entry.selected)
        .map(|index| index + 1)
        .unwrap_or(0);
    let found = entries
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, entry)| entry.video_id.as_deref() == Some(video_id.as_str()))
        .map(|(index, _)| index);

    match found {
        Some(index) => {
            // Taken before the jump: VIDEO_CHANGED can beat the PATCH's reply.
            let (target, from) = (entries[index].video_id.clone(), playing_id(&core));
            match core.api.set_queue_index(index).await {
                Ok(()) => {
                    refresh_queue_after_jump(Arc::clone(&core), index, target, from);
                    json!({ "ok": true, "index": index })
                }
                Err(err) => json!({ "ok": false, "error": err.message }),
            }
        }
        // The queue moved on between the row being drawn and clicked; queueing
        // it is then the honest fallback.
        None => play_result(app, video_id).await,
    }
}

/// Open or close an expanding panel on the window that asked.
#[tauri::command]
pub fn set_panel(app: AppHandle, window: WebviewWindow, which: Option<String>) -> Value {
    let which = which.filter(|which| !which.is_empty());
    let core = core(&app);

    // Each surface comes back up showing whatever it was left showing — the
    // lyrics or the queue, both modes you left open. Not a search: that is a
    // query you have finished with. Only while connected: going offline closes
    // the panel on its own, and that is not you choosing to close it.
    if core.status() == crate::state::Status::Connected {
        let remembered = which
            .clone()
            .filter(|which| window::panel_is_restorable(which));
        let dropdown = window.label() == DROPDOWN;
        let stored = core.store.get(|s| {
            if dropdown {
                s.dropdown_panel.clone()
            } else {
                s.panel.clone()
            }
        });
        if stored != remembered {
            core.store.update_soon(|s| {
                if dropdown {
                    s.dropdown_panel = remembered;
                } else {
                    s.panel = remembered;
                }
            });
        }
    }

    // Resizing a window is AppKit work, queued as one hop so it stays in order
    // with the setters it posts (see AGENTS.md, main-thread rules).
    let (handle, target, panel) = (app.clone(), window.clone(), which.clone());
    let _ = app.run_on_main_thread(move || {
        if target.label() == DROPDOWN {
            crate::panel::set_expanded(&handle, &target, panel.as_deref());
        } else {
            window::set_expanded(&handle, &target, panel.as_deref());
        }
    });

    // A hidden dropdown keeps its panel but wants nothing; showing it asks again.
    let watching = window.label() != DROPDOWN || crate::panel::is_visible(&app);
    core.set_lyrics_wanted(window.label(), watching && which.as_deref() == Some("lyrics"));
    core.set_queue_wanted(window.label(), watching && which.as_deref() == Some("queue"));

    let spawned = Arc::clone(&core);
    tauri::async_runtime::spawn(async move {
        spawned.refresh_lyrics().await;
        spawned.refresh_queue().await;
    });

    json!({ "ok": true })
}

/// Artwork for the queue rows a surface is actually showing.
///
/// A command rather than an event, for the reason `search_tracks` is one:
/// `emit` formats a JS source string *per webview*, and two surfaces looking at
/// different parts of the same queue want different bytes. Resolving every slot
/// up front would be megabytes for rows nobody has scrolled to.
///
/// Keyed by video id, which makes it stale-proof without a token: a reply that
/// lands after the queue moved is applied to whatever row now holds that id, or
/// to none at all.
#[tauri::command]
pub async fn queue_art(app: AppHandle, ids: Vec<String>, size: Option<u32>) -> ArtReply {
    let core = core(&app);
    let size = size.unwrap_or(THUMB_PX).clamp(THUMB_PX, COVER_PX);

    let mut wanted: Vec<String> = Vec::new();
    for id in ids {
        if wanted.len() >= QUEUE_ART_MAX {
            break;
        }
        if !id.is_empty() && !wanted.contains(&id) {
            wanted.push(id);
        }
    }

    if core.is_local() {
        let art = crate::local::Local::queue_art(&core, wanted, size)
            .await
            .into_iter()
            .map(|(id, art)| (id, SharedStr(art)))
            .collect();
        return ArtReply { ok: true, art };
    }

    // One pass against the same keep-alive connection, the way the four-item
    // queue already resolved its thumbnails; most of these are cache hits.
    let resolved = future::join_all(wanted.into_iter().map(|id| {
        let core = Arc::clone(&core);
        async move {
            let src = core.queue_art_src(&id)?;
            let art = core.api.fetch_cover(Some(&src), size).await?;
            Some((id, art))
        }
    }))
    .await;

    let art = resolved
        .into_iter()
        .flatten()
        .map(|(id, art)| (id, SharedStr(art)))
        .collect();

    ArtReply { ok: true, art }
}

/// `queue_art`'s reply. Typed rather than a `Value` so the cached `data:` URLs
/// are serialised straight from their shared buffers: a `Value::String` would
/// copy each one — tens of KB, up to `QUEUE_ART_MAX` of them — per request.
#[derive(serde::Serialize)]
pub struct ArtReply {
    ok: bool,
    art: std::collections::BTreeMap<String, SharedStr>,
}

/// An `Arc<str>` that serialises as the string it holds (serde's `rc` feature
/// is not enabled).
pub struct SharedStr(Arc<str>);

impl serde::Serialize for SharedStr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// Jump to a queue slot the caller can see.
///
/// `play_queued` searches *forward* from the playing track, deliberately, so a
/// duplicate id earlier in the queue cannot rewind you. The queue panel shows
/// already-played rows, which that search can never reach: it would fall through
/// to `play_result` and queue a second copy, leaving the list looking unchanged
/// with the track still in it — exactly the failure `play_queued` exists to
/// avoid. So the panel sends the slot index it drew.
///
/// The id travels with it and is checked against the live queue, so a list drawn
/// a moment ago cannot jump you to whatever has since moved into that slot.
#[tauri::command]
pub async fn play_queue_index(app: AppHandle, index: usize, video_id: String) -> Value {
    let core = core(&app);
    if core.is_local() {
        return local_jump(&core, Some(index), &video_id).await;
    }
    let entries = search::queue_entries(core.api.queue().await.ok().flatten().as_ref());

    let matches = entries
        .get(index)
        .is_some_and(|entry| entry.video_id.as_deref() == Some(video_id.as_str()));
    if !matches {
        // The queue moved under the drawn list; find the track by id instead.
        return play_queued(app, video_id).await;
    }

    let from = playing_id(&core);
    match core.api.set_queue_index(index).await {
        Ok(()) => {
            refresh_queue_after_jump(Arc::clone(&core), index, Some(video_id), from);
            json!({ "ok": true, "index": index })
        }
        Err(err) => json!({ "ok": false, "error": err.message }),
    }
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
        if label == DROPDOWN {
            if let Ok(menu) = crate::tray::build_panel_menu(&handle) {
                crate::panel::popup_menu(&handle, &target, &menu);
            }
        } else if let Ok(menu) = crate::tray::build_widget_menu(&handle) {
            let _ = target.popup_menu(&menu);
        }
    });
}

/// The widget's own close button. Exactly what "Hide widget" in the menu does —
/// the window goes away and "Show floating widget" in the tray brings it back —
/// so the button is a shortcut to a dismissal that already existed rather than a
/// second, differently-behaved way out.
#[tauri::command]
pub fn hide_widget(app: AppHandle) {
    if let Some(widget) = app.get_webview_window(WIDGET) {
        let _ = widget.hide();
        crate::on_screen(&app, WIDGET, false);
    }
}

/// Escape inside the dropdown closes it, like a real menu.
#[tauri::command]
pub fn hide_panel(app: AppHandle) {
    if let Some(panel) = app.get_webview_window(DROPDOWN) {
        crate::panel::hide(&app, &panel);
    }
}

#[tauri::command]
pub fn retry(app: AppHandle) -> Value {
    crate::ws::retry(&app);
    json!({ "ok": true })
}

#[cfg(test)]
mod tests {
    use super::art_host;

    #[test]
    fn playlist_art_only_fetches_youtube_image_hosts() {
        for host in ["yt3.googleusercontent.com", "i.ytimg.com", "www.gstatic.com", "yt3.ggpht.com"] {
            assert!(art_host(host), "{host}");
        }
        for host in ["gstatic.com.evil.test", "evilgstatic.com", "127.0.0.1", "localhost"] {
            assert!(!art_host(host), "{host}");
        }
    }
}
