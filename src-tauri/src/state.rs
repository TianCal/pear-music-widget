//! The player state machine: one snapshot, pushed to every window that renders
//! it. The floating widget and the menu-bar dropdown are two views of the same
//! state, so they can never disagree.
//!
//! Repeat is deliberately absent: `POST /switch-repeat` is accepted but
//! `GET /repeat-mode` always answers null on the shipped api-server plugin, so
//! the widget can never show a truthful repeat state.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::api::{Api, ErrorCode};
use crate::lyrics::{self, Lyrics};
use crate::search::{self, UpNextItem};
use crate::store::Store;

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
    pub song: Option<Song>,
    /// Artwork as a `data:` URL — see `api::fetch_cover`.
    pub cover: Option<String>,
    pub upnext: Vec<UpNextItem>,
    pub lyrics: Option<Lyrics>,
    /// idle | loading | ready | none
    pub lyrics_state: String,
    pub is_playing: bool,
    pub position: f64,
    pub volume: f64,
    pub muted: bool,
    pub shuffle: bool,
    pub like: Option<String>,
}

impl PlayerState {
    fn new(skin: String, panel_skin: String, tint: f64) -> Self {
        Self {
            status: "connecting".into(),
            status_message: String::new(),
            skin,
            panel_skin,
            tint,
            song: None,
            cover: None,
            upnext: Vec::new(),
            lyrics: None,
            lyrics_state: "idle".into(),
            is_playing: false,
            position: 0.0,
            volume: 100.0,
            muted: false,
            shuffle: false,
            like: None,
        }
    }
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
    volume: Mutex<VolumeCalibration>,
    /// Window labels with a lyrics panel open. No point reaching out to the
    /// network for a panel nobody is looking at.
    lyrics_wanted_by: Mutex<HashSet<String>>,
    cover_token: AtomicU64,
    upnext_token: AtomicU64,
    lyrics_token: AtomicU64,
}

impl Core {
    pub fn new(app: AppHandle, store: Arc<Store>, api: Arc<Api>) -> Self {
        let (skin, panel_skin) = (crate::window::skin_of(&store), crate::window::panel_skin_of(&store));
        let tint = store.get(|s| s.tint.clamp(0.0, 1.0));
        Self {
            app,
            store,
            api,
            player: Mutex::new(PlayerState::new(skin, panel_skin, tint)),
            volume: Mutex::new(VolumeCalibration::default()),
            lyrics_wanted_by: Mutex::new(HashSet::new()),
            cover_token: AtomicU64::new(0),
            upnext_token: AtomicU64::new(0),
            lyrics_token: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> PlayerState {
        self.player.lock().expect("player lock").clone()
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
            state.cover = None;
            state.like = None;
        });

        let Some(song) = song else { return };

        let token = self.cover_token.fetch_add(1, Ordering::SeqCst) + 1;
        let (cover, like) = tokio::join!(
            self.api.fetch_cover(song.image_src.as_deref()),
            self.api.like_state(),
        );
        if token != self.cover_token.load(Ordering::SeqCst) {
            return; // a newer song won the race
        }

        let like = like
            .ok()
            .flatten()
            .and_then(|value| value.get("state").and_then(Value::as_str).map(str::to_string));
        self.update(|state| {
            state.cover = cover;
            state.like = like;
        });

        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            core.refresh_upnext().await;
            core.refresh_lyrics().await;
        });
    }

    /// "Next tracks" for the stack skin. Only fetched when that skin is showing
    /// on *either* surface — it is an extra request plus artwork on every track
    /// change.
    pub async fn refresh_upnext(self: &Arc<Self>) {
        let wanted = crate::window::skin_of(&self.store) == "stack"
            || crate::window::panel_skin_of(&self.store) == "stack";
        if !wanted {
            self.update(|state| state.upnext.clear());
            return;
        }

        let token = self.upnext_token.fetch_add(1, Ordering::SeqCst) + 1;
        let Ok(Some(queue)) = self.api.queue().await else {
            return;
        };
        if token != self.upnext_token.load(Ordering::SeqCst) {
            return;
        }

        let items = search::parse_queue_upcoming(&queue, 4);
        let mut resolved = Vec::with_capacity(items.len());
        for item in items {
            let thumbnail = self.api.fetch_cover(item.thumbnail.as_deref()).await;
            resolved.push(UpNextItem { thumbnail, ..item });
        }
        if token != self.upnext_token.load(Ordering::SeqCst) {
            return;
        }

        self.update(|state| state.upnext = resolved);
    }

    /// Lyrics come from LRCLib (see `lyrics.rs`) and are only fetched while a
    /// lyrics panel is actually open.
    pub async fn refresh_lyrics(self: &Arc<Self>) {
        let wanted = !self.lyrics_wanted_by.lock().expect("lyrics lock").is_empty();
        let song = self.player.lock().expect("player lock").song.clone();

        let Some(song) = song.filter(|_| wanted) else {
            self.update(|state| {
                state.lyrics = None;
                state.lyrics_state = "idle".into();
            });
            return;
        };

        let token = self.lyrics_token.fetch_add(1, Ordering::SeqCst) + 1;
        self.update(|state| {
            state.lyrics = None;
            state.lyrics_state = "loading".into();
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

        self.update(|state| {
            state.lyrics_state = if result.is_some() { "ready" } else { "none" }.into();
            state.lyrics = result;
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
    /// optimistic defaults until the player emits its first change event.
    pub async fn refresh_all(self: &Arc<Self>) {
        let (song, shuffle, volume) = tokio::join!(
            self.api.song(),
            self.api.shuffle_state(),
            self.api.volume_state()
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
            tauri::async_runtime::spawn(async move { core.refresh_upnext().await });
        }

        let shuffle = shuffle
            .ok()
            .flatten()
            .and_then(|value| value.get("state").and_then(Value::as_bool));
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
            self.update(|state| {
                state.is_playing = false;
                state.upnext.clear();
            });
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
}
