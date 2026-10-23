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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::time::Duration;

use rodio::cpal::traits::HostTrait;
use rodio::{DeviceTrait, Sink, Source};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

const FADE_IN: Duration = Duration::from_millis(400);
const TICK: Duration = Duration::from_millis(50);

struct Output {
    mixer: rodio::mixer::Mixer,
    device: Option<String>,
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
            .name("sorter-audio".into())
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

    fn current(&self) -> bool {
        !self.dead.load(Ordering::SeqCst)
            && self.device
                == rodio::cpal::default_host()
                    .default_output_device()
                    .and_then(|device| device.name().ok())
    }
}

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

    fn mixer(&self) -> Option<rodio::mixer::Mixer> {
        let mut output = self.output.lock().expect("output lock");
        if !output.as_ref().is_some_and(Output::current) {
            *output = Output::open();
        }
        output.as_ref().map(|output| output.mixer.clone())
    }

    /// A new sink on `path` from `at` seconds. Blocking file IO.
    fn sink_at(&self, path: &Path, at: f64) -> Result<(Sink, f64), String> {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut source = rodio::Decoder::try_from(file).map_err(|e| e.to_string())?;
        let mut offset = 0.0;
        if at > 0.0 && source.try_seek(Duration::from_secs_f64(at)).is_ok() {
            offset = at;
        }
        let mixer = self.mixer().ok_or("No audio output device")?;
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
        let (path, chorus) = {
            let guard = self.now.lock().expect("now lock");
            match guard.as_ref() {
                Some(now) if now.id == id => (now.path.clone(), now.chorus),
                _ => return Ok(()),
            }
        };
        let (sink, offset) = self.sink_at(&path, at)?;
        *self.now.lock().expect("now lock") = Some(Playing {
            id: id.to_string(),
            path,
            sink,
            offset,
            chorus,
            paused: false,
            ended: false,
            touched: true,
        });
        Ok(())
    }

    pub fn toggle(&self) -> Result<(), String> {
        let restart = {
            let mut guard = self.now.lock().expect("now lock");
            let Some(now) = guard.as_mut() else {
                return Ok(());
            };
            if now.ended {
                // The song played out: play goes back to its chorus.
                Some((now.id.clone(), now.path.clone(), now.chorus))
            } else {
                now.paused = !now.paused;
                now.touched = true;
                if now.paused {
                    now.sink.pause();
                } else {
                    now.sink.play();
                }
                None
            }
        };
        match restart {
            Some((id, path, start)) => self.play(&id, &path, start),
            None => Ok(()),
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
        loop {
            std::thread::sleep(TICK);
            beat = beat.wrapping_add(1);
            let Some(this) = player.upgrade() else { return };
            let progress = {
                let this = &this;
                let mut guard = this.now.lock().expect("now lock");
                let Some(now) = guard.as_mut() else { continue };
                if !now.paused && !now.ended && now.sink.empty() {
                    now.ended = true;
                }
                (beat % 4 == 0).then(|| now.progress())
            };
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
