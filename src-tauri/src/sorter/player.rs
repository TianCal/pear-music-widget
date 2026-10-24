//! Clip playback. Same rules as the widget's local player: every start is a
//! fresh decoder seeked before it is appended to a fresh `Sink`, and the old
//! sink is dropped — never `Sink::clear` or `Sink::try_seek`, which wait on an
//! audio callback a vanished device never runs again. The output stream lives
//! on its own thread and is reopened when it errors or the default device
//! changes.
//!
//! A card starts at its chorus (faded in) and plays on from there to the end
//! of the song, where it stops: nothing loops and nothing advances on its own.
//!
//! The ticker holds only a `Weak`, so dropping the player when the sorter
//! window closes ends the ticker and, with it, the output stream's thread.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rodio::{Sink, Source};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::audio::{self, Output};

const FADE_IN: Duration = Duration::from_millis(400);
const TICK: Duration = Duration::from_millis(50);
/// Ticks between checks that the output device is still there and still the
/// default; a dead sink never empties, so the card would never end.
const DEVICE_CHECK_TICKS: u32 = 8;
/// After a restart on a new device fails (none to open), wait this long
/// before trying again rather than every check.
const RESTART_BACKOFF: Duration = Duration::from_secs(5);
const AUDIO_THREAD: &str = "sorter-audio";

struct Playing {
    id: String,
    path: PathBuf,
    sink: Sink,
    /// Where in the file this sink started.
    offset: f64,
    /// The chorus, where play starts again once the song has ended.
    chorus: f64,
    paused: bool,
    ended: bool,
    /// The user moved the playhead (or paused): a late chorus must not move it.
    touched: bool,
}

impl Playing {
    fn position(&self) -> f64 {
        self.offset + self.sink.get_pos().as_secs_f64()
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub id: String,
    pub position: f64,
    pub playing: bool,
}

pub struct Player {
    output: Mutex<Option<Output>>,
    now: Mutex<Option<Playing>>,
}

impl Player {
    pub fn new(app: AppHandle) -> Arc<Self> {
        let player = Arc::new(Self {
            output: Mutex::new(None),
            now: Mutex::new(None),
        });
        let ticker = Arc::downgrade(&player);
        std::thread::Builder::new()
            .name("sorter-tick".into())
            .spawn(move || Self::run(ticker, app))
            .expect("spawn ticker");
        player
    }

    /// A new sink on `path` from `at` seconds. Blocking file IO.
    fn sink_at(&self, path: &Path, at: f64) -> Result<(Sink, f64), String> {
        // The output first: with no device there is no point reading the file.
        let mixer = audio::mixer(&self.output, AUDIO_THREAD).ok_or("No audio output device")?;
        let (source, offset) = audio::decode_from(path, at)?;
        let sink = Sink::connect_new(&mixer);
        sink.append(source.fade_in(FADE_IN));
        Ok((sink, offset))
    }

    /// Play `path` from its chorus at `chorus` on. Blocking file IO.
    pub fn play(&self, id: &str, path: &Path, chorus: f64) -> Result<(), String> {
        let (sink, offset) = self.sink_at(path, chorus)?;
        *self.now.lock().expect("now lock") = Some(Playing {
            id: id.to_string(),
            path: path.to_path_buf(),
            sink,
            offset,
            chorus: offset,
            paused: false,
            ended: false,
            touched: false,
        });
        Ok(())
    }

    /// Play the current song from `at`. Blocking file IO.
    pub fn seek(&self, id: &str, at: f64) -> Result<(), String> {
        self.restart(id, at)
    }

    /// Start `id` again from `at` on a fresh sink, as long as it is still the
    /// card playing. The file is opened without the lock held, so the card may
    /// change meanwhile; then this sink is dropped rather than replacing it.
    fn restart(&self, id: &str, at: f64) -> Result<(), String> {
        let path = {
            let guard = self.now.lock().expect("now lock");
            match guard.as_ref() {
                Some(now) if now.id == id => now.path.clone(),
                _ => return Ok(()),
            }
        };
        let (sink, offset) = self.sink_at(&path, at)?;
        let mut guard = self.now.lock().expect("now lock");
        if let Some(now) = guard.as_mut().filter(|now| now.id == id) {
            now.sink = sink;
            now.offset = offset;
            now.paused = false;
            now.ended = false;
            now.touched = true;
        }
        Ok(())
    }

    pub fn toggle(&self) -> Result<(), String> {
        enum Then {
            Nothing,
            Replay(String, PathBuf, f64),
            Resume(String, f64),
        }
        let then = {
            let mut guard = self.now.lock().expect("now lock");
            let Some(now) = guard.as_mut() else {
                return Ok(());
            };
            if now.ended {
                // The song played out: play goes back to its chorus.
                Then::Replay(now.id.clone(), now.path.clone(), now.chorus)
            } else if now.paused && !audio::live(&self.output) {
                // The device went away while paused; `play` on its sink would
                // do nothing. Carry on from here on the current default.
                Then::Resume(now.id.clone(), now.position())
            } else {
                now.paused = !now.paused;
                now.touched = true;
                if now.paused {
                    now.sink.pause();
                } else {
                    now.sink.play();
                }
                Then::Nothing
            }
        };
        match then {
            Then::Replay(id, path, start) => self.play(&id, &path, start),
            Then::Resume(id, at) => self.restart(&id, at),
            Then::Nothing => Ok(()),
        }
    }

    pub fn stop(&self) {
        *self.now.lock().expect("now lock") = None;
    }

    /// `id` is playing from where it was started, untouched by the user.
    pub fn untouched(&self, id: &str) -> bool {
        self.now
            .lock()
            .expect("now lock")
            .as_ref()
            .is_some_and(|now| now.id == id && !now.touched)
    }

    fn run(player: Weak<Self>, app: AppHandle) {
        let mut beat = 0u32;
        let mut failed_at: Option<std::time::Instant> = None;
        loop {
            std::thread::sleep(TICK);
            beat = beat.wrapping_add(1);
            let Some(this) = player.upgrade() else { return };
            let (progress, stale) = {
                let mut guard = this.now.lock().expect("now lock");
                let Some(now) = guard.as_mut() else { continue };
                if !now.paused && !now.ended && now.sink.empty() {
                    now.ended = true;
                }
                let running = !now.paused && !now.ended;
                let due = failed_at.is_none_or(|at| at.elapsed() >= RESTART_BACKOFF);
                let stale = (running && due && beat % DEVICE_CHECK_TICKS == 0 && !audio::live(&this.output))
                    .then(|| (now.id.clone(), now.position()));
                ((beat % 4 == 0).then(|| now.progress()), stale)
            };
            // The device went away or the default moved: carry on from here.
            if let Some((id, at)) = stale {
                failed_at = this.restart(&id, at).is_err().then(std::time::Instant::now);
            }
            if let Some(progress) = progress {
                let _ = app.emit_to(super::LABEL, "sorter-progress", progress);
            }
        }
    }
}

impl Playing {
    fn progress(&self) -> Progress {
        Progress {
            id: self.id.clone(),
            position: self.position(),
            playing: !self.paused && !self.ended,
        }
    }
}
