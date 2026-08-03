//! Realtime link to the API server's `/api/v1/ws` endpoint.
//!
//! The connection state is the app's connection state: once a token is cached,
//! `ensure_token` returns without touching the network, so a dead API server
//! produces **no error there at all**. The socket's close is the only signal —
//! which is why the close handler reports `offline` regardless of whether a
//! connection was ever established. Getting this wrong left the widget stuck on
//! "Connecting…" forever.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tauri::{AppHandle, Manager};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;

use crate::api::ErrorCode;
use crate::state::Core;

const RECONNECT_MIN_MS: u64 = 1000;
const RECONNECT_MAX_MS: u64 = 15000;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Realtime {
    /// Pulsed by the tray's Reconnect, the setup screen's Retry, and by any
    /// command that discovers the server has gone away.
    wake: Notify,
    /// Set after a denied authorisation so we do not re-open the dialog on a
    /// loop; only a manual retry clears it.
    parked: AtomicBool,
    attempt: AtomicU32,
}

impl Realtime {
    pub fn new() -> Self {
        Self {
            wake: Notify::new(),
            parked: AtomicBool::new(false),
            attempt: AtomicU32::new(0),
        }
    }

    /// Manual retry: drop any backoff, unpark, and reconnect now.
    pub fn request_retry(&self) {
        self.attempt.store(0, Ordering::SeqCst);
        self.parked.store(false, Ordering::SeqCst);
        self.wake.notify_waiters();
    }

    fn backoff(&self) -> Duration {
        let attempt = self.attempt.fetch_add(1, Ordering::SeqCst);
        let delay = RECONNECT_MIN_MS
            .saturating_mul(1u64 << attempt.min(8))
            .min(RECONNECT_MAX_MS);
        Duration::from_millis(delay)
    }

    /// Sleep, unless a manual retry arrives first.
    async fn wait(&self, delay: Duration) {
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = self.wake.notified() => {}
        }
    }
}

/// Ask the realtime task to reconnect now. A no-op before setup finishes.
pub fn retry(app: &AppHandle) {
    if let Some(realtime) = app.try_state::<Arc<Realtime>>() {
        realtime.request_retry();
    }
}

pub fn spawn(core: Arc<Core>, realtime: Arc<Realtime>) {
    tauri::async_runtime::spawn(async move {
        loop {
            // Parked after a denied authorisation: wait for a manual retry.
            if realtime.parked.load(Ordering::SeqCst) {
                realtime.wake.notified().await;
                continue;
            }

            match connect_once(&core, &realtime).await {
                Outcome::Reconnect => {
                    let delay = realtime.backoff();
                    realtime.wait(delay).await;
                }
                Outcome::Parked => {}
            }
        }
    });
}

enum Outcome {
    Reconnect,
    Parked,
}

async fn connect_once(core: &Arc<Core>, realtime: &Arc<Realtime>) -> Outcome {
    // Only advertise "connecting" on the first attempt or after a manual retry.
    // Once we know we are offline, staying offline while retrying in the
    // background beats flickering between the two on every backoff tick.
    if realtime.attempt.load(Ordering::SeqCst) == 0 && core.status() != "connected" {
        core.set_status("connecting", "");
    }

    let token = match core.api.ensure_token().await {
        Ok(token) => token,
        Err(err) => {
            if err.code == ErrorCode::Denied {
                realtime.parked.store(true, Ordering::SeqCst);
                core.set_status("denied", &err.message);
                return Outcome::Parked;
            }
            let status = if err.code == ErrorCode::Offline {
                "offline"
            } else {
                "unauthorized"
            };
            core.set_status(status, &err.message);
            return Outcome::Reconnect;
        }
    };

    let url = format!(
        "{}{}/ws?token={}",
        core.store.ws_base(),
        crate::api::API,
        urlencode(&token)
    );

    let socket = match tokio::time::timeout(HANDSHAKE_TIMEOUT, tokio_tungstenite::connect_async(url))
        .await
    {
        Ok(Ok((socket, _response))) => socket,
        Ok(Err(err)) => {
            core.set_status("offline", &format!("Could not reach the API server: {err}"));
            return Outcome::Reconnect;
        }
        Err(_) => {
            core.set_status("offline", "Could not reach the API server");
            return Outcome::Reconnect;
        }
    };

    realtime.attempt.store(0, Ordering::SeqCst);
    core.set_status("connected", "");

    let (_sink, mut stream) = socket.split();
    let mut policy_rejection = false;

    loop {
        let next = tokio::select! {
            message = stream.next() => message,
            // A manual retry tears the socket down rather than waiting for it
            // to notice on its own.
            _ = realtime.wake.notified() => None,
        };

        match next {
            Some(Ok(Message::Text(raw))) => {
                if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&raw) {
                    core.handle_message(&payload).await;
                }
            }
            Some(Ok(Message::Binary(raw))) => {
                if let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&raw) {
                    core.handle_message(&payload).await;
                }
            }
            Some(Ok(Message::Close(frame))) => {
                policy_rejection = frame.map(|frame| u16::from(frame.code) == 1008).unwrap_or(false);
                break;
            }
            Some(Ok(_)) => {}
            // An error is always followed by the stream ending; both land here.
            Some(Err(_)) | None => break,
        }
    }

    if policy_rejection {
        // The server rejected our token — drop it so the next attempt re-authorises.
        core.api.clear_token();
        core.set_status("unauthorized", "The API server rejected the saved token");
    } else {
        core.set_status(
            "offline",
            if core.status() == "connected" {
                "Connection closed"
            } else {
                "Could not reach the API server"
            },
        );
    }

    Outcome::Reconnect
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
