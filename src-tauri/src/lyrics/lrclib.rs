//! LRCLib: the three tiers that ask `lrclib.net` — exact with everything
//! known, exact on a cleaned title, then free-text search picked by duration.

use serde_json::Value;

use super::{parse_lrc, How, LyricLine, Lyrics, TIMEOUT, USER_AGENT};
use crate::text::urlencode_form;

const ENDPOINT: &str = "https://lrclib.net/api";

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
        .map(|(key, value)| format!("{key}={}", urlencode_form(value)))
        .collect::<Vec<_>>()
        .join("&")
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

/// LRCLib exact match with everything known; LRCLib checks the duration.
pub(super) async fn lrclib_exact(
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
pub(super) async fn lrclib_cleaned(http: &reqwest::Client, title: &str, artist: &str) -> Option<Lyrics> {
    let params = [("track_name", title.to_string()), ("artist_name", artist.to_string())];
    let payload = request(http, &format!("/get?{}", query_string(&params))).await;
    shape(payload.as_ref(), How::Cleaned)
}

/// LRCLib free-text search, ranked by duration. With `slack`, a best hit
/// further than that from `duration` is rejected.
pub(super) async fn lrclib_search(
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

/// An LRCLib hit within `LOCAL_DURATION_SLACK` of the file, or any hit when
/// either duration is unknown.
fn within_duration(hit: &Value, duration: f64, slack: f64) -> bool {
    match hit.get("duration").and_then(Value::as_f64) {
        Some(theirs) if duration > 0.0 && theirs > 0.0 => (theirs - duration).abs() <= slack,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::super::LOCAL_DURATION_SLACK;
    use super::*;

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
}
