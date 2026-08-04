//! `POST /api/v1/search` returns YouTube Music's raw innertube payload — around
//! 270KB of deeply nested renderers with no stable path to the results. Rather
//! than walk a fixed path that a client update would break, collect every
//! `musicResponsiveListItemRenderer` in the tree and keep the ones that carry a
//! videoId. Songs, albums and videos all use that renderer, so one pass gets the
//! lot in roughly the order the app displays them.

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

pub const MAX_RESULTS: usize = 10;
const MAX_DEPTH: usize = 40;
const MAX_ITEMS: usize = 200;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct SearchResult {
    #[serde(rename = "videoId")]
    pub video_id: String,
    pub title: String,
    pub subtitle: String,
    pub thumbnail: Option<Arc<str>>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct UpNextItem {
    #[serde(rename = "videoId")]
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub thumbnail: Option<Arc<str>>,
}

/// One entry per queue slot, with the playing one marked.
#[derive(Clone, Debug, PartialEq)]
pub struct QueueEntry {
    pub video_id: Option<String>,
    pub selected: bool,
}

/// Walk a chain of object keys, stopping at the first one that is missing.
fn dig<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut node = value;
    for key in path {
        node = node.get(key)?;
    }
    Some(node)
}

fn collect_items<'a>(node: &'a Value, out: &mut Vec<&'a Value>, depth: usize) {
    if depth > MAX_DEPTH || out.len() > MAX_ITEMS {
        return;
    }
    match node {
        Value::Array(items) => {
            for child in items {
                collect_items(child, out, depth + 1);
            }
        }
        Value::Object(map) => {
            if let Some(renderer) = map.get("musicResponsiveListItemRenderer") {
                out.push(renderer);
            }
            for child in map.values() {
                collect_items(child, out, depth + 1);
            }
        }
        _ => {}
    }
}

/// Albums and videos hang their id off the play overlay rather than playlistItemData.
fn video_id_of(item: &Value) -> Option<&str> {
    dig(item, &["playlistItemData", "videoId"])
        .and_then(Value::as_str)
        .or_else(|| {
            dig(
                item,
                &[
                    "overlay",
                    "musicItemThumbnailOverlayRenderer",
                    "content",
                    "musicPlayButtonRenderer",
                    "playNavigationEndpoint",
                    "watchEndpoint",
                    "videoId",
                ],
            )
            .and_then(Value::as_str)
        })
        .filter(|id| !id.is_empty())
}

fn runs_text(node: Option<&Value>) -> String {
    node.and_then(|node| node.get("runs"))
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .filter_map(|run| run.get("text").and_then(Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default()
}

fn column_text(item: &Value, index: usize) -> String {
    let column = item
        .get("flexColumns")
        .and_then(Value::as_array)
        .and_then(|columns| columns.get(index))
        .and_then(|column| column.get("musicResponsiveListItemFlexColumnRenderer"));
    runs_text(column.and_then(|column| column.get("text")))
}

/// Smallest thumbnail is plenty for a 30px row and keeps the fetch cheap.
fn thumbnail_of(item: &Value) -> Option<Arc<str>> {
    dig(
        item,
        &["thumbnail", "musicThumbnailRenderer", "thumbnail", "thumbnails"],
    )
    .and_then(Value::as_array)
    .and_then(|list| list.first())
    .and_then(|first| first.get("url"))
    .and_then(Value::as_str)
    .map(Arc::from)
}

pub fn parse_search_results(payload: &Value) -> Vec<SearchResult> {
    let mut items = Vec::new();
    collect_items(payload, &mut items, 0);

    let mut seen = Vec::new();
    let mut results = Vec::new();

    for item in items {
        let Some(video_id) = video_id_of(item) else {
            continue;
        };
        if seen.iter().any(|id| id == video_id) {
            continue;
        }

        let title = column_text(item, 0).trim().to_string();
        if title.is_empty() {
            continue;
        }

        seen.push(video_id.to_string());
        results.push(SearchResult {
            video_id: video_id.to_string(),
            title,
            subtitle: column_text(item, 1).trim().to_string(),
            thumbnail: thumbnail_of(item),
        });

        if results.len() >= MAX_RESULTS {
            break;
        }
    }

    results
}

/// Queue items come either bare or wrapped; unwrap to the video renderer.
fn video_renderer(item: &Value) -> Option<&Value> {
    item.get("playlistPanelVideoRenderer").or_else(|| {
        dig(
            item,
            &[
                "playlistPanelVideoWrapperRenderer",
                "primaryRenderer",
                "playlistPanelVideoRenderer",
            ],
        )
    })
}

fn queue_renderers(queue: &Value) -> Vec<Option<&Value>> {
    queue
        .get("items")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(video_renderer).collect())
        .unwrap_or_default()
}

/// Flat view of the queue: one entry per slot, with the playing one marked.
pub fn queue_entries(queue: Option<&Value>) -> Vec<QueueEntry> {
    let Some(queue) = queue else {
        return Vec::new();
    };
    queue_renderers(queue)
        .into_iter()
        .map(|renderer| QueueEntry {
            video_id: renderer
                .and_then(|r| r.get("videoId"))
                .and_then(Value::as_str)
                .map(str::to_string),
            selected: renderer
                .and_then(|r| r.get("selected"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
        .collect()
}

/// The tracks queued after the one playing, for the stack skin's "Next tracks".
/// `selected` marks the current item; anything before it has already played.
pub fn parse_queue_upcoming(queue: &Value, limit: usize) -> Vec<UpNextItem> {
    let renderers = queue_renderers(queue);

    let current = renderers.iter().position(|renderer| {
        renderer
            .and_then(|r| r.get("selected"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    });
    let start = current.map(|index| index + 1).unwrap_or(0);

    let mut out = Vec::new();
    for renderer in renderers.iter().skip(start) {
        if out.len() >= limit {
            break;
        }
        let Some(renderer) = renderer else { continue };
        let Some(video_id) = renderer.get("videoId").and_then(Value::as_str) else {
            continue;
        };

        let byline = renderer
            .get("longBylineText")
            .or_else(|| renderer.get("shortBylineText"));
        let artist = runs_text(byline)
            .split('•')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();

        out.push(UpNextItem {
            video_id: video_id.to_string(),
            title: runs_text(renderer.get("title")).trim().to_string(),
            artist,
            thumbnail: dig(renderer, &["thumbnail", "thumbnails"])
                .and_then(Value::as_array)
                .and_then(|list| list.first())
                .and_then(|first| first.get("url"))
                .and_then(Value::as_str)
                .map(Arc::from),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_results_at_any_depth_and_dedupes() {
        let payload = json!({
            "a": { "b": [{ "musicResponsiveListItemRenderer": {
                "playlistItemData": { "videoId": "one" },
                "flexColumns": [
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "Song " }, { "text": "A" }] } } },
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "Artist" }] } } }
                ],
                "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [{ "url": "http://cover" }] } } }
            }}]},
            // Same id again, plus one with no title: both are dropped.
            "c": [
                { "musicResponsiveListItemRenderer": { "playlistItemData": { "videoId": "one" }, "flexColumns": [] } },
                { "musicResponsiveListItemRenderer": { "playlistItemData": { "videoId": "two" }, "flexColumns": [] } }
            ]
        });

        let results = parse_search_results(&payload);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].video_id, "one");
        assert_eq!(results[0].title, "Song A");
        assert_eq!(results[0].subtitle, "Artist");
        assert_eq!(results[0].thumbnail.as_deref(), Some("http://cover"));
    }

    #[test]
    fn reads_the_id_off_the_play_overlay() {
        let payload = json!({ "musicResponsiveListItemRenderer": {
            "overlay": { "musicItemThumbnailOverlayRenderer": { "content": { "musicPlayButtonRenderer": {
                "playNavigationEndpoint": { "watchEndpoint": { "videoId": "overlaid" } } } } } },
            "flexColumns": [{ "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "T" }] } } }]
        }});
        assert_eq!(parse_search_results(&payload)[0].video_id, "overlaid");
    }

    #[test]
    fn upcoming_starts_after_the_selected_track() {
        let queue = json!({ "items": [
            { "playlistPanelVideoRenderer": { "videoId": "played", "title": { "runs": [{ "text": "Played" }] } } },
            { "playlistPanelVideoRenderer": { "videoId": "now", "selected": true, "title": { "runs": [{ "text": "Now" }] } } },
            { "playlistPanelVideoWrapperRenderer": { "primaryRenderer": { "playlistPanelVideoRenderer": {
                "videoId": "next",
                "title": { "runs": [{ "text": "Next" }] },
                "longBylineText": { "runs": [{ "text": "Band • Album" }] },
                "thumbnail": { "thumbnails": [{ "url": "http://thumb" }] }
            }}}}
        ]});

        let upcoming = parse_queue_upcoming(&queue, 4);
        assert_eq!(upcoming.len(), 1);
        assert_eq!(upcoming[0].video_id, "next");
        assert_eq!(upcoming[0].artist, "Band");
        assert_eq!(upcoming[0].thumbnail.as_deref(), Some("http://thumb"));

        let entries = queue_entries(Some(&queue));
        assert_eq!(entries.len(), 3);
        assert!(entries[1].selected);
        assert_eq!(entries[2].video_id.as_deref(), Some("next"));
    }
}
