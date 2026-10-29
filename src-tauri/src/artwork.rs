//! Album covers for local files that carry none.
//!
//! The second module allowed to contact a non-localhost host (after
//! `lyrics.rs`). Lookup order:
//!
//! 1. iTunes Search, album entity, Taiwan then US storefront.
//!    The Taiwan store credits Mandopop under its Chinese names, which is what
//!    local tags and `artistNames` produce; the US store romanises them.
//! 2. Deezer album search — romanised for Chinese releases, so in practice the
//!    fallback for Western music.
//! 3. iTunes Search, song entity, for files whose "album" is a folder of
//!    singles rather than a release.
//! 4. YouTube Music search through the local API server, square song art only;
//!    16:9 video frames are not covers.
//!
//! A result is used only when both the artist and the album (or title) match
//! after folding to Simplified Chinese and dropping punctuation — a wrong
//! cover is worse than the placeholder.
//!
//! Results are cached under `~/Library/Caches/pear-music-widget/artwork`:
//! `<key>.jpg` for a hit, `<key>.miss` for a miss that expires after a week
//! (a day if YouTube Music could not be asked). Albums and tracks are keyed
//! separately, so one hit covers a whole album. **A failed request is never
//! cached as a miss** — a timeout or iTunes' rate limit says nothing about
//! whether a cover exists.
//!
//! iTunes requests are spaced to its ~20-a-minute budget and one album is
//! looked up by one caller at a time. The playing track is `Priority::Now`;
//! queue rows and prefetch are `Background`, run one at a time, and hold off
//! their next request while a `Now` lookup is running.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use regex::Regex;
use serde_json::Value;

use crate::state::Core;

/// Stored size; requests are downscaled from it. Twice `COVER_PX` keeps the
/// card sharp on a Retina display.
const STORE_PX: u32 = 640;
const MISS_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// A miss made while YouTube Music was unreachable is retried sooner.
const PARTIAL_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// After a provider failure, wait this long before asking again.
const FAILED_TTL: Duration = Duration::from_secs(60 * 60);
/// iTunes Search's budget is about 20 requests a minute.
const ITUNES_SPACING: Duration = Duration::from_millis(3200);
const TIMEOUT: Duration = Duration::from_secs(8);
/// Hong Kong's catalogue is almost Taiwan's, and every storefront tried costs
/// a request out of iTunes' small budget.
const ITUNES_COUNTRIES: [&str; 2] = ["tw", "us"];
static DIR: OnceLock<PathBuf> = OnceLock::new();

static BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[(（\[【][^)）\]】]*[)）\]】]").expect("bracket regex"));
static RELEASE_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*-\s*(single|ep)\s*$|\s+(ep|single)\s*$").expect("suffix regex")
});

#[derive(Clone, Debug)]
pub struct Wanted {
    /// Already mapped through `artistNames`.
    pub artist: String,
    pub album: String,
    pub title: String,
}

fn dir() -> PathBuf {
    DIR.get_or_init(|| {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join("Library/Caches/pear-music-widget/artwork")
    })
    .clone()
}

// ------------------------------------------------------------------ matching

/// Simplified, lowercased, letters and digits only.
pub(crate) fn fold(text: &str) -> String {
    fast2s::convert(text)
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// `fold` after dropping bracketed notes and a trailing "- Single"/"EP".
fn core_name(text: &str) -> String {
    let text = BRACKETS.replace_all(text, " ");
    fold(&RELEASE_SUFFIX.replace(&text, ""))
}

/// How well a provider's name matches ours: 3 identical, 2 identical once
/// notes are dropped, 1 one a prefix of the other, 0 no match. Exact beats
/// stripped so 范特西 wins over 范特西 (Live) - Single.
pub fn name_score(ours: &str, theirs: &str) -> u8 {
    let folded = fold(ours);
    if folded.is_empty() {
        return 0;
    }
    if folded == fold(theirs) {
        return 3;
    }
    let (a, b) = (core_name(ours), core_name(theirs));
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    if a == b {
        return 2;
    }
    let (short, long) = if a.chars().count() <= b.chars().count() { (&a, &b) } else { (&b, &a) };
    if short.chars().count() >= 2 && long.starts_with(short.as_str()) {
        1
    } else {
        0
    }
}

pub fn artist_matches(ours: &str, theirs: &str) -> bool {
    let ours = fold(&crate::lyrics::lead_artist(ours));
    let theirs = fold(theirs);
    !ours.is_empty() && !theirs.is_empty() && (theirs.contains(&ours) || ours.contains(&theirs))
}

/// Best candidate by name score among those whose artist matches.
fn best<'a>(
    candidates: impl Iterator<Item = (&'a str, &'a str, &'a str)>,
    artist: &str,
    name: &str,
) -> Option<String> {
    candidates
        .filter(|(their_artist, _, _)| artist_matches(artist, their_artist))
        .map(|(_, their_name, url)| (name_score(name, their_name), url))
        .filter(|(score, url)| *score > 0 && !url.is_empty())
        .max_by_key(|(score, _)| *score)
        .map(|(_, url)| url.to_string())
}

// --------------------------------------------------------------------- cache

fn key(kind: &str, artist: &str, name: &str) -> String {
    let text = format!("{kind}\0{}\0{}", fold(&crate::lyrics::lead_artist(artist)), core_name(name));
    format!("{kind}-{:016x}", crate::fsutil::stable_hash(&text))
}

enum Cached {
    Hit(Vec<u8>),
    Miss,
    Unknown,
}

/// Contents of a `.miss` written while YouTube Music was unreachable: the
/// answer is only partial, so it expires sooner.
const PARTIAL: &[u8] = b"partial";
/// Contents of a `.miss` written when a provider failed (timeout, rate limit,
/// a host this network cannot reach). Not an answer, so it is short — but long
/// enough that a provider which keeps failing is not asked on every play.
const FAILED: &[u8] = b"failed";

fn read_cache(key: &str) -> Cached {
    let base = dir();
    if let Ok(bytes) = std::fs::read(base.join(format!("{key}.jpg"))) {
        return Cached::Hit(bytes);
    }
    let miss = base.join(format!("{key}.miss"));
    let ttl = match std::fs::read(&miss) {
        Ok(body) if body == PARTIAL => PARTIAL_TTL,
        Ok(body) if body == FAILED => FAILED_TTL,
        Ok(_) => MISS_TTL,
        Err(_) => return Cached::Unknown,
    };
    let fresh = std::fs::metadata(&miss)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|at| SystemTime::now().duration_since(at).ok())
        .is_some_and(|age| age < ttl);
    if fresh {
        Cached::Miss
    } else {
        Cached::Unknown
    }
}

/// `read_cache` for the lookup, which runs on an async worker: a hit is a
/// 640 px JPEG, a real read, so it goes to a blocking thread.
async fn read_cached(key: &str) -> Cached {
    let key = key.to_string();
    tauri::async_runtime::spawn_blocking(move || read_cache(&key))
        .await
        .unwrap_or(Cached::Unknown)
}

/// The first hit among `keys` (`Ok(Some)`), a miss for every one of them
/// (`Ok(None)`), or `Err` when at least one is still unknown. One trip to a
/// blocking thread for the lot.
async fn read_cached_any(keys: &[String]) -> Result<Option<(String, Vec<u8>)>, ()> {
    let keys = keys.to_vec();
    tauri::async_runtime::spawn_blocking(move || {
        let mut unknown = false;
        for key in keys {
            match read_cache(&key) {
                Cached::Hit(bytes) => return Ok(Some((key, bytes))),
                Cached::Miss => {}
                Cached::Unknown => unknown = true,
            }
        }
        if unknown {
            Err(())
        } else {
            Ok(None)
        }
    })
    .await
    .unwrap_or(Err(()))
}

enum Outcome {
    Hit(Vec<u8>),
    Miss { partial: bool },
    Failed,
}

/// On a blocking thread, whatever the outcome: the lookup runs on an async
/// worker, and even a one-byte miss file is a write the worker should not
/// wait on.
fn write_cache(key: &str, outcome: Outcome) {
    let key = key.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let base = dir();
        if std::fs::create_dir_all(&base).is_err() {
            return;
        }
        match outcome {
            Outcome::Hit(bytes) => {
                // Hits never expire, so a torn one would stand forever: synced
                // and renamed into place.
                let _ = crate::fsutil::write_atomic(&base.join(format!("{key}.jpg")), &bytes);
                let _ = std::fs::remove_file(base.join(format!("{key}.miss")));
            }
            Outcome::Miss { partial } => {
                let body: &[u8] = if partial { PARTIAL } else { b"" };
                let _ = std::fs::write(base.join(format!("{key}.miss")), body);
            }
            Outcome::Failed => {
                let _ = std::fs::write(base.join(format!("{key}.miss")), FAILED);
            }
        }
    });
}

/// Every key this track's cover could be stored under, album first. Callers
/// memoise sized copies by these, so one album's cover is resized once.
pub fn keys(wanted: &Wanted) -> Vec<String> {
    album_key(wanted)
        .into_iter()
        .chain(std::iter::once(key("track", &wanted.artist, &wanted.title)))
        .collect()
}

/// A cached cover and the key it was found under, without the network — for
/// search rows, which are drawn too often to be worth a lookup each.
pub fn cached(wanted: &Wanted) -> Option<(String, Vec<u8>)> {
    keys(wanted).into_iter().find_map(|key| match read_cache(&key) {
        Cached::Hit(bytes) => Some((key, bytes)),
        _ => None,
    })
}

fn album_key(wanted: &Wanted) -> Option<String> {
    (!core_name(&wanted.album).is_empty()).then(|| key("album", &wanted.artist, &wanted.album))
}

/// Forget remembered misses so a renamed artist is looked up again.
pub fn forget_misses(wanted: &Wanted) {
    for key in keys(wanted) {
        let _ = std::fs::remove_file(dir().join(format!("{key}.miss")));
    }
}

// ------------------------------------------------------------------- lookup

/// Whether a lookup is still worth finishing. Checked before the lookup takes
/// its slot and before every request, so skipping ten tracks does not leave ten
/// lookups ahead of the one on screen.
pub type Alive = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Clone)]
pub enum Priority {
    /// The cover on the card right now, for as long as `Alive` says so.
    Now(Alive),
    /// Queue rows and the next track: wanted, but never ahead of `Now`.
    Background,
}

impl Priority {
    pub fn background() -> Self {
        Self::Background
    }

    fn is_now(&self) -> bool {
        matches!(self, Self::Now(_))
    }

    fn alive(&self) -> bool {
        match self {
            Self::Now(alive) => alive(),
            Self::Background => true,
        }
    }
}

static NOW: LazyLock<tokio::sync::Semaphore> = LazyLock::new(|| tokio::sync::Semaphore::new(2));
static BACKGROUND: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(1));
/// `Now` lookups in progress; background requests wait for zero.
static NOW_ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct NowGuard;

impl NowGuard {
    fn enter() -> Self {
        NOW_ACTIVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

impl Drop for NowGuard {
    fn drop(&mut self) {
        NOW_ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}
/// One lookup per album (or track) at a time: a screen of rows from one album
/// waits for the first and then reads its answer from the cache.
static IN_FLIGHT: LazyLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
/// iTunes Search allows about 20 requests a minute; past that it answers 403
/// for a while. Requests are spaced rather than retried.
static ITUNES_NEXT: LazyLock<tokio::sync::Mutex<Instant>> =
    LazyLock::new(|| tokio::sync::Mutex::new(Instant::now()));

/// A provider's answer: `Ok(None)` is a clean "no match", which may be cached;
/// `Err` is a failure (timeout, rate limit, server error), which must not be.
type Probe<T> = Result<Option<T>, ()>;

/// JPEG bytes at `STORE_PX` and the key they are cached under, from cache or
/// network.
pub async fn find(core: &Arc<Core>, wanted: &Wanted, priority: Priority) -> Option<(String, Vec<u8>)> {
    if fold(&wanted.artist).is_empty() {
        return None;
    }
    let keys = keys(wanted);
    if let Ok(answer) = read_cached_any(&keys).await {
        return answer;
    }

    let slot = {
        let mut map = IN_FLIGHT.lock().expect("artwork in-flight");
        Arc::clone(map.entry(keys[0].clone()).or_default())
    };
    if !priority.alive() {
        return None;
    }
    let _same_album = slot.lock().await;
    // Counted only once the album slot is ours: a background lookup holding
    // this slot must not wait on us while we wait on it.
    let _now = priority.is_now().then(NowGuard::enter);
    let permits = if priority.is_now() { &*NOW } else { &*BACKGROUND };
    let _permit = permits.acquire().await.ok()?;
    if !priority.alive() {
        return None;
    }
    let found = lookup(core, wanted, &priority).await;
    IN_FLIGHT
        .lock()
        .expect("artwork in-flight")
        .retain(|_, held| Arc::strong_count(held) > 1);
    found
}

/// The network half. Reads the cache again first: whoever held this album's
/// slot may have just answered it.
async fn lookup(core: &Arc<Core>, wanted: &Wanted, priority: &Priority) -> Option<(String, Vec<u8>)> {
    let http = core.api.client();

    if let Some(album_key) = album_key(wanted) {
        match read_cached(&album_key).await {
            Cached::Hit(bytes) => return Some((album_key, bytes)),
            Cached::Miss => {}
            Cached::Unknown => {
                let (mut found, mut failed) = itunes_albums(http, wanted, priority).await?;
                if found.is_none() && !priority.alive() {
                    return None;
                }
                if found.is_none() {
                    match deezer(http, wanted).await {
                        Ok(url) => found = url,
                        Err(()) => failed = true,
                    }
                }
                if let Some(url) = found {
                    match download(http, &url).await {
                        Some(bytes) => {
                            write_cache(&album_key, Outcome::Hit(bytes.clone()));
                            return Some((album_key, bytes));
                        }
                        None => failed = true,
                    }
                }
                write_cache(&album_key, if failed { Outcome::Failed } else { Outcome::Miss { partial: false } });
            }
        }
    }

    let track_key = key("track", &wanted.artist, &wanted.title);
    match read_cached(&track_key).await {
        Cached::Hit(bytes) => Some((track_key, bytes)),
        Cached::Miss => None,
        Cached::Unknown => {
            // Store by store rather than through `itunes_albums`: a song URL
            // whose download fails is worth trying in the next store.
            let mut failed = false;
            for country in ITUNES_COUNTRIES {
                if !priority.alive() {
                    return None;
                }
                match itunes(http, wanted, "song", country, priority).await {
                    Ok(Some(url)) => match download(http, &url).await {
                        Some(bytes) => {
                            write_cache(&track_key, Outcome::Hit(bytes.clone()));
                            return Some((track_key, bytes));
                        }
                        None => failed = true,
                    },
                    Ok(None) => {}
                    Err(()) => failed = true,
                }
            }
            if !priority.alive() {
                return None;
            }
            match youtube(core, wanted).await {
                Ok(Some(bytes)) => {
                    write_cache(&track_key, Outcome::Hit(bytes.clone()));
                    Some((track_key, bytes))
                }
                // A provider failed: not an answer, but not worth asking again
                // on every play either.
                Ok(None) | Err(()) if failed => {
                    write_cache(&track_key, Outcome::Failed);
                    None
                }
                // YouTube Music not running is not a failure of the lookup,
                // but it is not a full answer either.
                Ok(None) => {
                    write_cache(&track_key, Outcome::Miss { partial: false });
                    None
                }
                Err(()) => {
                    write_cache(&track_key, Outcome::Miss { partial: true });
                    None
                }
            }
        }
    }
}

/// Ask each iTunes store in turn for an album's artwork. The first URL found,
/// and whether any store failed to answer (so a miss is not the full answer);
/// `None` if the lookup was abandoned for a track skipped past.
async fn itunes_albums(
    http: &reqwest::Client,
    wanted: &Wanted,
    priority: &Priority,
) -> Option<(Option<String>, bool)> {
    let mut failed = false;
    for country in ITUNES_COUNTRIES {
        if !priority.alive() {
            return None;
        }
        match itunes(http, wanted, "album", country, priority).await {
            Ok(Some(url)) => return Some((Some(url), failed)),
            Ok(None) => {}
            Err(()) => failed = true,
        }
    }
    Some((None, failed))
}

async fn get_json(http: &reqwest::Client, url: &str, query: &[(&str, &str)]) -> Result<Value, ()> {
    let url = reqwest::Url::parse_with_params(url, query).map_err(|_| ())?;
    let res = http.get(url).timeout(TIMEOUT).send().await.map_err(|_| ())?;
    if !res.status().is_success() {
        return Err(());
    }
    res.json().await.map_err(|_| ())
}

/// Hold the next iTunes request until its slot. Background requests first
/// wait for every `Now` lookup to finish, so they do not take its slots.
async fn pace_itunes(priority: &Priority) {
    if !priority.is_now() {
        while NOW_ACTIVE.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    let wait = {
        let mut next = ITUNES_NEXT.lock().await;
        let now = Instant::now();
        let at = (*next).max(now);
        *next = at + ITUNES_SPACING;
        at - now
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

/// Search terms without the notes that make providers miss: bracketed
/// remaster tags, bitrates, "黑胶转录 24-96".
fn term(wanted: &Wanted, name: &str) -> String {
    let name = BRACKETS.replace_all(name, " ");
    format!("{} {}", crate::lyrics::lead_artist(&wanted.artist), name.trim())
}

async fn itunes(
    http: &reqwest::Client,
    wanted: &Wanted,
    entity: &str,
    country: &str,
    priority: &Priority,
) -> Probe<String> {
    let (name, field) = if entity == "album" {
        (&wanted.album, "collectionName")
    } else {
        (&wanted.title, "trackName")
    };
    let term = term(wanted, name);
    pace_itunes(priority).await;
    let body = get_json(
        http,
        "https://itunes.apple.com/search",
        &[("term", &term), ("entity", entity), ("country", country), ("limit", "25")],
    )
    .await?;
    let Some(results) = body.get("results").and_then(Value::as_array) else {
        return Err(());
    };
    fn text<'a>(item: &'a Value, key: &str) -> &'a str {
        item.get(key).and_then(Value::as_str).unwrap_or_default()
    }
    let candidates: Vec<(&str, &str, &str)> = results
        .iter()
        .map(|item| (text(item, "artistName"), text(item, field), text(item, "artworkUrl100")))
        .collect();
    // The CDN serves any square size from the same path.
    Ok(best(candidates.into_iter(), &wanted.artist, name)
        .map(|url| url.replace("100x100bb", &format!("{STORE_PX}x{STORE_PX}bb"))))
}

async fn deezer(http: &reqwest::Client, wanted: &Wanted) -> Probe<String> {
    let term = term(wanted, &wanted.album);
    let body = get_json(http, "https://api.deezer.com/search/album", &[("q", &term)]).await?;
    let Some(data) = body.get("data").and_then(Value::as_array) else {
        return Err(());
    };
    let candidates: Vec<(&str, &str, &str)> = data
        .iter()
        .map(|item| {
            (
                item.pointer("/artist/name").and_then(Value::as_str).unwrap_or_default(),
                item.get("title").and_then(Value::as_str).unwrap_or_default(),
                item.get("cover_xl").and_then(Value::as_str).unwrap_or_default(),
            )
        })
        .collect();
    Ok(best(candidates.into_iter(), &wanted.artist, &wanted.album))
}

/// Square song art from YouTube Music's own search, through the API server.
/// `Err` when YouTube Music is not reachable.
async fn youtube(core: &Arc<Core>, wanted: &Wanted) -> Probe<Vec<u8>> {
    let query = format!("{} {}", wanted.title, crate::lyrics::lead_artist(&wanted.artist));
    let payload = core.api.search(&query).await.map_err(|_| ())?;
    let results = crate::search::parse_search_results(&payload);
    let Some(hit) = results
        .iter()
        .filter(|item| {
            item.thumbnail
                .as_deref()
                .is_some_and(|src| src.contains("googleusercontent.com"))
        })
        .filter(|item| artist_matches(&wanted.artist, &item.subtitle))
        .map(|item| (name_score(&wanted.title, &item.title), item))
        .filter(|(score, _)| *score >= 2)
        .max_by_key(|(score, _)| *score)
        .map(|(_, item)| item)
    else {
        return Ok(None);
    };
    let data_url = core
        .api
        .fetch_cover(hit.thumbnail.as_deref(), STORE_PX)
        .await
        .ok_or(())?;
    let (_, encoded) = data_url.split_once("base64,").ok_or(())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| ())?;
    Ok(normalise(&bytes))
}

async fn download(http: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    let res = http.get(url).timeout(TIMEOUT).send().await.ok()?;
    if !res.status().is_success() {
        return None;
    }
    let bytes = res.bytes().await.ok()?;
    let bytes = bytes.to_vec();
    tauri::async_runtime::spawn_blocking(move || normalise(&bytes))
        .await
        .ok()
        .flatten()
}

/// Re-encode as JPEG no larger than `STORE_PX`, so the cache holds one format
/// and a provider's 3000px original never lands on disk.
pub fn normalise(bytes: &[u8]) -> Option<Vec<u8>> {
    resize(bytes, STORE_PX)
}

/// `resize` for bytes that came out of the disk cache under `key`. A record
/// that no longer decodes is dropped, so the next lookup fetches the cover
/// again instead of returning the same unusable hit, and no cover, forever.
pub fn resize_cached(key: &str, bytes: &[u8], size: u32) -> Option<Vec<u8>> {
    let sized = resize(bytes, size);
    if sized.is_none() {
        let _ = std::fs::remove_file(dir().join(format!("{key}.jpg")));
    }
    sized
}

pub fn resize(bytes: &[u8], size: u32) -> Option<Vec<u8>> {
    let image = image::load_from_memory(bytes).ok()?;
    let image = if image.width() > size || image.height() > size {
        image.thumbnail(size, size)
    } else {
        image
    };
    let mut out = std::io::Cursor::new(Vec::new());
    image.to_rgb8().write_to(&mut out, image::ImageFormat::Jpeg).ok()?;
    Some(out.into_inner())
}

pub fn data_url(jpeg: &[u8]) -> Arc<str> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(jpeg);
    Arc::from(format!("data:image/jpeg;base64,{encoded}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_titles_beat_live_singles() {
        assert_eq!(name_score("范特西", "范特西"), 3);
        assert_eq!(name_score("范特西", "范特西 (Live) - Single"), 2);
        assert_eq!(name_score("范特西", "依然范特西"), 0);
    }

    #[test]
    fn traditional_and_simplified_fold_together() {
        assert_eq!(name_score("乐行者", "樂行者"), 3);
        assert!(artist_matches("林俊杰", "林俊傑"));
        assert!(artist_matches("周杰伦", "周杰倫"));
        assert!(!artist_matches("周杰伦", "Jay Chou"));
    }

    #[test]
    fn release_notes_do_not_block_a_match() {
        assert_eq!(name_score("JJ陆 (黑胶转录 24-96)", "JJ陸"), 2);
        assert_eq!(name_score("范特西 Plus EP", "范特西 Plus"), 2);
    }

    #[test]
    fn collaborations_match_on_the_lead_artist() {
        assert!(artist_matches("林俊杰, 张惠妹", "林俊傑"));
    }

    #[test]
    fn keys_ignore_script_and_notes() {
        assert_eq!(key("album", "林俊杰", "JJ陆 (黑胶转录 24-96)"), key("album", "林俊傑", "JJ陸"));
    }
}

/// `cargo test artwork_live -- --ignored --nocapture` — hits iTunes.
#[cfg(test)]
mod live {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn artwork_live() {
        let http = reqwest::Client::new();
        for (artist, album, title) in [
            ("周杰伦", "八度空间", "米兰的小铁匠"),
            ("周杰伦", "十一月的萧邦", "飘移(头文字D THE MOVIE主题曲)"),
            ("林俊杰", "JJ陆 (黑胶转录DSD5.6Mhz,海蝶OBTW08JJ01VL)", "黑武士 (LP24 96)"),
            ("林俊杰", "单曲与合作", "我们很好"),
            ("Jaychou", "范特西", "双截棍"),
        ] {
            let wanted = Wanted { artist: artist.into(), album: album.into(), title: title.into() };
            let mut album_hit = None;
            for country in ITUNES_COUNTRIES {
                album_hit = itunes(&http, &wanted, "album", country, &Priority::Background).await.ok().flatten();
                if album_hit.is_some() { break; }
            }
            let song_hit = itunes(&http, &wanted, "song", "tw", &Priority::Background).await.ok().flatten();
            println!("{artist} / {album}: album={} song={}", album_hit.is_some(), song_hit.is_some());
        }
    }
}
