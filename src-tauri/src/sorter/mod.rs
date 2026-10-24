//! Sort Songs: a window that plays each unrated song of a local folder from
//! its chorus and records like or dislike, then moves the dislikes to the
//! Trash.
//!
//! **Nothing here exists until the window opens.** `open` builds the runtime
//! (session, player, HTTP client) and the window; closing the window destroys
//! both, and the player's ticker and output stream go with it. While closed the
//! widget pays for one empty `Mutex<Option<…>>`.
//!
//! Ratings go through `likes.rs` into the folder's `.pear-music/likes.json`,
//! like the player's own. The player's in-memory copy is not reloaded, so a
//! rating the player saves later can overwrite one made here.

mod chorus;
mod cover;
mod player;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial, NSVisualEffectState};

use crate::likes::{Likes, Rating};
use crate::local::Track;
use crate::lyrics::Lyrics;
use crate::state::{Core, Song};
use crate::store::Store;
use chorus::Chorus;
use player::Player;

pub const LABEL: &str = "sorter";
const COVER_PX: u32 = 600;
/// Lyrics are looked up this many cards ahead, so most cards open on a chorus.
const PREFETCH: usize = 4;
/// How long a card waits for its lyrics before playing from the fallback.
const LYRICS_WAIT: Duration = Duration::from_millis(2500);
/// Pause before looking a miss up again.
const RETRY_AFTER: Duration = Duration::from_secs(3);

/// The runtime while the window is open; `None` the rest of the time.
static RUNTIME: Mutex<Option<Arc<Sorter>>> = Mutex::new(None);

fn runtime() -> Option<Arc<Sorter>> {
    RUNTIME.lock().expect("sorter runtime").clone()
}

struct Session {
    root: PathBuf,
    tracks: Vec<Track>,
    likes: Likes,
    /// Indexes into `tracks` still to sort; `queue[0]` is on the card.
    queue: Vec<usize>,
    /// Ratings made here, newest last: (track, rating it had before).
    history: Vec<(usize, Option<Rating>)>,
}

struct Sorter {
    player: Arc<Player>,
    http: reqwest::Client,
    session: Mutex<Option<Session>>,
    names: Mutex<BTreeMap<String, String>>,
    choruses: Mutex<HashMap<String, Chorus>>,
    /// Lyrics as looked up, `None` for a miss, kept for the lyrics pane.
    lyrics: Mutex<HashMap<String, Option<Lyrics>>>,
    /// Covers already made, so a card shown before the page listened can ask.
    covers: Mutex<HashMap<String, Option<String>>>,
    /// Lyrics lookups started, so a prefetch is never asked twice.
    asked: Mutex<HashSet<String>>,
    /// Held across each lyrics lookup: one at a time, as the player does. Run
    /// side by side, the providers time out or refuse, and the cache records
    /// those failures as misses for a week.
    lookup: tokio::sync::Mutex<()>,
    /// The folder being scanned, while it is.
    scanning: Mutex<Option<String>>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Card {
    id: String,
    title: String,
    artist: String,
    album: String,
    duration: f64,
    file: String,
    chorus: Option<Chorus>,
    /// Other songs of the same name still to sort; they come straight after.
    same_name: usize,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    folder: Option<String>,
    card: Option<Card>,
    total: usize,
    remaining: usize,
    liked: usize,
    disliked: usize,
    can_undo: bool,
    scanning: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Disliked {
    id: String,
    title: String,
    artist: String,
    album: String,
    file: String,
}

impl Sorter {
    fn song(&self, track: &Track) -> Song {
        let names = self.names.lock().expect("names lock");
        Song {
            video_id: track.id.clone(),
            title: track.title.clone(),
            artist: crate::store::map_artist(&names, &track.artist),
            raw_artist: track.artist.clone(),
            album: track.album.clone(),
            song_duration: track.duration,
            image_src: None,
            url: None,
            media_type: Some("LOCAL".into()),
            ..Song::default()
        }
    }

    fn view(&self) -> View {
        let guard = self.session.lock().expect("session lock");
        let scanning = self.scanning.lock().expect("scanning lock").clone();
        let Some(s) = guard.as_ref() else {
            return View {
                folder: None,
                card: None,
                total: 0,
                remaining: 0,
                liked: 0,
                disliked: 0,
                can_undo: false,
                scanning,
            };
        };
        let rated = |want: Rating| {
            s.tracks
                .iter()
                .filter(|t| s.likes.get(&t.id) == Some(want))
                .count()
        };
        let card = s.queue.first().map(|&i| {
            let track = &s.tracks[i];
            Card {
                id: track.id.clone(),
                title: track.title.clone(),
                artist: self.song(track).artist,
                album: track.album.clone(),
                duration: track.duration,
                file: relative(&s.root, &track.path),
                chorus: self
                    .choruses
                    .lock()
                    .expect("chorus lock")
                    .get(&track.id)
                    .cloned(),
                same_name: {
                    let key = title_key(&track.title);
                    s.queue[1..]
                        .iter()
                        .filter(|&&j| !key.is_empty() && title_key(&s.tracks[j].title) == key)
                        .count()
                },
            }
        });
        View {
            folder: Some(s.root.to_string_lossy().into_owned()),
            card,
            total: s.tracks.len(),
            remaining: s.queue.len(),
            liked: rated(Rating::Like),
            disliked: rated(Rating::Dislike),
            can_undo: !s.history.is_empty(),
            scanning,
        }
    }

    fn current(&self) -> Option<Track> {
        let guard = self.session.lock().expect("session lock");
        let s = guard.as_ref()?;
        s.queue.first().map(|&i| s.tracks[i].clone())
    }

    fn ahead(&self) -> Vec<Track> {
        let guard = self.session.lock().expect("session lock");
        let Some(s) = guard.as_ref() else {
            return Vec::new();
        };
        s.queue
            .iter()
            .take(PREFETCH + 1)
            .map(|&i| s.tracks[i].clone())
            .collect()
    }
}

/// A title without its bracketed notes, folded: 小酒窩, 小酒窝 (Live) and
/// 小酒窝(feat.蔡卓妍)（粤语版） all give 小酒窝.
fn title_key(title: &str) -> String {
    let bare = title.split(['(', '（', '[', '【']).next().unwrap_or(title);
    fast2s::convert(bare)
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// The unrated tracks in folder order, except that each song's namesakes
/// follow it directly, so versions of one song are judged side by side.
fn queue_of(tracks: &[Track], likes: &Likes) -> Vec<usize> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for (i, track) in tracks.iter().enumerate() {
        if likes.get(&track.id).is_some() {
            continue;
        }
        let key = title_key(&track.title);
        match by_key.get(&key).filter(|_| !key.is_empty()) {
            Some(&g) => groups[g].push(i),
            None => {
                by_key.insert(key, groups.len());
                groups.push(vec![i]);
            }
        }
    }
    groups.into_iter().flatten().collect()
}

/// Write ratings, then have the player read them again if this folder is the
/// one it has open, so its next save keeps them. Blocking IO.
fn write_ratings(app: &AppHandle, save: crate::likes::Save, root: PathBuf) {
    save.write();
    if let Some(core) = app.try_state::<Arc<Core>>() {
        let core = core.inner().clone();
        core.local().reload_likes(&core, &root);
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn emit(app: &AppHandle, event: &str, payload: impl Serialize + Clone) {
    let _ = app.emit_to(LABEL, event, payload);
}

// ---------------------------------------------------------------- settings

/// The sorter's own last folder, kept apart from the player's settings.
#[derive(Default, Serialize, Deserialize)]
struct Own {
    #[serde(default)]
    folder: Option<String>,
}

fn own_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/pear-music-sorter/settings.json")
}

fn own() -> Own {
    std::fs::read(own_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_own(own: &Own) {
    let path = own_path();
    if let (Some(dir), Ok(json)) = (path.parent(), serde_json::to_vec_pretty(own)) {
        let temp = path.with_extension("tmp");
        if std::fs::create_dir_all(dir).is_ok() && std::fs::write(&temp, json).is_ok() {
            let _ = std::fs::rename(&temp, &path);
        }
    }
}

// ------------------------------------------------------------- open / close

/// Tray → Sort Songs…: pause whatever is playing, then show the window,
/// building it (and the runtime) if it is not open. Main thread.
pub fn open(app: &AppHandle) {
    pause_playback(app);
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    *RUNTIME.lock().expect("sorter runtime") = Some(Arc::new(Sorter {
        player: Player::new(app.clone()),
        http: reqwest::Client::builder()
            .user_agent(crate::lyrics::USER_AGENT)
            .build()
            .expect("build http client"),
        session: Mutex::new(None),
        names: Mutex::new(BTreeMap::new()),
        choruses: Mutex::new(HashMap::new()),
        lyrics: Mutex::new(HashMap::new()),
        covers: Mutex::new(HashMap::new()),
        asked: Mutex::new(HashSet::new()),
        lookup: tokio::sync::Mutex::new(()),
        scanning: Mutex::new(None),
    }));
    let built = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("sorter/index.html".into()))
        .title("Sort Songs")
        .inner_size(460.0, 760.0)
        .min_inner_size(400.0, 620.0)
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .transparent(true)
        .build();
    match built {
        Ok(window) => {
            let _ = apply_vibrancy(
                &window,
                NSVisualEffectMaterial::Sidebar,
                Some(NSVisualEffectState::FollowsWindowActiveState),
                None,
            );
            let _ = window.set_focus();
            start(app);
        }
        Err(err) => {
            eprintln!("sorter: cannot open window: {err}");
            closed(app);
        }
    }
}

/// The window is gone: drop the runtime, and with it the player.
pub fn closed(app: &AppHandle) {
    if let Some(sorter) = RUNTIME.lock().expect("sorter runtime").take() {
        sorter.player.stop();
    }
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
}

/// Pause the player — YouTube Music or a local folder — if it is playing.
fn pause_playback(app: &AppHandle) {
    let Some(core) = app.try_state::<Arc<Core>>() else {
        return;
    };
    let core = core.inner().clone();
    if !core.snapshot().is_playing {
        return;
    }
    if core.is_local() {
        tauri::async_runtime::spawn_blocking(move || core.local().toggle_play(&core));
    } else if core.status() == "connected" {
        core.update(|state| state.is_playing = false);
        tauri::async_runtime::spawn(async move {
            let _ = core.api.toggle_play().await;
        });
    }
}

// ------------------------------------------------------------------- cards

async fn chorus_for(sorter: &Arc<Sorter>, track: &Track) -> Chorus {
    if let Some(found) = sorter.choruses.lock().expect("chorus lock").get(&track.id) {
        return found.clone();
    }
    let _turn = sorter.lookup.lock().await;
    // Another caller may have finished this track while we queued.
    if let Some(found) = sorter.choruses.lock().expect("chorus lock").get(&track.id) {
        return found.clone();
    }
    let song = sorter.song(track);
    let mut lyrics = crate::lyrics::fetch_lyrics(&sorter.http, &song).await;
    if lyrics.is_none() {
        // The lookup caches a provider that failed (a timeout, a refusal) as
        // a miss for a week. Ask once more before believing it.
        crate::lyrics::forget_miss(&track.id);
        tokio::time::sleep(RETRY_AFTER).await;
        lyrics = crate::lyrics::fetch_lyrics(&sorter.http, &song).await;
    }
    let found = chorus::find(lyrics.as_ref(), &track.title, track.duration);
    sorter
        .lyrics
        .lock()
        .expect("lyrics lock")
        .insert(track.id.clone(), lyrics);
    sorter
        .choruses
        .lock()
        .expect("chorus lock")
        .insert(track.id.clone(), found.clone());
    found
}

/// Look up lyrics for the next few cards in the background, one after another.
fn prefetch(sorter: &Arc<Sorter>) {
    let tracks: Vec<Track> = sorter
        .ahead()
        .into_iter()
        .skip(1)
        .filter(|track| {
            sorter
                .asked
                .lock()
                .expect("asked lock")
                .insert(track.id.clone())
        })
        .collect();
    if tracks.is_empty() {
        return;
    }
    let sorter = Arc::clone(sorter);
    tauri::async_runtime::spawn(async move {
        for track in tracks {
            // Window closed: stop looking.
            if runtime().is_none() {
                return;
            }
            chorus_for(&sorter, &track).await;
        }
    });
}

fn emit_lyrics(app: &AppHandle, sorter: &Sorter, id: &str) {
    let lyrics = sorter
        .lyrics
        .lock()
        .expect("lyrics lock")
        .get(id)
        .cloned()
        .flatten();
    emit(app, "sorter-lyrics", json!({ "id": id, "lyrics": lyrics }));
}

/// The card `id` is still up, in a window that is still open.
fn still_current(id: &str) -> bool {
    runtime()
        .and_then(|sorter| sorter.current())
        .is_some_and(|track| track.id == id)
}

/// Put the current card on screen and start playing it: from the chorus if
/// the lyrics come back in time, else from the fallback, moving to the chorus
/// if they land before the user has moved the playhead.
fn present(app: &AppHandle, sorter: &Arc<Sorter>) {
    emit(app, "sorter-view", sorter.view());
    let Some(track) = sorter.current() else {
        sorter.player.stop();
        return;
    };

    let (app_cover, sorter_cover, cover_track) = (app.clone(), Arc::clone(sorter), track.clone());
    tauri::async_runtime::spawn_blocking(move || {
        let artist = sorter_cover.song(&cover_track).artist;
        let url = cover::cover(&cover_track, &artist, COVER_PX);
        sorter_cover
            .covers
            .lock()
            .expect("covers lock")
            .insert(cover_track.id.clone(), url.clone());
        emit(
            &app_cover,
            "sorter-cover",
            json!({ "id": cover_track.id, "url": url }),
        );
    });

    let (app, sorter) = (app.clone(), Arc::clone(sorter));
    tauri::async_runtime::spawn(async move {
        // Its own task, so giving up waiting does not cancel it halfway.
        let mut lookup = tauri::async_runtime::spawn({
            let (sorter, track) = (Arc::clone(&sorter), track.clone());
            async move { chorus_for(&sorter, &track).await }
        });
        let found = tokio::time::timeout(LYRICS_WAIT, &mut lookup).await;
        let fallback = !matches!(found, Ok(Ok(_)));
        if !fallback {
            emit_lyrics(&app, &sorter, &track.id);
            prefetch(&sorter);
        }
        let first = match found {
            Ok(Ok(found)) => found,
            _ => chorus::find(None, &track.title, track.duration),
        };
        if !still_current(&track.id) {
            return;
        }
        emit(
            &app,
            "sorter-chorus",
            json!({ "id": track.id, "chorus": first }),
        );
        play(&app, &sorter, &track, first.start).await;
        if fallback {
            let Ok(found) = lookup.await else { return };
            prefetch(&sorter);
            if still_current(&track.id) {
                emit_lyrics(&app, &sorter, &track.id);
            }
            if found.how != "fallback"
                && sorter.player.untouched(&track.id)
                && still_current(&track.id)
            {
                emit(
                    &app,
                    "sorter-chorus",
                    json!({ "id": track.id, "chorus": found }),
                );
                play(&app, &sorter, &track, found.start).await;
            }
        }
    });
}

async fn play(app: &AppHandle, sorter: &Arc<Sorter>, track: &Track, start: f64) {
    let (app, player, track) = (app.clone(), Arc::clone(&sorter.player), track.clone());
    let _ = tauri::async_runtime::spawn_blocking(move || {
        if !still_current(&track.id) {
            return;
        }
        if let Err(err) = player.play(&track.id, &track.path, start) {
            emit(&app, "sorter-error", format!("{}: {err}", track.title));
        }
    })
    .await;
}

/// Open `root`: scan, load ratings, queue the unrated. Blocking IO.
fn open_folder(app: &AppHandle, sorter: &Arc<Sorter>, root: PathBuf) -> Result<(), String> {
    if !root.is_dir() {
        return Err(format!("{} is not mounted", root.display()));
    }
    *sorter.scanning.lock().expect("scanning lock") = Some(root.to_string_lossy().into_owned());
    emit(app, "sorter-view", sorter.view());
    let tracks = crate::local::scan(&root);
    let likes = Likes::load(&root, &tracks);
    let queue = queue_of(&tracks, &likes);
    sorter.player.stop();
    *sorter.scanning.lock().expect("scanning lock") = None;
    let names = app.state::<Arc<Store>>().get(|s| s.artist_names.clone());
    *sorter.names.lock().expect("names lock") = names;
    *sorter.session.lock().expect("session lock") = Some(Session {
        root: root.clone(),
        tracks,
        likes,
        queue,
        history: Vec::new(),
    });
    save_own(&Own {
        folder: Some(root.to_string_lossy().into_owned()),
    });
    present(app, sorter);
    Ok(())
}

/// The folder to start in: the sorter's last, else the player's.
fn start(app: &AppHandle) {
    let Some(sorter) = runtime() else { return };
    let folder = own()
        .folder
        .or_else(|| app.state::<Arc<Store>>().get(|s| s.local_folder.clone()));
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Some(folder) = folder {
            if let Err(err) = open_folder(&app, &sorter, PathBuf::from(folder)) {
                emit(&app, "sorter-error", err);
            }
        }
        emit(&app, "sorter-view", sorter.view());
    });
}

// --------------------------------------------------------------- commands

fn no_window() -> String {
    "The sorter is not open".into()
}

#[tauri::command]
pub fn sorter_view() -> Option<View> {
    runtime().map(|sorter| sorter.view())
}

/// What the cover and lyrics events carried for `id`, for a page that was not
/// listening yet. A field is absent while it is still being found.
#[tauri::command]
pub fn sorter_extras(id: String) -> Value {
    let Some(sorter) = runtime() else {
        return json!({});
    };
    let mut out = serde_json::Map::new();
    if let Some(cover) = sorter.covers.lock().expect("covers lock").get(&id) {
        out.insert("cover".into(), json!(cover));
    }
    if let Some(lyrics) = sorter.lyrics.lock().expect("lyrics lock").get(&id) {
        out.insert("lyrics".into(), json!(lyrics));
    }
    Value::Object(out)
}

#[tauri::command]
pub async fn sorter_choose_folder(app: AppHandle) -> Result<(), String> {
    let sorter = runtime().ok_or_else(no_window)?;
    tauri::async_runtime::spawn_blocking(move || match crate::tray::choose_folder() {
        Some(path) => open_folder(&app, &sorter, path),
        None => Ok(()),
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Rate the card (`like` or not) and move on.
#[tauri::command]
pub fn sorter_rate(app: AppHandle, like: bool) -> Result<(), String> {
    let sorter = runtime().ok_or_else(no_window)?;
    let saved = {
        let mut guard = sorter.session.lock().expect("session lock");
        let Some(s) = guard.as_mut() else {
            return Ok(());
        };
        let Some(&i) = s.queue.first() else {
            return Ok(());
        };
        // Pick up anything the player saved since.
        s.likes = Likes::load(&s.root, &s.tracks);
        let before = s.likes.get(&s.tracks[i].id);
        let rating = if like { Rating::Like } else { Rating::Dislike };
        let save = s.likes.set(&s.tracks[i], Some(rating));
        s.history.push((i, before));
        s.queue.remove(0);
        // Rated in the player meanwhile: nothing left to decide.
        let (likes, tracks) = (&s.likes, &s.tracks);
        s.queue.retain(|&j| likes.get(&tracks[j].id).is_none());
        save.map(|save| (save, s.root.clone()))
    };
    if let Some((save, root)) = saved {
        let app = app.clone();
        tauri::async_runtime::spawn_blocking(move || write_ratings(&app, save, root));
    }
    present(&app, &sorter);
    Ok(())
}

/// Send the card to the back of the queue, unrated.
#[tauri::command]
pub fn sorter_skip(app: AppHandle) {
    let Some(sorter) = runtime() else { return };
    {
        let mut guard = sorter.session.lock().expect("session lock");
        let Some(s) = guard.as_mut() else { return };
        if s.queue.len() < 2 {
            return;
        }
        let first = s.queue.remove(0);
        s.queue.push(first);
    }
    present(&app, &sorter);
}

/// Take back the last rating made here and put that song back on the card.
#[tauri::command]
pub fn sorter_undo(app: AppHandle) {
    let Some(sorter) = runtime() else { return };
    let save = {
        let mut guard = sorter.session.lock().expect("session lock");
        let Some(s) = guard.as_mut() else { return };
        let Some((i, before)) = s.history.pop() else {
            return;
        };
        s.likes = Likes::load(&s.root, &s.tracks);
        let save = s.likes.set(&s.tracks[i], before);
        s.queue.retain(|&j| j != i);
        s.queue.insert(0, i);
        save.map(|save| (save, s.root.clone()))
    };
    if let Some((save, root)) = save {
        let app = app.clone();
        tauri::async_runtime::spawn_blocking(move || write_ratings(&app, save, root));
    }
    present(&app, &sorter);
}

/// Ask, then clear every like and dislike in the folder: all its songs go
/// back in the queue. Cleared ratings are written as dated tombstones, as any
/// cleared rating is, so the fallback copy cannot bring them back.
#[tauri::command]
pub fn sorter_reset_ratings(app: AppHandle) {
    let Some(sorter) = runtime() else { return };
    let counts = {
        let guard = sorter.session.lock().expect("session lock");
        guard.as_ref().map(|s| {
            let count = |want| {
                s.tracks
                    .iter()
                    .filter(|t| s.likes.get(&t.id) == Some(want))
                    .count()
            };
            (s.root.clone(), count(Rating::Like), count(Rating::Dislike))
        })
    };
    let Some((root, liked, disliked)) = counts else {
        return;
    };
    if liked + disliked == 0 {
        return;
    }
    let name = root.file_name().map_or_else(
        || root.to_string_lossy().into_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let handle = app.clone();
    crate::macos::confirm(
        &app,
        format!("Clear all ratings in “{name}”?"),
        format!(
            "{liked} liked and {disliked} disliked songs become unrated and go back in the queue. \
             This cannot be undone."
        ),
        "Clear Ratings",
        move || {
            tauri::async_runtime::spawn_blocking(move || reset_ratings(&handle));
        },
    );
}

/// Blocking IO.
fn reset_ratings(app: &AppHandle) {
    let Some(sorter) = runtime() else { return };
    let saved = {
        let mut guard = sorter.session.lock().expect("session lock");
        let Some(s) = guard.as_mut() else { return };
        s.likes = Likes::load(&s.root, &s.tracks);
        let rated: Vec<usize> = (0..s.tracks.len())
            .filter(|&i| s.likes.get(&s.tracks[i].id).is_some())
            .collect();
        let mut last = None;
        for i in rated {
            last = s.likes.set(&s.tracks[i], None).or(last);
        }
        s.history.clear();
        s.queue = queue_of(&s.tracks, &s.likes);
        last.map(|save| (save, s.root.clone()))
    };
    if let Some((save, root)) = saved {
        write_ratings(app, save, root);
    }
    present(app, &sorter);
}

#[tauri::command]
pub async fn sorter_toggle_play() -> Result<(), String> {
    let sorter = runtime().ok_or_else(no_window)?;
    tauri::async_runtime::spawn_blocking(move || sorter.player.toggle())
        .await
        .map_err(|e| e.to_string())?
}

/// Play the card's song from `at` seconds.
#[tauri::command]
pub async fn sorter_seek(at: f64) -> Result<(), String> {
    let sorter = runtime().ok_or_else(no_window)?;
    let Some(track) = sorter.current() else {
        return Ok(());
    };
    let at = at.clamp(0.0, (track.duration - 0.5).max(0.0));
    tauri::async_runtime::spawn_blocking(move || sorter.player.seek(&track.id, at))
        .await
        .map_err(|e| e.to_string())?
}

/// Play the card's song from its chorus again.
#[tauri::command]
pub async fn sorter_replay(app: AppHandle) {
    let Some(sorter) = runtime() else { return };
    let Some(track) = sorter.current() else {
        return;
    };
    let start = sorter
        .choruses
        .lock()
        .expect("chorus lock")
        .get(&track.id)
        .map_or_else(
            || chorus::find(None, &track.title, track.duration).start,
            |c| c.start,
        );
    play(&app, &sorter, &track, start).await;
}

#[tauri::command]
pub fn sorter_disliked() -> Vec<Disliked> {
    let Some(sorter) = runtime() else {
        return Vec::new();
    };
    let guard = sorter.session.lock().expect("session lock");
    let Some(s) = guard.as_ref() else {
        return Vec::new();
    };
    s.tracks
        .iter()
        .filter(|t| s.likes.is_disliked(&t.id))
        .map(|t| Disliked {
            id: t.id.clone(),
            title: t.title.clone(),
            artist: sorter.song(t).artist,
            album: t.album.clone(),
            file: relative(&s.root, &t.path),
        })
        .collect()
}

/// Move these disliked songs (and their `._` shadows) to the Trash. The
/// review sheet that lists them, with its own Move to Trash button, is the
/// confirmation. Returns how many moved.
#[tauri::command]
pub async fn sorter_trash(app: AppHandle, ids: Vec<String>) -> Result<usize, String> {
    let sorter = runtime().ok_or_else(no_window)?;
    if ids.is_empty() {
        return Ok(0);
    }
    let worker = Arc::clone(&sorter);
    let (moved, failed) = tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        move || trash_disliked(&app, &worker, &ids)
    })
    .await
    .map_err(|e| e.to_string())?;
    if !failed.is_empty() {
        emit(&app, "sorter-error", failed.join("\n"));
    }
    emit(&app, "sorter-view", sorter.view());
    Ok(moved)
}

/// Blocking IO. Only what is still disliked: the answer may come after an undo.
fn trash_disliked(app: &AppHandle, sorter: &Sorter, ids: &[String]) -> (usize, Vec<String>) {
    let targets: Vec<Track> = {
        let guard = sorter.session.lock().expect("session lock");
        let Some(s) = guard.as_ref() else {
            return (0, Vec::new());
        };
        s.tracks
            .iter()
            .filter(|t| ids.contains(&t.id) && s.likes.is_disliked(&t.id))
            .cloned()
            .collect()
    };
    let mut moved = Vec::new();
    let mut failed = Vec::new();
    for track in targets {
        match crate::macos::trash(&track.path) {
            Ok(()) => {
                if let (Some(dir), Some(name)) = (track.path.parent(), track.path.file_name()) {
                    let shadow = dir.join(format!("._{}", name.to_string_lossy()));
                    if shadow.exists() {
                        let _ = crate::macos::trash(&shadow);
                    }
                }
                moved.push(track.id);
            }
            Err(err) => failed.push(format!("{}: {err}", track.title)),
        }
    }
    let save = {
        let mut guard = sorter.session.lock().expect("session lock");
        guard.as_mut().and_then(|s| {
            s.likes = Likes::load(&s.root, &s.tracks);
            let mut last = None;
            for track in s.tracks.iter().filter(|t| moved.contains(&t.id)) {
                last = s.likes.forget(track).or(last);
            }
            // Drop the trashed tracks, renumbering queue and history.
            let mut next = 0;
            let map: Vec<Option<usize>> = s
                .tracks
                .iter()
                .map(|t| {
                    if moved.contains(&t.id) {
                        None
                    } else {
                        next += 1;
                        Some(next - 1)
                    }
                })
                .collect();
            s.queue = s.queue.iter().filter_map(|&i| map[i]).collect();
            s.history = s
                .history
                .iter()
                .filter_map(|&(i, r)| map[i].map(|j| (j, r)))
                .collect();
            s.tracks.retain(|t| !moved.contains(&t.id));
            last.map(|save| (save, s.root.clone()))
        })
    };
    if let Some((save, root)) = save {
        write_ratings(app, save, root);
    }
    (moved.len(), failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(n: usize, title: &str) -> Track {
        Track {
            id: format!("local-{n:016x}"),
            path: PathBuf::from(format!("/nowhere/{n:02} {title}.flac")),
            title: title.into(),
            artist: "林俊杰".into(),
            album: String::new(),
            duration: 200.0,
            has_art: false,
            size: 0,
        }
    }

    #[test]
    fn versions_fold_to_one_key() {
        assert_eq!(title_key("小酒窩"), "小酒窝");
        assert_eq!(title_key("小酒窝(feat.蔡卓妍)（粤语版）"), "小酒窝");
        assert_eq!(title_key("主角 (ft.MACHI)"), "主角");
        assert_eq!(title_key("K-O"), "ko");
    }

    #[test]
    fn namesakes_follow_the_first_of_them() {
        let tracks = [
            track(0, "小酒窩"),
            track(1, "江南"),
            track(2, "主角"),
            track(3, "小酒窝 (Live)"),
            track(4, "江南"),
        ];
        let likes = Likes::load(Path::new("/nowhere"), &tracks);
        assert_eq!(queue_of(&tracks, &likes), vec![0, 3, 1, 4, 2]);
    }
}
