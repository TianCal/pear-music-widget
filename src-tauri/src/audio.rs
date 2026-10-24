//! Output streams and decoders shared by the folder player (`local.rs`) and
//! the sorter's clip player. Both follow the same rules: every start is a
//! fresh decoder appended to a fresh `Sink`, and the old sink is dropped —
//! never `Sink::clear` or `Sink::try_seek`, which wait on an audio callback a
//! vanished device never runs again.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use rodio::cpal::traits::HostTrait;
use rodio::{DeviceTrait, Source};

/// An open output stream. The `OutputStream` itself lives on a thread of its
/// own (it is not `Send`) until `_keep` is dropped; we hold its mixer.
pub struct Output {
    mixer: rodio::mixer::Mixer,
    device: Option<String>,
    /// Set from the stream's error callback — the device went away.
    dead: Arc<AtomicBool>,
    _keep: mpsc::Sender<()>,
}

impl Output {
    fn open(thread: &str) -> Option<Self> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (keep, hold) = mpsc::channel::<()>();
        let dead = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&dead);
        std::thread::Builder::new()
            .name(thread.into())
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

/// A mixer on a live stream for the current default device, reopening the
/// stream (on a thread named `thread`) when the old device has gone or is no
/// longer the default.
pub fn mixer(output: &Mutex<Option<Output>>, thread: &str) -> Option<rodio::mixer::Mixer> {
    let mut output = output.lock().expect("output lock");
    if !output.as_ref().is_some_and(Output::current) {
        *output = Output::open(thread);
    }
    output.as_ref().map(|output| output.mixer.clone())
}

/// An output exists and is still the default device.
pub fn live(output: &Mutex<Option<Output>>) -> bool {
    output.lock().expect("output lock").as_ref().is_some_and(Output::current)
}

/// A decoder on `path`, seeked to `at` seconds when it can be, and where in
/// the file it actually starts. Blocking file IO.
pub fn decode_from(path: &Path, at: f64) -> Result<(impl Source + Send + 'static, f64), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut source = rodio::Decoder::try_from(file).map_err(|e| e.to_string())?;
    let mut offset = 0.0;
    if at > 0.0 && source.try_seek(Duration::from_secs_f64(at)).is_ok() {
        offset = at;
    }
    Ok((source, offset))
}
