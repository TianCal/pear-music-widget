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

/// A pathological queue is capped the way the search tree is by `MAX_ITEMS`.
pub const QUEUE_MAX: usize = 500;

/// One queue slot, as the renderer draws it.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QueueTrack {
    /// Index of the **raw** queue slot, which is what `PATCH /queue` takes. Not
    /// the position in `QueueView::items`: slots naming no video are dropped
    /// from the list, and must not shift the indices a jump sends back.
    pub index: usize,
    pub video_id: String,
    pub title: String,
    pub artist: String,
    /// `lengthText`, already formatted by the player. Empty when absent.
    pub duration: String,
    /// The artwork **source** URL, deliberately not serialised. Resolving one
    /// `data:` URL per slot would be megabytes on an event Tauri formats once
    /// per webview, and the two surfaces want different sizes anyway — the
    /// renderer asks for the rows it is actually showing. See `queue_art`.
    #[serde(skip)]
    pub art: Option<Arc<str>>,
}

/// The whole queue: every slot that names a track, and which of them is playing.
///
/// Text only — around 110 bytes a track, so even a capped 500-track queue is in
/// the same class as one cover, and it is pushed at the same rate: once a track,
/// never on the position tick.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QueueView {
    pub items: Arc<[QueueTrack]>,
    /// Index into `items` of the playing track — for drawing only. A row is
    /// *played* by slot index and video id, never by this.
    pub current: Option<usize>,
    /// Set when the parse stopped at `QUEUE_MAX`.
    pub truncated: bool,
}

impl QueueView {
    pub fn empty() -> Self {
        Self {
            items: Arc::from([]),
            current: None,
            truncated: false,
        }
    }
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

/// The **largest** artwork the payload offers for a slot, not the first.
///
/// The list runs smallest-first, and `at_most` only ever shrinks a URL — so
/// taking `thumbnails[0]` and asking for anything larger would leave a 60px
/// JPEG drawn at several times its size. Rows shrink it back down for free;
/// there is no cost to starting from the biggest one on offer.
fn queue_art_of(renderer: &Value) -> Option<Arc<str>> {
    dig(renderer, &["thumbnail", "thumbnails"])
        .and_then(Value::as_array)?
        .last()
        .and_then(|last| last.get("url"))
        .and_then(Value::as_str)
        .map(Arc::from)
}

/// One queue slot's metadata. `None` for a slot that names no track — a divider,
/// or a renderer shape we do not know.
fn queue_track(index: usize, renderer: Option<&Value>) -> Option<QueueTrack> {
    let renderer = renderer?;
    let video_id = renderer.get("videoId").and_then(Value::as_str)?;

    // The byline is "Artist • Album • Year"; only the first part is the artist.
    let byline = renderer
        .get("longBylineText")
        .or_else(|| renderer.get("shortBylineText"));
    let artist = runs_text(byline)
        .split('•')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();

    Some(QueueTrack {
        index,
        video_id: video_id.to_string(),
        title: runs_text(renderer.get("title")).trim().to_string(),
        artist,
        duration: runs_text(renderer.get("lengthText")).trim().to_string(),
        art: queue_art_of(renderer),
    })
}

/// The whole queue, played tracks and all. `selected` marks the current item;
/// anything before it has already played.
///
/// This and `queue_entries` are deliberately two projections rather than one
/// function: this one drops slots that name no track, so its list positions are
/// its own, while `queue_entries` keeps every slot because *its* positions are
/// the ones `set_queue_index` takes. `QueueTrack::index` is the bridge.
pub fn parse_queue(queue: Option<&Value>, limit: usize) -> QueueView {
    let Some(queue) = queue else {
        return QueueView::empty();
    };

    let renderers = queue_renderers(queue);
    let mut items: Vec<QueueTrack> = Vec::new();
    let mut current = None;
    let mut truncated = false;

    for (slot, renderer) in renderers.iter().enumerate() {
        if items.len() >= limit {
            truncated = true;
            break;
        }
        let selected = renderer
            .and_then(|r| r.get("selected"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let Some(track) = queue_track(slot, *renderer) else {
            continue;
        };
        if selected {
            current = Some(items.len());
        }
        items.push(track);
    }

    QueueView {
        items: Arc::from(items),
        current,
        truncated,
    }
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

    fn sample_queue() -> Value {
        json!({ "items": [
            { "playlistPanelVideoRenderer": { "videoId": "played", "title": { "runs": [{ "text": "Played" }] } } },
            { "playlistPanelVideoRenderer": { "videoId": "now", "selected": true, "title": { "runs": [{ "text": "Now" }] } } },
            { "playlistPanelVideoWrapperRenderer": { "primaryRenderer": { "playlistPanelVideoRenderer": {
                "videoId": "next",
                "title": { "runs": [{ "text": "Next" }] },
                "longBylineText": { "runs": [{ "text": "Band • Album" }] },
                "lengthText": { "runs": [{ "text": "3:21" }] },
                "thumbnail": { "thumbnails": [{ "url": "http://small" }, { "url": "http://large" }] }
            }}}}
        ]})
    }

    #[test]
    fn the_queue_keeps_played_tracks_and_marks_the_playing_one() {
        let queue = sample_queue();
        let view = parse_queue(Some(&queue), QUEUE_MAX);

        assert_eq!(view.items.len(), 3, "already-played tracks are kept");
        assert_eq!(view.current, Some(1));
        assert!(!view.truncated);

        assert_eq!(view.items[2].video_id, "next");
        assert_eq!(view.items[2].index, 2);
        assert_eq!(view.items[2].artist, "Band", "the album is stripped");
        assert_eq!(view.items[2].duration, "3:21");
        // The largest offered, not the first — see `queue_art_of`.
        assert_eq!(view.items[2].art.as_deref(), Some("http://large"));

        // The raw slot view is unchanged: it is what `set_queue_index` takes.
        let entries = queue_entries(Some(&queue));
        assert_eq!(entries.len(), 3);
        assert!(entries[1].selected);
        assert_eq!(entries[2].video_id.as_deref(), Some("next"));
    }

    #[test]
    fn a_slot_naming_no_video_is_dropped_without_shifting_the_rest() {
        let queue = json!({ "items": [
            { "playlistPanelVideoRenderer": { "title": { "runs": [{ "text": "No id" }] } } },
            { "playlistPanelVideoRenderer": { "videoId": "real", "selected": true, "title": { "runs": [{ "text": "Real" }] } } }
        ]});

        let view = parse_queue(Some(&queue), QUEUE_MAX);
        assert_eq!(view.items.len(), 1);
        assert_eq!(view.current, Some(0), "current indexes the drawn list");
        assert_eq!(view.items[0].index, 1, "but the slot index is the raw one");
    }

    #[test]
    fn nothing_selected_leaves_current_unset() {
        let queue = json!({ "items": [
            { "playlistPanelVideoRenderer": { "videoId": "a", "title": { "runs": [{ "text": "A" }] } } }
        ]});
        assert_eq!(parse_queue(Some(&queue), QUEUE_MAX).current, None);
        assert!(parse_queue(None, QUEUE_MAX).items.is_empty());
    }

    #[test]
    fn a_long_queue_is_capped_and_says_so() {
        let queue = sample_queue();
        let view = parse_queue(Some(&queue), 2);
        assert_eq!(view.items.len(), 2);
        assert!(view.truncated);
    }
}
