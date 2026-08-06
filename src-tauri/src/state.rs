//! The player state machine: one snapshot, pushed to every window that renders
//! it. The floating widget and the menu-bar dropdown are two views of the same
//! state, so they can never disagree.
//!
//! `repeat` is the player's own mode string — `ALL`, `ONE`, `NONE`, or `None`
//! until the player has told us. It is pushed on `REPEAT_CHANGED` and pulled by
//! `refresh_all`; see `commands::command` for why the button only ever cycles
//! between `ALL` and `ONE`.
//!
//! **The heavy fields travel on their own channels.** Artwork, the queue and the
//! lyrics are each an order of magnitude larger than everything else put
//! together, and they change once a track where `position` changes once a
//! second. Tauri's `emit` serialises the payload and then formats *a separate
//! JS source string per webview* to eval, so a cover riding along on the
//! position tick was being copied five times a second and parsed twice as
//! JavaScript — for a number that moved by one. Hence `cover`, `queue` and
//! `lyrics` are separate events, pushed only when they actually change.
//!
//! The queue goes one step further: it carries **text only**. Artwork for its
//! rows is asked for by id, for the rows a surface is actually showing, and
//! comes back in a command reply — see `commands::queue_art`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::api::{Api, ErrorCode, COVER_PX};
use crate::lyrics::{self, LyricLine, Lyrics};
use crate::search::{self, QueueView, QUEUE_MAX};
use crate::store::{SkinCorners, Store};

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Song {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub song_duration: f64,
    pub image_src: Option<String>,
    pub url: Option<String>,
    pub media_type: Option<String>,
}

impl Song {
    /// The socket and the REST route describe a track slightly differently;
    /// both funnel through here. A payload with no videoId is not a track.
    fn from_value(raw: Option<&Value>) -> Option<Self> {
        let raw = raw?;
        let video_id = raw.get("videoId").and_then(Value::as_str)?;
        if video_id.is_empty() {
            return None;
        }

        let text = |key: &str, fallback: &str| {
            raw.get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or(fallback)
                .to_string()
        };
        let optional = |key: &str| {
            raw.get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };

        Some(Self {
            video_id: video_id.to_string(),
            title: text("title", "Unknown title"),
            artist: text("artist", ""),
            album: text("album", ""),
            song_duration: raw
                .get("songDuration")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            image_src: optional("imageSrc"),
            url: optional("url"),
            media_type: optional("mediaType"),
        })
    }
}

/// Everything small enough to ride the once-a-second position tick. Around
/// 400 bytes serialised, and nothing in here grows with the track.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerState {
    /// connecting | connected | offline | unauthorized | denied
    pub status: String,
    pub status_message: String,
    pub skin: String,
    pub panel_skin: String,
    /// How strongly the cover's colours wash the card, 0..=1. Mixed in the
    /// renderer, so it travels with the rest of the state.
    pub tint: f64,
    /// Seconds to shift the lyric roll by, positive turning the lines over
    /// earlier. The roll is driven in the renderer, so — like `tint` — the
    /// setting has to reach it as state rather than staying in the store.
    pub lyrics_offset: f64,
    /// Which top-right toggles to draw, for **every** skin rather than for the
    /// current one: the two surfaces can be on different skins, and they share
    /// this one snapshot, so each picks its own set out of the map. A handful of
    /// booleans on a payload this size is nothing, and the alternative — the
    /// renderer asking — would leave the buttons a frame behind the menu that
    /// turned them off.
    pub corners: SkinCorners,
    /// Seconds of stillness before they fade; 0 keeps them up. Driven in the
    /// renderer, so — like `tint` — it travels as state.
    pub corners_autohide: f64,
    pub song: Option<Song>,
    pub is_playing: bool,
    pub position: f64,
    pub volume: f64,
    pub muted: bool,
    pub shuffle: bool,
    /// `ALL` | `ONE` | `NONE`, or `None` while the player has not said. The
    /// renderer draws the loop unlit for the last two, so an unknown mode
    /// cannot be mistaken for repeat being on.
    pub repeat: Option<String>,
    pub like: Option<String>,
}

impl PlayerState {
    fn new(
        skin: String,
        panel_skin: String,
        tint: f64,
        lyrics_offset: f64,
        corners: SkinCorners,
        corners_autohide: f64,
    ) -> Self {
        Self {
            status: "connecting".into(),
            status_message: String::new(),
            skin,
            panel_skin,
            tint,
            lyrics_offset,
            corners,
            corners_autohide,
            song: None,
            is_playing: false,
            position: 0.0,
            volume: 100.0,
            muted: false,
            shuffle: false,
            repeat: None,
            like: None,
        }
    }
}

/// The lyrics panel's whole world, pushed on the `lyrics` event. The state
/// string travels with the words so the note under the roll can never describe
/// a different fetch than the one showing.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsView {
    /// idle | loading | ready | none
    pub state: String,
    pub lyrics: Option<Arc<Lyrics>>,
}

impl LyricsView {
    fn idle() -> Self {
        Self {
            state: "idle".into(),
            lyrics: None,
        }
    }

    /// The same words in Simplified Chinese. Only the lines are touched — the
    /// state string and `how` describe the fetch, not the text.
    ///
    /// Text that is already simplified, or not Chinese at all, comes back
    /// unchanged, so this is safe to run over every line rather than trying to
    /// guess whether a track needs it.
    fn simplified(&self) -> Self {
        let Some(lyrics) = &self.lyrics else {
            return self.clone();
        };
        Self {
            state: self.state.clone(),
            lyrics: Some(Arc::new(Lyrics {
                synced: lyrics.synced,
                how: lyrics.how,
                lines: lyrics
                    .lines
                    .iter()
                    .map(|line| LyricLine {
                        time: line.time,
                        text: fast2s::convert(&line.text),
                    })
                    .collect(),
            })),
        }
    }
}

/// What a freshly loaded window needs before its first event arrives: the
/// state plus the three channels it would otherwise have to wait a track for.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bootstrap {
    pub state: PlayerState,
    /// Artwork as a `data:` URL — see `api::fetch_cover`.
    pub cover: Option<Arc<str>>,
    pub queue: QueueView,
    pub lyrics: LyricsView,
    /// The panel the widget was last left showing, for the renderer to reopen.
    pub panel: Option<String>,
}

// ------------------------------------------------------------------- volume

/// YouTube Music applies its own curve between the volume we POST and the
/// volume it reports back — measured on 3.12.0 as
/// `reported = 100*(15^(sent/100)-1)/14`, so POSTing 80 echoes back as 55.
/// Rather than hardcode that curve (it comes from a plugin the user can turn
/// off), treat the value we set as the truth for a moment afterwards. Without
/// this the slider visibly snaps to the curved number about a second after the
/// user lets go of the drag.
const VOLUME_ECHO: Duration = Duration::from_millis(1500);

#[derive(Default)]
struct VolumeCalibration {
    echo_until: Option<Instant>,
    we_set: Option<f64>,
    /// `None` = not calibrated yet, `Some(1.0)` = no curve applied.
    curve_base: Option<f64>,
    send_seq: u64,
    awaiting_echo: Option<(f64, u64)>,
}

impl VolumeCalibration {
    fn own_echo(&self) -> Option<f64> {
        match (self.echo_until, self.we_set) {
            (Some(until), Some(value)) if Instant::now() < until => Some(value),
            _ => None,
        }
    }

    /// Map a reported value back onto the scale the slider actually uses, which
    /// is what keeps a late echo — or a volume change made inside YouTube Music
    /// — from yanking the slider onto a different scale.
    fn reported_to_slider(&self, reported: f64) -> f64 {
        match self.curve_base {
            Some(base) if base > 1.001 => {
                let slider = 100.0 * (1.0 + (base - 1.0) * reported / 100.0).ln() / base.ln();
                slider.clamp(0.0, 100.0).round()
            }
            _ => reported,
        }
    }
}

/// Numerically solve `b` for a single observed (sent → echoed) pair, where the
/// reported value follows `100*(b^(x/100)-1)/(b-1)`.
///
/// Calibration is display-only and can never send the wrong volume: the volume
/// command always POSTs the raw slider value. It can fail — you only ever set
/// volume at the extremes, no echo arrives, or the pair does not match the
/// expected shape — and the consequence is limited to a volume changed *outside*
/// the widget showing on the player's raw gain scale. It self-corrects on the
/// first mid-range drag.
fn solve_volume_curve(sent: f64, echoed: f64) -> Option<f64> {
    let u = sent / 100.0;
    let v = echoed / 100.0;

    if u <= 0.05 || u >= 0.95 {
        return None; // endpoints are fixed for every b
    }
    if (sent - echoed).abs() <= 2.0 {
        return Some(1.0); // identity
    }
    if v <= 0.0 || v >= u {
        return None; // not the shape we expect
    }

    // (b^u - 1)/(b - 1) decreases monotonically in b, so bisect in log space.
    let mut lo = 1.0001f64;
    let mut hi = 1e6f64;
    for _ in 0..60 {
        let mid = (lo * hi).sqrt();
        if (mid.powf(u) - 1.0) / (mid - 1.0) > v {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some((lo * hi).sqrt())
}

// --------------------------------------------------------------------- core

/// Everything the app shares. Held as Tauri managed state and cloned (as an
/// `Arc`) into the realtime task and the pollers.
pub struct Core {
    pub app: AppHandle,
    pub store: Arc<Store>,
    pub api: Arc<Api>,
    player: Mutex<PlayerState>,
    /// The three heavy channels. Separate locks as well as separate events:
    /// resolving artwork must not block a position tick behind it.
    cover: Mutex<Option<Arc<str>>>,
    queue: Mutex<QueueView>,
    /// What the renderers are showing, script conversion already applied.
    lyrics: Mutex<LyricsView>,
    /// The same words as they were fetched. Kept because the conversion is
    /// one-way — converted text cannot be turned back — so switching Simplified
    /// Chinese off has to re-derive from these rather than from what is on
    /// screen, and doing that without a round trip to LRCLib is the point.
    lyrics_source: Mutex<LyricsView>,
    volume: Mutex<VolumeCalibration>,
    /// Window labels with a lyrics panel open. No point reaching out to the
    /// network for a panel nobody is looking at.
    lyrics_wanted_by: Mutex<HashSet<String>>,
    /// The same, for the queue panel. The skin half of "is anyone looking" is
    /// answered from the store instead — see `queue_wanted`.
    queue_wanted_by: Mutex<HashSet<String>>,
    cover_token: AtomicU64,
    queue_token: AtomicU64,
    lyrics_token: AtomicU64,
}

impl Core {
    pub fn new(app: AppHandle, store: Arc<Store>, api: Arc<Api>) -> Self {
        let (skin, panel_skin) = (crate::window::skin_of(&store), crate::window::panel_skin_of(&store));
        let tint = store.get(|s| s.tint.clamp(0.0, 1.0));
        // The menu offers ±2s; the file is documented as hand-editable, so a
        // wider nudge typed in by hand is honoured — just not one that would
        // park the roll a whole verse away from the music.
        let lyrics_offset = store.get(|s| s.lyrics_offset.clamp(-10.0, 10.0));
        let corners = store.get(|s| s.corners.clone());
        // Hand-editable file, so clamp rather than trust.
        let corners_autohide = store.get(|s| s.corners_autohide.clamp(0.0, 60.0));
        let queue_wanted_by = match store.get(|s| s.panel.clone()).as_deref() {
            Some("queue") => HashSet::from([crate::window::WIDGET.to_string()]),
            _ => HashSet::new(),
        };
        Self {
            app,
            store,
            api,
            player: Mutex::new(PlayerState::new(skin, panel_skin, tint, lyrics_offset, corners, corners_autohide)),
            cover: Mutex::new(None),
            queue: Mutex::new(QueueView::empty()),
            lyrics: Mutex::new(LyricsView::idle()),
            lyrics_source: Mutex::new(LyricsView::idle()),
            volume: Mutex::new(VolumeCalibration::default()),
            lyrics_wanted_by: Mutex::new(HashSet::new()),
            // `window::create` restores the stored panel before the window is
            // ever on screen, so the widget can come up with a queue panel open
            // and ask for its state before the renderer has told us it is open.
            // Seeding from the same store entry is what stops that first paint
            // being an empty list corrected a round trip later.
            queue_wanted_by: Mutex::new(queue_wanted_by),
            cover_token: AtomicU64::new(0),
            queue_token: AtomicU64::new(0),
            lyrics_token: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> PlayerState {
        self.player.lock().expect("player lock").clone()
    }

    /// Everything a window needs to draw itself from cold.
    pub fn bootstrap(&self) -> Bootstrap {
        Bootstrap {
            state: self.snapshot(),
            cover: self.cover.lock().expect("cover lock").clone(),
            queue: self.queue.lock().expect("queue lock").clone(),
            lyrics: self.lyrics.lock().expect("lyrics lock").clone(),
            panel: self.store.get(|s| s.panel.clone()),
        }
    }

    pub fn status(&self) -> String {
        self.player.lock().expect("player lock").status.clone()
    }

    /// Apply a patch and push to the renderers only when something actually
    /// changed. Both windows get the same snapshot.
    pub fn update(&self, patch: impl FnOnce(&mut PlayerState)) {
        let next = {
            let mut player = self.player.lock().expect("player lock");
            let before = player.clone();
            patch(&mut player);
            if *player == before {
                return;
            }
            player.clone()
        };
        let _ = self.app.emit("state", &next);
    }

    // ------------------------------------------------------- heavy channels

    /// Each of these pushes only on a real change, and drops its lock before
    /// emitting: `emit` formats the payload once per webview, which is not work
    /// to be holding a lock the position tick wants through.
    fn set_cover(&self, cover: Option<Arc<str>>) {
        {
            let mut held = self.cover.lock().expect("cover lock");
            if *held == cover {
                return;
            }
            *held = cover.clone();
        }
        let _ = self.app.emit("cover", &cover);
    }

    fn set_queue(&self, view: QueueView) {
        {
            let mut held = self.queue.lock().expect("queue lock");
            if *held == view {
                return;
            }
            *held = view.clone();
        }
        let _ = self.app.emit("queue", &view);
    }

    /// The artwork source URL a row was parsed with, for `queue_art` to resolve.
    /// Looked up by video id rather than by index so a request that crosses a
    /// queue change is applied to the right track or to none.
    pub fn queue_art_src(&self, video_id: &str) -> Option<Arc<str>> {
        self.queue
            .lock()
            .expect("queue lock")
            .items
            .iter()
            .find(|track| track.video_id == video_id)
            .and_then(|track| track.art.clone())
    }

    fn set_lyrics(&self, view: LyricsView) {
        *self.lyrics_source.lock().expect("lyrics lock") = view.clone();
        self.publish_lyrics(view);
    }

    /// Convert if the setting asks for it, then push — still only on a real
    /// change, since a track whose words are already simplified converts to
    /// itself and must not re-emit for every window on every fetch.
    ///
    /// The store is read before the lyrics lock is taken, never inside it.
    fn publish_lyrics(&self, view: LyricsView) {
        let view = if self.store.get(|s| s.simplify_lyrics) {
            view.simplified()
        } else {
            view
        };
        {
            let mut held = self.lyrics.lock().expect("lyrics lock");
            if *held == view {
                return;
            }
            *held = view.clone();
        }
        let _ = self.app.emit("lyrics", &view);
    }

    /// Re-derive the shown words from the fetched ones. The Simplified Chinese
    /// toggle is the only caller: it has to reach the panel that is open now,
    /// not just the next track's words.
    pub fn restyle_lyrics(&self) {
        let source = self.lyrics_source.lock().expect("lyrics lock").clone();
        self.publish_lyrics(source);
    }

    // ---------------------------------------------------------------- tracks

    /// Swap in a new song, then resolve its artwork and like state out of band.
    pub async fn apply_song(self: &Arc<Self>, raw: Option<&Value>) {
        let song = Song::from_value(raw);
        let current = self.player.lock().expect("player lock").song.clone();

        if song.as_ref().map(|s| &s.video_id) == current.as_ref().map(|s| &s.video_id) {
            // Same track: metadata may still have been enriched (album, duration).
            if let Some(song) = song {
                self.update(|state| state.song = Some(song));
            }
            return;
        }

        self.update(|state| {
            state.song = song.clone();
            state.like = None;
        });
        self.set_cover(None);

        let Some(song) = song else { return };

        let token = self.cover_token.fetch_add(1, Ordering::SeqCst) + 1;
        let (cover, like) = tokio::join!(
            self.api.fetch_cover(song.image_src.as_deref(), COVER_PX),
            self.api.like_state(),
        );
        if token != self.cover_token.load(Ordering::SeqCst) {
            return; // a newer song won the race
        }

        let like = like
            .ok()
            .flatten()
            .and_then(|value| value.get("state").and_then(Value::as_str).map(str::to_string));
        self.set_cover(cover);
        self.update(|state| state.like = like);

        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            core.refresh_queue().await;
            core.refresh_lyrics().await;
        });
    }

    /// Whether any surface is looking at the queue — a skin whose layout draws
    /// it, or a queue panel open on that surface. The same bargain as the
    /// lyrics: a request on every track change is not work to do for nobody.
    ///
    /// The skin half is answered from the store, because a skin is a property of
    /// the surface rather than something the renderer has to report.
    fn queue_wanted(&self) -> bool {
        if crate::window::skin_shows_queue(&crate::window::skin_of(&self.store))
            || crate::window::skin_shows_queue(&crate::window::panel_skin_of(&self.store))
        {
            return true;
        }
        !self.queue_wanted_by.lock().expect("queue lock").is_empty()
    }

    pub fn set_queue_wanted(&self, label: &str, wanted: bool) {
        let mut open = self.queue_wanted_by.lock().expect("queue lock");
        if wanted {
            open.insert(label.to_string());
        } else {
            open.remove(label);
        }
    }

    /// The whole queue, for the queue panel and Stack's "Next tracks" strip
    /// alike — one fetch, one channel, two readers.
    ///
    /// No artwork is resolved here. That is the difference from the four-item
    /// version this replaced: a full queue's worth of `data:` URLs would be
    /// megabytes on an event, and the two surfaces want different sizes anyway.
    pub async fn refresh_queue(self: &Arc<Self>) {
        if !self.queue_wanted() {
            self.set_queue(QueueView::empty());
            return;
        }

        let token = self.queue_token.fetch_add(1, Ordering::SeqCst) + 1;
        let Ok(Some(queue)) = self.api.queue().await else {
            return;
        };
        // One await inside the guarded region, so one check: if a newer refresh
        // started while this response was in flight, its result is the truth.
        if token != self.queue_token.load(Ordering::SeqCst) {
            return;
        }

        self.set_queue(search::parse_queue(Some(&queue), QUEUE_MAX));
    }

    /// Lyrics come from LRCLib (see `lyrics.rs`) and are only fetched while a
    /// lyrics panel is actually open.
    pub async fn refresh_lyrics(self: &Arc<Self>) {
        let wanted = !self.lyrics_wanted_by.lock().expect("lyrics lock").is_empty();
        let song = self.player.lock().expect("player lock").song.clone();

        let Some(song) = song.filter(|_| wanted) else {
            self.set_lyrics(LyricsView::idle());
            return;
        };

        let token = self.lyrics_token.fetch_add(1, Ordering::SeqCst) + 1;
        self.set_lyrics(LyricsView {
            state: "loading".into(),
            lyrics: None,
        });

        let result = lyrics::fetch_lyrics(self.api.client(), &song).await;

        // A newer request, or the track moved on while we were waiting.
        if token != self.lyrics_token.load(Ordering::SeqCst) {
            return;
        }
        let still_playing = self
            .player
            .lock()
            .expect("player lock")
            .song
            .as_ref()
            .map(|s| s.video_id.clone())
            == Some(song.video_id.clone());
        if !still_playing {
            return;
        }

        self.set_lyrics(LyricsView {
            state: if result.is_some() { "ready" } else { "none" }.into(),
            lyrics: result.map(Arc::new),
        });
    }

    pub fn set_lyrics_wanted(&self, label: &str, wanted: bool) {
        let mut open = self.lyrics_wanted_by.lock().expect("lyrics lock");
        if wanted {
            open.insert(label.to_string());
        } else {
            open.remove(label);
        }
    }

    /// Ground truth pull — the websocket's initial values for shuffle/volume are
    /// optimistic defaults until the player emits its first change event, and
    /// `REPEAT_CHANGED` only ever fires on a *change*, so the mode has to be
    /// asked for once per connection or the loop would sit unlit until someone
    /// pressed it.
    pub async fn refresh_all(self: &Arc<Self>) {
        let (song, shuffle, volume, repeat) = tokio::join!(
            self.api.song(),
            self.api.shuffle_state(),
            self.api.volume_state(),
            self.api.repeat_mode()
        );

        let song = song.ok().flatten();
        if let Some(song) = &song {
            self.apply_song(Some(song)).await;
        }

        // `apply_song` only refreshes the queue when the track actually
        // changed, so a reconnect on the same track would otherwise leave
        // "Next tracks" empty until the song ended.
        {
            let core = Arc::clone(self);
            tauri::async_runtime::spawn(async move { core.refresh_queue().await });
        }

        let shuffle = shuffle
            .ok()
            .flatten()
            .and_then(|value| value.get("state").and_then(Value::as_bool));
        // Null is a real answer here — it means the player has not reported a
        // mode yet — so it stays `None` rather than being defaulted to NONE.
        let repeat = repeat
            .ok()
            .flatten()
            .and_then(|value| value.get("mode").and_then(Value::as_str).map(str::to_string));
        let volume = volume.ok().flatten();
        let reported = volume
            .as_ref()
            .and_then(|value| value.get("state").and_then(Value::as_f64));
        let muted = volume
            .as_ref()
            .and_then(|value| value.get("isMuted").and_then(Value::as_bool));

        let level = {
            let calibration = self.volume.lock().expect("volume lock");
            calibration
                .own_echo()
                .or_else(|| reported.map(|value| calibration.reported_to_slider(value)))
        };

        self.update(|state| {
            if let Some(shuffle) = shuffle {
                state.shuffle = shuffle;
            }
            if repeat.is_some() {
                state.repeat = repeat.clone();
            }
            if let Some(level) = level {
                state.volume = level;
            }
            if let Some(muted) = muted {
                state.muted = muted;
            }
            if let Some(song) = &song {
                if let Some(paused) = song.get("isPaused").and_then(Value::as_bool) {
                    state.is_playing = !paused;
                }
                if let Some(elapsed) = song.get("elapsedSeconds").and_then(Value::as_f64) {
                    state.position = elapsed;
                }
            }
        });
    }

    // -------------------------------------------------------------- realtime

    pub fn set_status(self: &Arc<Self>, status: &str, message: &str) {
        self.update(|state| {
            state.status = status.to_string();
            state.status_message = message.to_string();
        });

        if status == "connected" {
            let core = Arc::clone(self);
            tauri::async_runtime::spawn(async move { core.refresh_all().await });
        } else if status != "connecting" {
            // Drop the queue too: it belongs to a player we can no longer see.
            self.update(|state| state.is_playing = false);
            self.set_queue(QueueView::empty());
        }

        crate::tray::refresh(&self.app);
    }

    pub async fn handle_message(self: &Arc<Self>, msg: &Value) {
        let kind = msg.get("type").and_then(Value::as_str).unwrap_or_default();
        let number = |key: &str| msg.get(key).and_then(Value::as_f64);
        let flag = |key: &str| msg.get(key).and_then(Value::as_bool).unwrap_or(false);

        match kind {
            "PLAYER_INFO" => {
                self.apply_song(msg.get("song")).await;
                let volume = number("volume");
                self.update(|state| {
                    state.is_playing = flag("isPlaying");
                    state.position = number("position").unwrap_or(0.0);
                    if let Some(volume) = volume {
                        state.volume = volume;
                    }
                    state.muted = flag("muted");
                    state.shuffle = flag("shuffle");
                });
            }
            "VIDEO_CHANGED" => {
                self.apply_song(msg.get("song")).await;
                // The payload carries the new track's paused flag; the player
                // does not always follow a track change with PLAYER_STATE_CHANGED.
                let paused = msg
                    .get("song")
                    .and_then(|song| song.get("isPaused"))
                    .and_then(Value::as_bool);
                self.update(|state| {
                    state.position = number("position").unwrap_or(0.0);
                    if let Some(paused) = paused {
                        state.is_playing = !paused;
                    }
                });
            }
            "PLAYER_STATE_CHANGED" => {
                let position = number("position");
                self.update(|state| {
                    state.is_playing = flag("isPlaying");
                    if let Some(position) = position {
                        state.position = position;
                    }
                });
            }
            "POSITION_CHANGED" => {
                let position = number("position").unwrap_or(0.0);
                self.update(|state| state.position = position);
            }
            "VOLUME_CHANGED" => {
                let reported = number("volume");
                let muted = flag("muted");

                let level = {
                    let mut calibration = self.volume.lock().expect("volume lock");

                    // Only calibrate against the most recent send: mid-drag
                    // there are several echoes in flight and none of them pairs
                    // with the value we last set.
                    if let (Some(reported), Some((sent, seq))) =
                        (reported, calibration.awaiting_echo)
                    {
                        if seq == calibration.send_seq {
                            if let Some(base) = solve_volume_curve(sent, reported) {
                                calibration.curve_base = Some(base);
                            }
                            calibration.awaiting_echo = None;
                        }
                    }

                    calibration
                        .own_echo()
                        .or_else(|| reported.map(|value| calibration.reported_to_slider(value)))
                };

                self.update(|state| {
                    if let Some(level) = level {
                        state.volume = level;
                    }
                    state.muted = muted;
                });
            }
            "SHUFFLE_CHANGED" => {
                let shuffle = flag("shuffle");
                self.update(|state| state.shuffle = shuffle);
            }
            // Fires for a press inside YouTube Music as well as for our own, so
            // this is what keeps the loop honest when the mode is changed
            // somewhere else.
            "REPEAT_CHANGED" => {
                let repeat = msg
                    .get("repeat")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if repeat.is_some() {
                    self.update(|state| state.repeat = repeat.clone());
                }
            }
            _ => {}
        }
    }

    // -------------------------------------------------------------- commands

    /// Remember what we just set so our own echoes do not fight the cursor, and
    /// record the pair the curve is calibrated from.
    pub fn note_volume_send(&self, value: f64) {
        let mut calibration = self.volume.lock().expect("volume lock");
        calibration.we_set = Some(value);
        calibration.echo_until = Some(Instant::now() + VOLUME_ECHO);
        calibration.send_seq += 1;
        calibration.awaiting_echo = Some((value, calibration.send_seq));
    }

    /// A command failing because the server went away should move the whole app
    /// to that state, not just report an error into the void.
    pub fn note_error(self: &Arc<Self>, code: ErrorCode, message: &str) {
        match code {
            ErrorCode::Offline => self.set_status("offline", message),
            ErrorCode::Unauthorized => self.set_status("unauthorized", message),
            _ => return,
        }
        crate::ws::retry(&self.app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learns_the_players_curve_from_one_pair() {
        // reported = 100*(15^(sent/100)-1)/14 — the shape measured on 3.12.0.
        let sent = 80.0;
        let echoed = 100.0 * (15f64.powf(sent / 100.0) - 1.0) / 14.0;
        let base = solve_volume_curve(sent, echoed).expect("solvable");
        assert!((base - 15.0).abs() < 0.01, "expected b≈15, got {base}");

        let calibration = VolumeCalibration {
            curve_base: Some(base),
            ..Default::default()
        };
        // ...and maps the echo back onto the slider's own scale.
        assert_eq!(calibration.reported_to_slider(echoed), 80.0);
    }

    #[test]
    fn refuses_to_calibrate_off_the_endpoints() {
        assert_eq!(solve_volume_curve(0.0, 0.0), None);
        assert_eq!(solve_volume_curve(100.0, 100.0), None);
        // An echo louder than what we sent is not the shape we expect.
        assert_eq!(solve_volume_curve(50.0, 70.0), None);
        // No curve applied at all reads as identity, not as a failure.
        assert_eq!(solve_volume_curve(50.0, 50.0), Some(1.0));
    }

    #[test]
    fn an_uncalibrated_slider_shows_the_raw_value() {
        let calibration = VolumeCalibration::default();
        assert_eq!(calibration.reported_to_slider(55.0), 55.0);
    }

    #[test]
    fn simplifies_the_words_and_leaves_the_rest_alone() {
        let view = LyricsView {
            state: "ready".into(),
            lyrics: Some(Arc::new(Lyrics {
                synced: true,
                how: "exact",
                lines: vec![
                    LyricLine {
                        time: Some(0.0),
                        text: "後來我總算學會了如何去愛".into(),
                    },
                    // The reason this is not a character table: 乾 is 干 in
                    // 乾淨 and stays 乾 in 乾坤. Any per-character mapping gets
                    // one of those two wrong, in the same line.
                    LyricLine {
                        time: Some(1.0),
                        text: "乾淨的乾坤".into(),
                    },
                    // Already simplified, and not Chinese at all: both have to
                    // survive untouched, since every line is converted rather
                    // than the track being sniffed first.
                    LyricLine {
                        time: Some(2.0),
                        text: "Baby, already 简体 and ASCII".into(),
                    },
                ],
            })),
        };

        let converted = view.simplified();
        let lyrics = converted.lyrics.as_ref().expect("lyrics");
        assert_eq!(lyrics.lines[0].text, "后来我总算学会了如何去爱");
        assert_eq!(lyrics.lines[1].text, "干净的乾坤");
        assert_eq!(lyrics.lines[2].text, "Baby, already 简体 and ASCII");

        // Timings and the fetch's own description belong to the fetch, not the
        // text — losing a timestamp here would stop the roll following along.
        assert_eq!(lyrics.lines[0].time, Some(0.0));
        assert_eq!(lyrics.lines[2].time, Some(2.0));
        assert!(lyrics.synced);
        assert_eq!(lyrics.how, "exact");
        assert_eq!(converted.state, "ready");
    }

    #[test]
    fn a_track_with_no_lyrics_converts_to_itself() {
        let idle = LyricsView::idle();
        assert_eq!(idle.simplified(), idle);
    }
}
