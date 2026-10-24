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

/// An artist credited on a track, with the browse id of their YouTube Music
/// page (`UC…`), or an empty id when YouTube Music did not link the name.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct ArtistLink {
    /// As shown: `artistNames` applied on the playing `Song`.
    pub name: String,
    pub id: String,
    /// YouTube Music's own name, which an artist without an id is searched by.
    pub query: String,
}

impl ArtistLink {
    pub fn named(name: &str) -> Self {
        Self { name: name.to_string(), id: String::new(), query: name.to_string() }
    }
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
    /// The byline's artist part before `artistNames`: `artist` is drawn, this
    /// is what the playing track is relabelled from (`Core::dress`).
    #[serde(skip)]
    pub credit: String,
    /// Each credited artist with a page, in byline order. Not serialised: only
    /// the playing track's links reach the renderer, on its `Song`.
    #[serde(skip)]
    pub artists: Arc<[ArtistLink]>,
    /// `lengthText`, already formatted by the player. Empty when absent.
    pub duration: String,
    /// The artwork **source** URL, deliberately not serialised. Resolving one
    /// `data:` URL per slot would be megabytes on an event Tauri formats once
    /// per webview, and the two surfaces want different sizes anyway — the
    /// renderer asks for the rows it is actually showing. See `queue_art`.
    #[serde(skip)]
    pub art: Option<Arc<str>>,
    /// `LIKE` / `DISLIKE` for a rated local file, so rows can show it. Absent
    /// for YouTube Music, whose queue does not say.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<&'static str>,
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

/// A library playlist, as the playlist picker draws it.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Playlist {
    /// The browse id ("VL…"); the API server strips the prefix itself.
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub thumbnail: Option<Arc<str>>,
}

pub const PLAYLISTS_MAX: usize = 100;

fn collect_renderers<'a>(node: &'a Value, keys: &[&str], out: &mut Vec<&'a Value>, depth: usize) {
    if depth > MAX_DEPTH || out.len() > MAX_ITEMS {
        return;
    }
    match node {
        Value::Array(items) => {
            for child in items {
                collect_renderers(child, keys, out, depth + 1);
            }
        }
        Value::Object(map) => {
            for key in keys {
                if let Some(renderer) = map.get(*key) {
                    out.push(renderer);
                }
            }
            for child in map.values() {
                collect_renderers(child, keys, out, depth + 1);
            }
        }
        _ => {}
    }
}

/// `GET /library/playlists` returns the Library > Playlists browse page, drawn
/// as a grid (`musicTwoRowItemRenderer`) or a list
/// (`musicResponsiveListItemRenderer`) depending on the account's view setting.
/// Only rows that browse to a playlist are kept, which drops "New playlist".
pub fn parse_library_playlists(payload: &Value) -> Vec<Playlist> {
    let mut items = Vec::new();
    collect_renderers(
        payload,
        &["musicTwoRowItemRenderer", "musicResponsiveListItemRenderer"],
        &mut items,
        0,
    );

    let mut playlists: Vec<Playlist> = Vec::new();
    for item in items {
        let Some(id) = dig(item, &["navigationEndpoint", "browseEndpoint", "browseId"])
            .and_then(Value::as_str)
            .filter(|id| id.starts_with("VL"))
        else {
            continue;
        };
        if playlists.iter().any(|known| known.id == id) {
            continue;
        }

        let two_row = item.get("title").is_some();
        let (title, subtitle) = if two_row {
            (runs_text(item.get("title")), runs_text(item.get("subtitle")))
        } else {
            (column_text(item, 0), column_text(item, 1))
        };
        let title = title.trim().to_string();
        if title.is_empty() {
            continue;
        }
        let thumbnail = if two_row {
            dig(
                item,
                &["thumbnailRenderer", "musicThumbnailRenderer", "thumbnail", "thumbnails"],
            )
            .and_then(Value::as_array)
            .and_then(|list| list.first())
            .and_then(|first| first.get("url"))
            .and_then(Value::as_str)
            .map(Arc::from)
        } else {
            thumbnail_of(item)
        };

        playlists.push(Playlist {
            id: id.to_string(),
            title,
            subtitle: subtitle.trim().to_string(),
            thumbnail,
        });
        if playlists.len() >= PLAYLISTS_MAX {
            break;
        }
    }
    playlists
}

/// An artist page's Top songs shelf, as the artist popup draws it.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ArtistPage {
    /// `thumbnail` holds the source URL from the parse; `artist::page`
    /// swaps in a `data:` URL.
    pub songs: Vec<SearchResult>,
    /// The "Show all" playlist ("VL…") holding every top song, for Play all.
    pub all_songs: Option<String>,
}

/// The first `musicShelfRenderer` on an artist page is Top songs; the albums,
/// singles and videos below it are carousels of another renderer. Rows are
/// title, artist, plays, album, so the subtitle is the last two.
pub fn parse_artist_page(payload: &Value) -> ArtistPage {
    let mut shelves = Vec::new();
    collect_renderers(payload, &["musicShelfRenderer"], &mut shelves, 0);
    let Some(shelf) = shelves.first() else {
        return ArtistPage { songs: Vec::new(), all_songs: None };
    };

    let mut items = Vec::new();
    collect_items(shelf, &mut items, 0);
    let mut songs: Vec<SearchResult> = Vec::new();
    for item in items {
        let Some(video_id) = video_id_of(item) else { continue };
        if songs.iter().any(|song| song.video_id == video_id) {
            continue;
        }
        let title = column_text(item, 0).trim().to_string();
        if title.is_empty() {
            continue;
        }
        let details: Vec<String> = [2, 3]
            .iter()
            .map(|&index| column_text(item, index).trim().to_string())
            .filter(|text| !text.is_empty())
            .collect();
        let subtitle = if details.is_empty() {
            column_text(item, 1).trim().to_string()
        } else {
            details.join(" · ")
        };
        // The largest of the row's thumbnails (60 and 120 px): rows are drawn
        // at retina size, and `at_most` only ever shrinks.
        let thumbnail = dig(
            item,
            &["thumbnail", "musicThumbnailRenderer", "thumbnail", "thumbnails"],
        )
        .and_then(Value::as_array)
        .and_then(|list| list.last())
        .and_then(|last| last.get("url"))
        .and_then(Value::as_str)
        .map(Arc::from);
        songs.push(SearchResult { video_id: video_id.to_string(), title, subtitle, thumbnail });
        if songs.len() >= MAX_RESULTS {
            break;
        }
    }

    let browse_id = |node: Option<&Value>| {
        node.and_then(|node| dig(node, &["browseEndpoint", "browseId"]))
            .and_then(Value::as_str)
            .filter(|id| id.starts_with("VL"))
            .map(str::to_string)
    };
    let all_songs = browse_id(shelf.get("bottomEndpoint")).or_else(|| {
        browse_id(
            dig(shelf, &["title", "runs"])
                .and_then(Value::as_array)
                .and_then(|runs| runs.first())
                .and_then(|run| run.get("navigationEndpoint")),
        )
    });

    ArtistPage { songs, all_songs }
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

/// Joiners YouTube Music writes as runs of their own between names.
const JOINERS: [&str; 5] = ["和", "與", "与", "and", "x"];

/// The artists in a byline's first part. A run that browses to an artist page
/// gives its id. YouTube Music links only the artists it has matched, and
/// sometimes none ("MC HotDog熱狗/艾怡良" is one plain run), so plain text is
/// split into names with an empty id for `artist::page` to find by name.
/// Uploader channels (`MUSIC_PAGE_TYPE_USER_CHANNEL`) have no Top songs and
/// are left out.
fn artist_links(byline: Option<&Value>) -> Vec<ArtistLink> {
    let runs = byline
        .and_then(|node| node.get("runs"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut links = Vec::new();
    for run in runs {
        let text = run.get("text").and_then(Value::as_str).unwrap_or("");
        if text.contains('•') {
            break;
        }
        let Some(browse) = dig(run, &["navigationEndpoint", "browseEndpoint"]) else {
            links.extend(
                crate::store::split_credit(text)
                    .into_iter()
                    .filter(|name| !JOINERS.contains(&name.to_lowercase().as_str()))
                    .map(ArtistLink::named),
            );
            continue;
        };
        if page_type(browse) != Some("MUSIC_PAGE_TYPE_ARTIST") {
            continue;
        }
        let id = browse.get("browseId").and_then(Value::as_str).unwrap_or("");
        let name = text.trim();
        if !id.is_empty() && !name.is_empty() {
            links.push(ArtistLink { id: id.to_string(), ..ArtistLink::named(name) });
        }
    }
    links
}

fn page_type(browse: &Value) -> Option<&str> {
    dig(
        browse,
        &[
            "browseEndpointContextSupportedConfigs",
            "browseEndpointContextMusicConfig",
            "pageType",
        ],
    )
    .and_then(Value::as_str)
}

/// The artist page a search for `name` names: the first artist link (the top
/// result card, then artist rows and credits) whose text is `name` once
/// folded. A different artist is never taken for it.
pub fn find_artist(payload: &Value, name: &str) -> Option<String> {
    fn walk(node: &Value, wanted: &str, depth: usize) -> Option<String> {
        if depth > MAX_DEPTH {
            return None;
        }
        match node {
            Value::Array(items) => items.iter().find_map(|child| walk(child, wanted, depth + 1)),
            Value::Object(map) => {
                let browse = dig(node, &["navigationEndpoint", "browseEndpoint"])
                    .filter(|browse| page_type(browse) == Some("MUSIC_PAGE_TYPE_ARTIST"));
                if let Some(browse) = browse {
                    // A run carries its text; an artist row, its first column.
                    let text = match map.get("text").and_then(Value::as_str) {
                        Some(text) => text.to_string(),
                        None => column_text(node, 0),
                    };
                    if crate::artwork::fold(&text) == wanted {
                        if let Some(id) = browse.get("browseId").and_then(Value::as_str) {
                            return Some(id.to_string());
                        }
                    }
                }
                map.values().find_map(|child| walk(child, wanted, depth + 1))
            }
            _ => None,
        }
    }
    let wanted = crate::artwork::fold(name);
    if wanted.is_empty() {
        return None;
    }
    walk(payload, &wanted, 0)
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
        credit: artist.clone(),
        artists: Arc::from(artist_links(byline)),
        artist,
        duration: runs_text(renderer.get("lengthText")).trim().to_string(),
        art: queue_art_of(renderer),
        rating: None,
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

    #[test]
    fn library_playlists_come_from_grid_and_list_views() {
        let payload = json!({ "grid": { "items": [
            // "New playlist" opens a dialog rather than browsing: dropped.
            { "musicTwoRowItemRenderer": {
                "title": { "runs": [{ "text": "New playlist" }] },
                "navigationEndpoint": { "createPlaylistEndpoint": {} }
            }},
            { "musicTwoRowItemRenderer": {
                "title": { "runs": [{ "text": "Liked Music" }] },
                "subtitle": { "runs": [{ "text": "Auto playlist" }] },
                "navigationEndpoint": { "browseEndpoint": { "browseId": "VLLM" } },
                "thumbnailRenderer": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [{ "url": "http://liked" }] } } }
            }},
            // The same playlist again: dropped.
            { "musicTwoRowItemRenderer": {
                "title": { "runs": [{ "text": "Liked Music" }] },
                "navigationEndpoint": { "browseEndpoint": { "browseId": "VLLM" } }
            }},
            // An album browses to MPRE…, not a playlist: dropped.
            { "musicTwoRowItemRenderer": {
                "title": { "runs": [{ "text": "Album" }] },
                "navigationEndpoint": { "browseEndpoint": { "browseId": "MPREb_x" } }
            }}
        ]}, "list": [{ "musicResponsiveListItemRenderer": {
            "navigationEndpoint": { "browseEndpoint": { "browseId": "VLPLabc" } },
            "flexColumns": [
                { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "Night drive" }] } } },
                { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "38 songs" }] } } }
            ]
        }}]});

        let playlists = parse_library_playlists(&payload);
        let ids: Vec<_> = playlists.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["VLLM", "VLPLabc"]);
        assert_eq!(playlists[0].subtitle, "Auto playlist");
        assert_eq!(playlists[0].thumbnail.as_deref(), Some("http://liked"));
        assert_eq!(playlists[1].title, "Night drive");
        assert_eq!(playlists[1].subtitle, "38 songs");
    }

    fn browse_run(text: &str, id: &str, page: &str) -> Value {
        json!({ "text": text, "navigationEndpoint": { "browseEndpoint": {
            "browseId": id,
            "browseEndpointContextSupportedConfigs": {
                "browseEndpointContextMusicConfig": { "pageType": page } } } } })
    }

    #[test]
    fn queue_slots_link_each_artist_before_the_album() {
        let queue = json!({ "items": [{ "playlistPanelVideoRenderer": {
            "videoId": "v1",
            "title": { "runs": [{ "text": "纵横四海" }] },
            "longBylineText": { "runs": [
                browse_run("周杰倫", "UCjay", "MUSIC_PAGE_TYPE_ARTIST"),
                { "text": " & " },
                browse_run("費玉清", "UCfei", "MUSIC_PAGE_TYPE_ARTIST"),
                { "text": " • " },
                browse_run("Album", "MPREb_x", "MUSIC_PAGE_TYPE_ALBUM"),
                { "text": " • " },
                { "text": "2022年" }
            ] }
        } }, { "playlistPanelVideoRenderer": {
            "videoId": "v2",
            "title": { "runs": [{ "text": "Cover" }] },
            "longBylineText": { "runs": [
                browse_run("Someone", "UCsomeone", "MUSIC_PAGE_TYPE_USER_CHANNEL"),
                { "text": " • " },
                { "text": "1.2M views" }
            ] }
        } }] });

        let view = parse_queue(Some(&queue), QUEUE_MAX);
        let song = &view.items[0];
        assert_eq!(song.credit, "周杰倫 & 費玉清");
        assert_eq!(
            &*song.artists,
            &[
                ArtistLink { id: "UCjay".into(), ..ArtistLink::named("周杰倫") },
                ArtistLink { id: "UCfei".into(), ..ArtistLink::named("費玉清") },
            ]
        );
        // An uploader's channel is not an artist page.
        assert_eq!(view.items[1].credit, "Someone");
        assert!(view.items[1].artists.is_empty());
    }

    #[test]
    fn unlinked_credits_split_into_names_to_find_by_name() {
        let byline = json!({ "runs": [
            { "text": "MC HotDog熱狗/艾怡良" },
            { "text": " • " },
            { "text": "Album" }
        ] });
        let name = ArtistLink::named;
        assert_eq!(artist_links(Some(&byline)), [name("MC HotDog熱狗"), name("艾怡良")]);

        // Joiners are runs of their own; only the matched artist is linked.
        let byline = json!({ "runs": [
            { "text": "苏运莹" }, { "text": "、" }, { "text": "岑宁儿" }, { "text": "和" },
            browse_run("艾怡良", "UCeve", "MUSIC_PAGE_TYPE_ARTIST")
        ] });
        assert_eq!(
            artist_links(Some(&byline)),
            [name("苏运莹"), name("岑宁儿"), ArtistLink { id: "UCeve".into(), ..name("艾怡良") }]
        );
    }

    #[test]
    fn finds_an_artist_by_exact_name_only() {
        let payload = json!({ "contents": [
            { "musicCardShelfRenderer": { "title": { "runs": [
                browse_run("艾怡良", "UCeve", "MUSIC_PAGE_TYPE_ARTIST") ] } } },
            { "musicResponsiveListItemRenderer": {
                "navigationEndpoint": { "browseEndpoint": { "browseId": "UCcen",
                    "browseEndpointContextSupportedConfigs": { "browseEndpointContextMusicConfig": {
                        "pageType": "MUSIC_PAGE_TYPE_ARTIST" } } } },
                "flexColumns": [{ "musicResponsiveListItemFlexColumnRenderer": {
                    "text": { "runs": [{ "text": "岑寧兒" }] } } }]
            } }
        ] });
        assert_eq!(find_artist(&payload, "艾怡良").as_deref(), Some("UCeve"));
        // Folded: case, spacing and traditional/simplified script.
        assert_eq!(find_artist(&payload, "岑宁儿").as_deref(), Some("UCcen"));
        assert_eq!(find_artist(&payload, "Someone Else"), None);
    }

    #[test]
    fn reads_top_songs_and_play_all_off_an_artist_page() {
        let column = |runs: Value| json!({ "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": runs } } });
        let row = |id: &str, title: &str| json!({ "musicResponsiveListItemRenderer": {
            "playlistItemData": { "videoId": id },
            "flexColumns": [
                column(json!([{ "text": title }])),
                column(json!([browse_run("李荣浩", "UClrh", "MUSIC_PAGE_TYPE_ARTIST")])),
                column(json!([{ "text": "播放次数：6456万" }])),
                column(json!([browse_run("纵横四海", "MPREb_x", "MUSIC_PAGE_TYPE_ALBUM")])),
            ],
            "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [
                { "url": "http://w60" }, { "url": "http://w120" } ] } } }
        } });
        let page = json!({ "contents": { "sectionListRenderer": { "contents": [
            { "musicShelfRenderer": {
                "title": { "runs": [{ "text": "热门歌曲", "navigationEndpoint": {
                    "browseEndpoint": { "browseId": "VLOLAK5top" } } }] },
                "contents": [row("a", "对等关系"), row("b", "恋人")]
            } },
            // Albums below are a carousel; nothing in it is a song row.
            { "musicCarouselShelfRenderer": { "contents": [
                { "musicTwoRowItemRenderer": { "title": { "runs": [{ "text": "嗯" }] } } }
            ] } }
        ] } } });

        let parsed = parse_artist_page(&page);
        assert_eq!(parsed.all_songs.as_deref(), Some("VLOLAK5top"));
        assert_eq!(parsed.songs.len(), 2);
        assert_eq!(parsed.songs[0].video_id, "a");
        assert_eq!(parsed.songs[0].title, "对等关系");
        assert_eq!(parsed.songs[0].subtitle, "播放次数：6456万 · 纵横四海");
        assert_eq!(parsed.songs[0].thumbnail.as_deref(), Some("http://w120"));

        let empty = parse_artist_page(&json!({ "contents": {} }));
        assert!(empty.songs.is_empty() && empty.all_songs.is_none());
    }
}
