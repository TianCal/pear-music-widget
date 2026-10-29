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
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lofty::prelude::*;
use rodio::Sink;
use serde::{Deserialize, Serialize};

use crate::api::ImageCache;
use crate::audio::{self, Output};
use crate::likes::{Likes, Rating};
use crate::search::{QueueTrack, QueueView, SearchResult};
use crate::state::{Core, Like, Repeat, Song};

/// What the enabled symphonia features decode: FLAC, WAV, MP3, AAC and ALAC in
/// MP4, Vorbis in Ogg, AIFF.
const EXTENSIONS: [&str; 7] = ["flac", "wav", "mp3", "m4a", "aac", "ogg", "aiff"];
const TICK: Duration = Duration::from_millis(250);
/// Ticks between checks that the system output device is still the one the
/// stream was opened on. CoreAudio keeps a stream on its original device.
const DEVICE_CHECK_TICKS: u64 = 8;
/// Ticks paused before the sink and the output stream are let go. A running
/// stream mixes silence at full rate and holds a sleep assertion in
/// coreaudiod, so a folder left paused must not keep it open. 30 s.
const PARK_AFTER_TICKS: u64 = 120;
/// "Previous" within this many seconds of the start goes back a track; later
/// it restarts the one playing, as every player does.
const RESTART_AFTER: f64 = 3.0;
const SEARCH_MAX: usize = 20;
/// How long a `queue_art` reply waits for covers still being looked up. The
/// rest finish in the background and land on the next request for that row.
const QUEUE_ART_WAIT: Duration = Duration::from_millis(1500);

/// Prefix for local ids. Also keeps them inside `lyrics_cache::safe_name`.
pub const ID_PREFIX: &str = "local-";
/// Concurrent decodes of files' own art; see `Local::decodes`.
const FILE_ART_DECODES: usize = 2;

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
    /// The index holds only `None` or `Embedded`; the sidecar is found per
    /// scan, since it can come and go without the audio file changing.
    #[serde(default, alias = "has_art", deserialize_with = "art_field")]
    pub art: Art,
    /// Bytes on disk, from the listing. Part of the likes fingerprint.
    #[serde(default)]
    pub size: u64,
}

/// What a track's own cover is, so one `cover.jpg` shared by an album is
/// decoded once for the album rather than once per row.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Art {
    #[default]
    None,
    /// A picture in the file's own tags.
    Embedded,
    /// A `cover.jpg` (or the like) beside the file.
    Sidecar(PathBuf),
}

/// An index written before `Art` stored `has_art`, a bool that meant
/// embedded — the sidecar was always layered on at scan time. Read it as
/// such so those files parse and need no tag re-read.
fn art_field<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Art, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Field {
        Flag(bool),
        Kind(Art),
    }
    Ok(match Field::deserialize(d)? {
        Field::Flag(true) => Art::Embedded,
        Field::Flag(false) => Art::None,
        Field::Kind(art) => art,
    })
}

impl Track {
    /// As `Core` shows it: raw credit kept, `artistNames` applied.
    pub fn song(&self, names: &BTreeMap<String, String>) -> Song {
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
            ..Song::default()
        }
    }
}

/// Where a folder opens: the track and position it was left at, its scope and
/// its modes, as saved by `remember`.
#[derive(Debug, PartialEq)]
struct Resume {
    cursor: usize,
    position: f64,
    liked_only: bool,
    repeat: Repeat,
    shuffled: bool,
}

impl Resume {
    /// `liked_only` is the scope asked for, or `None` for whichever the folder
    /// was left in. Liked Songs needs something liked to play; a resume track
    /// outside it gives way to the first liked one after it, from the start.
    fn plan(
        root: &Path,
        saved: Option<&crate::store::SavedFolder>,
        tracks: &[Track],
        likes: &Likes,
        liked_only: Option<bool>,
    ) -> Self {
        let resumed = saved.and_then(|saved| {
            let track = saved.track.as_ref()?;
            let at = tracks
                .iter()
                .position(|t| t.path.strip_prefix(root).is_ok_and(|rel| rel == Path::new(track)))?;
            Some((at, saved.position))
        });
        let (mut cursor, mut position) = resumed.unwrap_or((0, 0.0));

        let liked_only = liked_only.unwrap_or_else(|| saved.is_some_and(|saved| saved.liked_only))
            && likes.liked_count() > 0;
        if liked_only && !likes.is_liked(&tracks[cursor].id) {
            let n = tracks.len();
            if let Some(at) = (1..=n).map(|k| (cursor + k) % n).find(|&i| likes.is_liked(&tracks[i].id)) {
                cursor = at;
                position = 0.0;
            }
        }

        Self {
            cursor,
            position,
            liked_only,
            repeat: saved
                .and_then(|saved| saved.repeat.as_deref().and_then(Repeat::parse))
                .filter(|mode| *mode == Repeat::One)
                .unwrap_or(Repeat::All),
            shuffled: saved.is_some_and(|saved| saved.shuffle),
        }
    }
}

struct Session {
    /// The folder, so an advance can tell "this file is bad" from "the drive
    /// is gone" and stop instead of walking the whole order.
    root: PathBuf,
    tracks: Vec<Track>,
    /// Each track's title, raw artist and album lowercased, for `search`:
    /// made once rather than for every track on every keystroke.
    haystacks: Vec<String>,
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
    /// `All` or `One`: a folder has no repeat-off.
    repeat: Repeat,
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

    /// Drop `tracks[index]` (its file is gone), renumbering `order`. The slot
    /// it held in the sequence becomes the track after it. Returns whether it
    /// was the current track.
    fn remove(&mut self, index: usize) -> bool {
        let was_current = self.order.get(self.cursor) == Some(&index);
        self.tracks.remove(index);
        self.haystacks.remove(index);
        let slot = self.order.iter().position(|&i| i == index);
        self.order.retain(|&i| i != index);
        for i in self.order.iter_mut() {
            if *i > index {
                *i -= 1;
            }
        }
        if slot.is_some_and(|slot| slot < self.cursor) {
            self.cursor -= 1;
        }
        if self.cursor >= self.order.len() {
            self.cursor = 0;
        }
        was_current
    }

    fn relative(&self, track: &Track) -> Option<String> {
        track
            .path
            .strip_prefix(&self.root)
            .ok()
            .map(|rel| rel.to_string_lossy().into_owned())
    }
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
    /// Opened when something actually plays, and closed again once playback
    /// has been paused for `PARK_AFTER_TICKS`: neither a YouTube-only user nor
    /// a folder restored paused should hold CoreAudio open.
    output: Mutex<Option<Output>>,
    playing: Mutex<Option<Playing>>,
    /// Where a paused track stands when no sink holds it: loaded paused, or
    /// paused long enough to be let go. Pressing play reopens it from here.
    parked: Mutex<Option<f64>>,
    /// Consecutive ticks spent paused, counted up to `PARK_AFTER_TICKS`.
    idle_ticks: AtomicU64,
    /// Held across a whole load. The ticker only *tries* it, so it can never
    /// see the gap between one sink and the next and advance a second time.
    loading: Mutex<()>,
    ticks: AtomicU64,
    /// Whether each saved folder is mounted, refreshed by the ticker so the
    /// menus never stat a possibly dead mount on the main thread.
    present: Mutex<HashMap<String, bool>>,
    /// Sized, encoded artwork: `file:<path>@<px>` for a file's own art,
    /// `side:<sidecar path>@<px>` for a folder's, `<artwork key>@<px>` for
    /// an online cover.
    images: Mutex<ImageCache>,
    /// File art decodes in flight at once. A screen of queue rows arrives
    /// together; unbounded, every row read and decoded its own copy of the
    /// same sidecar against a USB drive. Each decode checks the memo once it
    /// has its turn, so the rows after the first find the album's copy.
    decodes: tokio::sync::Semaphore,
}

impl Local {
    pub fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            session: Mutex::new(None),
            output: Mutex::new(None),
            playing: Mutex::new(None),
            parked: Mutex::new(None),
            idle_ticks: AtomicU64::new(0),
            loading: Mutex::new(()),
            ticks: AtomicU64::new(0),
            present: Mutex::new(HashMap::new()),
            images: Mutex::new(ImageCache::new()),
            decodes: tokio::sync::Semaphore::new(FILE_ART_DECODES),
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
        if let Some(playing) = self.playing.lock().expect("playing lock").as_ref() {
            return playing.position();
        }
        self.parked().unwrap_or(0.0)
    }

    fn parked(&self) -> Option<f64> {
        *self.parked.lock().expect("parked lock")
    }


    /// Decode `track` from `start` seconds into a new sink and swap it in,
    /// dropping (never clearing) the old one. Blocking file IO.
    ///
    /// Paused, the file is only opened to prove it decodes and the track is
    /// parked: nothing touches the output, so restoring a folder at launch
    /// does not start a stream that would idle until someone pressed play.
    fn start(&self, track: &Track, play: bool, start: f64) -> Result<(), String> {
        let (source, offset) = audio::decode_from(&track.path, start)?;
        self.idle_ticks.store(0, Ordering::Relaxed);
        if !play {
            *self.playing.lock().expect("playing lock") = None;
            *self.parked.lock().expect("parked lock") = Some(offset);
            return Ok(());
        }
        let mixer = audio::mixer(&self.output, "local-audio").ok_or("No audio output device")?;
        let sink = Sink::connect_new(&mixer);
        sink.set_volume(self.level());
        sink.append(source);
        *self.playing.lock().expect("playing lock") = Some(Playing { sink, offset });
        *self.parked.lock().expect("parked lock") = None;
        Ok(())
    }

    /// An output exists and is still the default device.
    fn output_live(&self) -> bool {
        audio::live(&self.output)
    }

    fn stop(&self) {
        // Dropping, not `clear()`: see the module note.
        *self.playing.lock().expect("playing lock") = None;
        *self.parked.lock().expect("parked lock") = None;
    }

    /// Let a paused track go: remember where it stands, drop its sink and
    /// close the output stream. Called under `loading`.
    fn park(&self) {
        let position = self.position();
        *self.playing.lock().expect("playing lock") = None;
        *self.parked.lock().expect("parked lock") = Some(position);
        *self.output.lock().expect("output lock") = None;
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
        if !local.is_active() && core.status() == crate::state::Status::Connected && core.snapshot().is_playing {
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
        let saved = core
            .store
            .get(|s| s.saved_folders().into_iter().find(|f| f.path == path));
        // Liked Songs only if there is something liked to play.
        if liked_only == Some(true) && likes.liked_count() == 0 {
            crate::macos::inform(
                &core.app,
                "No liked songs in this folder yet".into(),
                "Playing the whole folder. Like a song with the heart to add it to Liked Songs.".into(),
            );
        }
        let Resume {
            cursor,
            position,
            liked_only,
            repeat,
            shuffled,
        } = Resume::plan(&folder, saved.as_ref(), &tracks, &likes, liked_only);

        let volume = core.snapshot().volume;
        let mut session = Session {
            root: folder,
            order: (0..tracks.len()).collect(),
            haystacks: tracks.iter().map(haystack).collect(),
            tracks,
            likes,
            cursor,
            playing: false,
            shuffle: false,
            liked_only,
            repeat,
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
            state.source = crate::state::Source::Local;
            state.status = crate::state::Status::Connected;
            state.status_message = String::new();
            state.shuffle = shuffled;
            state.repeat = Some(repeat);
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
                Some((s.root.clone(), s.relative(track)?, s.liked_only, s.repeat, s.shuffle))
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
                saved.repeat = Some(repeat.as_str().to_string());
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
        let like = Like::from(self.rating_of(&track.id));
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
        let like = Like::from(now);
        core.update(|state| {
            if state.song.as_ref().is_some_and(|song| song.video_id == current.id) {
                state.like = Some(like);
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

    /// The memo key for a track's own art: per file for an embedded
    /// picture, per sidecar for a folder's, so an album shares one resize.
    fn file_key(track: &Track, size: u32) -> Option<String> {
        match &track.art {
            Art::None => None,
            Art::Embedded => Some(format!("file:{}@{size}", track.path.display())),
            Art::Sidecar(sidecar) => Some(format!("side:{}@{size}", sidecar.display())),
        }
    }

    /// The cover if it is already sized in memory. No IO at all.
    fn cover_now(&self, core: &Arc<Core>, track: &Track, size: u32) -> Option<Arc<str>> {
        if let Some(hit) = Self::file_key(track, size).and_then(|key| self.memo(&key)) {
            return Some(hit);
        }
        crate::artwork::keys(&Self::wanted(core, track))
            .iter()
            .find_map(|key| self.memo(&format!("{key}@{size}")))
    }

    /// The file's own art, sized; blocking IO and decode, so callers run it
    /// off the async workers.
    fn file_art(&self, track: &Track, size: u32) -> Option<Arc<str>> {
        let key = Self::file_key(track, size)?;
        if let Some(hit) = self.memo(&key) {
            return Some(hit);
        }
        let art = read_art(track, size)?;
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
        if track.art != Art::None {
            // A turn at decoding first: the memo is checked again inside
            // `file_art`, so rows sharing a sidecar decode it once.
            let _turn = self.decodes.acquire().await.ok()?;
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
        let cached = key.clone();
        let url = tauri::async_runtime::spawn_blocking(move || {
            crate::artwork::resize_cached(&cached, &bytes, size)
                .map(|sized| crate::artwork::data_url(&sized))
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

    /// Read `root`'s ratings again after Sort Songs wrote them, so this
    /// session's next save does not put back what was changed there. Does
    /// nothing unless `root` is the open folder. Blocking IO.
    pub fn reload_likes(&self, core: &Arc<Core>, root: &Path) {
        let _loading = self.loading.lock().expect("loading lock");
        // The read happens off the session lock: the menus take that lock on
        // the main thread, and a drive that is slow or half unplugged would
        // stall the menu bar for as long as the read takes. `loading` keeps
        // the session from being replaced or trimmed meanwhile, so only the
        // root needs checking again.
        let Some(tracks) = self.with(|s| (s.root == root).then(|| s.tracks.clone())).flatten() else {
            return;
        };
        let likes = Likes::load(root, &tracks);
        let Some(current) = self
            .with(|s| {
                if s.root != root {
                    return None;
                }
                s.likes = likes;
                s.rescope();
                Some(s.current().map(|t| (t.id.clone(), s.likes.get(&t.id))))
            })
            .flatten()
        else {
            return;
        };
        if let Some((id, rating)) = current {
            let like = Like::from(rating);
            core.update(|state| {
                if state.song.as_ref().is_some_and(|song| song.video_id == id) {
                    state.like = Some(like);
                }
            });
        }
        core.publish_local_queue();
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

        self.drop_locked(core, &[id.to_string()]);
        Ok(())
    }

    /// Files under `root` were moved to the Trash elsewhere (Sort Songs): take
    /// them out of the session if that folder is the one open.
    pub fn forget_tracks(&self, core: &Arc<Core>, root: &Path, ids: &[String]) {
        let _loading = self.loading.lock().expect("loading lock");
        if self.with(|s| s.root == root) == Some(true) {
            self.drop_locked(core, ids);
        }
    }

    /// Take `ids` out of the session and, if the current track was among them,
    /// carry on with the one after it. Under `loading`.
    fn drop_locked(&self, core: &Arc<Core>, ids: &[String]) {
        let next = self.with(|s| {
            let mut was_current = false;
            for id in ids {
                if let Some(index) = s.tracks.iter().position(|t| &t.id == id) {
                    was_current |= s.remove(index);
                }
            }
            // Liked Songs emptied by this delete: the folder is still there,
            // so fall back to all of it rather than leaving it.
            if s.order.is_empty() && !s.tracks.is_empty() {
                s.liked_only = false;
                s.rescope();
            }
            (was_current, s.cursor, s.playing, s.tracks.is_empty())
        });

        match next {
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
    }

    // -------------------------------------------------------------- commands

    pub fn toggle_play(&self, core: &Arc<Core>) {
        // Under `loading`, so a pause cannot land on a sink a load is about to
        // mark as playing, nor a press restart a track the tick is advancing.
        let _loading = self.loading.lock().expect("loading lock");
        let Some(playing) = self.with(|s| s.playing) else { return };
        // A sink on a dead or no-longer-default device would "resume" in
        // silence, and a parked track has no sink at all: (re)start it on a
        // live output at the same place instead.
        if !playing && (self.parked().is_some() || !self.output_live()) {
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
            s.repeat = if s.repeat == Repeat::All { Repeat::One } else { Repeat::All };
            s.repeat
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

    /// What Local Music opens: the folder last played if its drive is here,
    /// otherwise the most recent saved folder that is.
    pub fn target(&self, store: &crate::store::Store) -> Option<String> {
        store.get(|s| {
            s.local_folder
                .iter()
                .cloned()
                .chain(s.saved_folders().into_iter().map(|f| f.path))
                .find(|path| self.folder_present(path))
        })
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
        // Idle ticks come every `DEVICE_CHECK_TICKS` already (`spawn_ticker`),
        // so each one checks the mounts.
        if !self.is_active() {
            self.refresh_present(core);
            return;
        }
        if self.ticks.load(Ordering::Relaxed) % DEVICE_CHECK_TICKS == 0 {
            self.refresh_present(core);
        }
        // A load is swapping sinks: what we would read is the gap between them.
        let Ok(_loading) = self.loading.try_lock() else { return };
        let Some((playing, repeat, cursor)) = self.with(|s| (s.playing, s.repeat, s.cursor))
        else {
            return;
        };
        let (ended, position) = match self.playing.lock().expect("playing lock").as_ref() {
            Some(p) => (p.sink.empty(), p.position()),
            None => (true, 0.0),
        };

        if playing && ended {
            // Natural end. Folder order wraps under ALL, ONE replays.
            if repeat == Repeat::One && self.load_locked(core, cursor, true) {
                return;
            }
            self.advance_locked(core, cursor, 1, true);
            return;
        }

        // The device went away or the default moved (AirPods connected): carry
        // on from here on whatever is the default now.
        let check = self.ticks.fetch_add(1, Ordering::Relaxed) % DEVICE_CHECK_TICKS == 0;
        let loaded = self.playing.lock().expect("playing lock").is_some();

        // Paused for a while: let the stream go rather than mix silence.
        if playing {
            self.idle_ticks.store(0, Ordering::Relaxed);
        } else if self.idle_ticks.fetch_add(1, Ordering::Relaxed) + 1 == PARK_AFTER_TICKS {
            self.park();
            return;
        }

        let stale = check && loaded && playing && !self.output_live();
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
    ///
    /// With no folder open there is nothing to advance, only mounts to watch
    /// for the menus, so the thread wakes every two seconds rather than four
    /// times a second — most people using only YouTube Music never open one.
    pub fn spawn_ticker(core: Arc<Core>) {
        let _ = std::thread::Builder::new()
            .name("local-tick".into())
            .spawn(move || loop {
                let idle = !core.local().is_active();
                std::thread::sleep(if idle { TICK * DEVICE_CHECK_TICKS as u32 } else { TICK });
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
                        credit: track.artist.clone(),
                        artists: Arc::from([]),
                        duration: clock(track.duration),
                        art: Some(Arc::from(track.id.as_str())),
                        rating: s.likes.get(&track.id),
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
                    .zip(&s.haystacks)
                    .filter(|(t, haystack)| {
                        if haystack.contains(&needle) {
                            return true;
                        }
                        // The renamed artist is the one field not in the
                        // haystack, since renames change without a rescan.
                        let shown = crate::store::map_artist(names, &t.artist);
                        shown != t.artist && shown.to_lowercase().contains(&needle)
                    })
                    .take(SEARCH_MAX)
                    .map(|(t, _)| t.clone())
                    .collect()
            })
            .unwrap_or_default();
        hits.into_iter()
            .map(|t| {
                let artist = crate::store::map_artist(names, &t.artist);
                let size = crate::api::THUMB_PX;
                let own = self.file_art(&t, size);
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
                    let sized = crate::artwork::resize_cached(&key, &bytes, size)?;
                    let url = crate::artwork::data_url(&sized);
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

/// What `search` matches a track on, lowercased once. The fields are joined
/// by a character no query contains, so a match never spans two of them.
fn haystack(track: &Track) -> String {
    format!("{}\u{0}{}\u{0}{}", track.title, track.artist, track.album).to_lowercase()
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
    let hash = crate::fsutil::stable_hash(&path.to_string_lossy());
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
    let sidecars: HashMap<PathBuf, PathBuf> = files
        .iter()
        .filter_map(|file| file.path.parent())
        .collect::<HashSet<_>>()
        .into_iter()
        .filter_map(|dir| Some((dir.to_path_buf(), sidecar_in(dir)?)))
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
            if track.art == Art::None {
                if let Some(sidecar) = track.path.parent().and_then(|dir| sidecars.get(dir)) {
                    track.art = Art::Sidecar(sidecar.clone());
                }
            }
            track.size = entry.size;
            track
        })
        .collect();

    if changed {
        let entries: Vec<&Indexed> = files.iter().filter_map(|file| index.get(&file.path)).collect();
        if let Ok(json) = serde_json::to_vec(&entries) {
            // The sorter and the player can scan one folder at once; unique
            // temp names keep their writes from tearing each other.
            let _ = crate::fsutil::write_atomic(&index_file, &json);
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
        art: Art::None,
        size: 0,
    }
}

fn read_track(root: &Path, path: &Path) -> Track {
    let mut track = untagged_track(root, path);
    if let Ok(file) = lofty::read_from_path(path) {
        track.duration = file.properties().duration().as_secs_f64();
        if file.tags().iter().any(|tag| !tag.pictures().is_empty()) {
            track.art = Art::Embedded;
        }
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

/// The track's own art as the scan found it, downscaled to `size` so a 3 MB
/// scan never rides an event.
fn read_art(track: &Track, size: u32) -> Option<Arc<str>> {
    let bytes = art_bytes(track)?;
    Some(crate::artwork::data_url(&crate::artwork::resize(&bytes, size)?))
}

/// The file's own picture or its folder's sidecar, as stored. Blocking IO.
pub fn art_bytes(track: &Track) -> Option<Vec<u8>> {
    match &track.art {
        Art::None => None,
        Art::Embedded => embedded_art(&track.path),
        Art::Sidecar(sidecar) => std::fs::read(sidecar).ok(),
    }
}

fn embedded_art(path: &Path) -> Option<Vec<u8>> {
    let file = lofty::read_from_path(path).ok()?;
    file.tags()
        .iter()
        .flat_map(|tag| tag.pictures())
        .next()
        .map(|picture| picture.data().to_vec())
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
                art: Art::None,
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
            haystacks: tracks.iter().map(haystack).collect(),
            tracks,
            likes,
            cursor: 0,
            playing: false,
            shuffle: false,
            liked_only: false,
            repeat: Repeat::All,
            volume: 100.0,
            muted: false,
            started: false,
            shown_second: 0,
        };
        (session, root)
    }

    #[test]
    fn resume_picks_up_the_saved_track_and_modes() {
        let (s, root) = session_with(&[], "resume");
        let saved = crate::store::SavedFolder {
            path: root.to_string_lossy().into_owned(),
            track: Some("3.flac".into()),
            position: 42.0,
            repeat: Some("ONE".into()),
            shuffle: true,
            ..Default::default()
        };
        let plan = Resume::plan(&root, Some(&saved), &s.tracks, &s.likes, None);
        assert_eq!(
            plan,
            Resume { cursor: 3, position: 42.0, liked_only: false, repeat: Repeat::One, shuffled: true }
        );
        // Nothing saved: the top, whole folder, repeat-all.
        let fresh = Resume::plan(&root, None, &s.tracks, &s.likes, None);
        assert_eq!(fresh.cursor, 0);
        assert_eq!(fresh.repeat, Repeat::All);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resume_under_liked_songs_moves_to_a_liked_track() {
        let (s, root) = session_with(&[1, 4], "resume-liked");
        let saved = crate::store::SavedFolder {
            path: root.to_string_lossy().into_owned(),
            track: Some("2.flac".into()),
            position: 30.0,
            liked_only: true,
            ..Default::default()
        };
        let plan = Resume::plan(&root, Some(&saved), &s.tracks, &s.likes, None);
        assert_eq!((plan.cursor, plan.position, plan.liked_only), (4, 0.0, true));
        // Asked for the whole folder: stays put.
        let whole = Resume::plan(&root, Some(&saved), &s.tracks, &s.likes, Some(false));
        assert_eq!((whole.cursor, whole.position, whole.liked_only), (2, 30.0, false));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn removing_tracks_keeps_the_cursor_on_the_same_song() {
        let (mut s, root) = session_with(&[], "remove");
        s.cursor = 3;
        assert!(!s.remove(1));
        assert_eq!(s.tracks[s.order[s.cursor]].id, "local-3");
        // Removing the current track: its slot now holds the one after it.
        assert!(s.remove(s.order[s.cursor]));
        assert_eq!(s.tracks[s.order[s.cursor]].id, "local-4");
        let _ = std::fs::remove_dir_all(root);
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

    #[test]
    fn an_old_index_with_has_art_still_parses() {
        let entry = |art: &str| {
            format!(
                r#"{{"size":1,"modified":2,"track":{{"id":"local-a","path":"/m/a.flac","title":"A","artist":"B","album":"C","duration":1.0,{art}}}}}"#
            )
        };
        let old: Indexed = serde_json::from_str(&entry(r#""has_art":true"#)).unwrap();
        assert_eq!(old.track.art, Art::Embedded);
        let old: Indexed = serde_json::from_str(&entry(r#""has_art":false"#)).unwrap();
        assert_eq!(old.track.art, Art::None);
        let new: Indexed = serde_json::from_str(&entry(r#""art":"embedded""#)).unwrap();
        assert_eq!(new.track.art, Art::Embedded);
        let json = serde_json::to_string(&new).unwrap();
        assert!(json.contains(r#""art":"embedded""#), "{json}");
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
