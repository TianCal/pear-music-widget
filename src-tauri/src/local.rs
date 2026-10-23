//! Local-folder playback: a second source behind the same state machine.
//!
//! While a folder is active the YouTube Music connection is ignored (see the
//! `is_local` guards in `state.rs`) and this module drives `Core` instead —
//! the same `state`, `cover`, `queue` and `lyrics` channels, so both surfaces
//! render a local track exactly as they render a streamed one.
//!
//! Audio is decoded in Rust with rodio/symphonia; nothing touches the renderer
//! but metadata and `data:` artwork, which keeps the CSP and rule 1 intact.
//!
//! **Every track gets a fresh `Sink`, and the old one is dropped.** rodio's
//! `Sink::clear` and `try_seek` wait for the audio callback, and a stream whose
//! device has gone (headphones unplugged) never calls back again — so either
//! one would hang the caller for good. Dropping a sink only sets a flag. Seeking
//! reopens the file at the new position for the same reason. Loads do file IO,
//! so they run on blocking threads (or the ticker's own thread), never on the
//! AppKit main thread or an async worker.
//!
//! Tracks keep their **raw** credits; `artistNames` is applied on the way out
//! (song, queue, search) so renaming an artist needs no rescan.
//!
//! Artwork is embedded or sidecar first, then `artwork::find` — asynchronously,
//! so a track starts playing before its cover has been looked up. Sized
//! `data:` URLs live in one byte-capped `ImageCache`; online covers are keyed
//! by album, so a whole album's queue rows share one resize.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lofty::prelude::*;
use rodio::cpal::traits::HostTrait;
use rodio::{DeviceTrait, Sink, Source};
use serde::{Deserialize, Serialize};

use crate::api::ImageCache;
use crate::likes::{Likes, Rating};
use crate::search::{QueueTrack, QueueView, SearchResult};
use crate::state::{Core, Song};

/// What the enabled symphonia features decode: FLAC, WAV, MP3, AAC and ALAC in
/// MP4, Vorbis in Ogg, AIFF.
const EXTENSIONS: [&str; 7] = ["flac", "wav", "mp3", "m4a", "aac", "ogg", "aiff"];
const TICK: Duration = Duration::from_millis(250);
/// Ticks between checks that the system output device is still the one the
/// stream was opened on. CoreAudio keeps a stream on its original device.
const DEVICE_CHECK_TICKS: u64 = 8;
/// "Previous" within this many seconds of the start goes back a track; later
/// it restarts the one playing, as every player does.
const RESTART_AFTER: f64 = 3.0;
const SEARCH_MAX: usize = 20;
/// How long a `queue_art` reply waits for covers still being looked up. The
/// rest finish in the background and land on the next request for that row.
const QUEUE_ART_WAIT: Duration = Duration::from_millis(1500);

/// Prefix for local ids. Also keeps them inside `lyrics_cache::safe_name`.
pub const ID_PREFIX: &str = "local-";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    /// As tagged, or the top folder's name when untagged — never mapped.
    pub artist: String,
    pub album: String,
    pub duration: f64,
    /// Embedded picture or sidecar image found at scan time. Without one the
    /// file is never reopened for art — `artwork::find` is asked instead.
    pub has_art: bool,
    /// Bytes on disk, from the listing. Part of the likes fingerprint.
    #[serde(default)]
    pub size: u64,
}

impl Track {
    fn song(&self, names: &BTreeMap<String, String>) -> Song {
        Song {
            video_id: self.id.clone(),
            title: self.title.clone(),
            artist: crate::store::map_artist(names, &self.artist),
            raw_artist: self.artist.clone(),
            album: self.album.clone(),
            song_duration: self.duration,
            image_src: None,
            url: None,
            media_type: Some("LOCAL".into()),
        }
    }
}

struct Session {
    /// The folder, so an advance can tell "this file is bad" from "the drive
    /// is gone" and stop instead of walking the whole order.
    root: PathBuf,
    tracks: Vec<Track>,
    /// Likes and dislikes, stored on the drive — see `likes.rs`.
    likes: Likes,
    /// Play order as indexes into `tracks`; shuffled in place.
    order: Vec<usize>,
    /// Position in `order` of the current track.
    cursor: usize,
    /// What the user asked for, as opposed to what the sink is doing: an empty
    /// sink while this is set means the track just ended.
    playing: bool,
    shuffle: bool,
    /// Source → Liked Songs: `order` holds only liked tracks (plus the one
    /// playing, kept until it ends so switching never cuts a song off).
    liked_only: bool,
    repeat: String,
    volume: f64,
    muted: bool,
    /// Whether the current track actually opened. `remember` saves only a
    /// track that did, so a file that will not play never becomes the place
    /// to resume.
    started: bool,
    /// Whole second last pushed on `state`. The renderer interpolates between
    /// pushes, so the 250 ms tick only emits when this changes — the same once
    /// a second YouTube Music's POSITION_CHANGED gives.
    shown_second: u64,
}

impl Session {
    fn current(&self) -> Option<&Track> {
        self.order.get(self.cursor).map(|&i| &self.tracks[i])
    }

    /// Whether advancing may land on `tracks[i]`: never a dislike, and under
    /// Liked Songs only a like.
    fn reachable(&self, i: usize) -> bool {
        let id = &self.tracks[i].id;
        !self.likes.is_disliked(id) && (!self.liked_only || self.likes.is_liked(id))
    }

    /// Rebuild `order` for the current scope, keeping the playing track where
    /// it is in the sequence. Folder order, or under shuffle the existing
    /// shuffled order with newly in-scope tracks shuffled onto the end.
    fn rescope(&mut self) {
        let current = self.order.get(self.cursor).copied();
        let in_scope = |s: &Self, i: usize| {
            Some(i) == current || !s.liked_only || s.likes.is_liked(&s.tracks[i].id)
        };
        let order: Vec<usize> = if self.shuffle {
            let mut kept: Vec<usize> = self.order.iter().copied().filter(|&i| in_scope(self, i)).collect();
            let held: HashSet<usize> = kept.iter().copied().collect();
            let mut added: Vec<usize> = (0..self.tracks.len())
                .filter(|i| !held.contains(i) && in_scope(self, *i))
                .collect();
            shuffle(&mut added);
            kept.extend(added);
            kept
        } else {
            (0..self.tracks.len()).filter(|&i| in_scope(self, i)).collect()
        };
        self.cursor = current
            .and_then(|c| order.iter().position(|&i| i == c))
            .unwrap_or(0);
        self.order = order;
    }

    fn relative(&self, track: &Track) -> Option<String> {
        track
            .path
            .strip_prefix(&self.root)
            .ok()
            .map(|rel| rel.to_string_lossy().into_owned())
    }
}

/// An open output stream. The `OutputStream` itself lives on a thread of its
/// own (it is not `Send`) until `_keep` is dropped; we hold its mixer.
struct Output {
    mixer: rodio::mixer::Mixer,
    device: Option<String>,
    /// Set from the stream's error callback — the device went away.
    dead: Arc<AtomicBool>,
    _keep: mpsc::Sender<()>,
}

impl Output {
    fn open() -> Option<Self> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (keep, hold) = mpsc::channel::<()>();
        let dead = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&dead);
        std::thread::Builder::new()
            .name("local-audio".into())
            .spawn(move || {
                let device = rodio::cpal::default_host().default_output_device();
                let name = device.as_ref().and_then(|d| d.name().ok());
                let stream = device
                    .and_then(|d| rodio::OutputStreamBuilder::from_device(d).ok())
                    .and_then(|builder| {
                        builder
                            .with_error_callback(move |_| flag.store(true, Ordering::SeqCst))
                            .open_stream()
                            .ok()
                    });
                let Some(mut stream) = stream else {
                    let _ = ready_tx.send(None);
                    return;
                };
                stream.log_on_drop(false);
                let _ = ready_tx.send(Some((stream.mixer().clone(), name)));
                // Until the `Output` is dropped.
                let _ = hold.recv();
            })
            .ok()?;
        let (mixer, device) = ready_rx.recv().ok()??;
        Some(Self {
            mixer,
            device,
            dead,
            _keep: keep,
        })
    }

    /// Still usable: not errored, and still the system's default device.
    fn current(&self) -> bool {
        !self.dead.load(Ordering::SeqCst) && self.device == default_device_name()
    }
}

fn default_device_name() -> Option<String> {
    rodio::cpal::default_host()
        .default_output_device()
        .and_then(|device| device.name().ok())
}

/// The track in the output: its sink, and where in the file it started, since
/// a seek starts a new sink partway in.
struct Playing {
    sink: Sink,
    offset: f64,
}

impl Playing {
    fn position(&self) -> f64 {
        self.offset + self.sink.get_pos().as_secs_f64()
    }
}

pub struct Local {
    active: AtomicBool,
    session: Mutex<Option<Session>>,
    /// Opened on first use, not at launch: a YouTube-only user should never
    /// hold CoreAudio open.
    output: Mutex<Option<Output>>,
    playing: Mutex<Option<Playing>>,
    /// Held across a whole load. The ticker only *tries* it, so it can never
    /// see the gap between one sink and the next and advance a second time.
    loading: Mutex<()>,
    ticks: AtomicU64,
    /// Whether each saved folder is mounted, refreshed by the ticker so the
    /// menus never stat a possibly dead mount on the main thread.
    present: Mutex<HashMap<String, bool>>,
    /// Sized, encoded artwork: `file:<path>@<px>` for a file's own art,
    /// `<artwork key>@<px>` for an online cover.
    images: Mutex<ImageCache>,
}

impl Local {
    pub fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            session: Mutex::new(None),
            output: Mutex::new(None),
            playing: Mutex::new(None),
            loading: Mutex::new(()),
            ticks: AtomicU64::new(0),
            present: Mutex::new(HashMap::new()),
            images: Mutex::new(ImageCache::new()),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    fn with<R>(&self, f: impl FnOnce(&mut Session) -> R) -> Option<R> {
        self.session.lock().expect("local lock").as_mut().map(f)
    }

    fn level(&self) -> f32 {
        self.with(|s| if s.muted { 0.0 } else { (s.volume / 100.0).powi(2) })
            .unwrap_or(1.0) as f32
    }

    fn apply_volume(&self) {
        let level = self.level();
        if let Some(playing) = self.playing.lock().expect("playing lock").as_ref() {
            playing.sink.set_volume(level);
        }
    }

    fn position(&self) -> f64 {
        self.playing
            .lock()
            .expect("playing lock")
            .as_ref()
            .map_or(0.0, Playing::position)
    }

    /// A mixer on a live stream for the current default device, reopening the
    /// stream when the old device has gone or is no longer the default.
    fn mixer(&self) -> Option<rodio::mixer::Mixer> {
        let mut output = self.output.lock().expect("output lock");
        if !output.as_ref().is_some_and(Output::current) {
            *output = Output::open();
        }
        output.as_ref().map(|output| output.mixer.clone())
    }

    /// Decode `track` from `start` seconds into a new sink and swap it in,
    /// dropping (never clearing) the old one. Blocking file IO.
    fn start(&self, track: &Track, play: bool, start: f64) -> Result<(), String> {
        let file = std::fs::File::open(&track.path).map_err(|e| e.to_string())?;
        let mut source = rodio::Decoder::try_from(file).map_err(|e| e.to_string())?;
        let mut offset = 0.0;
        if start > 0.0 && source.try_seek(Duration::from_secs_f64(start)).is_ok() {
            offset = start;
        }
        let mixer = self.mixer().ok_or("No audio output device")?;
        let sink = Sink::connect_new(&mixer);
        sink.set_volume(self.level());
        if !play {
            sink.pause();
        }
        sink.append(source);
        *self.playing.lock().expect("playing lock") = Some(Playing { sink, offset });
        Ok(())
    }

    /// An output exists and is still the default device.
    fn output_live(&self) -> bool {
        self.output
            .lock()
            .expect("output lock")
            .as_ref()
            .is_some_and(Output::current)
    }

    fn stop(&self) {
        // Dropping, not `clear()`: see the module note.
        *self.playing.lock().expect("playing lock") = None;
    }

    // ------------------------------------------------------------ lifecycle

    /// Scan `folder` and make it the source — from YouTube Music or from
    /// another folder — paused where it was last left. Nothing starts playing.
    pub async fn open(core: &Arc<Core>, folder: PathBuf) -> Result<usize, String> {
        Self::open_scoped(core, folder, None).await
    }

    /// `open`, choosing Liked Songs (`Some(true)`) or the whole folder
    /// (`Some(false)`) rather than whichever the folder was left in.
    pub async fn open_scoped(
        core: &Arc<Core>,
        folder: PathBuf,
        liked_only: Option<bool>,
    ) -> Result<usize, String> {
        let scan_root = folder.clone();
        let (tracks, likes) = tauri::async_runtime::spawn_blocking(move || {
            let tracks = scan(&scan_root);
            let likes = Likes::load(&scan_root, &tracks);
            (tracks, likes)
        })
        .await
        .map_err(|err| err.to_string())?;
        if tracks.is_empty() {
            return Err(format!("No audio files in {}", folder.display()));
        }
        let count = tracks.len();

        // Quiet the streaming player before ours takes over; two at once is
        // never what anyone wants.
        let local = core.local();
        if !local.is_active() && core.status() == "connected" && core.snapshot().is_playing {
            let _ = core.api.toggle_play().await;
        }

        let (handle, swapper) = (Arc::clone(core), Arc::clone(&local));
        let _ = tauri::async_runtime::spawn_blocking(move || {
            swapper.swap_in(&handle, folder, tracks, likes, liked_only)
        })
        .await;
        crate::tray::refresh(&core.app);
        Ok(count)
    }

    /// Replace whatever is playing with `folder`, under `loading` so neither
    /// the ticker nor a command can land between the old session and the new.
    fn swap_in(
        &self,
        core: &Arc<Core>,
        folder: PathBuf,
        tracks: Vec<Track>,
        likes: Likes,
        liked_only: Option<bool>,
    ) {
        let _loading = self.loading.lock().expect("loading lock");
        if self.is_active() {
            self.remember(core);
            self.stop();
        }

        let path = folder.to_string_lossy().into_owned();
        let resume = core
            .store
            .get(|s| s.saved_folders().into_iter().find(|f| f.path == path));
        let resumed = resume.as_ref().and_then(|saved| {
            let track = saved.track.as_ref()?;
            let at = tracks
                .iter()
                .position(|t| t.path.strip_prefix(&folder).is_ok_and(|rel| rel == Path::new(track)))?;
            Some((at, saved.position))
        });
        let (mut cursor, mut position) = resumed.unwrap_or((0, 0.0));

        // Liked Songs only if there is something liked to play.
        if liked_only == Some(true) && likes.liked_count() == 0 {
            crate::macos::inform(
                &core.app,
                "No liked songs in this folder yet".into(),
                "Playing the whole folder. Like a song with the heart to add it to Liked Songs.".into(),
            );
        }
        let liked_only = liked_only
            .unwrap_or_else(|| resume.as_ref().is_some_and(|saved| saved.liked_only))
            && likes.liked_count() > 0;
        if liked_only && !likes.is_liked(&tracks[cursor].id) {
            // The resume point is not a liked track: start at the first liked
            // one after it, from its beginning.
            let n = tracks.len();
            if let Some(at) = (1..=n).map(|k| (cursor + k) % n).find(|&i| likes.is_liked(&tracks[i].id)) {
                cursor = at;
                position = 0.0;
            }
        }

        let repeat = resume
            .as_ref()
            .and_then(|saved| saved.repeat.clone())
            .filter(|mode| mode == "ONE")
            .unwrap_or_else(|| "ALL".into());
        let shuffled = resume.as_ref().is_some_and(|saved| saved.shuffle);

        let volume = core.snapshot().volume;
        let mut session = Session {
            root: folder,
            order: (0..tracks.len()).collect(),
            tracks,
            likes,
            cursor,
            playing: false,
            shuffle: false,
            liked_only,
            repeat: repeat.clone(),
            volume,
            muted: false,
            started: false,
            shown_second: 0,
        };
        session.rescope();
        if shuffled {
            // The resume track first, the rest of the scope shuffled after it.
            let current = session.order[session.cursor];
            let mut rest: Vec<usize> = session.order.iter().copied().filter(|&i| i != current).collect();
            shuffle(&mut rest);
            session.order = std::iter::once(current).chain(rest).collect();
            session.cursor = 0;
            session.shuffle = true;
        }
        let cursor = session.cursor;
        *self.session.lock().expect("local lock") = Some(session);

        core.store.update(|s| {
            s.source = Some("local".into());
            s.local_folder = Some(path.clone());
            s.remember_folder(&path);
        });
        // Flipped inside the player lock: see `Core::update_remote`.
        core.update(|state| {
            self.active.store(true, Ordering::SeqCst);
            state.status = "connected".into();
            state.status_message = String::new();
            state.shuffle = shuffled;
            state.repeat = Some(repeat.clone());
            state.muted = false;
            state.is_playing = false;
        });
        self.load_at_locked(core, cursor, false, position);
    }

    /// Save the current track and position against the folder, so switching
    /// away and back — or quitting — resumes here.
    pub fn remember(&self, core: &Arc<Core>) {
        if !self.is_active() {
            return;
        }
        let Some((root_path, track, liked_only, repeat, shuffled)) = self
            .with(|s| {
                if !s.started {
                    return None;
                }
                let track = s.current()?;
                Some((s.root.clone(), s.relative(track)?, s.liked_only, s.repeat.clone(), s.shuffle))
            })
            .flatten()
        else {
            return;
        };
        // An unplugged drive: whatever is "current" now is not a place anyone
        // was listening to. Checked outside the session lock — the menus take
        // it on the main thread, and a stalled mount can make this slow.
        if !root_path.is_dir() {
            return;
        }
        let root = root_path.to_string_lossy().into_owned();
        let position = self.position();
        core.store.update(|s| {
            if let Some(saved) = s.local_folders.iter_mut().find(|f| f.path == root) {
                saved.track = Some(track.clone());
                saved.position = position;
                saved.liked_only = liked_only;
                saved.repeat = Some(repeat.clone());
                saved.shuffle = shuffled;
            }
        });
    }

    /// Hand playback back to YouTube Music. Waits out a load in progress, so
    /// one cannot reopen the output and start a sink after we have left.
    /// Blocking: call off the main thread.
    pub fn close(core: &Arc<Core>) {
        let local = core.local();
        let _loading = local.loading.lock().expect("loading lock");
        local.close_locked(core);
    }

    fn close_locked(&self, core: &Arc<Core>) {
        let local = self;
        if !local.is_active() {
            return;
        }
        local.remember(core);
        local.active.store(false, Ordering::SeqCst);
        local.stop();
        *local.output.lock().expect("output lock") = None;
        *local.session.lock().expect("local lock") = None;
        core.store.update(|s| s.source = Some("youtube".into()));
        core.leave_local();
        crate::ws::retry(&core.app);
        crate::tray::refresh(&core.app);
    }

    /// Resume the stored folder at launch, if that is where we were left.
    pub fn restore(core: &Arc<Core>) {
        let (source, folder) =
            core.store.get(|s| (s.source.clone(), s.local_folder.as_ref().map(PathBuf::from)));
        let Some(folder) = folder.filter(|_| source.as_deref() == Some("local")) else {
            return;
        };
        if !folder.is_dir() {
            return;
        }
        let core = Arc::clone(core);
        tauri::async_runtime::spawn(async move {
            let _ = Local::open(&core, folder).await;
        });
    }

    /// Load `order[cursor]` and publish it. Blocking: call off the main thread
    /// and off async workers. Returns whether it decoded.
    fn load(&self, core: &Arc<Core>, cursor: usize, play: bool) -> bool {
        let _loading = self.loading.lock().expect("loading lock");
        self.load_locked(core, cursor, play)
    }

    fn load_locked(&self, core: &Arc<Core>, cursor: usize, play: bool) -> bool {
        self.load_at_locked(core, cursor, play, 0.0)
    }

    fn load_at_locked(&self, core: &Arc<Core>, cursor: usize, play: bool, start: f64) -> bool {
        let Some((track, started)) = self.try_start(cursor, play, start) else {
            return false;
        };
        self.publish(core, track, play && started, started);
        // Where the sink really is: a resume whose seek failed starts at 0,
        // and showing the saved time would then save the wrong place on pause.
        let position = self.position();
        if started && position > 0.0 {
            self.with(|s| s.shown_second = position as u64);
            core.update(|state| state.position = position);
        }
        started
    }

    /// Point the cursor at `cursor` and start it, publishing nothing. `None`
    /// when there is no such track or the folder has been closed meanwhile.
    fn try_start(&self, cursor: usize, play: bool, start: f64) -> Option<(Track, bool)> {
        let track = self
            .with(|s| {
                s.cursor = cursor.min(s.order.len().saturating_sub(1));
                s.shown_second = 0;
                s.current().cloned()
            })
            .flatten()?;
        let started = match self.start(&track, play, start) {
            Ok(()) => true,
            Err(err) => {
                eprintln!("local: cannot play {}: {err}", track.path.display());
                self.stop();
                false
            }
        };
        // `close` holds `loading` too, so this only trips if the folder was
        // replaced; never leave a sink running for a folder that is gone.
        if !self.is_active() {
            self.stop();
            return None;
        }
        self.with(|s| {
            s.playing = play && started;
            s.started = started;
        });
        Some((track, started))
    }

    /// Tell both surfaces, once, which track is now loaded.
    /// `started` is false for a track that would not open: it is shown, but
    /// not saved as the place to resume — that would overwrite the real one.
    fn publish(&self, core: &Arc<Core>, track: Track, playing: bool, started: bool) {
        // A track kept in Liked Songs only because it was playing when the
        // scope changed (or was unliked) is dropped once another one is up.
        self.with(|s| {
            if s.liked_only {
                s.rescope();
            }
        });
        // Only what is already sized in memory: no image decode on the path
        // that starts the audio.
        let cover = self.cover_now(core, &track, crate::api::COVER_PX);
        let missing = cover.is_none();
        let like = self.rating_of(&track.id).map_or("INDIFFERENT", Rating::as_str);
        core.show_local(track.song(&names(core)), cover, playing, like);
        if missing {
            self.fetch_cover(core, track);
        }
        self.warm_next(core);
        if started {
            self.remember(core);
        }
    }

    fn rating_of(&self, id: &str) -> Option<Rating> {
        self.with(|s| s.likes.get(id)).flatten()
    }

    /// The heart and its right-click: pressing the state you are in clears it,
    /// as YouTube Music's buttons do. Disliking the track that is playing moves
    /// on, since a disliked track is one you have said you do not want to hear.
    ///
    /// Under `loading`, so the track rated is the one on screen — not the next
    /// one a load in progress has already moved the cursor to.
    pub fn rate(&self, core: &Arc<Core>, rating: Rating) {
        let _loading = self.loading.lock().expect("loading lock");
        let Some((current, now, save, cursor)) = self
            .with(|s| {
                let track = s.current()?.clone();
                let now = s.likes.get(&track.id);
                let next = if now == Some(rating) { None } else { Some(rating) };
                let save = s.likes.set(&track, next);
                Some((track, next, save, s.cursor))
            })
            .flatten()
        else {
            return;
        };
        if let Some(save) = save {
            save.write();
        }
        // Only the playing track is rated, and it is always in `order`, so the
        // scope needs no rebuild here; an unliked one leaves Liked Songs when
        // `publish` rescopes after it ends.
        let like = now.map_or("INDIFFERENT", Rating::as_str);
        core.update(|state| {
            if state.song.as_ref().is_some_and(|song| song.video_id == current.id) {
                state.like = Some(like.to_string());
            }
        });
        core.publish_local_queue();

        let playing = self.with(|s| s.playing).unwrap_or(false);
        if now == Some(Rating::Dislike) && playing {
            self.advance_locked(core, cursor, 1, true);
        }
    }

    /// Step through the order from `cursor` by `step` until a track plays,
    /// trying each at most once and publishing only the one that started (or
    /// the last failure) — so a run of bad files is one update, not one per
    /// file. An unplugged drive stops at the first failure.
    fn advance_locked(&self, core: &Arc<Core>, cursor: usize, step: isize, play: bool) {
        let Some((len, root)) = self.with(|s| (s.order.len().max(1) as isize, s.root.clone())) else {
            return;
        };
        let mut last = None;
        for k in 1..=len {
            let next = (cursor as isize + step * k).rem_euclid(len) as usize;
            // Disliked tracks are never reached by advancing, nor unliked ones
            // under Liked Songs; picking one from the queue still plays it.
            let reachable = self
                .with(|s| s.order.get(next).is_some_and(|&i| s.reachable(i)))
                .unwrap_or(false);
            if !reachable {
                continue;
            }
            let Some((track, started)) = self.try_start(next, play, 0.0) else {
                return;
            };
            if started {
                self.publish(core, track, play, true);
                return;
            }
            last = Some(track);
            if !root.is_dir() {
                break;
            }
        }
        match last {
            Some(track) => self.publish(core, track, false, false),
            // Everything else is disliked: stop rather than replay it.
            None => {
                self.stop();
                self.with(|s| s.playing = false);
                core.update(|state| state.is_playing = false);
            }
        }
    }

    fn advance(&self, core: &Arc<Core>, step: isize) {
        let _loading = self.loading.lock().expect("loading lock");
        let Some(cursor) = self.with(|s| s.cursor) else { return };
        self.advance_locked(core, cursor, step, true);
    }

    /// Resolve the playing track's cover off the audio path and show it if the
    /// track is still the one playing when the answer arrives.
    fn fetch_cover(&self, core: &Arc<Core>, track: Track) {
        let core = Arc::clone(core);
        tauri::async_runtime::spawn(async move {
            let local = core.local();
            let id = track.id.clone();
            let watcher = Arc::clone(&core);
            let alive: crate::artwork::Alive = Arc::new(move || {
                watcher.snapshot().song.is_some_and(|song| song.video_id == id)
            });
            let cover = local
                .cover(&core, &track, crate::api::COVER_PX, crate::artwork::Priority::Now(alive))
                .await;
            let Some(cover) = cover else {
                return;
            };
            let current = core.snapshot().song.map(|song| song.video_id);
            if current.as_deref() == Some(track.id.as_str()) {
                core.set_cover(Some(cover));
            }
        });
    }

    /// Size the next track's cover now, so skipping to it — or reaching it —
    /// shows the artwork with the first frame instead of after a decode.
    fn warm_next(&self, core: &Arc<Core>) {
        let Some(next) = self
            .with(|s| s.order.get((s.cursor + 1) % s.order.len().max(1)).map(|&i| s.tracks[i].clone()))
            .flatten()
        else {
            return;
        };
        let core = Arc::clone(core);
        tauri::async_runtime::spawn(async move {
            let _ = core
                .local()
                .cover(&core, &next, crate::api::COVER_PX, crate::artwork::Priority::background())
                .await;
        });
    }

    fn wanted(core: &Arc<Core>, track: &Track) -> crate::artwork::Wanted {
        crate::artwork::Wanted {
            artist: crate::store::map_artist(&names(core), &track.artist),
            album: track.album.clone(),
            title: track.title.clone(),
        }
    }

    fn memo(&self, key: &str) -> Option<Arc<str>> {
        self.images.lock().expect("images lock").get(key)
    }

    fn remember_art(&self, key: String, art: Arc<str>) {
        self.images.lock().expect("images lock").insert(key, art);
    }

    fn file_key(track: &Track, size: u32) -> String {
        format!("file:{}@{size}", track.path.display())
    }

    /// The cover if it is already sized in memory. No IO at all.
    fn cover_now(&self, core: &Arc<Core>, track: &Track, size: u32) -> Option<Arc<str>> {
        if track.has_art {
            if let Some(hit) = self.memo(&Self::file_key(track, size)) {
                return Some(hit);
            }
        }
        crate::artwork::keys(&Self::wanted(core, track))
            .iter()
            .find_map(|key| self.memo(&format!("{key}@{size}")))
    }

    /// The file's own art, sized; blocking IO and decode, so callers run it
    /// off the async workers.
    fn file_art(&self, track: &Track, size: u32) -> Option<Arc<str>> {
        let key = Self::file_key(track, size);
        if let Some(hit) = self.memo(&key) {
            return Some(hit);
        }
        let art = read_art(&track.path, size)?;
        self.remember_art(key, art.clone());
        Some(art)
    }

    /// The track's cover at `size`: its own art, else an online cover.
    async fn cover(
        self: &Arc<Self>,
        core: &Arc<Core>,
        track: &Track,
        size: u32,
        priority: crate::artwork::Priority,
    ) -> Option<Arc<str>> {
        if let Some(hit) = self.cover_now(core, track, size) {
            return Some(hit);
        }
        if track.has_art {
            let (local, owned) = (Arc::clone(self), track.clone());
            let art = tauri::async_runtime::spawn_blocking(move || local.file_art(&owned, size))
                .await
                .ok()
                .flatten();
            if art.is_some() {
                return art;
            }
        }
        let (key, bytes) = crate::artwork::find(core, &Self::wanted(core, track), priority).await?;
        let url = tauri::async_runtime::spawn_blocking(move || {
            crate::artwork::resize(&bytes, size).map(|sized| crate::artwork::data_url(&sized))
        })
        .await
        .ok()
        .flatten()?;
        self.remember_art(format!("{key}@{size}"), url.clone());
        Some(url)
    }

    /// Artwork for queue rows: the file's own, else an online cover. One
    /// lookup per album — rows after the first read the sized copy. Replies
    /// after `QUEUE_ART_WAIT` with whatever is ready; lookups still running
    /// carry on and warm the cache, and the renderer asks again for any row it
    /// draws without art.
    pub async fn queue_art(core: &Arc<Core>, ids: Vec<String>, size: u32) -> Vec<(String, Arc<str>)> {
        let local = core.local();
        let tracks: Vec<Track> = ids.iter().filter_map(|id| local.track(id)).collect();
        let tasks: Vec<_> = tracks
            .into_iter()
            .map(|track| {
                let (core, local) = (Arc::clone(core), Arc::clone(&local));
                tauri::async_runtime::spawn(async move {
                    let art = local
                        .cover(&core, &track, size, crate::artwork::Priority::background())
                        .await?;
                    Some((track.id, art))
                })
            })
            .collect();
        let deadline = tokio::time::Instant::now() + QUEUE_ART_WAIT;
        let mut ready = Vec::new();
        let mut late = Vec::new();
        for mut task in tasks {
            match tokio::time::timeout_at(deadline, &mut task).await {
                Ok(Ok(Some(hit))) => ready.push(hit),
                Ok(_) => {}
                Err(_) => late.push(task),
            }
        }
        // Rows that missed the reply: once their covers are cached, say so —
        // ids only, the art itself still travels by request — and each
        // surface asks again for whatever it is showing without art.
        if !late.is_empty() {
            let app = core.app.clone();
            tauri::async_runtime::spawn(async move {
                let mut landed = false;
                for task in late {
                    landed |= matches!(task.await, Ok(Some(_)));
                }
                if landed {
                    let _ = tauri::Emitter::emit(&app, "queue-art-ready", ());
                }
            });
        }
        ready
    }

    /// Artist names changed: re-label what is showing and give the covers that
    /// were missed under the old name another try.
    pub fn renamed(&self, core: &Arc<Core>) {
        let Some(track) = self.with(|s| s.current().cloned()).flatten() else {
            return;
        };
        crate::artwork::forget_misses(&Self::wanted(core, &track));
        if core.bootstrap().cover.is_none() {
            self.fetch_cover(core, track);
        }
    }

    // ----------------------------------------------------------------- scope

    /// For the Source menu: `(liked_only, liked count)` while a folder plays.
    pub fn scope(&self) -> Option<(bool, usize)> {
        if !self.is_active() {
            return None;
        }
        self.with(|s| (s.liked_only, s.likes.liked_count()))
    }

    /// Source → Liked Songs or Local Music while a folder is already playing.
    /// Nothing is cut off: a playing track outside the new scope stays pinned
    /// and finishes, and the next track in scope follows.
    /// Blocking: call off the main thread.
    pub fn set_scope(&self, core: &Arc<Core>, liked_only: bool) {
        let _loading = self.loading.lock().expect("loading lock");
        let changed = self
            .with(|s| {
                if liked_only && s.likes.liked_count() == 0 {
                    return false;
                }
                s.liked_only = liked_only;
                s.rescope();
                true
            })
            .unwrap_or(false);
        if changed {
            core.publish_local_queue();
            self.remember(core);
        }
        // Always: AppKit toggled the checkmark when it was clicked, even when
        // the switch was refused.
        crate::tray::refresh(&core.app);
    }

    // ----------------------------------------------------------------- trash

    /// The playing track, for the delete confirmation.
    pub fn current(&self) -> Option<Track> {
        self.with(|s| s.current().cloned()).flatten()
    }

    /// Move `id` to the Trash and carry on with the track after it. Checked by
    /// id so a dialog answered after the track changed cannot trash the next.
    pub fn trash(&self, core: &Arc<Core>, id: &str) -> Result<(), String> {
        // Under `loading` throughout: the rating write must not interleave
        // with one from `rate`, nor the index rewrite below with a load.
        let _loading = self.loading.lock().expect("loading lock");
        let Some(track) = self.track(id) else {
            return Err("That track is no longer in the folder".into());
        };
        crate::macos::trash(&track.path)?;
        if let Some(save) = self.with(|s| s.likes.forget(&track)).flatten() {
            save.write();
        }
        // The AppleDouble shadow exFAT keeps beside it is meaningless alone.
        if let (Some(dir), Some(name)) = (track.path.parent(), track.path.file_name()) {
            let shadow = dir.join(format!("._{}", name.to_string_lossy()));
            if shadow.exists() {
                let _ = crate::macos::trash(&shadow);
            }
        }

        let next = self.with(|s| {
            let Some(index) = s.tracks.iter().position(|t| t.id == id) else {
                return None;
            };
            let was_current = s.order.get(s.cursor) == Some(&index);
            s.tracks.remove(index);
            let slot = s.order.iter().position(|&i| i == index);
            s.order.retain(|&i| i != index);
            for i in s.order.iter_mut() {
                if *i > index {
                    *i -= 1;
                }
            }
            // The slot the deleted track held is now the one after it.
            if let Some(slot) = slot {
                if slot < s.cursor {
                    s.cursor -= 1;
                }
            }
            if s.cursor >= s.order.len() {
                s.cursor = 0;
            }
            // Liked Songs emptied by this delete: the folder is still there,
            // so fall back to all of it rather than leaving it.
            if s.order.is_empty() && !s.tracks.is_empty() {
                s.liked_only = false;
                s.rescope();
            }
            Some((was_current, s.cursor, s.playing, s.tracks.is_empty()))
        });

        match next.flatten() {
            Some((_, _, _, true)) => self.close_locked(core),
            Some((true, cursor, playing, _)) => {
                // The deleted file's slot now holds the track after it — unless
                // that one is out of reach, which advancing never lands on.
                let unreachable = self
                    .with(|s| s.order.get(cursor).is_some_and(|&i| !s.reachable(i)))
                    .unwrap_or(false);
                if unreachable || (!self.load_locked(core, cursor, playing) && playing) {
                    self.advance_locked(core, cursor, 1, playing);
                }
            }
            _ => core.publish_local_queue(),
        }
        Ok(())
    }

    // -------------------------------------------------------------- commands

    pub fn toggle_play(&self, core: &Arc<Core>) {
        // Under `loading`, so a pause cannot land on a sink a load is about to
        // mark as playing, nor a press restart a track the tick is advancing.
        let _loading = self.loading.lock().expect("loading lock");
        let Some(playing) = self.with(|s| s.playing) else { return };
        // A sink on a dead or no-longer-default device would "resume" in
        // silence: restart it on a live one at the same place instead.
        if !playing && !self.output_live() {
            let position = self.position();
            let Some(track) = self.with(|s| s.current().cloned()).flatten() else { return };
            if self.start(&track, true, position).is_ok() {
                self.with(|s| s.playing = true);
                core.update(|state| state.is_playing = true);
            }
            return;
        }
        let resumable = {
            let held = self.playing.lock().expect("playing lock");
            match held.as_ref().filter(|p| !p.sink.empty()) {
                Some(p) if playing => {
                    p.sink.pause();
                    true
                }
                Some(p) => {
                    p.sink.play();
                    true
                }
                None => false,
            }
        };
        if !resumable {
            // Nothing loaded (it ended, or never decoded): start it over.
            let cursor = self.with(|s| s.cursor).unwrap_or(0);
            self.load_locked(core, cursor, true);
            return;
        }
        self.with(|s| s.playing = !playing);
        core.update(|state| state.is_playing = !playing);
        if playing {
            self.remember(core);
        }
    }

    pub fn next(&self, core: &Arc<Core>) {
        self.advance(core, 1);
    }

    pub fn previous(&self, core: &Arc<Core>) {
        if self.position() > RESTART_AFTER {
            self.seek(core, 0.0);
            return;
        }
        self.advance(core, -1);
    }

    /// Reopen the file at `seconds` in a new sink — never `Sink::try_seek`,
    /// which waits on the audio callback.
    pub fn seek(&self, core: &Arc<Core>, seconds: f64) {
        let _loading = self.loading.lock().expect("loading lock");
        let seconds = seconds.max(0.0);
        let Some((track, playing)) = self
            .with(|s| s.current().cloned().map(|track| (track, s.playing)))
            .flatten()
        else {
            return;
        };
        if self.start(&track, playing, seconds).is_err() {
            return;
        }
        self.with(|s| s.shown_second = seconds as u64);
        core.update(|state| state.position = seconds);
    }

    pub fn set_volume(&self, core: &Arc<Core>, volume: f64) {
        self.with(|s| s.volume = volume);
        self.apply_volume();
        core.update(|state| state.volume = volume);
    }

    pub fn toggle_mute(&self, core: &Arc<Core>) {
        let muted = self.with(|s| {
            s.muted = !s.muted;
            s.muted
        });
        self.apply_volume();
        if let Some(muted) = muted {
            core.update(|state| state.muted = muted);
        }
    }

    /// Reshuffle everything after the current track, or restore folder order,
    /// keeping the playing track where it is.
    pub fn toggle_shuffle(&self, core: &Arc<Core>) {
        let shuffle = self.with(|s| {
            let Some(&current) = s.order.get(s.cursor) else {
                return s.shuffle;
            };
            s.shuffle = !s.shuffle;
            if s.shuffle {
                // Within the scope: Liked Songs shuffles only liked tracks.
                let mut rest: Vec<usize> = s
                    .order
                    .iter()
                    .copied()
                    .filter(|&i| i != current)
                    .collect();
                shuffle(&mut rest);
                s.order = std::iter::once(current).chain(rest).collect();
                s.cursor = 0;
            } else {
                s.rescope();
            }
            s.shuffle
        });
        if let Some(shuffle) = shuffle {
            core.update(|state| state.shuffle = shuffle);
            core.publish_local_queue();
            self.remember(core);
        }
    }

    /// Mirrors the widget's two-mode button: ALL ↔ ONE.
    pub fn cycle_repeat(&self, core: &Arc<Core>) {
        let mode = self.with(|s| {
            s.repeat = if s.repeat == "ALL" { "ONE" } else { "ALL" }.into();
            s.repeat.clone()
        });
        if let Some(mode) = mode {
            core.update(|state| state.repeat = Some(mode));
            self.remember(core);
        }
    }

    /// Jump to a slot of the play order, verified by id like the remote path.
    ///
    /// Search covers the whole folder, so under Liked Songs a result can be
    /// outside `order`. It is slotted in after the current track and played,
    /// pinned like any out-of-scope track until the next one is up.
    pub fn play_id(&self, core: &Arc<Core>, index: Option<usize>, id: &str) -> bool {
        let found = self.with(|s| {
            let matches = |slot: usize| s.order.get(slot).is_some_and(|&i| s.tracks[i].id == id);
            let slot = match index {
                Some(slot) if matches(slot) => Some(slot),
                _ => (0..s.order.len()).find(|&slot| matches(slot)),
            };
            slot.or_else(|| {
                let at = s.tracks.iter().position(|t| t.id == id)?;
                let slot = (s.cursor + 1).min(s.order.len());
                s.order.insert(slot, at);
                Some(slot)
            })
        });
        match found.flatten() {
            Some(slot) => self.load(core, slot, true),
            None => false,
        }
    }

    // ------------------------------------------------------------- realtime

    /// Last known mount state of a saved folder; unknown counts as present.
    pub fn folder_present(&self, path: &str) -> bool {
        self.present.lock().expect("present lock").get(path).copied().unwrap_or(true)
    }

    fn refresh_present(&self, core: &Arc<Core>) {
        let folders = core.store.get(|s| s.saved_folders());
        let present: HashMap<String, bool> = folders
            .into_iter()
            .map(|f| {
                let here = Path::new(&f.path).is_dir();
                (f.path, here)
            })
            .collect();
        let changed = {
            let mut held = self.present.lock().expect("present lock");
            let changed = *held != present;
            *held = present;
            changed
        };
        if changed {
            crate::tray::refresh(&core.app);
        }
    }

    fn tick(&self, core: &Arc<Core>) {
        if self.ticks.load(Ordering::Relaxed) % DEVICE_CHECK_TICKS == 0 {
            self.refresh_present(core);
        }
        if !self.is_active() {
            self.ticks.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // A load is swapping sinks: what we would read is the gap between them.
        let Ok(_loading) = self.loading.try_lock() else { return };
        let Some((playing, repeat, cursor)) = self.with(|s| (s.playing, s.repeat.clone(), s.cursor))
        else {
            return;
        };
        let (ended, position) = match self.playing.lock().expect("playing lock").as_ref() {
            Some(p) => (p.sink.empty(), p.position()),
            None => (true, 0.0),
        };

        if playing && ended {
            // Natural end. Folder order wraps under ALL, ONE replays.
            if repeat == "ONE" && self.load_locked(core, cursor, true) {
                return;
            }
            self.advance_locked(core, cursor, 1, true);
            return;
        }

        // The device went away or the default moved (AirPods connected): carry
        // on from here on whatever is the default now.
        let check = self.ticks.fetch_add(1, Ordering::Relaxed) % DEVICE_CHECK_TICKS == 0;
        let loaded = self.playing.lock().expect("playing lock").is_some();
        let stale = check && loaded && !self.output_live();
        if stale {
            if let Some(track) = self.with(|s| s.current().cloned()).flatten() {
                let _ = self.start(&track, playing, position);
            }
        }

        let second = position as u64;
        let changed = self
            .with(|s| std::mem::replace(&mut s.shown_second, second) != second)
            .unwrap_or(false);
        if changed && playing {
            core.update(|state| state.position = position);
        }
    }

    /// Its own thread rather than an async task: a tick may load the next
    /// track, which is file IO.
    pub fn spawn_ticker(core: Arc<Core>) {
        let _ = std::thread::Builder::new()
            .name("local-tick".into())
            .spawn(move || loop {
                std::thread::sleep(TICK);
                core.local().tick(&core);
            });
    }

    // ----------------------------------------------------------- projections

    pub fn queue_view(&self, names: &BTreeMap<String, String>) -> QueueView {
        self.with(|s| QueueView {
            items: s
                .order
                .iter()
                .enumerate()
                .take(crate::search::QUEUE_MAX)
                .map(|(slot, &i)| {
                    let track = &s.tracks[i];
                    QueueTrack {
                        index: slot,
                        video_id: track.id.clone(),
                        title: track.title.clone(),
                        artist: crate::store::map_artist(names, &track.artist),
                        duration: clock(track.duration),
                        art: Some(Arc::from(track.id.as_str())),
                        rating: s.likes.get(&track.id).map(Rating::as_str),
                    }
                })
                .collect(),
            current: Some(s.cursor),
            truncated: s.order.len() > crate::search::QUEUE_MAX,
        })
        .unwrap_or_else(QueueView::empty)
    }

    pub fn track(&self, id: &str) -> Option<Track> {
        self.with(|s| s.tracks.iter().find(|t| t.id == id).cloned())
            .flatten()
    }


    /// Plain substring search over title, artist (raw or renamed) and album.
    /// Covers come from the file or the artwork cache, never the network.
    pub fn search(&self, query: &str, names: &BTreeMap<String, String>) -> Vec<SearchResult> {
        let needle = query.to_lowercase();
        let hits: Vec<Track> = self
            .with(|s| {
                s.tracks
                    .iter()
                    .filter(|t| {
                        let shown = crate::store::map_artist(names, &t.artist);
                        [&t.title, &t.artist, &shown, &t.album]
                            .iter()
                            .any(|field| field.to_lowercase().contains(&needle))
                    })
                    .take(SEARCH_MAX)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        hits.into_iter()
            .map(|t| {
                let artist = crate::store::map_artist(names, &t.artist);
                let size = crate::api::THUMB_PX;
                let own = t.has_art.then(|| self.file_art(&t, size)).flatten();
                let thumbnail = own.or_else(|| {
                    let wanted = crate::artwork::Wanted {
                        artist: artist.clone(),
                        album: t.album.clone(),
                        title: t.title.clone(),
                    };
                    let keys = crate::artwork::keys(&wanted);
                    if let Some(hit) = keys.iter().find_map(|key| self.memo(&format!("{key}@{size}"))) {
                        return Some(hit);
                    }
                    let (key, bytes) = crate::artwork::cached(&wanted)?;
                    let url = crate::artwork::data_url(&crate::artwork::resize(&bytes, size)?);
                    self.remember_art(format!("{key}@{size}"), url.clone());
                    Some(url)
                });
                SearchResult {
                thumbnail,
                subtitle: [artist.as_str(), t.album.as_str()]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join(" • "),
                video_id: t.id,
                title: t.title,
            }})
            .collect()
    }
}

fn names(core: &Core) -> BTreeMap<String, String> {
    core.store.get(|s| s.artist_names.clone())
}

// ------------------------------------------------------------------- helpers

fn clock(seconds: f64) -> String {
    let total = seconds.round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

fn shuffle(items: &mut [usize]) {
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    for i in (1..items.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        items.swap(i, (seed % (i as u64 + 1)) as usize);
    }
}

/// Stable across launches so lyrics-cache entries and timing overrides stick.
fn id_for(path: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{ID_PREFIX}{hash:016x}")
}

/// `2001 - 范特西` → `范特西`: the organiser's year prefix is not album text.
fn album_from_dir(name: &str) -> String {
    match name.split_once(" - ") {
        Some((year, rest)) if year.len() == 4 && year.chars().all(|c| c.is_ascii_digit()) => {
            rest.trim().to_string()
        }
        _ => name.trim_start_matches('_').to_string(),
    }
}

/// Tag readers working in parallel on a cold scan. A USB SSD answers several
/// small reads at once far faster than one after another.
const SCAN_THREADS: usize = 4;

/// A file as listed: enough to tell whether its indexed tags are still good.
struct Listed {
    path: PathBuf,
    size: u64,
    modified: u64,
}

/// One entry of the on-disk library index.
#[derive(Serialize, Deserialize)]
struct Indexed {
    size: u64,
    modified: u64,
    track: Track,
}

/// `~/Library/Caches/pear-music-widget/library/<hash of root>.json`.
/// A stable file-name-safe name for a folder, shared by the library index and
/// the likes fallback.
pub fn folder_key(root: &Path) -> String {
    id_for(root).trim_start_matches(ID_PREFIX).to_string()
}

fn index_path(root: &Path) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let name = folder_key(root);
    PathBuf::from(home)
        .join("Library/Caches/pear-music-widget/library")
        .join(format!("{name}.json"))
}

/// Every audio file under `root`, tags read only for files the index does not
/// already know at this size and modification time. Listing a 360-file drive
/// is a quarter second; reading every tag cold is ten or more, which is the
/// difference between the widget being ready and not at launch.
pub fn scan(root: &Path) -> Vec<Track> {
    let mut files = Vec::new();
    collect(root, &mut files);
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let index_file = index_path(root);
    let mut index: HashMap<PathBuf, Indexed> = std::fs::read(&index_file)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<Indexed>>(&bytes).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|entry| (entry.track.path.clone(), entry))
        .collect();
    let known = |file: &Listed, index: &HashMap<PathBuf, Indexed>| {
        index
            .get(&file.path)
            .is_some_and(|entry| entry.size == file.size && entry.modified == file.modified)
    };

    // Sidecars are per folder: look once per folder, not once per file.
    let with_sidecar: HashSet<PathBuf> = files
        .iter()
        .filter_map(|file| file.path.parent())
        .collect::<HashSet<_>>()
        .into_iter()
        .filter(|dir| sidecar_in(dir).is_some())
        .map(Path::to_path_buf)
        .collect();

    let stale: Vec<&Listed> = files.iter().filter(|file| !known(file, &index)).collect();
    // Each result travels with its own file, and a tag reader that panics on
    // one odd file costs that file its tags rather than shifting every later
    // track onto the wrong path.
    let fresh: Vec<(&Listed, Track)> = std::thread::scope(|scope| {
        let chunk = stale.len().div_ceil(SCAN_THREADS).max(1);
        let workers: Vec<_> = stale
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|file| {
                            let track = std::panic::catch_unwind(|| read_track(root, &file.path))
                                .unwrap_or_else(|_| untagged_track(root, &file.path));
                            (*file, track)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap_or_default())
            .collect()
    });
    let changed = !fresh.is_empty() || index.len() != files.len();
    for (file, track) in fresh {
        index.insert(
            file.path.clone(),
            Indexed {
                size: file.size,
                modified: file.modified,
                track,
            },
        );
    }

    let tracks: Vec<Track> = files
        .iter()
        .filter_map(|file| index.get(&file.path))
        .map(|entry| {
            let mut track = entry.track.clone();
            track.has_art |= track.path.parent().is_some_and(|dir| with_sidecar.contains(dir));
            track.size = entry.size;
            track
        })
        .collect();

    if changed {
        let entries: Vec<&Indexed> = files.iter().filter_map(|file| index.get(&file.path)).collect();
        if let (Some(dir), Ok(json)) = (index_file.parent(), serde_json::to_vec(&entries)) {
            let temp = index_file.with_extension("tmp");
            if std::fs::create_dir_all(dir).is_ok() && std::fs::write(&temp, json).is_ok() {
                let _ = std::fs::rename(&temp, &index_file);
            }
        }
    }
    tracks
}

fn collect(dir: &Path, out: &mut Vec<Listed>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        // `._*` are AppleDouble shadows on exFAT drives, not audio.
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            collect(&path, out);
            continue;
        }
        let audio = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
        if !audio {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let modified = meta
            .modified()
            .ok()
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(Listed {
            path,
            size: meta.len(),
            modified,
        });
    }
}

/// What the path alone says about a file: title, folder artist and album.
fn untagged_track(root: &Path, path: &Path) -> Track {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // The first folder under the root is the artist in an Artist/Album tree —
    // but only a folder: a file sitting in the root has none.
    let top = path
        .strip_prefix(root)
        .ok()
        .filter(|rel| rel.components().count() >= 2)
        .and_then(|rel| rel.components().next())
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .unwrap_or_default();

    Track {
        id: id_for(path),
        path: path.to_path_buf(),
        title: stem,
        artist: top,
        album: album_from_dir(&parent),
        duration: 0.0,
        has_art: false,
        size: 0,
    }
}

fn read_track(root: &Path, path: &Path) -> Track {
    let mut track = untagged_track(root, path);
    if let Ok(file) = lofty::read_from_path(path) {
        track.duration = file.properties().duration().as_secs_f64();
        track.has_art = file.tags().iter().any(|tag| !tag.pictures().is_empty());
        if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
            if let Some(title) = tag.title().filter(|t| !t.trim().is_empty()) {
                track.title = title.trim().to_string();
            }
            if let Some(artist) = tag.artist().filter(|t| !t.trim().is_empty()) {
                track.artist = artist.trim().to_string();
            }
            // Rip-site watermarks ("…-MOOFEEL.COM") are not album names.
            let watermark = |t: &str| {
                let t = t.to_ascii_lowercase();
                t.contains(".com") || t.contains("www.")
            };
            if let Some(album) = tag
                .album()
                .filter(|t| !t.trim().is_empty() && !watermark(t))
            {
                track.album = album.trim().to_string();
            }
        }
    }
    track
}

/// Embedded picture first, then a conventional sidecar, downscaled to `size`
/// so a 3 MB scan never rides an event.
fn read_art(path: &Path, size: u32) -> Option<Arc<str>> {
    let bytes = embedded_art(path).or_else(|| sidecar_art(path))?;
    Some(crate::artwork::data_url(&crate::artwork::resize(&bytes, size)?))
}

fn embedded_art(path: &Path) -> Option<Vec<u8>> {
    let file = lofty::read_from_path(path).ok()?;
    file.tags()
        .iter()
        .flat_map(|tag| tag.pictures())
        .next()
        .map(|picture| picture.data().to_vec())
}

fn sidecar_art(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(sidecar_in(path.parent()?)?).ok()
}

fn sidecar_in(dir: &Path) -> Option<PathBuf> {
    ["cover", "folder", "front", "Cover", "Folder", "Front"]
        .iter()
        .flat_map(|name| ["jpg", "jpeg", "png"].map(|ext| dir.join(format!("{name}.{ext}"))))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with(likes_on: &[usize], name: &str) -> (Session, PathBuf) {
        let root = std::env::temp_dir().join(format!("pmw-scope-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let tracks: Vec<Track> = (0..5)
            .map(|i| Track {
                id: format!("local-{i}"),
                path: root.join(format!("{i}.flac")),
                title: format!("T{i}"),
                artist: "A".into(),
                album: String::new(),
                duration: 100.0,
                has_art: false,
                size: i as u64,
            })
            .collect();
        let mut likes = Likes::load(&root, &tracks);
        for &i in likes_on {
            likes.set(&tracks[i], Some(Rating::Like)).unwrap().write();
        }
        let session = Session {
            root: root.clone(),
            order: (0..tracks.len()).collect(),
            tracks,
            likes,
            cursor: 0,
            playing: false,
            shuffle: false,
            liked_only: false,
            repeat: "ALL".into(),
            volume: 100.0,
            muted: false,
            started: false,
            shown_second: 0,
        };
        (session, root)
    }

    #[test]
    fn liked_songs_keeps_the_playing_track_until_it_ends() {
        let (mut s, root) = session_with(&[1, 3], "pin");
        s.cursor = 2; // playing track 2, not liked
        s.liked_only = true;
        s.rescope();
        assert_eq!(s.order, vec![1, 2, 3]);
        assert_eq!(s.order[s.cursor], 2);
        assert!(!s.reachable(2) && s.reachable(1) && s.reachable(3));

        // Once track 3 is up, track 2 drops out.
        s.cursor = 2;
        s.rescope();
        assert_eq!(s.order, vec![1, 3]);
        assert_eq!(s.order[s.cursor], 3);

        // Back to the whole folder, still on track 3.
        s.liked_only = false;
        s.rescope();
        assert_eq!(s.order, vec![0, 1, 2, 3, 4]);
        assert_eq!(s.order[s.cursor], 3);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn shuffled_liked_songs_stays_within_the_likes() {
        let (mut s, root) = session_with(&[0, 2, 4], "shuffle");
        s.liked_only = true;
        s.rescope();
        s.shuffle = true;
        s.rescope();
        let mut seen = s.order.clone();
        seen.sort();
        assert_eq!(seen, vec![0, 2, 4]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn strips_year_prefix() {
        assert_eq!(album_from_dir("2001 - 范特西"), "范特西");
        assert_eq!(album_from_dir("_单曲与合作"), "单曲与合作");
        assert_eq!(album_from_dir("Live - 2004"), "Live - 2004");
    }

    #[test]
    fn ids_are_cache_safe() {
        let id = id_for(Path::new("/Volumes/X/双截棍.wav"));
        assert!(id.starts_with(ID_PREFIX));
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    /// `PMW_SCAN=/path cargo test scan_folder -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn scan_folder() {
        let root = std::env::var("PMW_SCAN").expect("PMW_SCAN");
        let tracks = scan(Path::new(&root));
        for track in tracks.iter().take(400) {
            let art = embedded_art(&track.path).map(|b| b.len()).unwrap_or(0);
            println!(
                "{} | {} | {} | {:.0}s | art {}",
                track.title, track.artist, track.album, track.duration, art
            );
        }
        println!("{} tracks", tracks.len());
    }
}

/// `PMW_SCAN=/path cargo test local_costs -- --ignored --nocapture`
#[cfg(test)]
mod costs {
    use super::*;
    use rodio::Source;
    use std::time::Instant;

    #[test]
    #[ignore]
    fn local_costs() {
        let root = std::env::var("PMW_SCAN").expect("PMW_SCAN");
        let at = Instant::now();
        let tracks = scan(Path::new(&root));
        println!("scan {} tracks: {:?}", tracks.len(), at.elapsed());

        let at = Instant::now();
        let with_art = tracks.iter().take(60).filter(|t| embedded_art(&t.path).is_some()).count();
        println!("embedded_art x60 ({with_art} hits): {:?}", at.elapsed());

        let hires = tracks.iter().find(|t| t.title.contains("LP24")).expect("24/96 track");
        let at = Instant::now();
        let decoder = rodio::Decoder::try_from(std::fs::File::open(&hires.path).unwrap()).unwrap();
        let (rate, channels) = (decoder.sample_rate(), decoder.channels());
        let samples = decoder.take((rate as usize) * channels as usize * 10).count();
        println!("decode 10s of {} Hz x{}: {:?} ({samples} samples)", rate, channels, at.elapsed());

        if let Some(bytes) = tracks.iter().find_map(|t| embedded_art(&t.path)) {
            let at = Instant::now();
            let _ = crate::artwork::resize(&bytes, 320);
            println!("resize embedded {} KB to 320: {:?}", bytes.len() / 1024, at.elapsed());
        }
    }
}
