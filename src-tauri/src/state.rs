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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};

use crate::api::{Api, ErrorCode, COVER_PX};
use crate::jyutping::Jyutping;
use crate::lyrics::{self, LyricLine, Lyrics};
use crate::search::{self, ArtistLink, QueueView, QUEUE_MAX};
use crate::store::{SkinCorners, Store};

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Song {
    pub video_id: String,
    pub title: String,
    /// The credit as shown — `artistNames` applied. Lyrics look this up.
    pub artist: String,
    /// The credit as the source gave it, for the rename menu and for
    /// re-applying `artistNames` after it changes.
    #[serde(skip)]
    pub raw_artist: String,
    pub album: String,
    pub song_duration: f64,
    pub image_src: Option<String>,
    pub url: Option<String>,
    pub media_type: Option<String>,
    /// Each credited artist with a YouTube Music page, for the artist popup.
    /// Empty for local files and for uploads credited to a channel.
    pub artists: Vec<ArtistLink>,
    /// `/song`'s own title and credit. `Core::dress` relabels the track from
    /// its queue slot; lyrics are still looked up with these (`for_lyrics`).
    /// Empty for local files.
    #[serde(skip)]
    pub api_title: String,
    #[serde(skip)]
    pub api_artist: String,
    /// The artist channel from `/song`'s `artistUrl`: the link to use until
    /// the queue slot gives the real ones. Only for songs and official videos,
    /// whose page owner is the artist rather than an uploader.
    #[serde(skip)]
    pub channel: Option<String>,
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

        let media_type = optional("mediaType");
        let by_artist = matches!(media_type.as_deref(), Some("AUDIO" | "ORIGINAL_MUSIC_VIDEO"));
        let channel = optional("artistUrl")
            .filter(|_| by_artist)
            .and_then(|url| Some(url.split_once("/channel/")?.1.to_string()))
            .filter(|id| !id.is_empty());

        Some(Self {
            video_id: video_id.to_string(),
            title: text("title", "Unknown title"),
            artist: text("artist", ""),
            raw_artist: text("artist", ""),
            album: text("album", ""),
            song_duration: raw
                .get("songDuration")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            image_src: optional("imageSrc"),
            url: optional("url"),
            media_type,
            artists: Vec::new(),
            api_title: text("title", "Unknown title"),
            api_artist: text("artist", ""),
            channel,
        })
    }

    /// The song as lyrics are looked up: `/song`'s own words, whatever the
    /// queue relabelled it to, so a lookup and its cached miss do not depend
    /// on whether the queue had arrived yet. A rename of the credit as shown
    /// (the one the Rename menu offers) still steers the lookup.
    pub fn for_lyrics(&self, names: &std::collections::BTreeMap<String, String>) -> Self {
        if self.api_title.is_empty() {
            return self.clone();
        }
        let shown = crate::store::map_artist(names, &self.raw_artist);
        let artist = if shown != self.raw_artist {
            shown
        } else {
            crate::store::map_artist(names, &self.api_artist)
        };
        Self { title: self.api_title.clone(), artist, ..self.clone() }
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
    /// Native glass is installed once per window and only shown or hidden when
    /// this changes; the renderer uses the same flag for its fallback surface.
    pub liquid_glass: bool,
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
    fn apply_position(&mut self, position: f64) {
        // Play/pause events can carry the media timeline's currentTime rather
        // than song-relative progress. REST and PLAYER_INFO cache that value
        // too. Keep the last valid playhead instead of clamping to the end,
        // which would select the final lyric on every pause.
        if !position.is_finite() || position < 0.0 {
            return;
        }
        if let Some(song) = &self.song {
            if song.song_duration > 0.0 && position > song.song_duration {
                return;
            }
        }
        self.position = position;
    }

    fn new(
        skin: String,
        panel_skin: String,
        liquid_glass: bool,
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
            liquid_glass,
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

    /// Derive every display-only lyric treatment from the fetched text. The
    /// Jyutping lookup deliberately happens before Simplified Chinese is
    /// assigned to `text`: 心裏 may be shown as 心里, but its reading must
    /// remain `sam1 leoi5`, not the reading of the different character 里.
    ///
    /// Text that is already simplified, or not Chinese at all, comes back
    /// unchanged, so this is safe to run over every line rather than trying to
    /// guess whether a track needs it.
    fn styled(&self, simplify: bool, jyutping: Option<&Jyutping>) -> Self {
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
                        text: if simplify {
                            fast2s::convert(&line.text)
                        } else {
                            line.text.clone()
                        },
                        jyutping: jyutping.and_then(|engine| engine.annotate(&line.text)),
                    })
                    .collect(),
            })),
        }
    }

    #[cfg(test)]
    fn simplified(&self) -> Self {
        self.styled(true, None)
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

// -------------------------------------------------------------- coalescing

/// How long a finished `refresh_all` (or a queue fetch it would repeat) counts
/// as fresh. Opening the dropdown, the widget and a menu in quick succession
/// would otherwise ask the player the same five questions each time.
const REFRESH_TTL: Duration = Duration::from_millis(2500);

/// A queue fetched longer ago than this is refetched when any surface comes on
/// screen. The queue has no push channel, so edits made inside YouTube Music
/// would otherwise wait for the next track change while the widget stays up.
const QUEUE_SHOW_TTL: Duration = Duration::from_secs(5);

/// How long after a track change its like state is read a second time.
const LIKE_RECHECK: Duration = Duration::from_millis(1500);

/// Waits before each queue read after a shuffle change, in ms: the first
/// catches YouTube Music's new order, the second an unshuffle's restored
/// opening tracks, which need a whole-playlist fetch first.
const REORDER_REFRESH_MS: [u64; 2] = [700, 900];

/// One run at a time. A caller arriving while a run is in flight either gives
/// up (the run in flight answers it) or, with `rerun`, asks the holder to go
/// round once more, so a request made after a change always gets a fetch that
/// started after it — however many requests pile up, that is two fetches.
#[derive(Default)]
struct Gate {
    busy: AtomicBool,
    again: AtomicBool,
}

impl Gate {
    /// Whether the caller now holds the gate and should run.
    fn enter(&self, rerun: bool) -> bool {
        if !self.busy.swap(true, Ordering::SeqCst) {
            self.again.store(false, Ordering::SeqCst);
            return true;
        }
        if !rerun {
            return false;
        }
        self.again.store(true, Ordering::SeqCst);
        // The holder may have let go between the two lines above.
        if !self.busy.swap(true, Ordering::SeqCst) {
            self.again.store(false, Ordering::SeqCst);
            return true;
        }
        false
    }

    /// Done with one run. `true` means a rerun was asked for and the caller
    /// still holds the gate: run again, then call `leave` again.
    fn leave(&self) -> bool {
        if self.again.swap(false, Ordering::SeqCst) {
            return true;
        }
        self.busy.store(false, Ordering::SeqCst);
        // A rerun asked for between the two lines above.
        self.again.swap(false, Ordering::SeqCst) && !self.busy.swap(true, Ordering::SeqCst)
    }
}

/// `/like-state`'s `{ "state": "LIKE" | … }`, read leniently.
fn like_of(reply: crate::api::Result<Option<Value>>) -> Option<String> {
    reply.ok().flatten().and_then(|value| {
        value
            .get("state")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

/// `elapsed`, or `None` for never.
fn age(at: &Mutex<Option<Instant>>) -> Option<Duration> {
    at.lock().expect("time lock").map(|at| at.elapsed())
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
    jyutping: Arc<Jyutping>,
    volume: Mutex<VolumeCalibration>,
    /// Window labels with a lyrics panel open. No point reaching out to the
    /// network for a panel nobody is looking at.
    lyrics_wanted_by: Mutex<HashSet<String>>,
    /// The same, for the queue panel. The skin half of "is anyone looking" is
    /// answered from the store instead — see `queue_wanted`.
    queue_wanted_by: Mutex<HashSet<String>>,
    /// Window labels currently on screen, told by whoever shows or hides one.
    /// While this is empty — nearly all the time, for a menu-bar app — the
    /// per-second `state` push is not emitted and the queue and lyrics are not
    /// fetched; showing a surface catches it up. See `set_on_screen`.
    on_screen: Mutex<HashSet<String>>,
    /// A queue or lyrics refresh was skipped because nobody could see it.
    queue_stale: AtomicBool,
    lyrics_stale: AtomicBool,
    cover_token: AtomicU64,
    queue_token: AtomicU64,
    /// Bumped by each shuffle change; see `refresh_queue_after_reorder`.
    reorder_token: AtomicU64,
    lyrics_token: AtomicU64,
    /// Woken whenever a lookup is superseded, so the old one is dropped at its
    /// next await instead of finishing a whole chain of requests for a track
    /// that is no longer playing. A dropped lookup caches nothing, so it can
    /// never leave a miss behind for a song it did not finish asking about.
    lyrics_superseded: tokio::sync::Notify,
    /// Guards background display conversion independently of network fetches.
    /// A setting change or new source invalidates work already in flight.
    lyrics_style_token: AtomicU64,
    /// One `refresh_all` at a time, and none again within `REFRESH_TTL`.
    refresh_gate: Gate,
    /// Set by a forced `refresh_all` that found one already running, so the
    /// rerun it asked for is forced too.
    refresh_forced: AtomicBool,
    refreshed_at: Mutex<Option<Instant>>,
    /// One `/queue` fetch at a time; overlapping refreshes merge (see `Gate`).
    queue_gate: Gate,
    queue_fetched_at: Mutex<Option<Instant>>,
    /// The last `POSITION_CHANGED`. It arrives about once a second while the
    /// player plays and not at all while it is paused, which is what lets the
    /// play-state poller tell when the glyph is likely to be wrong.
    position_at: Mutex<Option<Instant>>,
}

impl Core {
    pub fn new(app: AppHandle, store: Arc<Store>, api: Arc<Api>, jyutping: Arc<Jyutping>) -> Self {
        let (skin, panel_skin) = (
            crate::window::skin_of(&store),
            crate::window::panel_skin_of(&store),
        );
        let liquid_glass = store.get(|s| s.liquid_glass);
        let tint = store.get(|s| s.tint.clamp(0.0, 1.0));
        // The menu offers ±2s; the file is documented as hand-editable, so a
        // wider nudge typed in by hand is honoured — just not one that would
        // park the roll a whole verse away from the music.
        let lyrics_offset = store.get(|s| s.lyrics_offset_for(None));
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
            player: Mutex::new(PlayerState::new(
                skin,
                panel_skin,
                liquid_glass,
                tint,
                lyrics_offset,
                corners,
                corners_autohide,
            )),
            cover: Mutex::new(None),
            queue: Mutex::new(QueueView::empty()),
            lyrics: Mutex::new(LyricsView::idle()),
            lyrics_source: Mutex::new(LyricsView::idle()),
            jyutping,
            volume: Mutex::new(VolumeCalibration::default()),
            lyrics_wanted_by: Mutex::new(HashSet::new()),
            // `window::create` restores the stored panel before the window is
            // ever on screen, so the widget can come up with a queue panel open
            // and ask for its state before the renderer has told us it is open.
            // Seeding from the same store entry is what stops that first paint
            // being an empty list corrected a round trip later.
            queue_wanted_by: Mutex::new(queue_wanted_by),
            on_screen: Mutex::new(HashSet::new()),
            queue_stale: AtomicBool::new(false),
            lyrics_stale: AtomicBool::new(false),
            cover_token: AtomicU64::new(0),
            queue_token: AtomicU64::new(0),
            reorder_token: AtomicU64::new(0),
            lyrics_token: AtomicU64::new(0),
            lyrics_superseded: tokio::sync::Notify::new(),
            lyrics_style_token: AtomicU64::new(0),
            refresh_gate: Gate::default(),
            refresh_forced: AtomicBool::new(false),
            refreshed_at: Mutex::new(None),
            queue_gate: Gate::default(),
            queue_fetched_at: Mutex::new(None),
            position_at: Mutex::new(None),
        }
    }

    /// Seconds since the last `POSITION_CHANGED`, or `None` if none came yet.
    pub fn position_age(&self) -> Option<Duration> {
        age(&self.position_at)
    }

    /// Push `state` to the surfaces on screen only. A hidden webview would
    /// still have Tauri format and evaluate the payload once a second for
    /// nobody; `set_on_screen` hands one the latest as it comes back.
    fn emit_state(&self, state: &PlayerState) {
        let labels: Vec<String> = self
            .on_screen
            .lock()
            .expect("screen lock")
            .iter()
            .cloned()
            .collect();
        for label in labels {
            let _ = self.app.emit_to(label.as_str(), "state", state);
        }
    }

    /// Whether any surface is on screen.
    pub fn showing(&self) -> bool {
        !self.on_screen.lock().expect("screen lock").is_empty()
    }

    /// Of `labels`, whether any belongs to a surface that is on screen.
    fn any_on_screen(&self, labels: &HashSet<String>) -> bool {
        let on_screen = self.on_screen.lock().expect("screen lock");
        labels.iter().any(|label| on_screen.contains(label))
    }

    /// A surface was shown or hidden. Call on the same path as the `show` or
    /// `hide` itself: reading `is_visible` back right after a setter can still
    /// report the old answer.
    ///
    /// Coming on screen catches up on what was skipped while hidden: the
    /// latest `state` (emits go only to surfaces on screen), and the queue and
    /// lyrics if a track change left them stale. When nothing at all was on
    /// screen the pollers were standing down, so the player's ground truth is
    /// pulled too (`refresh_all`, which skips itself if one just ran).
    pub fn set_on_screen(self: &Arc<Self>, label: &str, shown: bool) {
        let woke = {
            let mut on_screen = self.on_screen.lock().expect("screen lock");
            let was_empty = on_screen.is_empty();
            if shown {
                on_screen.insert(label.to_string()) && was_empty
            } else {
                on_screen.remove(label);
                false
            }
        };
        // The widget's document reports hidden even on screen, so it cannot
        // tell for itself when to stop its progress loop; this tells it.
        let _ = self
            .app
            .emit("on-screen", serde_json::json!({ "label": label, "shown": shown }));
        if !shown {
            return;
        }
        // This surface heard no `state` while it was hidden.
        let _ = self.app.emit_to(label, "state", &self.snapshot());
        let remote = !self.is_local() && self.status() == "connected";
        let queue_old = remote
            && self
                .queue_fetched_at
                .lock()
                .expect("time lock")
                .is_none_or(|at| at.elapsed() >= QUEUE_SHOW_TTL);
        let queue = self.queue_stale.swap(false, Ordering::SeqCst) || queue_old;
        let lyrics = self.lyrics_stale.swap(false, Ordering::SeqCst);
        let resync = woke && remote;
        if queue || lyrics || resync {
            let core = Arc::clone(self);
            let since = Instant::now();
            tauri::async_runtime::spawn(async move {
                if resync {
                    // Fetches the queue as well, unless one just ran.
                    core.refresh_all(false).await;
                }
                let fetched = core
                    .queue_fetched_at
                    .lock()
                    .expect("time lock")
                    .is_some_and(|at| at >= since);
                if queue && !fetched {
                    core.refresh_queue().await;
                }
                if lyrics {
                    core.refresh_lyrics().await;
                }
            });
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

    // ----------------------------------------------------------- local source

    pub fn local(&self) -> Arc<crate::local::Local> {
        self.app.state::<Arc<crate::local::Local>>().inner().clone()
    }

    /// While a folder is playing, YouTube Music's socket and pollers are
    /// ignored rather than torn down, so going back is just a reconnect.
    pub fn is_local(&self) -> bool {
        self.app
            .try_state::<Arc<crate::local::Local>>()
            .is_some_and(|local| local.is_active())
    }

    /// The local counterpart of `apply_song`: the artwork is already in hand
    /// and there is no like state to ask for.
    pub fn show_local(
        self: &Arc<Self>,
        song: Song,
        cover: Option<Arc<str>>,
        playing: bool,
        like: &'static str,
    ) {
        self.cover_token.fetch_add(1, Ordering::SeqCst);
        self.update(|state| {
            state.song = Some(song);
            state.position = 0.0;
            state.like = Some(like.to_string());
            state.is_playing = playing;
        });
        self.set_cover(cover);
        self.publish_local_queue();
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move { core.refresh_lyrics().await });
        crate::tray::refresh(&self.app);
    }

    pub fn publish_local_queue(&self) {
        if !self.showing() {
            self.queue_stale.store(true, Ordering::SeqCst);
            return;
        }
        if self.queue_wanted() {
            let names = self.store.get(|s| s.artist_names.clone());
            self.set_queue(self.local().queue_view(&names));
        }
    }

    /// `artistNames` changed: re-label the song, the queue and retry the
    /// lyrics (and, for a file, the cover) a miss under the old name left out.
    pub fn remap_artists(self: &Arc<Self>) {
        let mut renamed = None;
        let current = self.player.lock().expect("player lock").song.clone();
        if let Some(current) = current {
            let dressed = self.dress(current.clone());
            self.update(|state| {
                // Only onto the song it was dressed from, never a newer one.
                if state.song.as_ref() == Some(&current) {
                    if dressed.artist != current.artist {
                        renamed = Some(current.video_id.clone());
                    }
                    state.song = Some(dressed);
                }
            });
        }
        if self.is_local() {
            self.local().renamed(self);
        }
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            core.refresh_queue().await;
            if let Some(id) = renamed {
                lyrics::forget_miss(&id);
                core.refresh_lyrics().await;
            }
        });
        crate::tray::refresh(&self.app);
    }

    /// Drop everything the folder put on screen before the socket reconnects.
    pub fn leave_local(self: &Arc<Self>) {
        self.update(|state| {
            state.status = "connecting".into();
            state.song = None;
            state.is_playing = false;
            state.position = 0.0;
            // The folder's modes are its own (saved per folder) and must not
            // leak onto YouTube Music: back to plain queue repeat until the
            // resync reports the player's real modes.
            state.shuffle = false;
            state.repeat = Some("ALL".into());
        });
        self.set_cover(None);
        self.set_queue(QueueView::empty());
        self.set_lyrics(LyricsView::idle());
    }

    /// Apply a patch and push to the renderers only when something actually
    /// changed. Both windows get the same snapshot.
    pub fn update(&self, patch: impl FnOnce(&mut PlayerState)) {
        // Keep the settings → player lock order, and resolve the shift in the
        // same snapshot as the song so neither surface sees the previous song's
        // override. No override map rides the position ticks.
        let next = self.store.get(|settings| {
            let mut player = self.player.lock().expect("player lock");
            let before = player.clone();
            patch(&mut player);
            player.lyrics_offset =
                settings.lyrics_offset_for(player.song.as_ref().map(|song| song.video_id.as_str()));
            if *player == before {
                return None;
            }
            Some(player.clone())
        });
        let Some(next) = next else { return };
        // Nobody to draw it; `set_on_screen` sends the latest when one shows.
        self.emit_state(&next);
    }

    /// `update` for writes that come from YouTube Music. Dropped once a folder
    /// is the source — checked inside the player lock, and `Local::open` flips
    /// the source inside the same lock, so a reply that was already in flight
    /// when the folder took over can never land on the folder's track.
    pub fn update_remote(&self, patch: impl FnOnce(&mut PlayerState)) {
        self.update(|state| {
            if !self.is_local() {
                patch(state);
            }
        });
    }

    // ------------------------------------------------------- heavy channels

    /// Each of these pushes only on a real change, and drops its lock before
    /// emitting: `emit` formats the payload once per webview, which is not work
    /// to be holding a lock the position tick wants through.
    pub(crate) fn set_cover(&self, cover: Option<Arc<str>>) {
        {
            let mut held = self.cover.lock().expect("cover lock");
            if *held == cover {
                return;
            }
            *held = cover.clone();
        }
        let _ = self.app.emit("cover", &cover);
    }

    pub(crate) fn set_queue(&self, view: QueueView) {
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

    fn set_lyrics(self: &Arc<Self>, view: LyricsView) {
        *self.lyrics_source.lock().expect("lyrics lock") = view.clone();
        self.restyle_lyrics();
    }

    /// Commit a fully styled view, still only on a real change.
    fn publish_lyrics(&self, view: LyricsView) {
        {
            let mut held = self.lyrics.lock().expect("lyrics lock");
            if *held == view {
                return;
            }
            *held = view.clone();
        }
        let _ = self.app.emit("lyrics", &view);
    }

    /// Re-derive the shown words from the fetched ones. Jyutping is dictionary
    /// work, so keep it off the AppKit/menu thread and reject the result if a
    /// newer track or toggle superseded it while it was running.
    pub fn restyle_lyrics(self: &Arc<Self>) {
        let source = self.lyrics_source.lock().expect("lyrics lock").clone();
        let (simplify, show_jyutping) = self
            .store
            .get(|settings| (settings.simplify_lyrics, settings.jyutping_lyrics));
        let token = self.lyrics_style_token.fetch_add(1, Ordering::SeqCst) + 1;

        if !show_jyutping || source.lyrics.is_none() {
            self.publish_lyrics(source.styled(simplify, None));
            return;
        }

        let core = Arc::clone(self);
        tauri::async_runtime::spawn_blocking(move || {
            let view = source.styled(simplify, Some(&core.jyutping));
            if token == core.lyrics_style_token.load(Ordering::SeqCst) {
                core.publish_lyrics(view);
            }
        });
    }

    // ---------------------------------------------------------------- tracks

    /// Swap in a new song now, then resolve its artwork and like state out of
    /// band. Never waits on the network: the realtime loop calls this inline,
    /// and a cover fetch (up to 8 s) must not hold up the position, play-state
    /// and volume messages queued behind it. `cover_token` drops a cover or
    /// like state that lands after a newer track (or a folder) took over.
    ///
    /// Returns whether it started a queue refresh, i.e. the track changed.
    pub fn apply_song(self: &Arc<Self>, raw: Option<&Value>) -> bool {
        // Callers awaited the player to get here; a folder may have taken over
        // meanwhile, and a stale YouTube track must not replace its track.
        if self.is_local() {
            return false;
        }
        let song = Song::from_value(raw).map(|song| self.dress(song));
        let current = self.player.lock().expect("player lock").song.clone();

        if song.as_ref().map(|s| &s.video_id) == current.as_ref().map(|s| &s.video_id) {
            // Same track: metadata may still have been enriched (album, duration).
            if let Some(song) = song {
                self.update_remote(|state| state.song = Some(song));
            }
            return false;
        }

        self.update_remote(|state| {
            state.song = song.clone();
            state.position = 0.0;
            state.like = None;
        });
        self.set_cover(None);

        let Some(song) = song else { return false };

        // The like state is asked for here, on every track change, rather
        // than on a timer: the server has no push for it.
        let token = self.cover_token.fetch_add(1, Ordering::SeqCst) + 1;
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            let (cover, like) = tokio::join!(
                core.api.fetch_cover(song.image_src.as_deref(), COVER_PX),
                core.api.like_state(),
            );
            if token != core.cover_token.load(Ordering::SeqCst) {
                return; // a newer song won the race
            }
            core.set_cover(cover);
            core.update_remote(|state| state.like = like_of(like));
            // The server reads the like button off the page, which can still
            // show the previous track's state this early. Ask once more.
            tokio::time::sleep(LIKE_RECHECK).await;
            let like = like_of(core.api.like_state().await);
            if like.is_some() && token == core.cover_token.load(Ordering::SeqCst) {
                core.update_remote(|state| state.like = like);
            }
        });

        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            core.refresh_queue().await;
            core.refresh_lyrics().await;
        });
        true
    }

    /// A track as the surfaces show it. `/song` has YouTube Music's canonical
    /// words, often romanised ("Free Soul" by "Ronghao Li"); the track's queue
    /// slot has them in the account's language ("纵横四海" by "李荣浩") and a
    /// link per artist, so the slot wins whenever the queue holds the track.
    /// An `artistNames` entry for the canonical credit is the user's own choice
    /// and still wins over the slot's credit.
    ///
    /// Rebuilt from the `api_*` fields every time, so it can be re-run when
    /// the queue or the names change. A local file only gets `artistNames`.
    fn dress(&self, mut song: Song) -> Song {
        let names = self.store.get(|s| s.artist_names.clone());
        let map = |credit: &str| crate::store::map_artist(&names, credit);
        if song.api_title.is_empty() {
            song.artist = map(&song.raw_artist);
            return song;
        }

        let slot = self
            .queue
            .lock()
            .expect("queue lock")
            .items
            .iter()
            .find(|track| track.video_id == song.video_id)
            .cloned();
        // A rename of the whole canonical credit; one of a single name in it
        // still lets the slot's words through.
        let renamed = crate::store::renames_whole(&names, &song.api_artist);

        song.title = slot
            .as_ref()
            .map(|track| track.title.clone())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| song.api_title.clone());
        song.raw_artist = slot
            .as_ref()
            .map(|track| track.credit.clone())
            .filter(|credit| !renamed && !credit.is_empty())
            .unwrap_or_else(|| song.api_artist.clone());
        song.artist = map(&song.raw_artist);
        // Names without an id are found by name when opened. Only a song or
        // an official video is credited to artists; an upload's byline names
        // whoever posted it. Without a slot `/song`'s credit is romanised, so
        // it is only linked when it names one artist and the channel is
        // theirs; the slot follows within a queue fetch.
        let by_artist = matches!(song.media_type.as_deref(), Some("AUDIO" | "ORIGINAL_MUSIC_VIDEO"));
        let mut links: Vec<ArtistLink> = match &slot {
            Some(track) => track.artists.to_vec(),
            None if crate::store::split_credit(&song.api_artist).len() == 1 => {
                vec![ArtistLink::named(&song.api_artist)]
            }
            None => Vec::new(),
        };
        links.retain(|link| by_artist || !link.id.is_empty());
        // `/song`'s channel is the page owner: the artist when only one is named.
        if let ([link], Some(channel)) = (links.as_mut_slice(), song.channel.as_ref()) {
            if link.id.is_empty() {
                link.id = channel.clone();
            }
        }
        if slot.is_none() {
            links.retain(|link| !link.id.is_empty());
        }
        song.artists = links
            .into_iter()
            .map(|link| ArtistLink { name: map(&link.query), ..link })
            .collect();
        song
    }

    /// The queue changed: relabel the playing track from its slot.
    fn relabel_song(&self) {
        let Some(current) = self.player.lock().expect("player lock").song.clone() else {
            return;
        };
        let dressed = self.dress(current.clone());
        if dressed == current {
            return;
        }
        // Only onto the song it was dressed from: an `apply_song` in between
        // (a later album or duration) wins, and relabels itself.
        self.update_remote(|state| {
            if state.song.as_ref() == Some(&current) {
                state.song = Some(dressed);
            }
        });
    }

    /// Whether any surface is looking at the queue — a skin whose layout draws
    /// it, or a queue panel open on that surface. The same bargain as the
    /// lyrics: a request on every track change is not work to do for nobody.
    ///
    /// The skin half is answered from the store, because a skin is a property of
    /// the surface rather than something the renderer has to report.
    ///
    /// Either way only while a surface is on screen: a hidden Stack card draws
    /// nothing, and `set_on_screen` refreshes it when it comes back.
    fn queue_wanted(&self) -> bool {
        if !self.showing() {
            return false;
        }
        if crate::window::skin_shows_queue(&crate::window::skin_of(&self.store))
            || crate::window::skin_shows_queue(&crate::window::panel_skin_of(&self.store))
        {
            return true;
        }
        let open = self.queue_wanted_by.lock().expect("queue lock").clone();
        self.any_on_screen(&open)
    }

    pub fn set_queue_wanted(&self, label: &str, wanted: bool) {
        let mut open = self.queue_wanted_by.lock().expect("queue lock");
        if wanted {
            open.insert(label.to_string());
        } else {
            open.remove(label);
        }
    }

    /// Refresh the queue once YouTube Music has finished reordering it. The
    /// shuffle event fires when the button flips, but the new order arrives
    /// from YouTube after that, and an unshuffle can land in two steps — the
    /// API server puts back a long playlist's opening tracks once the queue
    /// is rebuilt. Two timed reads cover both without polling a queue that can
    /// be thousands of items. A newer toggle supersedes an older one, and
    /// nothing is fetched while no surface shows the queue.
    fn refresh_queue_after_reorder(self: &Arc<Self>) {
        let token = self.reorder_token.fetch_add(1, Ordering::SeqCst) + 1;
        if !self.queue_visible() {
            return;
        }
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            for delay in REORDER_REFRESH_MS {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                if core.reorder_token.load(Ordering::SeqCst) != token || core.is_local() {
                    return;
                }
                core.refresh_queue().await;
            }
        });
    }

    /// The whole queue, for the queue panel and Stack's "Next tracks" strip
    /// alike — one fetch, one channel, two readers.
    ///
    /// No artwork is resolved here. That is the difference from the four-item
    /// version this replaced: a full queue's worth of `data:` URLs would be
    /// megabytes on an event, and the two surfaces want different sizes anyway.
    ///
    /// Each `/queue` is a full round trip into the YouTube Music renderer, so
    /// overlapping refreshes merge: one in flight, at most one more queued
    /// behind it (see `Gate`).
    pub async fn refresh_queue(self: &Arc<Self>) {
        if !self.queue_gate.enter(true) {
            return;
        }
        loop {
            self.refresh_queue_once().await;
            if !self.queue_gate.leave() {
                break;
            }
        }
    }

    /// Whether a queue view would be shown right now; marks it stale if not.
    fn queue_visible(self: &Arc<Self>) -> bool {
        if !self.showing() {
            self.queue_stale.store(true, Ordering::SeqCst);
            return false;
        }
        if !self.queue_wanted() {
            self.set_queue(QueueView::empty());
            return false;
        }
        true
    }

    /// Publish a `/queue` reply someone else fetched (the jump path polls the
    /// queue itself), superseding any refresh still in flight.
    pub fn adopt_queue(self: &Arc<Self>, queue: &Value) {
        if self.is_local() || !self.queue_visible() {
            return;
        }
        self.queue_token.fetch_add(1, Ordering::SeqCst);
        *self.queue_fetched_at.lock().expect("time lock") = Some(Instant::now());
        self.publish_queue(queue);
    }

    fn publish_queue(&self, queue: &Value) {
        let mut view = search::parse_queue(Some(queue), QUEUE_MAX);
        let names = self.store.get(|s| s.artist_names.clone());
        if !names.is_empty() {
            let items: Vec<_> = view
                .items
                .iter()
                .cloned()
                .map(|mut track| {
                    track.artist = crate::store::map_artist(&names, &track.artist);
                    track
                })
                .collect();
            view.items = Arc::from(items);
        }
        self.set_queue(view);
        self.relabel_song();
    }

    async fn refresh_queue_once(self: &Arc<Self>) {
        // Hidden: keep what is drawn rather than flash it empty, and refetch
        // when a surface shows.
        if !self.queue_visible() {
            return;
        }
        if self.is_local() {
            self.publish_local_queue();
            return;
        }

        let token = self.queue_token.fetch_add(1, Ordering::SeqCst) + 1;
        *self.queue_fetched_at.lock().expect("time lock") = Some(Instant::now());
        let Ok(Some(queue)) = self.api.queue().await else {
            return;
        };
        if self.is_local() {
            return;
        }
        // One await inside the guarded region, so one check: if a newer refresh
        // started while this response was in flight, its result is the truth.
        if token != self.queue_token.load(Ordering::SeqCst) {
            return;
        }
        self.publish_queue(&queue);
    }

    /// Lyrics come from LRCLib (see `lyrics.rs`) and are only fetched while a
    /// lyrics panel is actually open.
    pub async fn refresh_lyrics(self: &Arc<Self>) {
        let open = self.lyrics_wanted_by.lock().expect("lyrics lock").clone();
        // A lyrics panel left open on a hidden widget: nobody is reading, so
        // no lookup — the panel catches up when the widget shows again.
        if !open.is_empty() && !self.any_on_screen(&open) {
            self.lyrics_stale.store(true, Ordering::SeqCst);
            return;
        }
        let wanted = !open.is_empty();
        let song = self.player.lock().expect("player lock").song.clone();
        let names = self.store.get(|s| s.artist_names.clone());
        let song = song.map(|song| song.for_lyrics(&names));

        let token = self.lyrics_token.fetch_add(1, Ordering::SeqCst) + 1;
        self.lyrics_superseded.notify_waiters();
        let Some(song) = song.filter(|_| wanted) else {
            self.set_lyrics(LyricsView::idle());
            return;
        };

        self.set_lyrics(LyricsView {
            state: "loading".into(),
            lyrics: None,
        });

        // Created before the lookup starts, so a newer request's
        // `notify_waiters` reaches it even before its first poll.
        let superseded = self.lyrics_superseded.notified();
        let result = tokio::select! {
            result = lyrics::fetch_lyrics(self.api.client(), &song) => result,
            () = superseded => return,
        };

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

    /// A cache-menu retry should not replace lyrics already on screen.
    pub async fn retry_missing_lyrics(self: &Arc<Self>) {
        let missing = self.lyrics_source.lock().expect("lyrics lock").state == "none";
        if missing {
            self.refresh_lyrics().await;
        }
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
    ///
    /// Showing a surface asks too, but that is `force: false`: skipped while
    /// one is in flight or finished within `REFRESH_TTL`. A new connection
    /// forces it, because the cached answers belong to the previous one.
    pub async fn refresh_all(self: &Arc<Self>, force: bool) {
        if self.is_local() {
            return;
        }
        if !force && age(&self.refreshed_at).is_some_and(|age| age < REFRESH_TTL) {
            return;
        }
        if force {
            self.refresh_forced.store(true, Ordering::SeqCst);
        }
        if !self.refresh_gate.enter(force) {
            return;
        }
        loop {
            let forced = self.refresh_forced.swap(false, Ordering::SeqCst);
            self.refresh_all_once(forced).await;
            *self.refreshed_at.lock().expect("time lock") = Some(Instant::now());
            if !self.refresh_gate.leave() {
                break;
            }
        }
    }

    async fn refresh_all_once(self: &Arc<Self>, force: bool) {
        let started = Instant::now();
        let track = self.cover_token.load(Ordering::SeqCst);
        let (song, shuffle, volume, repeat, like) = tokio::join!(
            self.api.song(),
            self.api.shuffle_state(),
            self.api.volume_state(),
            self.api.repeat_mode(),
            self.api.like_state()
        );
        // The same race as `apply_song`: a folder opened while these were out.
        if self.is_local() {
            return;
        }

        let song = song.ok().flatten();
        let changed = song.as_ref().is_some_and(|song| self.apply_song(Some(song)));
        // A changed track asks for its own like state; this answer may predate
        // it, as it does when the socket delivered a new track meanwhile.
        let like = like_of(like).filter(|_| {
            !changed && song.is_some() && self.cover_token.load(Ordering::SeqCst) == track
        });

        let shuffle = shuffle
            .ok()
            .flatten()
            .and_then(|value| value.get("state").and_then(Value::as_bool));
        // Null is a real answer here — it means the player has not reported a
        // mode yet — so it stays `None` rather than being defaulted to NONE.
        let repeat = repeat.ok().flatten().and_then(|value| {
            value
                .get("mode")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
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

        self.update_remote(|state| {
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
            if like.is_some() {
                state.like = like.clone();
            }
            if let Some(song) = &song {
                if let Some(paused) = song.get("isPaused").and_then(Value::as_bool) {
                    state.is_playing = !paused;
                }
                if let Some(elapsed) = song.get("elapsedSeconds").and_then(Value::as_f64) {
                    state.apply_position(elapsed);
                }
            }
        });

        // `apply_song` only refreshes the queue when the track actually
        // changed, so a reconnect on the same track would otherwise leave
        // "Next tracks" empty until the song ended. Skipped when a fetch
        // already started during this refresh (PLAYER_INFO's track change on
        // connect) or, when not forced, within the TTL.
        let recent = self
            .queue_fetched_at
            .lock()
            .expect("time lock")
            .is_some_and(|at| at >= started || (!force && at.elapsed() < REFRESH_TTL));
        if !changed && !recent {
            self.refresh_queue().await;
        }
    }

    // -------------------------------------------------------------- realtime

    pub fn set_status(self: &Arc<Self>, status: &str, message: &str) {
        if self.is_local() {
            return;
        }
        let mut changed = false;
        self.update_remote(|state| {
            changed = state.status != status || state.status_message != message;
            state.status = status.to_string();
            state.status_message = message.to_string();
        });
        // Background reconnects report the same `offline` every backoff cycle;
        // the queue is already gone and rebuilding the tray menu (icon decode,
        // a whole NSMenu tree on the main thread) would change nothing.
        if !changed && status != "connected" {
            return;
        }

        if status == "connected" {
            let core = Arc::clone(self);
            tauri::async_runtime::spawn(async move { core.refresh_all(true).await });
        } else if status != "connecting" {
            // Drop the queue too: it belongs to a player we can no longer see.
            self.update_remote(|state| state.is_playing = false);
            self.set_queue(QueueView::empty());
        }

        crate::tray::refresh(&self.app);
    }

    /// Never awaits: see `apply_song`.
    pub fn handle_message(self: &Arc<Self>, msg: &Value) {
        if self.is_local() {
            return;
        }
        let kind = msg.get("type").and_then(Value::as_str).unwrap_or_default();
        let number = |key: &str| msg.get(key).and_then(Value::as_f64);
        let flag = |key: &str| msg.get(key).and_then(Value::as_bool).unwrap_or(false);

        match kind {
            "PLAYER_INFO" => {
                self.apply_song(msg.get("song"));
                let volume = number("volume");
                // The server starts from `NONE` until the player reports a
                // mode, so only a mode it could not have defaulted to is taken
                // as an answer; `refresh_all`'s `/repeat-mode` settles the rest
                // (and keeps "unknown" apart from `NONE`).
                let repeat = msg
                    .get("repeat")
                    .and_then(Value::as_str)
                    .filter(|mode| matches!(*mode, "ALL" | "ONE"))
                    .map(str::to_string);
                self.update_remote(|state| {
                    state.is_playing = flag("isPlaying");
                    state.apply_position(number("position").unwrap_or(0.0));
                    if let Some(volume) = volume {
                        state.volume = volume;
                    }
                    state.muted = flag("muted");
                    state.shuffle = flag("shuffle");
                    if repeat.is_some() {
                        state.repeat = repeat.clone();
                    }
                });
            }
            "VIDEO_CHANGED" => {
                self.apply_song(msg.get("song"));
                // The payload carries the new track's paused flag; the player
                // does not always follow a track change with PLAYER_STATE_CHANGED.
                let paused = msg
                    .get("song")
                    .and_then(|song| song.get("isPaused"))
                    .and_then(Value::as_bool);
                self.update_remote(|state| {
                    state.apply_position(number("position").unwrap_or(0.0));
                    if let Some(paused) = paused {
                        state.is_playing = !paused;
                    }
                });
            }
            "PLAYER_STATE_CHANGED" => {
                let position = number("position");
                self.update_remote(|state| {
                    state.is_playing = flag("isPlaying");
                    if let Some(position) = position {
                        state.apply_position(position);
                    }
                });
            }
            "POSITION_CHANGED" => {
                *self.position_at.lock().expect("time lock") = Some(Instant::now());
                let position = number("position").unwrap_or(0.0);
                self.update_remote(|state| state.apply_position(position));
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

                self.update_remote(|state| {
                    if let Some(level) = level {
                        state.volume = level;
                    }
                    state.muted = muted;
                });
            }
            "SHUFFLE_CHANGED" => {
                let shuffle = flag("shuffle");
                let changed = self.snapshot().shuffle != shuffle;
                self.update_remote(|state| state.shuffle = shuffle);
                // Shuffling reorders the queue but keeps the track, and
                // `apply_song` only refreshes the queue when the track moves —
                // so without this "Next tracks" keeps the old order and the
                // button looks like it only lit itself up.
                if changed {
                    self.refresh_queue_after_reorder();
                }
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
                    self.update_remote(|state| state.repeat = repeat.clone());
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

    fn playback_state() -> PlayerState {
        let mut state = PlayerState::new(
            "stack".into(),
            "stack".into(),
            true,
            1.0,
            0.0,
            SkinCorners::default(),
            0.0,
        );
        state.song = Some(Song {
            song_duration: 241.0,
            ..Song::default()
        });
        state
    }

    #[test]
    fn the_gate_merges_overlapping_runs_into_one_rerun() {
        let gate = Gate::default();
        assert!(gate.enter(true));
        // A non-rerun caller is answered by the run in flight.
        assert!(!gate.enter(false));
        assert!(!gate.leave(), "nothing asked for a rerun");
        assert!(gate.enter(true));
        // Several callers during one run: one more run, not several.
        assert!(!gate.enter(true));
        assert!(!gate.enter(true));
        assert!(gate.leave(), "a rerun was asked for");
        assert!(!gate.leave());
        // Released: the next caller runs straight away.
        assert!(gate.enter(false));
        assert!(!gate.leave());
    }

    #[test]
    fn pause_keeps_the_lyric_playhead_at_the_last_valid_position() {
        let mut state = playback_state();
        // Captured from the local player: progress ticks are song-relative,
        // but the pause event reports a media timestamp beyond the song's end.
        state.is_playing = true;
        state.apply_position(211.0);
        state.is_playing = false;
        state.apply_position(3445.0);
        assert_eq!(
            state.position, 211.0,
            "pause must not jump lyrics to the end"
        );
        // A cached REST/PLAYER_INFO response can repeat the same bad timestamp.
        state.apply_position(3445.0);
        assert_eq!(state.position, 211.0);
        state.apply_position(212.0);
        assert_eq!(state.position, 212.0);
    }

    #[test]
    fn valid_seeks_and_resume_still_update_the_lyric_playhead() {
        let mut state = playback_state();
        for position in [120.0, 30.0, 0.0, 241.0] {
            state.apply_position(position);
            assert_eq!(state.position, position);
        }
        state.apply_position(120.0);
        state.is_playing = true;
        state.apply_position(3445.0);
        assert_eq!(state.position, 120.0);
        state.apply_position(121.0);
        assert_eq!(state.position, 121.0);
    }

    #[test]
    fn position_validation_handles_invalid_values_and_unknown_duration() {
        let mut state = playback_state();
        state.apply_position(120.0);
        for position in [-1.0, f64::NAN, f64::INFINITY] {
            state.apply_position(position);
            assert_eq!(state.position, 120.0);
        }
        state.song.as_mut().unwrap().song_duration = 0.0;
        state.apply_position(300.0);
        assert_eq!(state.position, 300.0);
        state.song = None;
        state.apply_position(301.0);
        assert_eq!(state.position, 301.0);
    }

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
                        jyutping: None,
                    },
                    // The reason this is not a character table: 乾 is 干 in
                    // 乾淨 and stays 乾 in 乾坤. Any per-character mapping gets
                    // one of those two wrong, in the same line.
                    LyricLine {
                        time: Some(1.0),
                        text: "乾淨的乾坤".into(),
                        jyutping: None,
                    },
                    // Already simplified, and not Chinese at all: both have to
                    // survive untouched, since every line is converted rather
                    // than the track being sniffed first.
                    LyricLine {
                        time: Some(2.0),
                        text: "Baby, already 简体 and ASCII".into(),
                        jyutping: None,
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
    fn song_keeps_the_api_words_for_lyrics_and_an_artist_channel_for_songs_only() {
        let raw = serde_json::json!({
            "videoId": "p_oH-XgGqX0",
            "title": "Free Soul",
            "artist": "Jaychou",
            "artistUrl": "https://music.youtube.com/channel/UCLZ9hbBmb_5mEclHVY5yOBg",
            "mediaType": "AUDIO",
        });
        let mut song = Song::from_value(Some(&raw)).expect("a track");
        assert_eq!(song.channel.as_deref(), Some("UCLZ9hbBmb_5mEclHVY5yOBg"));

        // Relabelled from the queue; lyrics still ask with /song's words.
        song.title = "纵横四海".into();
        song.artist = "李荣浩".into();
        let names = std::collections::BTreeMap::from([("Jaychou".to_string(), "周杰伦".to_string())]);
        let query = song.for_lyrics(&names);
        assert_eq!((query.title.as_str(), query.artist.as_str()), ("Free Soul", "周杰伦"));

        // Renaming the credit as shown (the slot's) steers the lookup too.
        song.raw_artist = "李荣浩".into();
        let names = std::collections::BTreeMap::from([("李荣浩".to_string(), "李榮浩".to_string())]);
        assert_eq!(song.for_lyrics(&names).artist, "李榮浩");
        assert_eq!(song.for_lyrics(&Default::default()).artist, "Jaychou");

        // An upload's page owner is the uploader, not an artist.
        let upload = serde_json::json!({
            "videoId": "x", "title": "t", "artist": "a",
            "artistUrl": "https://music.youtube.com/channel/UCuploader",
            "mediaType": "USER_GENERATED_CONTENT",
        });
        assert_eq!(Song::from_value(Some(&upload)).expect("a track").channel, None);
    }

    #[test]
    fn a_track_with_no_lyrics_converts_to_itself() {
        let idle = LyricsView::idle();
        assert_eq!(idle.simplified(), idle);
    }

    #[test]
    fn jyutping_is_derived_before_simplification() {
        let view = LyricsView {
            state: "ready".into(),
            lyrics: Some(Arc::new(Lyrics {
                synced: true,
                how: "exact",
                lines: vec![LyricLine {
                    time: Some(1.0),
                    text: "我心裏".into(),
                    jyutping: None,
                }],
            })),
        };
        let engine = Jyutping::from_dir(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/jyutping"),
        );

        let converted = view.styled(true, Some(&engine));
        let line = &converted.lyrics.as_ref().expect("lyrics").lines[0];
        assert_eq!(line.text, "我心里");
        assert_eq!(line.jyutping.as_deref(), Some("ngo sam leoi"));
        assert_eq!(
            view.lyrics.as_ref().expect("source").lines[0].text,
            "我心裏"
        );
    }
}
