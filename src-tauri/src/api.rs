//! REST client for the api-server plugin's `/api/v1` surface, plus token
//! handling and cover fetching.
//!
//! All networking lives on this side of the IPC boundary. Cover art is fetched
//! here and handed to the renderer as a `data:` URL, which keeps the renderer's
//! CSP closed to the network *and* lets `<canvas>` read the pixels for colour
//! extraction without cross-origin tainting.
//!
//! If the plugin bumps to `/api/v2`: change `API` here and the socket path in
//! `ws.rs`. Verified against YouTube Music 3.12.0.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde_json::Value;

use crate::store::Store;

pub const API: &str = "/api/v1";

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(4);
/// The search payload is ~270KB and takes a second or two; 4s is not enough.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
const COVER_TIMEOUT: Duration = Duration::from_secs(8);

const IMAGE_CACHE_MAX: usize = 40;

/// Error codes surfaced to the renderer so it can render the right setup hint.
///   Offline      - nothing listening; YouTube Music closed or API Server plugin disabled
///   Unauthorized - no token yet, or the stored token was rejected
///   Denied       - the user pressed "Deny" on the in-app authorisation dialog
///   Error        - anything else
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    Offline,
    Unauthorized,
    Denied,
    Error,
}

#[derive(Clone, Debug)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
}

impl ApiError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub type Result<T> = std::result::Result<T, ApiError>;

/// A connection error and a timeout both mean "nothing is answering"; anything
/// else is a real fault worth reporting verbatim.
fn classify(err: &reqwest::Error) -> ApiError {
    if err.is_connect() {
        return ApiError::new(ErrorCode::Offline, "API server is not reachable");
    }
    if err.is_timeout() {
        return ApiError::new(ErrorCode::Offline, "API server timed out");
    }
    ApiError::new(ErrorCode::Error, err.to_string())
}

/// Insertion-ordered cache of cover URL → data URL. Cover URLs are stable per
/// video, so a small map saves refetching artwork on every track repeat.
struct ImageCache {
    order: Vec<String>,
    entries: HashMap<String, String>,
}

impl ImageCache {
    fn new() -> Self {
        Self {
            order: Vec::new(),
            entries: HashMap::new(),
        }
    }

    fn get(&self, key: &str) -> Option<String> {
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: String, value: String) {
        if self.entries.insert(key.clone(), value).is_none() {
            self.order.push(key);
        }
        while self.order.len() > IMAGE_CACHE_MAX {
            let oldest = self.order.remove(0);
            self.entries.remove(&oldest);
        }
    }
}

pub struct Api {
    store: Arc<Store>,
    http: reqwest::Client,
    images: Mutex<ImageCache>,
}

impl Api {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            // No pooling surprises: this talks to one localhost server and to
            // LRCLib, both of which are happy with keep-alive.
            http: reqwest::Client::builder()
                .user_agent(crate::lyrics::USER_AGENT)
                .build()
                .expect("build http client"),
            images: Mutex::new(ImageCache::new()),
        }
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.http
    }

    // ------------------------------------------------------------------ auth

    /// Ask the API server for a JWT. Pops a dialog inside YouTube Music the
    /// first time, so this deliberately has no timeout: it blocks until the
    /// user answers.
    pub async fn authorize(&self) -> Result<String> {
        let url = format!(
            "{}/auth/{}",
            self.store.http_base(),
            urlencode(&self.store.get(|s| s.client_id.clone()))
        );

        let res = self
            .http
            .post(url)
            .send()
            .await
            .map_err(|err| classify(&err))?;

        if res.status() == 403 {
            return Err(ApiError::new(
                ErrorCode::Denied,
                "Authorisation was denied in YouTube Music",
            ));
        }
        if !res.status().is_success() {
            return Err(ApiError::new(
                ErrorCode::Error,
                format!("Auth failed with HTTP {}", res.status().as_u16()),
            ));
        }

        let body: Value = res
            .json()
            .await
            .map_err(|_| ApiError::new(ErrorCode::Error, "Auth response was not JSON"))?;
        let token = body
            .get("accessToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| ApiError::new(ErrorCode::Error, "Auth response contained no token"))?
            .to_string();

        self.store.update(|s| s.token = Some(token.clone()));
        Ok(token)
    }

    /// Returns a usable token, requesting one if we do not have it cached.
    ///
    /// Note the consequence: once a token is cached this never touches the
    /// network, so a dead API server produces no error *here at all*. The
    /// websocket's close is the only signal — see `ws.rs`.
    pub async fn ensure_token(&self) -> Result<String> {
        match self.store.get(|s| s.token.clone()) {
            Some(token) => Ok(token),
            None => self.authorize().await,
        }
    }

    pub fn clear_token(&self) {
        self.store.update(|s| s.token = None);
    }

    // --------------------------------------------------------------- request

    /// Authenticated request against `/api/v1`. Retries once with a fresh token
    /// on 401/403, which covers the API server's secret being regenerated.
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<Option<Value>> {
        let mut retried = false;

        loop {
            let token = self.ensure_token().await?;
            let url = format!("{}{}{}", self.store.http_base(), API, path);

            let mut req = self
                .http
                .request(method.clone(), url)
                .bearer_auth(&token)
                .timeout(timeout);
            if let Some(payload) = &body {
                req = req.json(payload);
            }

            let res = req.send().await.map_err(|err| classify(&err))?;
            let status = res.status().as_u16();

            if status == 401 || status == 403 {
                self.clear_token();
                if !retried {
                    retried = true;
                    continue;
                }
                return Err(ApiError::new(
                    ErrorCode::Unauthorized,
                    "Token rejected by the API server",
                ));
            }
            if status == 204 {
                return Ok(None);
            }
            if !res.status().is_success() {
                return Err(ApiError::new(
                    ErrorCode::Error,
                    format!("HTTP {status} on {path}"),
                ));
            }

            let text = res.text().await.map_err(|err| classify(&err))?;
            if text.is_empty() {
                return Ok(None);
            }
            return Ok(serde_json::from_str(&text).ok());
        }
    }

    async fn get(&self, path: &str) -> Result<Option<Value>> {
        self.request(reqwest::Method::GET, path, None, DEFAULT_TIMEOUT)
            .await
    }

    async fn post(&self, path: &str, body: Option<Value>) -> Result<Option<Value>> {
        self.request(reqwest::Method::POST, path, body, DEFAULT_TIMEOUT)
            .await
    }

    // ------------------------------------------------------------- cover art

    pub async fn fetch_cover(&self, src: Option<&str>) -> Option<String> {
        let src = src.filter(|s| !s.is_empty())?;
        if let Some(hit) = self.images.lock().expect("image cache").get(src) {
            return Some(hit);
        }

        let res = self
            .http
            .get(src)
            .timeout(COVER_TIMEOUT)
            .send()
            .await
            .ok()?;
        if !res.status().is_success() {
            return None;
        }

        let mime = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("image/jpeg")
            .to_string();
        let bytes = res.bytes().await.ok()?;

        let data_url = format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        self.images
            .lock()
            .expect("image cache")
            .insert(src.to_string(), data_url.clone());
        Some(data_url)
    }

    // --------------------------------------------------------------- actions

    pub async fn toggle_play(&self) -> Result<()> {
        self.post("/toggle-play", None).await.map(|_| ())
    }
    pub async fn next(&self) -> Result<()> {
        self.post("/next", None).await.map(|_| ())
    }
    pub async fn previous(&self) -> Result<()> {
        self.post("/previous", None).await.map(|_| ())
    }
    pub async fn seek_to(&self, seconds: i64) -> Result<()> {
        self.post("/seek-to", Some(serde_json::json!({ "seconds": seconds })))
            .await
            .map(|_| ())
    }
    pub async fn set_volume(&self, volume: i64) -> Result<()> {
        self.post("/volume", Some(serde_json::json!({ "volume": volume })))
            .await
            .map(|_| ())
    }
    pub async fn toggle_mute(&self) -> Result<()> {
        self.post("/toggle-mute", None).await.map(|_| ())
    }
    pub async fn like(&self) -> Result<()> {
        self.post("/like", None).await.map(|_| ())
    }
    pub async fn dislike(&self) -> Result<()> {
        self.post("/dislike", None).await.map(|_| ())
    }
    pub async fn shuffle(&self) -> Result<()> {
        self.post("/shuffle", None).await.map(|_| ())
    }

    pub async fn search(&self, query: &str) -> Result<Value> {
        self.request(
            reqwest::Method::POST,
            "/search",
            Some(serde_json::json!({ "query": query })),
            SEARCH_TIMEOUT,
        )
        .await
        .map(|value| value.unwrap_or(Value::Null))
    }

    pub async fn add_to_queue(&self, video_id: &str) -> Result<()> {
        self.post(
            "/queue",
            Some(serde_json::json!({
                "videoId": video_id,
                "insertPosition": "INSERT_AFTER_CURRENT_VIDEO",
            })),
        )
        .await
        .map(|_| ())
    }

    pub async fn set_queue_index(&self, index: usize) -> Result<()> {
        self.request(
            reqwest::Method::PATCH,
            "/queue",
            Some(serde_json::json!({ "index": index })),
            DEFAULT_TIMEOUT,
        )
        .await
        .map(|_| ())
    }

    // --------------------------------------------------------------- queries

    // `/repeat-mode` is intentionally not used: it answers null unconditionally
    // on the shipped api-server plugin, so repeat state cannot be displayed
    // truthfully — hence no repeat button.
    pub async fn song(&self) -> Result<Option<Value>> {
        self.get("/song").await
    }
    pub async fn like_state(&self) -> Result<Option<Value>> {
        self.get("/like-state").await
    }
    pub async fn shuffle_state(&self) -> Result<Option<Value>> {
        self.get("/shuffle").await
    }
    pub async fn volume_state(&self) -> Result<Option<Value>> {
        self.get("/volume").await
    }
    pub async fn queue(&self) -> Result<Option<Value>> {
        self.get("/queue").await
    }
}

/// Minimal percent-encoding for the one path segment we interpolate.
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
