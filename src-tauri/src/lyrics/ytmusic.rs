//! YouTube Music, asked directly through InnerTube: the timed lyrics of a
//! video, and the song search that finds which video a local file is a copy
//! of.

use serde_json::Value;

use super::{How, LyricLine, Lyrics, LOCAL_DURATION_SLACK, TIMEOUT};
use crate::text::lead_artist;

const YTM_ENDPOINT: &str = "https://music.youtube.com/youtubei/v1";

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
pub(super) async fn fetch_ytmusic(http: &reqwest::Client, video_id: &str) -> Option<Lyrics> {
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
pub(super) async fn find_ytmusic_video(
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
mod tests {
    use super::*;

    #[test]
    fn subtitle_durations_parse() {
        assert_eq!(subtitle_duration("林俊傑 • 樂行者 • 4:32"), Some(272.0));
        assert_eq!(subtitle_duration("A • B • 1:02:03"), Some(3723.0));
        assert_eq!(subtitle_duration("林俊傑 • 樂行者"), None);
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
}
