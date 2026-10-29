//! Synced lyrics from YouTube Music, LRCLib, and Kugeci. The
//! api-server exposes no lyrics route, so the widget has to ask for them itself.
//!
//! **This and `artwork.rs` are the only places the app talks to anything other
//! than localhost.** Keep it that way: the renderer's CSP allows no network at
//! all, so anything fetched has to come through Rust. Hosts are `lrclib.net`,
//! `music.youtube.com`, and `www.kugeci.com`; requests use the track's title,
//! artist and video id.

mod kugeci;
mod lrclib;
mod ytmusic;

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::lyrics_cache;
use crate::state::Song;
use crate::text::{clean_title, has_cjk, lead_artist};

use kugeci::fetch_kugeci_for_song;
use lrclib::{lrclib_cleaned, lrclib_exact, lrclib_search};
use ytmusic::{fetch_ytmusic, find_ytmusic_video};

pub const USER_AGENT: &str = "pear-music-widget (https://github.com/TianCal/pear-music-widget)";
const TIMEOUT: Duration = Duration::from_secs(9);
const CACHE_MAX: usize = 60;

/// `Deserialize` for `lyrics_cache`, which reads these back off disk. Nothing
/// from the network is deserialised into this — sources are parsed by hand.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LyricLine {
    /// `None` on an unsynced block — those do not follow along.
    pub time: Option<f64>,
    pub text: String,
    /// Display-only romanisation. Raw fetched and cached lines leave it absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jyutping: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Lyrics {
    pub synced: bool,
    pub lines: Vec<LyricLine>,
    /// Which of the matching tiers found it; useful when a track comes back
    /// with someone else's words.
    pub how: How,
}

/// The lookup tier that found a set of lyrics. Stored in the disk cache and
/// sent to the renderer as its lowercase name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum How {
    Exact,
    Cleaned,
    Search,
    Ytmusic,
    Kugeci,
    /// A record whose tier is not one of the above: a file someone edited.
    #[serde(other)]
    Cached,
}

impl std::fmt::Display for How {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            How::Exact => "exact",
            How::Cleaned => "cleaned",
            How::Search => "search",
            How::Ytmusic => "ytmusic",
            How::Kugeci => "kugeci",
            How::Cached => "cached",
        };
        f.write_str(name)
    }
}

#[cfg(test)]
mod line_shape_tests {
    use super::*;

    #[test]
    fn old_cached_lines_load_without_display_only_jyutping() {
        let line: LyricLine = serde_json::from_str(r#"{"time":1.0,"text":"心裏"}"#)
            .expect("old cache shape still parses");
        assert_eq!(line.jyutping, None);
        let encoded = serde_json::to_string(&line).expect("serialises");
        assert!(!encoded.contains("jyutping"));
    }
}

/// Cache misses too (`None`), so a track with no lyrics is not looked up again
/// every time the panel is reopened.
static CACHE: LazyLock<Mutex<(Vec<String>, HashMap<String, Option<Lyrics>>)>> =
    LazyLock::new(|| Mutex::new((Vec::new(), HashMap::new())));

static STAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(\d+):(\d{1,2}(?:[.:]\d{1,3})?)\]").unwrap());
static BRACKETED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[^\]]*\]").unwrap());

/// `[mm:ss.xx]text`, possibly several stamps per line. Blank lines are kept —
/// they are the instrumental gaps, and the roll needs them to breathe.
pub fn parse_lrc(lrc: &str) -> Vec<LyricLine> {
    let mut lines: Vec<LyricLine> = Vec::new();

    for raw in lrc.split('\n') {
        let stamps: Vec<_> = STAMP.captures_iter(raw).collect();
        if stamps.is_empty() {
            continue;
        }

        let text = BRACKETED.replace_all(raw, "").trim().to_string();
        for stamp in stamps {
            let minutes: f64 = stamp[1].parse().unwrap_or(0.0);
            let seconds: f64 = stamp[2].replace(':', ".").parse().unwrap_or(f64::NAN);
            if seconds.is_finite() {
                lines.push(LyricLine {
                    time: Some(minutes * 60.0 + seconds),
                    text: text.clone(),
                    jyutping: None,
                });
            }
        }
    }

    lines.sort_by(|a, b| {
        a.time
            .unwrap_or(0.0)
            .partial_cmp(&b.time.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    lines
}

// ------------------------------------------------------------------ lookup

/// YouTube Music, three LRCLib tiers, then Kugeci.
///
/// YouTube Music goes first, because it is asked by **video id** — it is the one
/// tier that cannot answer with a different song's words, and it covers what
/// LRCLib has never heard of, which is smaller labels and much of the Mandarin
/// and Cantonese catalogue. It costs two requests where LRCLib's first tier
/// costs one, which the disk cache turns into a one-off per track.
///
/// The other three ask LRCLib, which needs help because YouTube Music titles
/// carry soundtrack credits and 《…》 wrappers it will not match on: exact with
/// everything we know, exact on a cleaned title, then free-text search picking
/// the hit whose duration is closest.
///
/// They also run when YouTube answered with an *unsynced* block, since LRCLib
/// may have the same song with timings. Only a synced result displaces the
/// block; nothing at all leaves the block in place.
pub async fn fetch_lyrics(http: &reqwest::Client, song: &Song) -> Option<Lyrics> {
    fetch_lyrics_traced(http, song).await.0
}

/// Where `fetch_lyrics_traced` found its answer. A miss from `Network` was
/// just made and may be a provider failing; one from `Memory` or `Disk` has
/// already been given its retry and is not worth another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Memory,
    Disk,
    Network,
}

/// `fetch_lyrics`, saying where the answer came from. A track with no id
/// has nothing to look up and reads as `Memory`: not an answer to retry.
pub async fn fetch_lyrics_traced(http: &reqwest::Client, song: &Song) -> (Option<Lyrics>, Origin) {
    if song.video_id.is_empty() {
        return (None, Origin::Memory);
    }
    if let Some(hit) = CACHE
        .lock()
        .expect("lyrics cache")
        .1
        .get(&song.video_id)
        .cloned()
    {
        return (hit, Origin::Memory);
    }

    // Disk before network, and a disk hit is promoted into memory so the panel
    // being reopened does not go back to the filesystem for it either. The
    // read is on a blocking thread: this runs on an async worker, and the
    // cache directory may be on a slow disk.
    let id = song.video_id.clone();
    let on_disk = tauri::async_runtime::spawn_blocking(move || lyrics_cache::load(&id))
        .await
        .ok()
        .flatten();
    if let Some(hit) = on_disk {
        remember(&song.video_id, &hit);
        return (hit, Origin::Disk);
    }

    let result = if song.video_id.starts_with(crate::local::ID_PREFIX) {
        lookup_local(http, song).await
    } else {
        lookup_streamed(http, song).await
    };

    remember(&song.video_id, &result);
    // Also off the worker: a store past the cap sweeps the whole directory.
    // Awaited, so a `forget_miss` the caller makes next cannot run before the
    // miss it means to forget has landed. By now the lookup is complete, so
    // a caller dropped here has still only cached a final answer.
    let (id, stored) = (song.video_id.clone(), result.clone());
    let _ = tauri::async_runtime::spawn_blocking(move || lyrics_cache::store(&id, &stored)).await;

    (result, Origin::Network)
}

/// A YouTube Music track: its own lyrics first, then LRCLib, then Kugeci.
async fn lookup_streamed(http: &reqwest::Client, song: &Song) -> Option<Lyrics> {
    let title = song.title.clone();
    let artist = lead_artist(&song.artist);
    let duration = song.song_duration.round();

    // 1. YouTube Music's own lyrics, for the track the player is actually
    //    playing. Being keyed by the video id, this is the only tier that
    //    cannot come back with a different song's words, so it goes first.
    let mut result = fetch_ytmusic(http, &song.video_id).await;

    // Unless they are unsynced. YouTube serves a plain block for a good part of
    // the catalogue, and LRCLib often has the same song *with* timings — a roll
    // is worth another three requests. Held as a floor rather than discarded:
    // if LRCLib has nothing, the block is still better than no words at all.
    let unsynced = match &result {
        Some(words) if !words.synced => result.take(),
        _ => None,
    };

    // 2. Exact, with everything we know.
    if result.is_none() && !title.is_empty() && !artist.is_empty() {
        result = lrclib_exact(http, &title, &artist, &song.album, duration).await;
    }

    // 3. Exact again on a cleaned title, without album or duration to pin it.
    let simple = clean_title(&title);
    if result.is_none() && !simple.is_empty() && simple != title && !artist.is_empty() {
        result = lrclib_cleaned(http, &simple, &artist).await;
    }

    // 4. Free-text search — this is what rescues soundtrack and 《…》 titles.
    if result.is_none() && !simple.is_empty() {
        result = lrclib_search(http, &simple, &artist, duration, None).await;
    }

    // 5. Kugeci carries timed lyrics for Chinese tracks the other sources miss.
    // It can upgrade plain LRCLib lyrics as well as YouTube's unsynced block.
    if result.as_ref().is_none_or(|words| !words.synced) {
        result = fetch_kugeci_for_song(http, song).await.or(result);
    }

    // Nothing timed anywhere, so keep whichever plain block we found.
    result.or(unsynced)
}

/// A local file. Its tags carry the real album and its exact duration — often
/// better than what YouTube reports — but there is no video id, so the order
/// leans on what that metadata can verify:
///
/// 1. LRCLib exact match with the full tags and duration (LRCLib checks it).
/// 2. Kugeci, for Chinese titles or artists only — the catalogue it covers,
///    and verified against title and artist. Asking it about anything else
///    only delays a miss.
/// 3. LRCLib exact on a cleaned title.
/// 4. LRCLib search, rejecting hits more than `LOCAL_DURATION_SLACK` off the
///    file — live-versus-studio timing is exactly what fuzzy search gets wrong.
/// 5. YouTube Music: search its catalogue for the song, check artist, title
///    and duration, and take that video's timed lyrics. Direct to YouTube, like
///    tier 1 of the streamed order, so YouTube Music need not be running.
///
/// Each tier keeps looking past unsynced words; the first plain block found is
/// the fallback if nothing timed turns up.
async fn lookup_local(http: &reqwest::Client, song: &Song) -> Option<Lyrics> {
    let title = song.title.clone();
    let artist = lead_artist(&song.artist);
    let duration = song.song_duration.round();
    let chinese = has_cjk(&title) || has_cjk(&song.artist);
    let mut plain: Option<Lyrics> = None;

    // 1.
    if !title.is_empty() && !artist.is_empty() {
        let found = lrclib_exact(http, &title, &artist, &song.album, duration).await;
        if let Some(found) = settle(found, &mut plain) {
            return Some(found);
        }
    }

    // 2.
    if chinese {
        if let Some(found) = settle(fetch_kugeci_for_song(http, song).await, &mut plain) {
            return Some(found);
        }
    }

    // 3.
    let simple = clean_title(&title);
    if !simple.is_empty() && simple != title && !artist.is_empty() {
        if let Some(found) = settle(lrclib_cleaned(http, &simple, &artist).await, &mut plain) {
            return Some(found);
        }
    }

    // 4.
    if !simple.is_empty() {
        let found = lrclib_search(http, &simple, &artist, duration, Some(LOCAL_DURATION_SLACK)).await;
        if let Some(found) = settle(found, &mut plain) {
            return Some(found);
        }
    }

    // 5.
    if let Some(video_id) = find_ytmusic_video(http, &title, &song.artist, duration).await {
        if let Some(found) = settle(fetch_ytmusic(http, &video_id).await, &mut plain) {
            return Some(found);
        }
    }

    plain
}

/// How far a local file's duration may be from a search hit's.
const LOCAL_DURATION_SLACK: f64 = 3.0;

/// Synced words end the search; plain ones are kept as the fallback (the
/// first found wins) and the search goes on.
fn settle(found: Option<Lyrics>, plain: &mut Option<Lyrics>) -> Option<Lyrics> {
    match found {
        Some(words) if words.synced => Some(words),
        Some(words) => {
            plain.get_or_insert(words);
            None
        }
        None => None,
    }
}

#[cfg(test)]
mod local_order_tests {
    use super::*;

    fn words(synced: bool, how: How) -> Lyrics {
        Lyrics {
            synced,
            lines: Vec::new(),
            how,
        }
    }

    #[test]
    fn settle_stops_on_synced_and_keeps_the_first_plain() {
        let mut plain = None;
        assert!(settle(Some(words(false, How::Exact)), &mut plain).is_none());
        assert!(settle(Some(words(false, How::Search)), &mut plain).is_none());
        assert_eq!(plain.as_ref().map(|w| w.how), Some(How::Exact));
        assert_eq!(settle(Some(words(true, How::Kugeci)), &mut plain).map(|w| w.how), Some(How::Kugeci));
    }
}

/// Forget one remembered miss, in memory and on disk — after the credit it
/// was looked up under has been renamed. Found lyrics are kept.
pub fn forget_miss(video_id: &str) {
    {
        let mut cache = CACHE.lock().expect("lyrics cache");
        let (order, entries) = &mut *cache;
        if matches!(entries.get(video_id), Some(None)) {
            entries.remove(video_id);
            order.retain(|id| id != video_id);
        }
    }
    lyrics_cache::forget_miss(video_id);
}

pub fn clear_negative_cache() {
    let mut cache = CACHE.lock().expect("lyrics cache");
    retain_found_lyrics(&mut cache);
}

fn retain_found_lyrics(cache: &mut (Vec<String>, HashMap<String, Option<Lyrics>>)) {
    let (order, entries) = cache;
    entries.retain(|_, result| result.is_some());
    order.retain(|video_id| entries.contains_key(video_id));
}

/// The in-memory half: the last `CACHE_MAX` lookups, misses included.
fn remember(video_id: &str, result: &Option<Lyrics>) {
    let mut cache = CACHE.lock().expect("lyrics cache");
    let (order, entries) = &mut *cache;
    if entries
        .insert(video_id.to_string(), result.clone())
        .is_none()
    {
        order.push(video_id.to_string());
    }
    while order.len() > CACHE_MAX {
        let oldest = order.remove(0);
        entries.remove(&oldest);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::Value;

    use super::*;

    #[test]
    fn clearing_memory_misses_keeps_found_lyrics_and_cache_order() {
        let words = Lyrics {
            synced: false,
            lines: Vec::new(),
            how: How::Ytmusic,
        };
        let mut cache = (
            vec!["miss".into(), "found".into()],
            HashMap::from([("miss".into(), None), ("found".into(), Some(words.clone()))]),
        );
        retain_found_lyrics(&mut cache);
        assert_eq!(cache.0, vec!["found"]);
        assert_eq!(cache.1.get("found"), Some(&Some(words)));
        assert!(!cache.1.contains_key("miss"));
    }

    #[tokio::test]
    #[ignore = "explicit queue audit; needs PMW_LYRICS_AUDIT_INPUT and PMW_LYRICS_AUDIT_OUTPUT"]
    async fn audit_queue_lyrics() {
        use futures_util::{stream, StreamExt};
        let input = std::env::var("PMW_LYRICS_AUDIT_INPUT").expect("audit input");
        let output = std::env::var("PMW_LYRICS_AUDIT_OUTPUT").expect("audit output");
        let tracks: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(input).unwrap()).unwrap();
        let http = reqwest::Client::new();
        // Fresh lookups in tests: the disk cache stays disabled, and each row
        // records only metadata and provenance, never the fetched lyric text.
        let mut pending = stream::iter(tracks.into_iter().enumerate()).map(|(index, track)| {
            let http = http.clone();
            async move {
                let text = |key| track.get(key).and_then(Value::as_str).unwrap_or("").to_string();
                let song = Song { video_id: text("videoId"), title: text("title"), artist: text("artist"),
                    album: text("album"), song_duration: track.get("duration").and_then(Value::as_f64).unwrap_or(0.0),
                    ..Song::default() };
                let result = tokio::time::timeout(Duration::from_secs(75), fetch_lyrics(&http, &song)).await;
                let (status, words) = match result {
                    Ok(Some(words)) => ("resolved", Some(words)),
                    Ok(None) => ("no_match", None),
                    Err(_) => ("timeout", None),
                };
                serde_json::json!({"position": index + 1, "videoId": song.video_id, "title": song.title,
                    "artist": song.artist, "album": song.album, "duration": song.song_duration, "status": status,
                    "source": words.as_ref().map(|words| words.how), "synced": words.as_ref().map(|words| words.synced),
                    "lineCount": words.as_ref().map(|words| words.lines.len())})
            }
        }).buffer_unordered(3);
        let mut rows = Vec::new();
        while let Some(row) = pending.next().await {
            println!(
                "audit {}: {} — {}: {} ({})",
                row["position"], row["title"], row["artist"], row["status"], row["source"]
            );
            rows.push(row);
            rows.sort_by_key(|row| row["position"].as_u64().unwrap());
            std::fs::write(&output, serde_json::to_string_pretty(&rows).unwrap()).unwrap();
        }
    }

    #[test]
    fn keeps_instrumental_gaps_and_sorts() {
        let lines = parse_lrc("[00:12.50]Second\n[00:03.00]First\nno stamp\n[00:20.00]");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "First");
        assert_eq!(lines[1].time, Some(12.5));
        // The blank line survives: it is the gap the roll needs.
        assert_eq!(lines[2].text, "");
    }

    #[test]
    fn one_line_can_carry_several_stamps() {
        let lines = parse_lrc("[00:01.00][01:00.00]Chorus");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].time, Some(1.0));
        assert_eq!(lines[1].time, Some(60.0));
    }
}

/// `PMW_SCAN=/path cargo test local_order_sample -- --ignored --nocapture`
#[cfg(test)]
mod local_order_sample {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn local_order_sample() {
        let root = std::env::var("PMW_SCAN").expect("PMW_SCAN");
        let names = std::collections::BTreeMap::from([
            ("Jaychou".to_string(), "周杰伦".to_string()),
            ("JJ".to_string(), "林俊杰".to_string()),
        ]);
        let tracks = crate::local::scan(std::path::Path::new(&root));
        let http = reqwest::Client::new();
        let step = (tracks.len() / 12).max(1);
        for track in tracks.iter().step_by(step).take(12) {
            let song = Song {
                video_id: track.id.clone(),
                title: track.title.clone(),
                artist: crate::store::map_artist(&names, &track.artist),
                raw_artist: track.artist.clone(),
                album: track.album.clone(),
                song_duration: track.duration,
                image_src: None,
                url: None,
                media_type: Some("LOCAL".into()),
                ..Song::default()
            };
            let found = lookup_local(&http, &song).await;
            println!(
                "{} — {} | {}",
                song.title,
                song.artist,
                found.map_or("none".to_string(), |w| format!("{} synced={}", w.how, w.synced))
            );
        }
    }
}
