//! Synced lyrics from YouTube Music, LRCLib, and Kugeci. The
//! api-server exposes no lyrics route, so the widget has to ask for them itself.
//!
//! **This and `artwork.rs` are the only places the app talks to anything other
//! than localhost.** Keep it that way: the renderer's CSP allows no network at
//! all, so anything fetched has to come through Rust. Hosts are `lrclib.net`,
//! `music.youtube.com`, and `www.kugeci.com`; requests use the track's title,
//! artist and video id.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::lyrics_cache;
use crate::state::Song;

const ENDPOINT: &str = "https://lrclib.net/api";
const YTM_ENDPOINT: &str = "https://music.youtube.com/youtubei/v1";
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
/// Separators are whole words, but against ASCII letters only (`(?-u:\b)`): a
/// Han character next to "feat." is a boundary, so "陈奕迅feat.王菲" splits.
static ARTIST_SPLIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)、|,|&|(?-u:\b)(?:and|with|feat|ft)(?-u:\b)\.?").unwrap()
});

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

/// Turn an LRCLib record into what the roll needs, preferring synced words.
fn shape(record: Option<&Value>, how: How) -> Option<Lyrics> {
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
                jyutping: None,
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

    let delta =
        |hit: &Value| (hit.get("duration").and_then(Value::as_f64).unwrap_or(0.0) - duration).abs();
    pool.into_iter().reduce(
        |best, hit| {
            if delta(hit) < delta(best) {
                hit
            } else {
                best
            }
        },
    )
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
                    jyutping: None,
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
                        jyutping: None,
                    },
                );
            }
            return Some(Lyrics {
                synced: true,
                lines,
                how: How::Ytmusic,
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
            jyutping: None,
        })
        .collect();

    lines
        .iter()
        .any(|line| !line.text.is_empty())
        .then(|| Lyrics {
            synced: false,
            lines,
            how: How::Ytmusic,
        })
}

/// Keyed by video id, so unlike a free-text search this can never come back
/// with a different song's words.
async fn fetch_ytmusic(http: &reqwest::Client, video_id: &str) -> Option<Lyrics> {
    let next = ytm_post(http, "next", ytm_body("videoId", video_id, YTM_WEB_CLIENT)).await?;
    let browse_id = lyrics_browse_id(&next)?.to_string();
    let browse = ytm_post(
        http,
        "browse",
        ytm_body("browseId", &browse_id, YTM_LYRICS_CLIENT),
    )
    .await?;
    shape_ytmusic(&browse)
}

// --------------------------------------------------------------- Kugeci

const KUGECI_ENDPOINT: &str = "https://www.kugeci.com";
const KUGECI_MAX_BYTES: usize = 1024 * 1024;

// Strip promotional labels only. Keeping Live / remix / acoustic qualifiers
// prevents a studio recording from borrowing timings from a different version.
static KUGECI_PROMO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:official(?:\s+music)?\s+video|official\s+audio|official\s+lyrics?|music\s+video|lyrics?\s+video|official|audio|lyrics?|m/?v|hd|4k)\b").unwrap()
});

// Empty brackets left by promotional labels are immaterial to matching.
static KUGECI_EMPTY_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(\s*\)|\[\s*\]|【\s*】").unwrap());

fn kugeci_title(title: &str) -> String {
    let title = KUGECI_PROMO.replace_all(title, " ");
    let title = KUGECI_EMPTY_BRACKETS.replace_all(&title, " ");
    WHITESPACE.replace_all(&title, " ").trim().to_string()
}

static KUGECI_SUBTITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*?)\s*[（(]([^()（）]+)[）)]\s*$").unwrap());
static KUGECI_VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:live|remix|mix|acoustic|instrumental|version|ver|cover|karaoke|edit|demo|part|pt|mono|stereo|remaster(?:ed)?)\b|现场|現場|翻唱|伴奏|混音|版|录音|錄音").unwrap()
});

/// A lyric hook is sometimes appended to a song title. Try the literal title
/// first, then omit a trailing subtitle; recording-version labels stay intact.
fn kugeci_titles(title: &str) -> Vec<String> {
    let full = kugeci_title(title);
    let mut titles = vec![full.clone()];
    if let Some(parts) = KUGECI_SUBTITLE.captures(&full) {
        let base = parts[1].trim();
        if !base.is_empty() && !KUGECI_VERSION.is_match(&parts[2]) {
            titles.push(base.to_string());
        }
    }
    titles
}

/// The keys a site title may match ours under. Computed once per search:
/// every row of the results is checked against the same ones.
fn kugeci_title_keys(title: &str) -> Vec<String> {
    kugeci_titles(title).iter().map(|title| kugeci_key(title)).collect()
}

fn kugeci_title_matches(candidate: &str, title: &str) -> bool {
    kugeci_title_matches_keys(candidate, &kugeci_title_keys(title))
}

fn kugeci_title_matches_keys(candidate: &str, wanted: &[String]) -> bool {
    let candidate = kugeci_key(&kugeci_title(candidate));
    !candidate.is_empty() && wanted.iter().any(|title| *title == candidate)
}

/// `artwork::fold`, plus 迴→回, which the site's titles use interchangeably.
fn kugeci_key(text: &str) -> String {
    crate::artwork::fold(text).replace('迴', "回")
}

static KUGECI_ARTIST_SPLIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)、|,|，|/|&|(?-u:\b)(?:and|with|feat|ft)(?-u:\b)\.?|和|与|與|及").unwrap()
});

fn kugeci_artist_key(artist: &str) -> String {
    let key = kugeci_key(artist);
    // Verified provider alias, not a general prefix/suffix fuzzy match.
    match key.as_str() {
        "华云龙kle" => "华云龙".into(),
        _ => key,
    }
}

/// Match literal artist names first, then segment collaborations using the
/// site's actual names. Splitting blindly on “和” breaks names like 楊和蘇.
fn kugeci_artists_match(credits: &[String], artist: &str) -> bool {
    let keys: Vec<_> = credits
        .iter()
        .map(|credit| kugeci_artist_key(credit))
        .collect();
    let artist_key = kugeci_artist_key(artist);
    if artist_key.is_empty() {
        return false;
    }
    if keys.contains(&artist_key) {
        return true;
    }
    if !KUGECI_ARTIST_SPLIT.is_match(artist) && !artist.chars().any(char::is_whitespace) {
        return false;
    }
    let names: Vec<&str> = keys
        .iter()
        .filter(|key| !key.is_empty())
        .map(String::as_str)
        .collect();
    if names.is_empty() {
        return false;
    }
    // Apply the one verified alias inside a collaboration too.
    let artist_key = artist_key.replace("华云龙kle", "华云龙");
    // The credit must be two or more of the site's names in a row, each
    // after the first optionally introduced by a joiner, and nothing else.
    // Tried every way, as the regex it replaces did, so one name being a
    // prefix of another cannot hide a match; the strings are a few words.
    fn spelled(rest: &str, names: &[&str], count: usize) -> bool {
        const JOINERS: [&str; 8] = ["featuring", "feat", "with", "and", "ft", "和", "与", "及"];
        if rest.is_empty() {
            return count >= 2;
        }
        let after_joiner = (count >= 1)
            .then(|| JOINERS.iter().filter_map(|joiner| rest.strip_prefix(joiner)))
            .into_iter()
            .flatten();
        std::iter::once(rest).chain(after_joiner).any(|from| {
            names
                .iter()
                .filter_map(|name| from.strip_prefix(name))
                .any(|left| spelled(left, names, count + 1))
        })
    }
    spelled(&artist_key, &names, 0)
}

/// The page's selectors, parsed once: a search page is walked row by row
/// and cell by cell, and parsing a selector for each was most of the walk.
struct Selectors {
    rows: scraper::Selector,
    cells: scraper::Selector,
    links: scraper::Selector,
    heading: scraper::Selector,
    credits: scraper::Selector,
    lyrics: scraper::Selector,
}

static SELECT: LazyLock<Selectors> = LazyLock::new(|| {
    let select = |selector| scraper::Selector::parse(selector).expect("static lyrics selector");
    Selectors {
        rows: select("#tablesort tbody tr"),
        cells: select("td"),
        links: select("a"),
        heading: select("main h1"),
        credits: select(".song-details-container a"),
        lyrics: select("#lyricsContainer"),
    }
});

fn kugeci_song_path(href: &str) -> Option<String> {
    let path = href.strip_prefix(KUGECI_ENDPOINT).unwrap_or(href);
    let id = path.strip_prefix("/song/")?;
    (!id.is_empty() && id.bytes().all(|ch| ch.is_ascii_alphanumeric())).then(|| path.to_string())
}

/// Search matches the entire query, so use the title alone, then verify the
/// artist in the same row. Footer recommendations must never become candidates.
fn kugeci_candidates(html: &str, title: &str, artist: &str) -> Vec<String> {
    let document = scraper::Html::parse_document(html);
    let title_key = kugeci_key(&kugeci_title(title));
    let artist_key = kugeci_key(artist);
    if title_key.is_empty() || artist_key.is_empty() {
        return Vec::new();
    }
    let wanted = kugeci_title_keys(title);
    let mut paths = Vec::new();
    for row in document.select(&SELECT.rows) {
        let cells: Vec<_> = row.select(&SELECT.cells).collect();
        let Some(link) = cells
            .get(1)
            .and_then(|cell| cell.select(&SELECT.links).next())
        else {
            continue;
        };
        let name = link.text().collect::<String>();
        if !kugeci_title_matches_keys(&name, &wanted) {
            continue;
        }
        let matches_artist = cells.get(2).is_some_and(|cell| {
            let credits = cell
                .select(&SELECT.links)
                .map(|link| link.text().collect::<String>())
                .collect::<Vec<_>>();
            kugeci_artists_match(&credits, artist)
        });
        if matches_artist {
            if let Some(path) = link.value().attr("href").and_then(kugeci_song_path) {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

fn shape_kugeci(html: &str, title: &str, artist: &str, duration: f64) -> Option<Lyrics> {
    let document = scraper::Html::parse_document(html);
    let name = document
        .select(&SELECT.heading)
        .next()?
        .text()
        .collect::<String>();
    if !kugeci_title_matches(&name, title) {
        return None;
    }
    let credits: Vec<_> = document
        .select(&SELECT.credits)
        .filter(|link| {
            link.value().attr("href").is_some_and(|href| {
                href.strip_prefix(KUGECI_ENDPOINT)
                    .unwrap_or(href)
                    .starts_with("/singer/")
            })
        })
        .map(|link| {
            let label = link.text().collect::<String>();
            label
                .trim()
                .strip_prefix("演唱：")
                .unwrap_or(label.trim())
                .trim()
                .to_string()
        })
        .collect();
    if !kugeci_artists_match(&credits, artist) {
        return None;
    }
    let container = document.select(&SELECT.lyrics).next()?;
    let mut lrc = String::new();
    for node in container.descendants() {
        match node.value() {
            scraper::Node::Text(text) => lrc.push_str(text),
            scraper::Node::Element(element) if element.name() == "br" => lrc.push('\n'),
            _ => {}
        }
    }
    let lines = parse_lrc(&lrc);
    if !lines.iter().any(|line| !line.text.is_empty()) {
        return None;
    }
    // The site provides no recording duration. Reject obvious mismatches, but
    // allow an outro after the last cue; cue time is not track duration.
    if duration > 0.0 && lines.last()?.time? > duration + 10.0 {
        return None;
    }
    Some(Lyrics {
        synced: true,
        lines,
        how: How::Kugeci,
    })
}

async fn kugeci_page(http: &reqwest::Client, path: &str) -> Option<String> {
    let mut response = http
        .get(format!("{KUGECI_ENDPOINT}{path}"))
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .timeout(TIMEOUT)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > KUGECI_MAX_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).ok()
}

static KUGECI_VIDEO_CREDIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*([^:：]+?)\s*[:：]\s*[「『]([^」』]+)[」』](.*?)[【\[(（]\s*official\s+(?:mv|music\s+video)\s*[】\])）]\s*$").unwrap()
});

static KUGECI_PLAIN_MV: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s*([^:：]+?)\s*[:：]\s*(.+?)[【\[]\s*official\s+(?:mv|music\s+video)\s*[】\]](.*)$",
    )
    .unwrap()
});
static KUGECI_LYRIC_VIDEO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(.+?)\s+-\s+([^『「【]+?)(?:[『「]([^』」]*)[』」])?[【\[](?:動態歌詞|动态歌词|動態歌詞字幕|动态歌词字幕)(?:\s*/?\s*lyrics)?[】\]]\s*$").unwrap()
});
// Some uploaders supply explicit performer credits without a video marker.
// Keep this form unadorned: annotated videos must use the stricter parsers.
static KUGECI_PLAIN_CREDIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*([^\[\]【】『』「」()（）]+?)\s+-\s+([^\[\]【】『』「」()（）]+?)\s*$")
        .unwrap()
});
static KUGECI_CCTV_CREDIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^歌曲Top\d+《([^》]+)》\s*(.+?)\s*【\d{4}年央视春晚】(?:｜订阅CCTV春晚)?\s*$")
        .unwrap()
});

fn kugeci_video_credit(title: &str) -> Option<(String, String)> {
    if let Some(parts) = KUGECI_VIDEO_CREDIT.captures(title) {
        if !parts[3].chars().any(|ch| ch.is_alphanumeric()) {
            return Some((parts[2].trim().into(), parts[1].trim().into()));
        }
    }
    if let Some(parts) = KUGECI_PLAIN_MV.captures(title) {
        if parts[2].contains(['「', '『', '」', '』']) {
            return None;
        }
        // Keep title version labels, and reject version/reaction annotations
        // appended after the official marker. Sponsor text is not a credit.
        if !KUGECI_VERSION.is_match(&parts[3]) && !parts[3].to_lowercase().contains("reaction") {
            let title = parts[2].trim_matches(|ch: char| {
                !ch.is_alphanumeric() && !matches!(ch, '(' | ')' | '（' | '）')
            });
            let artist = parts[1].trim_matches(|ch: char| !ch.is_alphanumeric());
            if !title.is_empty() && !artist.is_empty() {
                return Some((title.into(), artist.into()));
            }
        }
    }
    if let Some(parts) = KUGECI_LYRIC_VIDEO.captures(title) {
        if parts
            .get(3)
            .is_some_and(|annotation| KUGECI_VERSION.is_match(annotation.as_str()))
        {
            return None;
        }
        return Some((parts[2].trim().into(), parts[1].trim().into()));
    }
    if let Some(parts) = KUGECI_PLAIN_CREDIT.captures(title) {
        return Some((parts[2].trim().into(), parts[1].trim().into()));
    }
    if let Some(parts) = KUGECI_CCTV_CREDIT.captures(title) {
        return Some((parts[1].trim().into(), parts[2].trim().into()));
    }
    None
}

async fn fetch_kugeci_for_song(http: &reqwest::Client, song: &Song) -> Option<Lyrics> {
    tokio::time::timeout(TIMEOUT, async {
        if let Some(words) = fetch_kugeci(http, &song.title, &song.artist, song.song_duration).await
        {
            return Some(words);
        }
        // Music/lyric videos and CCTV clips can report publishers as artists.
        // Use only recognized explicit title/performer credit formats, and
        // verify both fields against the search row and fetched song page.
        let (title, artist) = kugeci_video_credit(&song.title)?;
        fetch_kugeci(http, &title, &artist, song.song_duration).await
    })
    .await
    .ok()
    .flatten()
}

async fn fetch_kugeci(
    http: &reqwest::Client,
    title: &str,
    artist: &str,
    duration: f64,
) -> Option<Lyrics> {
    if kugeci_key(title).is_empty() || kugeci_key(artist).is_empty() {
        return None;
    }
    // Bound the whole fallback, including the search and up to two duplicate
    // recordings, so an unavailable site cannot add several request timeouts.
    tokio::time::timeout(TIMEOUT, async {
        let mut queries = Vec::new();
        for title in kugeci_titles(title) {
            let query = title
                .trim_matches(|ch: char| !ch.is_alphanumeric())
                .to_string();
            if query.is_empty() {
                continue;
            }
            for query in [query.clone(), fast2s::convert(&query).replace('迴', "回")] {
                if !queries.contains(&query) {
                    queries.push(query);
                }
            }
        }
        let mut paths = Vec::new();
        for query in queries {
            if let Some(html) = kugeci_page(http, &format!("/search?q={}", urlencode(&query))).await
            {
                paths = kugeci_candidates(&html, title, artist);
                if !paths.is_empty() {
                    break;
                }
            }
        }
        for path in paths.into_iter().take(2) {
            if let Some(html) = kugeci_page(http, &path).await {
                if let Some(words) = shape_kugeci(&html, title, artist, duration) {
                    return Some(words);
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
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

/// LRCLib exact match with everything known; LRCLib checks the duration.
async fn lrclib_exact(
    http: &reqwest::Client,
    title: &str,
    artist: &str,
    album: &str,
    duration: f64,
) -> Option<Lyrics> {
    let mut params = vec![("track_name", title.to_string()), ("artist_name", artist.to_string())];
    if !album.is_empty() {
        params.push(("album_name", album.to_string()));
    }
    if duration > 0.0 {
        params.push(("duration", format!("{duration:.0}")));
    }
    let payload = request(http, &format!("/get?{}", query_string(&params))).await;
    shape(payload.as_ref(), How::Exact)
}

/// LRCLib exact match on a cleaned title, without album or duration to pin it.
async fn lrclib_cleaned(http: &reqwest::Client, title: &str, artist: &str) -> Option<Lyrics> {
    let params = [("track_name", title.to_string()), ("artist_name", artist.to_string())];
    let payload = request(http, &format!("/get?{}", query_string(&params))).await;
    shape(payload.as_ref(), How::Cleaned)
}

/// LRCLib free-text search, ranked by duration. With `slack`, a best hit
/// further than that from `duration` is rejected.
async fn lrclib_search(
    http: &reqwest::Client,
    title: &str,
    artist: &str,
    duration: f64,
    slack: Option<f64>,
) -> Option<Lyrics> {
    let q = format!("{title} {artist}").trim().to_string();
    let payload = request(http, &format!("/search?{}", query_string(&[("q", q)]))).await;
    let hit = payload
        .as_ref()
        .and_then(|hits| best_search_hit(hits, duration))
        .filter(|hit| slack.is_none_or(|slack| within_duration(hit, duration, slack)))?;
    shape(Some(hit), How::Search)
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

/// An LRCLib hit within `LOCAL_DURATION_SLACK` of the file, or any hit when
/// either duration is unknown.
fn within_duration(hit: &Value, duration: f64, slack: f64) -> bool {
    match hit.get("duration").and_then(Value::as_f64) {
        Some(theirs) if duration > 0.0 && theirs > 0.0 => (theirs - duration).abs() <= slack,
        _ => true,
    }
}

/// Han characters: the catalogue Kugeci covers.
fn has_cjk(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F)
    })
}

/// `3:45` or `1:02:03` at the end of a YouTube Music result's subtitle
/// (`Artist • Album • 3:45`), in seconds.
fn subtitle_duration(subtitle: &str) -> Option<f64> {
    let last = subtitle.rsplit('•').next()?.trim();
    let mut seconds = 0.0;
    let mut parts = 0;
    for part in last.split(':') {
        let value: u32 = part.parse().ok()?;
        seconds = seconds * 60.0 + f64::from(value);
        parts += 1;
    }
    (parts >= 2).then_some(seconds)
}

/// YouTube Music's search filter for songs (not videos, albums or artists).
const YTM_SONGS_FILTER: &str = "EgWKAQIIAWoKEAkQBRAKEAMQBA==";

/// Find the YouTube Music song a local file is a copy of: same artist, same
/// title (script and notes folded), duration within the slack. `None` rather
/// than a near miss — the words that come back are timed to this recording.
async fn find_ytmusic_video(
    http: &reqwest::Client,
    title: &str,
    artist: &str,
    duration: f64,
) -> Option<String> {
    let mut body = ytm_body("query", &format!("{title} {}", lead_artist(artist)), YTM_WEB_CLIENT);
    body["params"] = Value::String(YTM_SONGS_FILTER.into());
    let payload = ytm_post(http, "search", body).await?;
    crate::search::parse_search_results(&payload)
        .into_iter()
        .filter(|hit| crate::artwork::artist_matches(artist, &hit.subtitle))
        .filter(|hit| match subtitle_duration(&hit.subtitle) {
            Some(theirs) if duration > 0.0 => (theirs - duration).abs() <= LOCAL_DURATION_SLACK,
            _ => duration <= 0.0,
        })
        .map(|hit| (crate::artwork::name_score(title, &hit.title), hit))
        .filter(|(score, _)| *score >= 2)
        .max_by_key(|(score, _)| *score)
        .map(|(_, hit)| hit.video_id)
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

    #[test]
    fn kugeci_is_for_han_text_only() {
        assert!(has_cjk("不懂"));
        assert!(has_cjk("凍結"));
        assert!(!has_cjk("Always Online"));
        assert!(!has_cjk("Jay"));
    }

    #[test]
    fn subtitle_durations_parse() {
        assert_eq!(subtitle_duration("林俊傑 • 樂行者 • 4:32"), Some(272.0));
        assert_eq!(subtitle_duration("A • B • 1:02:03"), Some(3723.0));
        assert_eq!(subtitle_duration("林俊傑 • 樂行者"), None);
    }

    #[test]
    fn search_hits_far_from_the_file_are_rejected() {
        let near = serde_json::json!({ "duration": 271.0 });
        let far = serde_json::json!({ "duration": 300.0 });
        let unknown = serde_json::json!({});
        assert!(within_duration(&near, 272.0, LOCAL_DURATION_SLACK));
        assert!(!within_duration(&far, 272.0, LOCAL_DURATION_SLACK));
        assert!(within_duration(&unknown, 272.0, LOCAL_DURATION_SLACK));
        assert!(within_duration(&far, 0.0, LOCAL_DURATION_SLACK));
    }

    /// `cargo test local_lyrics_live -- --ignored --nocapture` — hits the network.
    #[tokio::test]
    #[ignore]
    async fn local_lyrics_live() {
        let http = reqwest::Client::new();
        let id = find_ytmusic_video(&http, "不懂", "林俊杰", 272.0).await;
        println!("ytmusic id for 不懂: {id:?}");
        assert!(id.is_some());
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

    // Synthetic text with the site's actual markup; no copyrighted lyric fixture.
    const KUGECI_SONG: &str = r#"<main><h1> 愛情 </h1>
        <div class="song-details-container"><a href="/writer/x">作词：Someone</a>
        <a href="/singer/a">演唱： Singer &amp; Co</a></div>
        <div id="lyricsContainer">[00:01.20]First &amp; second<br>[00:02.00]<br />[00:03.40]<span>Last</span></div>
        <div id="txt">Duplicated plain lyrics</div></main>"#;

    #[test]
    fn kugeci_parses_entities_breaks_and_instrumental_gaps() {
        let lyrics = shape_kugeci(
            KUGECI_SONG,
            "爱情 (Official Music Video)",
            "Singer & Co",
            4.0,
        )
        .unwrap();
        assert!(lyrics.synced);
        assert_eq!(lyrics.how, How::Kugeci);
        assert_eq!(lyrics.lines.len(), 3);
        assert_eq!(lyrics.lines[0].text, "First & second");
        assert_eq!(lyrics.lines[0].time, Some(1.2));
        assert_eq!(lyrics.lines[1].text, "");
        assert_eq!(lyrics.lines[2].text, "Last");
    }

    #[test]
    fn kugeci_rejects_different_recordings_and_missing_markup() {
        assert!(shape_kugeci(KUGECI_SONG, "爱情 (Live)", "Singer & Co", 4.0).is_none());
        assert!(shape_kugeci(KUGECI_SONG, "爱情", "Someone", 4.0).is_none());
        assert!(shape_kugeci(
            &KUGECI_SONG.replace("[00:03.40]", "[04:00.00]"),
            "爱情",
            "Singer & Co",
            4.0
        )
        .is_none());
        assert!(shape_kugeci("<main>Challenge page</main>", "爱情", "Singer", 4.0).is_none());
        assert!(shape_kugeci(
            &KUGECI_SONG.replace("lyricsContainer", "changed"),
            "爱情",
            "Singer & Co",
            4.0
        )
        .is_none());
    }

    #[test]
    fn kugeci_search_verifies_artist_and_ignores_footer_and_external_links() {
        let html = r#"<table id="tablesort"><tbody>
        <tr><td>date</td><td><a href="/song/studio">爱情</a></td><td><a href="/singer/a">Wrong singer</a></td></tr>
        <tr><td>date</td><td><a href="/song/live">爱情 (Live)</a></td><td><a>Singer &amp; Co</a></td></tr>
        <tr><td>date</td><td><a href="https://www.kugeci.com/song/right">愛情</a></td><td><a>Other</a><a>Singer &amp; Co</a></td><td><a href="/song/right">link</a></td></tr>
        <tr><td>date</td><td><a href="https://evil.test/song/wrong">爱情</a></td><td><a>Singer &amp; Co</a></td></tr>
        </tbody></table><footer><a href="/song/footer">爱情</a></footer>"#;
        assert_eq!(
            kugeci_candidates(html, "爱情", "Singer & Co"),
            vec!["/song/right"]
        );
        assert!(kugeci_candidates(html, "爱情", "").is_empty());
        for path in [
            "//evil.test/song/id",
            "/song/../other",
            "https://www.kugeci.com.evil.test/song/id",
        ] {
            assert!(kugeci_song_path(path).is_none());
        }
    }

    #[test]
    fn kugeci_lyric_video_keeps_recording_annotations() {
        for annotation in ["Live", "Remix", "Acoustic", "粤语版", "现场版"] {
            let title = format!("Singer - Song『{annotation}』【動態歌詞Lyrics】");
            assert!(kugeci_video_credit(&title).is_none(), "{annotation}");
        }
        assert_eq!(
            kugeci_video_credit("Singer - Song『a lyric hook』【動態歌詞Lyrics】"),
            Some(("Song".into(), "Singer".into()))
        );
    }

    #[test]
    fn kugeci_resolves_verified_queue_metadata_formats() {
        assert!(kugeci_artists_match(
            &["Vansdaddy".into(), "华云龙KLE".into()],
            "Vansdaddy和华云龙"
        ));
        let cases = [
            ("🏍C-BLOCK : 很高兴认识你  🛵【 OFFICIAL MV 】Sup Music X 陌陌  \"送给每个美好的相遇\"", "很高兴认识你", "C-BLOCK"),
            ("陳韻若 - 愛的迴歸線『在愛的迴歸線 陽光在手指間』【動態歌詞Lyrics】", "愛的迴歸線", "陳韻若"),
            ("歌曲Top9《答案》杨坤 郭采洁 【2014年央视春晚】｜订阅CCTV春晚", "答案", "杨坤 郭采洁"),
        ];
        for (video_title, title, artist) in cases {
            assert_eq!(
                kugeci_video_credit(video_title),
                Some((title.into(), artist.into()))
            );
        }
        assert!(kugeci_artists_match(
            &["杨坤".into(), "郭采洁".into()],
            "杨坤 郭采洁"
        ));
        assert!(kugeci_title_matches("爱的回归线", "愛的迴歸線"));
        assert!(!kugeci_artists_match(&["华云龙KLE".into()], "华云龙Other"));
        assert!(!kugeci_artists_match(&["A".into(), "B".into()], "AB"));
        assert!(kugeci_artists_match(
            &["楊和蘇KeyNG".into(), "JinJiBeWater_隼".into()],
            "楊和蘇KeyNG和JinJiBeWater_隼"
        ));
        for title in [
            "A : Song 【OFFICIAL MV】 (Live)",
            "A : Song 【OFFICIAL MV】 reaction",
            "A - Song【reaction Lyrics】",
            "歌曲Top9《答案》不同人【2014年其他节目】",
        ] {
            assert!(kugeci_video_credit(title).is_none());
        }
    }

    #[test]
    fn kugeci_extracts_current_san_xin_credit() {
        assert_eq!(
            kugeci_video_credit("KEY.L刘聪 ft. 李佳隆 - 散心"),
            Some(("散心".into(), "KEY.L刘聪 ft. 李佳隆".into()))
        );
    }

    #[test]
    fn kugeci_san_xin_verifies_search_and_page_credits() {
        let (title, artist) = kugeci_video_credit("KEY.L刘聪 ft. 李佳隆 - 散心").unwrap();
        let search = r#"<table id="tablesort"><tbody><tr><td>date</td>
            <td><a href="/song/Cc6WfjTb">散心</a></td>
            <td><a>KEY.L刘聪</a><a>李佳隆</a></td></tr></tbody></table>"#;
        let page = r#"<main><h1>散心</h1><div class="song-details-container">
            <a href="/singer/a">演唱： KEY.L刘聪</a>
            <a href="/singer/b">演唱： 李佳隆</a></div>
            <div id="lyricsContainer">[00:04.851]fixture<br />[00:06.718]line</div></main>"#;
        assert_eq!(
            kugeci_candidates(search, &title, &artist),
            vec!["/song/Cc6WfjTb"]
        );
        assert!(shape_kugeci(page, &title, &artist, 265.0).is_some());
        assert!(shape_kugeci(page, &title, "FirePanda", 265.0).is_none());
        assert!(shape_kugeci(page, &title, "KEY.L刘聪 ft. Someone", 265.0).is_none());
        for annotated in [
            "KEY.L刘聪 ft. 李佳隆 - 散心 (Live)",
            "A - Song【reaction Lyrics】",
        ] {
            assert!(kugeci_video_credit(annotated).is_none());
        }
    }

    #[tokio::test]
    #[ignore = "live Kugeci availability check; run explicitly"]
    async fn kugeci_live_san_xin_uploader() {
        let song = Song {
            title: "KEY.L刘聪 ft. 李佳隆 - 散心".into(),
            artist: "FirePanda".into(),
            song_duration: 265.0,
            ..Song::default()
        };
        let words = fetch_kugeci_for_song(&reqwest::Client::new(), &song)
            .await
            .expect("uploader title resolves to synced Kugeci lyrics");
        assert!(words.synced);
        assert_eq!(words.how, How::Kugeci);
        assert!(words.lines.len() > 20);
    }

    #[test]
    fn kugeci_extracts_explicit_music_video_credit_from_uploader_metadata() {
        let title = "功夫胖 KUNGFU-PEN ：「无赖」🐼 🐼 🐼 【 OFFICIAL MV  】";
        assert_eq!(
            kugeci_video_credit(title),
            Some(("无赖".into(), "功夫胖 KUNGFU-PEN".into()))
        );
        for title in ["A: Song", "A ：「Song」 reaction video", "Song (Live)"] {
            assert!(kugeci_video_credit(title).is_none());
        }
    }

    #[test]
    fn kugeci_matches_alternate_subtitle_without_changing_recording_version() {
        let search = r#"<table id="tablesort"><tbody><tr><td>date</td>
            <td><a href="/song/9QbM6b0K">月半小夜曲</a></td><td><a>TizzyT</a></td></tr></tbody></table>"#;
        for title in ["月半小夜曲 (你怎么不回答)", "月半小夜曲（你怎么不回答）"]
        {
            assert_eq!(
                kugeci_candidates(search, title, "Tizzy T"),
                vec!["/song/9QbM6b0K"]
            );
        }
        for title in [
            "月半小夜曲 (Live)",
            "月半小夜曲（现场版）",
            "月半小夜曲 (Remix)",
        ] {
            assert!(kugeci_candidates(search, title, "Tizzy T").is_empty());
        }
    }

    #[test]
    fn kugeci_matches_zhou_xuan_chinese_collaboration_metadata() {
        let search = r#"<table id="tablesort"><tbody><tr><td>date</td>
            <td><a href="/song/7CJI2dO0">周旋</a></td>
            <td><a>王以太</a><a>艾热 AIR</a></td></tr></tbody></table>"#;
        let artist = "王以太和艾热 AIR";
        assert_eq!(
            kugeci_candidates(search, "周旋", &artist),
            vec!["/song/7CJI2dO0"]
        );
        let page = r#"<main><h1>周旋</h1><div class="song-details-container">
            <a href="/singer/a">演唱： 王以太</a><a href="/singer/b">演唱： 艾热 AIR</a></div>
            <div id="lyricsContainer">[00:21.87]Example<br>[00:25.89]Next</div></main>"#;
        assert!(shape_kugeci(page, "周旋", &artist, 291.0).is_some());
    }

    #[test]
    fn kugeci_requires_all_collaborators_and_preserves_literal_artist_names() {
        let credits = vec!["王以太".to_string(), "艾热 AIR".to_string()];
        for artist in [
            "王以太和艾热 AIR",
            "王以太 & 艾熱 AIR",
            "王以太/艾热 AIR",
            "王以太 feat. 艾热 AIR",
        ] {
            assert!(kugeci_artists_match(&credits, artist), "{artist}");
        }
        assert!(!kugeci_artists_match(&credits, "王以太和其他歌手"));
        assert!(!kugeci_artists_match(&credits[..1], "王以太和艾热 AIR"));
        assert!(kugeci_artists_match(&["和平饭店".into()], "和平饭店"));
        assert!(kugeci_artists_match(&["Singer & Co".into()], "Singer & Co"));
    }

    #[tokio::test]
    #[ignore = "live Kugeci availability check; run explicitly"]
    async fn kugeci_live_verified_queue_formats() {
        let cases = [
            ("台北一夜", "Vansdaddy和华云龙", 196.0),
            ("🏍C-BLOCK : 很高兴认识你  🛵【 OFFICIAL MV 】Sup Music X 陌陌  \"送给每个美好的相遇\"", "ZHONG.TV", 259.0),
            ("陳韻若 - 愛的迴歸線『在愛的迴歸線 陽光在手指間』【動態歌詞Lyrics】", "Music Channel HM", 258.0),
            ("歌曲Top9《答案》杨坤 郭采洁 【2014年央视春晚】｜订阅CCTV春晚", "CCTV春晚", 205.0),
        ];
        let http = reqwest::Client::new();
        for (title, artist, song_duration) in cases {
            let song = Song {
                title: title.into(),
                artist: artist.into(),
                song_duration,
                ..Song::default()
            };
            let words = fetch_kugeci_for_song(&http, &song)
                .await
                .unwrap_or_else(|| panic!("no result for {title}"));
            assert!(words.synced);
            assert_eq!(words.how, How::Kugeci);
            assert!(words.lines.len() > 20);
        }
    }

    #[tokio::test]
    #[ignore = "live Kugeci availability check; run explicitly"]
    async fn kugeci_live_uploader_music_video() {
        let song = Song {
            title: "功夫胖 KUNGFU-PEN ：「无赖」🐼 🐼 🐼 【 OFFICIAL MV  】".into(),
            artist: "ZHONG.TV".into(),
            song_duration: 279.0,
            ..Song::default()
        };
        let words = fetch_kugeci_for_song(&reqwest::Client::new(), &song)
            .await
            .expect("explicit video title credit resolves to Kugeci");
        assert!(words.synced);
        assert_eq!(words.how, How::Kugeci);
    }

    #[tokio::test]
    #[ignore = "live Kugeci availability check; run explicitly"]
    async fn kugeci_live_alternate_subtitle() {
        let words = fetch_kugeci(
            &reqwest::Client::new(),
            "月半小夜曲 (你怎么不回答)",
            "Tizzy T",
            230.0,
        )
        .await
        .expect("alternate title matches the same recording");
        assert!(words.synced);
        assert_eq!(words.how, How::Kugeci);
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

    #[tokio::test]
    #[ignore = "live Kugeci availability check; run explicitly"]
    async fn kugeci_live_zhou_xuan() {
        let words = fetch_kugeci(&reqwest::Client::new(), "周旋", "王以太和艾热 AIR", 291.0)
            .await
            .expect("current track matches the collaboration and timings");
        assert!(words.synced);
        assert_eq!(words.how, How::Kugeci);
        assert!(words.lines.len() > 20);
    }

    #[tokio::test]
    #[ignore = "live Kugeci availability check; run explicitly"]
    async fn kugeci_live_supplied_song() {
        let words = fetch_kugeci(&reqwest::Client::new(), "月半小夜曲", "TizzyT", 0.0)
            .await
            .expect("supplied song found through title search");
        assert!(words.synced);
        assert_eq!(words.how, How::Kugeci);
        assert!(words.lines.len() > 20);
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
        assert_eq!(lyrics.how, How::Ytmusic);
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
        assert_eq!(lead_artist("A ft. B"), "A");
        assert_eq!(lead_artist("A with B"), "A");
        // Separators are whole words, not letters inside a name.
        assert_eq!(lead_artist("Taylor Swift"), "Taylor Swift");
        assert_eq!(lead_artist("Bill Withers"), "Bill Withers");
        assert_eq!(lead_artist("Daft Punk"), "Daft Punk");
        assert_eq!(lead_artist("Feather Lane"), "Feather Lane");
        // Glued to Han characters, as Chinese credits often are.
        assert_eq!(lead_artist("陈奕迅feat.王菲"), "陈奕迅");
        assert_eq!(lead_artist("周杰倫ft.費玉清"), "周杰倫");
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
