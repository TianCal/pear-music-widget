//! Synced lyrics from LRCLib, falling back to YouTube Music's own timed lyrics
//! — the two sources YouTube Music's `synced-lyrics` plugin leans on. The
//! api-server exposes no lyrics route, so the widget has to ask for them itself.
//!
//! **This is the only place the app talks to anything other than localhost.**
//! Keep it that way: the renderer's CSP allows no network at all, so anything
//! fetched has to come through here. Two hosts are reached, `lrclib.net` and
//! `music.youtube.com`, and nothing is sent to either beyond the track's title,
//! artist and video id.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::state::Song;

const ENDPOINT: &str = "https://lrclib.net/api";
const YTM_ENDPOINT: &str = "https://music.youtube.com/youtubei/v1";
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
    /// Which of the matching tiers found it; useful when a track comes back
    /// with someone else's words.
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

// ---------------------------------------------------------- YouTube Music

// YouTube Music carries timed lyrics for a great many tracks LRCLib has never
// heard of — smaller labels, and much of the Mandarin and Cantonese catalogue.
// Two calls, the same pair Pear Desktop's `synced-lyrics` plugin makes: `/next`
// names the lyrics tab for a video, `/browse` returns what is on it. Only the
// iOS Music client is served timings, hence the second client. Pear routes that
// call through a third-party proxy because its renderer is bound by CORS; we
// are not, so we ask YouTube directly and depend on nobody.
const YTM_WEB_CLIENT: (&str, &str) = ("WEB_REMIX", "1.20241202.01.00");
const YTM_LYRICS_CLIENT: (&str, &str) = ("26", "7.01.05");

fn ytm_body(key: &str, value: &str, client: (&str, &str)) -> Value {
    serde_json::json!({
        key: value,
        "context": { "client": { "clientName": client.0, "clientVersion": client.1 } },
    })
}

async fn ytm_post(http: &reqwest::Client, path: &str, body: Value) -> Option<Value> {
    let res = http
        .post(format!("{YTM_ENDPOINT}/{path}?prettyPrint=false"))
        .json(&body)
        .timeout(TIMEOUT)
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    res.json().await.ok()
}

/// `/next` lists the tabs shown beside the player; we want the lyrics one.
fn lyrics_browse_id(next: &Value) -> Option<&str> {
    let tabs = next
        .pointer(concat!(
            "/contents/singleColumnMusicWatchNextResultsRenderer",
            "/tabbedRenderer/watchNextTabbedResultsRenderer/tabs"
        ))?
        .as_array()?;

    tabs.iter().find_map(|tab| {
        let endpoint = tab.pointer("/tabRenderer/endpoint/browseEndpoint")?;
        let page_type = endpoint
            .pointer(concat!(
                "/browseEndpointContextSupportedConfigs",
                "/browseEndpointContextMusicConfig/pageType"
            ))
            .and_then(Value::as_str)?;
        if page_type != "MUSIC_PAGE_TYPE_TRACK_LYRICS" {
            return None;
        }
        endpoint.get("browseId").and_then(Value::as_str)
    })
}

/// Cue times come back as strings, but take a number too rather than drop a
/// whole track if that ever changes.
fn millis(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::String(text) => text.parse().ok(),
        other => other.as_f64(),
    }
}

/// Turn a `/browse` lyrics page into what the roll needs. When a track has no
/// lyrics the page holds a `musicMessageModel` apology instead, which falls
/// through every branch here and comes back as `None`.
pub fn shape_ytmusic(browse: &Value) -> Option<Lyrics> {
    // Absent on the older shape below, so this cannot be a hard requirement.
    let model = browse.pointer("/contents/elementRenderer/newElement/type/componentType/model");

    if let Some(timed) = model
        .and_then(|model| model.pointer("/timedLyricsModel/lyricsData/timedLyricsData"))
        .and_then(Value::as_array)
    {
        let mut lines: Vec<LyricLine> = timed
            .iter()
            .filter_map(|entry| {
                let start = millis(entry.pointer("/cueRange/startTimeMilliseconds"))?;
                // `♪` is YouTube's instrumental marker. Blank it so the roll
                // shows the gap breathing, the way LRCLib's empty lines do.
                let text = entry.get("lyricLine").and_then(Value::as_str)?.trim();
                Some(LyricLine {
                    time: Some(start / 1000.0),
                    text: if text == "♪" { "" } else { text }.to_string(),
                })
            })
            .collect();

        if !lines.is_empty() {
            // Without this the roll sits highlighting the opening line through
            // the whole intro.
            if lines[0].time.unwrap_or(0.0) > 0.3 {
                lines.insert(
                    0,
                    LyricLine {
                        time: Some(0.0),
                        text: String::new(),
                    },
                );
            }
            return Some(Lyrics {
                synced: true,
                lines,
                how: "ytmusic",
            });
        }
    }

    // Older, unsynced shape: one description shelf holding the whole song.
    let runs = model
        .and_then(|model| model.pointer("/lyricsModel/lyrics/runs"))
        .or_else(|| {
            browse.pointer(concat!(
                "/contents/sectionListRenderer/contents/0",
                "/musicDescriptionShelfRenderer/description/runs"
            ))
        })
        .and_then(Value::as_array)?;

    let plain: String = runs
        .iter()
        .filter_map(|run| run.get("text").and_then(Value::as_str))
        .collect();
    let lines: Vec<LyricLine> = plain
        .split('\n')
        .map(|text| LyricLine {
            time: None,
            text: text.trim().to_string(),
        })
        .collect();

    lines.iter().any(|line| !line.text.is_empty()).then(|| Lyrics {
        synced: false,
        lines,
        how: "ytmusic",
    })
}

/// Keyed by video id, so unlike a free-text search this can never come back
/// with a different song's words.
async fn fetch_ytmusic(http: &reqwest::Client, video_id: &str) -> Option<Lyrics> {
    let next = ytm_post(
        http,
        "next",
        ytm_body("videoId", video_id, YTM_WEB_CLIENT),
    )
    .await?;
    let browse_id = lyrics_browse_id(&next)?.to_string();
    let browse = ytm_post(
        http,
        "browse",
        ytm_body("browseId", &browse_id, YTM_LYRICS_CLIENT),
    )
    .await?;
    shape_ytmusic(&browse)
}

// ------------------------------------------------------------------ lookup

/// Four-tier match. The first three ask LRCLib, which needs help because
/// YouTube Music titles carry soundtrack credits and 《…》 wrappers it will not
/// match on: exact with everything we know, exact on a cleaned title, then
/// free-text search picking the hit whose duration is closest. Whatever that
/// leaves — smaller labels, most of the Chinese-language catalogue — YouTube
/// Music is asked for directly.
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

    // 4. YouTube Music's own timed lyrics.
    if result.is_none() {
        result = fetch_ytmusic(http, &song.video_id).await;
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

    fn timed(lines: Value) -> Value {
        serde_json::json!({
            "contents": { "elementRenderer": { "newElement": { "type": { "componentType": {
                "model": { "timedLyricsModel": { "lyricsData": { "timedLyricsData": lines } } }
            } } } } }
        })
    }

    #[test]
    fn reads_the_lyrics_tab_out_of_next() {
        let next = serde_json::json!({ "contents": {
            "singleColumnMusicWatchNextResultsRenderer": { "tabbedRenderer": {
                "watchNextTabbedResultsRenderer": { "tabs": [
                    { "tabRenderer": { "endpoint": { "browseEndpoint": {
                        "browseId": "MPTRxyz",
                        "browseEndpointContextSupportedConfigs": {
                            "browseEndpointContextMusicConfig": {
                                "pageType": "MUSIC_PAGE_TYPE_TRACK_RELATED" } } } } } },
                    { "tabRenderer": { "endpoint": { "browseEndpoint": {
                        "browseId": "MPLYxyz",
                        "browseEndpointContextSupportedConfigs": {
                            "browseEndpointContextMusicConfig": {
                                "pageType": "MUSIC_PAGE_TYPE_TRACK_LYRICS" } } } } } },
                ] } } } } });
        assert_eq!(lyrics_browse_id(&next), Some("MPLYxyz"));
        assert_eq!(lyrics_browse_id(&serde_json::json!({})), None);
    }

    #[test]
    fn shapes_ytmusic_cue_ranges() {
        let lyrics = shape_ytmusic(&timed(serde_json::json!([
            { "lyricLine": " Opening ", "cueRange": {
                "startTimeMilliseconds": "8200", "endTimeMilliseconds": "11000" } },
            { "lyricLine": "♪", "cueRange": {
                "startTimeMilliseconds": "11000", "endTimeMilliseconds": "14000" } },
        ])))
        .expect("synced lyrics");

        assert!(lyrics.synced);
        assert_eq!(lyrics.how, "ytmusic");
        // A held-open first line, so the roll does not sit on the opening lyric
        // through the intro.
        assert_eq!(lyrics.lines[0].time, Some(0.0));
        assert_eq!(lyrics.lines[0].text, "");
        assert_eq!(lyrics.lines[1].time, Some(8.2));
        assert_eq!(lyrics.lines[1].text, "Opening");
        // The instrumental marker becomes a gap, not a symbol.
        assert_eq!(lyrics.lines[2].text, "");
    }

    #[test]
    fn no_padding_when_the_first_line_is_already_at_the_top() {
        let lyrics = shape_ytmusic(&timed(serde_json::json!([
            { "lyricLine": "Straight in", "cueRange": {
                "startTimeMilliseconds": "0", "endTimeMilliseconds": "2000" } },
        ])))
        .expect("synced lyrics");
        assert_eq!(lyrics.lines.len(), 1);
    }

    #[test]
    fn falls_back_to_the_unsynced_description_shelf() {
        // No `elementRenderer` at all on this shape, so it has to be reachable
        // without one.
        let browse = serde_json::json!({ "contents": { "sectionListRenderer": { "contents": [
            { "musicDescriptionShelfRenderer": { "description": { "runs": [
                { "text": "First line\nSecond line" },
            ] } } },
        ] } } });

        let lyrics = shape_ytmusic(&browse).expect("plain lyrics");
        assert!(!lyrics.synced);
        assert_eq!(lyrics.lines.len(), 2);
        assert_eq!(lyrics.lines[0].time, None);
        assert_eq!(lyrics.lines[1].text, "Second line");
    }

    #[test]
    fn an_apology_page_is_not_lyrics() {
        // What YouTube returns for a track it has no lyrics for.
        let browse = serde_json::json!({
            "contents": { "elementRenderer": { "newElement": { "type": { "componentType": {
                "model": { "musicMessageModel": { "text": "Lyrics not available at this time." } }
            } } } } }
        });
        assert_eq!(shape_ytmusic(&browse), None);
        assert_eq!(shape_ytmusic(&timed(serde_json::json!([]))), None);
    }

    #[test]
    fn takes_the_lead_artist_only() {
        assert_eq!(lead_artist("A feat. B"), "A");
        assert_eq!(lead_artist("A、B"), "A");
        assert_eq!(lead_artist("A & B"), "A");
        assert_eq!(lead_artist("Solo"), "Solo");
    }
}

