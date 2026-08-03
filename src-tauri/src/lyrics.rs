//! Synced lyrics from LRCLib — the same source YouTube Music's own
//! `synced-lyrics` plugin uses. The api-server exposes no lyrics route, so the
//! widget has to ask for them itself.
//!
//! **This is the only place the app talks to anything other than localhost.**
//! Keep it that way: the renderer's CSP allows no network at all, so anything
//! fetched has to come through here.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::state::Song;

const ENDPOINT: &str = "https://lrclib.net/api";
pub const USER_AGENT: &str = "pear-music-widget (https://github.com/TianCal/pear-music-widget)";
const TIMEOUT: Duration = Duration::from_secs(9);
const CACHE_MAX: usize = 60;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct LyricLine {
    /// `None` on an unsynced block — those do not follow along.
    pub time: Option<f64>,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Lyrics {
    pub synced: bool,
    pub lines: Vec<LyricLine>,
    /// Which of the three matching tiers found it; useful when a track comes
    /// back with someone else's words.
    pub how: &'static str,
}

/// Cache misses too (`None`), so a track with no lyrics is not looked up again
/// every time the panel is reopened.
static CACHE: LazyLock<Mutex<(Vec<String>, HashMap<String, Option<Lyrics>>)>> =
    LazyLock::new(|| Mutex::new((Vec::new(), HashMap::new())));

static CLEAN_PATTERNS: LazyLock<[Regex; 5]> = LazyLock::new(|| {
    [
        Regex::new(r"\([^)]*\)").unwrap(),
        Regex::new(r"\[[^\]]*\]").unwrap(),
        Regex::new(r"《[^》]*》").unwrap(),
        Regex::new(r"【[^】]*】").unwrap(),
        Regex::new(r"(?i)\b(official|music\s+video|lyrics?|audio|m/?v|hd|4k)\b").unwrap(),
    ]
});
static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static STAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(\d+):(\d{1,2}(?:[.:]\d{1,3})?)\]").unwrap());
static BRACKETED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[^\]]*\]").unwrap());
static ARTIST_SPLIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)、|,|&|\band\b|feat\.?|ft\.?|with").unwrap());

/// YouTube Music titles carry a lot that LRCLib will not match on — bracketed
/// soundtrack credits, 《…》 wrappers, "Official Music Video". Strip them for the
/// fallback search, but try the untouched title first: for plenty of tracks the
/// full string is the exact match.
pub fn clean_title(title: &str) -> String {
    let mut stripped = title.to_string();
    for pattern in CLEAN_PATTERNS.iter() {
        stripped = pattern.replace_all(&stripped, " ").into_owned();
    }
    let stripped = WHITESPACE.replace_all(&stripped, " ").trim().to_string();
    if stripped.is_empty() {
        title.to_string()
    } else {
        stripped
    }
}

/// Collaborations are listed several ways; LRCLib matches on the lead artist.
pub fn lead_artist(artist: &str) -> String {
    let head = ARTIST_SPLIT.split(artist).next().unwrap_or("");
    WHITESPACE.replace_all(head, " ").trim().to_string()
}

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

/// Turn an LRCLib record into what the roll needs, preferring synced words.
fn shape(record: Option<&Value>, how: &'static str) -> Option<Lyrics> {
    let record = record?;

    if let Some(synced) = record.get("syncedLyrics").and_then(Value::as_str) {
        let lines = parse_lrc(synced);
        if !lines.is_empty() {
            return Some(Lyrics {
                synced: true,
                lines,
                how,
            });
        }
    }

    if let Some(plain) = record.get("plainLyrics").and_then(Value::as_str) {
        let lines: Vec<LyricLine> = plain
            .split('\n')
            .map(|text| LyricLine {
                time: None,
                text: text.trim().to_string(),
            })
            .collect();
        if lines.iter().any(|line| !line.text.is_empty()) {
            return Some(Lyrics {
                synced: false,
                lines,
                how,
            });
        }
    }

    None
}

/// Prefer a search hit whose duration is closest to the track we are playing.
fn best_search_hit(hits: &Value, duration: f64) -> Option<&Value> {
    let hits = hits.as_array()?;
    let has = |hit: &&Value, key: &str| hit.get(key).and_then(Value::as_str).is_some();

    let synced: Vec<&Value> = hits.iter().filter(|hit| has(hit, "syncedLyrics")).collect();
    let pool = if synced.is_empty() {
        hits.iter()
            .filter(|hit| has(hit, "plainLyrics"))
            .collect::<Vec<_>>()
    } else {
        synced
    };

    if duration <= 0.0 {
        return pool.into_iter().next();
    }

    let delta = |hit: &Value| {
        (hit.get("duration").and_then(Value::as_f64).unwrap_or(0.0) - duration).abs()
    };
    pool.into_iter().reduce(|best, hit| {
        if delta(hit) < delta(best) {
            hit
        } else {
            best
        }
    })
}

fn query_string(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{key}={}", urlencode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

async fn request(http: &reqwest::Client, path: &str) -> Option<Value> {
    let res = http
        .get(format!("{ENDPOINT}{path}"))
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .timeout(TIMEOUT)
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    res.json().await.ok()
}

/// Three-tier match, because YouTube Music titles carry soundtrack credits and
/// 《…》 wrappers that LRCLib will not match on: exact with everything we know,
/// exact on a cleaned title, then free-text search picking the hit whose
/// duration is closest.
pub async fn fetch_lyrics(http: &reqwest::Client, song: &Song) -> Option<Lyrics> {
    if song.video_id.is_empty() {
        return None;
    }
    if let Some(hit) = CACHE
        .lock()
        .expect("lyrics cache")
        .1
        .get(&song.video_id)
        .cloned()
    {
        return hit;
    }

    let title = song.title.clone();
    let artist = lead_artist(&song.artist);
    let duration = song.song_duration.round();

    let mut result = None;

    // 1. Exact, with everything we know.
    if !title.is_empty() && !artist.is_empty() {
        let mut params = vec![
            ("track_name", title.clone()),
            ("artist_name", artist.clone()),
        ];
        if !song.album.is_empty() {
            params.push(("album_name", song.album.clone()));
        }
        if duration > 0.0 {
            params.push(("duration", format!("{duration:.0}")));
        }
        let payload = request(http, &format!("/get?{}", query_string(&params))).await;
        result = shape(payload.as_ref(), "exact");
    }

    // 2. Exact again on a cleaned title, without album or duration to pin it.
    let simple = clean_title(&title);
    if result.is_none() && !simple.is_empty() && simple != title && !artist.is_empty() {
        let params = [
            ("track_name", simple.clone()),
            ("artist_name", artist.clone()),
        ];
        let payload = request(http, &format!("/get?{}", query_string(&params))).await;
        result = shape(payload.as_ref(), "cleaned");
    }

    // 3. Free-text search — this is what rescues soundtrack and 《…》 titles.
    if result.is_none() && !simple.is_empty() {
        let q = format!("{simple} {artist}").trim().to_string();
        let payload = request(http, &format!("/search?{}", query_string(&[("q", q)]))).await;
        result = payload
            .as_ref()
            .and_then(|hits| best_search_hit(hits, duration))
            .and_then(|hit| shape(Some(hit), "search"));
    }

    let mut cache = CACHE.lock().expect("lyrics cache");
    let (order, entries) = &mut *cache;
    if entries.insert(song.video_id.clone(), result.clone()).is_none() {
        order.push(song.video_id.clone());
    }
    while order.len() > CACHE_MAX {
        let oldest = order.remove(0);
        entries.remove(&oldest);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn strips_what_lrclib_will_not_match_on() {
        assert_eq!(clean_title("Song (Official Music Video)"), "Song");
        assert_eq!(clean_title("《Theme》 Track [4K]"), "Track");
        // Stripping everything leaves the original rather than an empty query.
        assert_eq!(clean_title("(Official)"), "(Official)");
    }

    #[test]
    fn takes_the_lead_artist_only() {
        assert_eq!(lead_artist("A feat. B"), "A");
        assert_eq!(lead_artist("A、B"), "A");
        assert_eq!(lead_artist("A & B"), "A");
        assert_eq!(lead_artist("Solo"), "Solo");
    }
}
