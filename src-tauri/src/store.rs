//! Hand-editable settings, kept at the same path the Electron build used so an
//! upgrade keeps your position, skins and cached token:
//!
//!   ~/Library/Application Support/pear-music-widget/settings.json
//!
//! Unknown keys are round-tripped rather than dropped — the file is documented
//! as editable, and silently eating a key someone added is worse than carrying it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::text::CREDIT_SEPARATORS;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Position {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct StoredSize {
    pub width: f64,
}

/// Which of the top-right toggles the card shows. All on by default — this
/// exists for people who want the chrome back off a 300×110 card, not as a
/// feature to opt into.
///
/// One struct rather than four loose booleans, so `settings.json` reads as a
/// group and the renderer gets them as one field on the state snapshot.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CornerButtons {
    pub queue: bool,
    pub lyrics: bool,
    pub search: bool,
    pub repeat: bool,
    pub shuffle: bool,
    /// Move the playing file to the Trash. Drawn only for a local track.
    pub trash: bool,
    /// The playlist picker: YouTube Music playlists and saved folders.
    pub playlists: bool,
}

impl CornerButtons {
    /// Menu key and label for each button, in menu order. `flag` maps the
    /// same keys, so a new button is one row here and one arm there.
    pub const BUTTONS: [(&'static str, &'static str); 7] = [
        ("queue", "Queue"),
        ("lyrics", "Lyrics"),
        ("playlists", "Playlists"),
        ("search", "Search"),
        ("shuffle", "Shuffle"),
        ("repeat", "Repeat"),
        ("trash", "Delete (local files)"),
    ];

    pub fn flag(&mut self, key: &str) -> Option<&mut bool> {
        Some(match key {
            "queue" => &mut self.queue,
            "lyrics" => &mut self.lyrics,
            "playlists" => &mut self.playlists,
            "search" => &mut self.search,
            "shuffle" => &mut self.shuffle,
            "repeat" => &mut self.repeat,
            "trash" => &mut self.trash,
            _ => return None,
        })
    }
}

impl Default for CornerButtons {
    fn default() -> Self {
        Self {
            queue: true,
            lyrics: true,
            search: true,
            repeat: true,
            shuffle: true,
            trash: true,
            playlists: true,
        }
    }
}

/// The set kept for each skin, since how much chrome a card can carry is a
/// property of the card. Keyed by skin name — see `window::SKINS`; a retired
/// skin's entry is kept in the file but never read.
pub type SkinCorners = BTreeMap<String, CornerButtons>;

/// Accepts both shapes the file has had: the current map of skin to buttons,
/// and the single flat set every skin shared before 1.5. A flat set is read
/// into *every* skin, so upgrading keeps exactly what you had until you change
/// one of them.
///
/// The two are told apart by their values, which cannot collide: a per-skin map
/// holds objects, the flat form holds booleans. `{}` is an empty map, which
/// simply means every skin is at its default.
fn corners_from_file<'de, D>(de: D) -> Result<SkinCorners, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Shape {
        PerSkin(SkinCorners),
        Flat(CornerButtons),
    }

    Ok(match Shape::deserialize(de)? {
        Shape::PerSkin(map) => map,
        Shape::Flat(flat) => crate::window::SKINS
            .iter()
            .map(|skin| ((*skin).to_string(), flat))
            .collect(),
    })
}

/// `menuBarStrip` is a boolean, but test builds stored the name of a design
/// there. Any name other than `off` meant shown; anything else unreadable
/// means hidden, rather than failing the whole file's parse.
fn strip_from_file<'de, D>(de: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(de)? {
        serde_json::Value::Bool(shown) => shown,
        serde_json::Value::String(name) => name != "off",
        _ => false,
    })
}

/// A hand edit that is not a string reads as Automatic rather than setting the
/// whole file aside.
fn ink_from_file<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(de)? {
        serde_json::Value::String(ink) => Some(ink),
        _ => None,
    })
}

fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    26538
}
fn default_client_id() -> String {
    "PearMusicWidget".into()
}
fn default_skin() -> String {
    "stack".into()
}
fn default_always_on_top() -> bool {
    true
}
fn default_show_widget() -> bool {
    true
}
fn default_opacity() -> f64 {
    1.0
}
fn default_liquid_glass() -> bool {
    true
}
fn default_tint() -> f64 {
    1.0
}
fn default_lyrics_cache_mb() -> f64 {
    crate::lyrics_cache::DEFAULT_MB
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(rename = "clientId", default = "default_client_id")]
    pub client_id: String,
    #[serde(default)]
    pub token: Option<String>,
    /// Last window position. Size is not stored here — see `sizes`.
    #[serde(default)]
    pub bounds: Option<Position>,
    /// The width the user last left **each skin** at.
    #[serde(default)]
    pub sizes: BTreeMap<String, StoredSize>,
    /// Floating widget layout — see `window::SKINS`. A retired skin such as
    /// Classic falls back to Stack through `window::valid_skin`.
    #[serde(default = "default_skin")]
    pub skin: String,
    /// Menu-bar dropdown layout. Kept separate from `skin` in case another
    /// skin returns; today both can only be Stack.
    #[serde(rename = "panelSkin", default = "default_skin")]
    pub panel_skin: String,
    /// Whether the strip at the top centre of the menu bar is up.
    #[serde(rename = "menuBarStrip", default, deserialize_with = "strip_from_file")]
    pub menu_bar_strip: bool,
    /// The folded strip's text colour: `auto`, `cover` or `#rrggbb` (see
    /// `strip::valid_ink`); Automatic when missing or unknown.
    #[serde(rename = "stripInk", default, deserialize_with = "ink_from_file")]
    pub strip_ink: Option<String>,
    /// Whether the floating widget was left showing, so hiding it lasts
    /// across a relaunch.
    #[serde(rename = "showWidget", default = "default_show_widget")]
    pub show_widget: bool,
    #[serde(rename = "alwaysOnTop", default = "default_always_on_top")]
    pub always_on_top: bool,
    #[serde(default = "default_opacity")]
    pub opacity: f64,
    /// Use macOS's native Liquid Glass material when the runtime supports it.
    #[serde(rename = "liquidGlass", default = "default_liquid_glass")]
    pub liquid_glass: bool,
    /// How strongly the cover's colours wash the card, 0..=1.
    #[serde(default = "default_tint")]
    pub tint: f64,
    /// Default lyric shift in seconds; positive turns lines over earlier.
    #[serde(rename = "lyricsOffset", default)]
    pub lyrics_offset: f64,
    /// Absolute overrides keyed by video ID. Missing entries inherit Timing;
    /// an explicit zero overrides a nonzero default.
    #[serde(rename = "lyricsOffsetsByVideo", default)]
    pub lyrics_offsets_by_video: BTreeMap<String, f64>,
    /// Convert the lyrics to Simplified Chinese before they are shown. Applies
    /// to the words only — everything else on the card is the player's.
    #[serde(rename = "simplifyLyrics", default)]
    pub simplify_lyrics: bool,
    /// Add a numberless Jyutping line under Chinese lyrics. This is derived
    /// locally from the fetched source text and never changes the cache.
    #[serde(rename = "jyutpingLyrics", default)]
    pub jyutping_lyrics: bool,
    /// How much disk the lyrics cache may use, in megabytes. 0 turns it off —
    /// what is already on disk stays until it is emptied from the menu. See
    /// `lyrics_cache`.
    #[serde(rename = "lyricsCacheMb", default = "default_lyrics_cache_mb")]
    pub lyrics_cache_mb: f64,
    /// The panel the floating widget was last left showing, restored on the
    /// next launch — see `window::panel_is_restorable`. A search is never
    /// stored: it is a query you have finished with.
    #[serde(default)]
    pub panel: Option<String>,
    /// The same for the menu-bar dropdown, reopened each time it drops down.
    #[serde(rename = "dropdownPanel", default)]
    pub dropdown_panel: Option<String>,
    /// Which top-right toggles the card shows, per skin.
    #[serde(default, deserialize_with = "corners_from_file")]
    pub corners: SkinCorners,
    /// Seconds of stillness before the corner buttons fade out, 0 to keep them
    /// up. They are chrome over someone else's artwork, and on the smallest
    /// skin they sit right on top of the title.
    #[serde(rename = "cornersAutohide", default)]
    pub corners_autohide: f64,
    /// Display names for artist credits, applied to YouTube Music and local
    /// files alike: `{"Jaychou": "周杰伦"}`. Lyrics and cover lookups use the
    /// mapped name, which is the point — providers match on the credited name.
    #[serde(rename = "artistNames", default)]
    pub artist_names: BTreeMap<String, String>,
    /// `"local"` while a folder is the source, else YouTube Music.
    #[serde(default)]
    pub source: Option<String>,
    /// The folder playing, or last played.
    #[serde(rename = "localFolder", default)]
    pub local_folder: Option<String>,
    /// Folders offered in the Source menu, most recent first, each with the
    /// place to resume. Paths are absolute; a drive that is not mounted keeps
    /// its entry and is shown as not connected.
    #[serde(rename = "localFolders", default)]
    pub local_folders: Vec<SavedFolder>,
    /// The YouTube Music playlist last started from the playlist picker, as its
    /// browse id. Only a hint for the picker's checkmark: playing something
    /// else in YouTube Music itself does not clear it.
    #[serde(rename = "youtubePlaylist", default)]
    pub youtube_playlist: Option<String>,

    /// Anything else the file carried, preserved on write.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Settings {
    pub fn lyrics_offset_for(&self, video_id: Option<&str>) -> f64 {
        let offset = video_id
            .and_then(|id| self.lyrics_offsets_by_video.get(id))
            .copied()
            .unwrap_or(self.lyrics_offset);
        if offset.is_finite() {
            offset.clamp(-20.0, 20.0)
        } else {
            0.0
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            client_id: default_client_id(),
            token: None,
            bounds: None,
            sizes: BTreeMap::new(),
            skin: default_skin(),
            panel_skin: default_skin(),
            menu_bar_strip: false,
            show_widget: default_show_widget(),
            strip_ink: None,
            always_on_top: default_always_on_top(),
            opacity: default_opacity(),
            liquid_glass: default_liquid_glass(),
            tint: default_tint(),
            lyrics_offset: 0.0,
            lyrics_offsets_by_video: BTreeMap::new(),
            simplify_lyrics: false,
            jyutping_lyrics: false,
            lyrics_cache_mb: default_lyrics_cache_mb(),
            panel: None,
            dropdown_panel: None,
            corners: SkinCorners::new(),
            corners_autohide: 0.0,
            artist_names: BTreeMap::new(),
            source: None,
            local_folder: None,
            local_folders: Vec::new(),
            youtube_playlist: None,
            extra: BTreeMap::new(),
        }
    }
}

/// A folder the Source menu offers, and where to pick it back up.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedFolder {
    pub path: String,
    /// The track to resume, relative to `path`.
    #[serde(default)]
    pub track: Option<String>,
    #[serde(default)]
    pub position: f64,
    /// Playing only the folder's liked tracks (Source → Liked Songs).
    #[serde(rename = "likedOnly", default)]
    pub liked_only: bool,
    /// `ALL` or `ONE`. YouTube Music keeps its own modes; a folder keeps
    /// these, so repeat-one survives a switch or a relaunch.
    #[serde(default)]
    pub repeat: Option<String>,
    #[serde(default)]
    pub shuffle: bool,
}

/// Most folders the Source menu keeps.
pub const SAVED_FOLDERS_MAX: usize = 8;

impl Settings {
    /// Saved folders, with a `localFolder` from before the list existed folded
    /// in, so upgrading does not lose it.
    pub fn saved_folders(&self) -> Vec<SavedFolder> {
        let mut folders = self.local_folders.clone();
        if let Some(path) = &self.local_folder {
            if !folders.iter().any(|f| &f.path == path) {
                folders.push(SavedFolder {
                    path: path.clone(),
                    ..SavedFolder::default()
                });
            }
        }
        folders
    }

    /// Put `path` first, keeping its resume point, and cap the list.
    pub fn remember_folder(&mut self, path: &str) {
        let mut folders = self.saved_folders();
        let entry = folders
            .iter()
            .position(|f| f.path == path)
            .map(|at| folders.remove(at))
            .unwrap_or_else(|| SavedFolder {
                path: path.to_string(),
                ..SavedFolder::default()
            });
        folders.insert(0, entry);
        folders.truncate(SAVED_FOLDERS_MAX);
        self.local_folders = folders;
    }
}

/// Whether `names` has an entry for the whole credit, as `map_artist` tries
/// first.
pub fn renames_whole(names: &BTreeMap<String, String>, credit: &str) -> bool {
    names.keys().any(|from| from.trim().eq_ignore_ascii_case(credit.trim()))
}

/// The credit as it should be shown. The whole string is tried first, so a
/// mapping can name a collaboration; then each credit separately, keeping
/// the separators the source used.
pub fn map_artist(names: &BTreeMap<String, String>, artist: &str) -> String {
    let lookup = |credit: &str| {
        let credit = credit.trim();
        names
            .iter()
            .find(|(from, _)| from.trim().eq_ignore_ascii_case(credit))
            .map(|(_, to)| to.clone())
    };
    if names.is_empty() || artist.trim().is_empty() {
        return artist.to_string();
    }
    if let Some(whole) = lookup(artist) {
        return whole;
    }
    let mut out = String::new();
    let mut last = 0;
    for sep in CREDIT_SEPARATORS.find_iter(artist) {
        let credit = &artist[last..sep.start()];
        out.push_str(&lookup(credit).unwrap_or_else(|| credit.to_string()));
        out.push_str(sep.as_str());
        last = sep.end();
    }
    let credit = &artist[last..];
    out.push_str(&lookup(credit).unwrap_or_else(|| credit.to_string()));
    out
}

pub struct Store {
    path: PathBuf,
    settings: Mutex<Settings>,
    /// Held across a write, with the text last written. Each write serialises
    /// the *latest* settings, so writers racing each other cannot leave an
    /// older snapshot on disk, and an unchanged file is not rewritten.
    written: Mutex<String>,
    /// Set by `update_soon`; cleared by the one background writer it keeps
    /// going, which writes again while this is set. A burst of edits is one
    /// writer and, usually, one write of the settings as they ended up.
    dirty: AtomicBool,
}

/// `~/Library/Application Support/pear-music-widget/settings.json` — the
/// literal directory the Electron build used, so settings survive the port.
fn settings_path() -> PathBuf {
    crate::fsutil::support_dir(crate::fsutil::APP).join("settings.json")
}

/// The settings file as it stands, without touching it; `None` if it is
/// missing or does not parse. For `--doctor`, which must not move a broken
/// file aside the way `Store::load` does.
pub fn peek() -> Option<Settings> {
    let text = std::fs::read_to_string(settings_path()).ok()?;
    serde_json::from_str(&text).ok()
}

impl Store {
    pub fn load() -> Self {
        let path = settings_path();
        let settings = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Settings>(&text).unwrap_or_else(|err| {
                // The next write would replace a file we could not read with
                // defaults, losing the token, sizes and saved folders to one
                // stray comma. Keep it aside so a hand edit can be repaired.
                let stamp = crate::fsutil::unix_secs();
                let aside = path.with_extension(format!("json.broken-{stamp}"));
                eprintln!("[store] settings.json does not parse ({err}); kept as {}", aside.display());
                let _ = std::fs::rename(&path, &aside);
                Settings::default()
            }),
            Err(_) => Settings::default(),
        };
        Self {
            path,
            settings: Mutex::new(settings),
            written: Mutex::new(String::new()),
            dirty: AtomicBool::new(false),
        }
    }

    /// Read a field. Cheap enough to call freely — everything is in memory.
    pub fn get<T>(&self, read: impl FnOnce(&Settings) -> T) -> T {
        read(&self.settings.lock().expect("settings lock"))
    }

    /// Mutate and persist. Failing to write is logged, never fatal: the widget
    /// stays usable on a read-only disk, it just forgets across launches.
    pub fn update(&self, edit: impl FnOnce(&mut Settings)) {
        edit(&mut self.settings.lock().expect("settings lock"));
        if let Err(err) = self.write() {
            eprintln!("[store] failed to persist settings: {err}");
        }
    }

    /// `update` for callers on the AppKit main thread or an async worker. The
    /// edit applies now; the fsync'd write runs on a blocking thread, where a
    /// slow disk stalls nobody. One writer at a time: an edit arriving while
    /// it runs marks the store dirty again and the same writer goes round once
    /// more, so a skin switch's several edits are one write, not one each.
    /// Writes always serialise the latest settings under the write lock, so
    /// concurrent ones still leave the newest on disk, and quitting calls
    /// `flush` so nothing queued here is lost to the exit.
    pub fn update_soon(self: &Arc<Self>, edit: impl FnOnce(&mut Settings)) {
        edit(&mut self.settings.lock().expect("settings lock"));
        if self.dirty.swap(true, Ordering::SeqCst) {
            return;
        }
        let store = Arc::clone(self);
        tauri::async_runtime::spawn_blocking(move || loop {
            store.dirty.store(false, Ordering::SeqCst);
            if let Err(err) = store.write() {
                eprintln!("[store] failed to persist settings: {err}");
            }
            // An edit that landed during the write found `dirty` set and did
            // not start a writer of its own; it is this one's to write.
            if !store.dirty.load(Ordering::SeqCst) {
                break;
            }
        });
    }

    /// Write now whatever `update_soon` has not yet written. For quitting,
    /// which may come straight after the edit that queued the write.
    pub fn flush(&self) {
        if let Err(err) = self.write() {
            eprintln!("[store] failed to persist settings: {err}");
        }
    }

    /// Temporary file, sync, rename: a crash or power loss mid-write leaves the
    /// previous file intact instead of a truncated one, which `load` would
    /// replace with defaults — losing the token, sizes and saved folders.
    fn write(&self) -> std::io::Result<()> {
        let mut written = self.written.lock().expect("settings write lock");
        let text = {
            let settings = self.settings.lock().expect("settings lock");
            serde_json::to_string_pretty(&*settings)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?
        };
        if *written == text {
            return Ok(());
        }
        crate::fsutil::write_atomic(&self.path, text.as_bytes())?;
        *written = text;
        Ok(())
    }

    /// The corner buttons for one skin. A skin with no entry yet is at its
    /// defaults, which is also what a fresh install looks like — the map is
    /// only written to once a toggle is actually changed.
    pub fn corners_for(&self, skin: &str) -> CornerButtons {
        self.get(|s| s.corners.get(skin).copied().unwrap_or_default())
    }

    pub fn set_corners_for(self: &Arc<Self>, skin: &str, corners: CornerButtons) {
        self.update_soon(|s| {
            s.corners.insert(skin.to_string(), corners);
        });
    }

    pub fn http_base(&self) -> String {
        self.get(|s| format!("http://{}:{}", s.host, s.port))
    }

    pub fn ws_base(&self) -> String {
        self.get(|s| format!("ws://{}:{}", s.host, s.port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::split_credit;

    /// Test builds stored a design name in `menuBarStrip`. A file holding one
    /// must still parse, or it would be set aside and the token lost with it.
    #[test]
    fn reads_a_stored_strip_design_as_shown() {
        let read = |text: &str| serde_json::from_str::<Settings>(text).expect("parses").menu_bar_strip;
        assert!(read(r#"{ "menuBarStrip": true }"#));
        assert!(!read(r#"{ "menuBarStrip": false }"#));
        assert!(read(r#"{ "menuBarStrip": "title" }"#));
        assert!(!read(r#"{ "menuBarStrip": "off" }"#));
        assert!(!read(r#"{ "menuBarStrip": 3 }"#));
        assert!(!read("{}"));
    }

    /// A `stripInk` that is not a string reads as unset, and a missing
    /// `showWidget` as shown, rather than setting the file aside.
    #[test]
    fn reads_strip_ink_and_show_widget_leniently() {
        let read = |text: &str| serde_json::from_str::<Settings>(text).expect("parses");
        assert_eq!(read(r##"{ "stripInk": "#ff0000" }"##).strip_ink.as_deref(), Some("#ff0000"));
        assert_eq!(read(r#"{ "stripInk": 1 }"#).strip_ink, None);
        assert_eq!(read(r#"{ "stripInk": {} }"#).strip_ink, None);
        assert!(read("{}").show_widget);
        assert!(!read(r#"{ "showWidget": false }"#).show_widget);
    }

    /// A settings file that fails to parse is silently replaced by the defaults
    /// — position, sizes and the cached token with it — so the shape change on
    /// `corners` has to read the old file, not just the new one.
    #[test]
    fn reads_the_pre_1_5_flat_corner_buttons_into_every_skin() {
        let text =
            r#"{ "port": 26538, "corners": { "queue": true, "lyrics": true, "search": false } }"#;
        let settings: Settings = serde_json::from_str(text).expect("old files still parse");

        assert_eq!(settings.port, 26538);
        for skin in crate::window::SKINS {
            let corners = settings.corners.get(skin).copied().unwrap_or_default();
            assert!(corners.queue);
            assert!(!corners.search);
            // Absent from the old file, so it lands on its default rather than
            // being read as off.
            assert!(corners.repeat);
            assert!(corners.shuffle);
        }
    }

    #[test]
    fn reads_and_keeps_a_set_per_skin() {
        let text = r#"{ "corners": { "classic": { "search": false }, "stack": {} } }"#;
        let settings: Settings = serde_json::from_str(text).expect("current shape parses");

        assert!(!settings.corners["classic"].search);
        assert!(settings.corners["classic"].queue);
        assert!(settings.corners["stack"].search);

        let round_tripped: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).expect("serialises"))
                .expect("re-parses");
        assert_eq!(round_tripped.corners, settings.corners);
    }

    #[test]
    fn a_file_with_no_corners_key_leaves_every_skin_at_its_default() {
        let settings: Settings = serde_json::from_str("{}").expect("parses");
        assert!(settings.liquid_glass);
        assert!(!settings.jyutping_lyrics);
        assert!(settings.corners.is_empty());
        assert_eq!(
            CornerButtons::default(),
            settings.corners.get("stack").copied().unwrap_or_default()
        );
    }

    #[test]
    fn persists_the_jyutping_toggle_and_unknown_keys() {
        let text = r#"{ "liquidGlass": false, "jyutpingLyrics": true, "futureOption": 7 }"#;
        let settings: Settings = serde_json::from_str(text).expect("parses");
        assert!(!settings.liquid_glass);
        assert!(settings.jyutping_lyrics);
        assert_eq!(settings.extra["futureOption"], 7);

        let encoded = serde_json::to_string(&settings).expect("serialises");
        assert!(encoded.contains("\"liquidGlass\":false"));
        assert!(encoded.contains("\"jyutpingLyrics\":true"));
        assert!(encoded.contains("\"futureOption\":7"));
    }

    #[test]
    fn artist_names_map_whole_credits_and_parts() {
        let names = BTreeMap::from([
            ("Jaychou".to_string(), "周杰伦".to_string()),
            ("JJ Lin".to_string(), "林俊杰".to_string()),
        ]);
        assert_eq!(map_artist(&names, "Jaychou"), "周杰伦");
        assert_eq!(map_artist(&names, "jaychou"), "周杰伦");
        assert_eq!(map_artist(&names, "JJ Lin, A-Mei"), "林俊杰, A-Mei");
        assert_eq!(map_artist(&names, "Jaychou & JJ Lin"), "周杰伦 & 林俊杰");
        assert_eq!(split_credit("MC HotDog熱狗/艾怡良, "), ["MC HotDog熱狗", "艾怡良"]);
        assert_eq!(split_credit("Dan + Shay"), ["Dan + Shay"]);
        // Substrings are not credits.
        assert_eq!(map_artist(&names, "Jaychoux"), "Jaychoux");
        assert_eq!(map_artist(&BTreeMap::new(), "Jaychou"), "Jaychou");
    }


    #[test]
    fn remembered_folders_move_to_the_front_and_keep_their_place() {
        let mut settings = Settings::default();
        settings.local_folder = Some("/Volumes/A/Music".into());
        settings.remember_folder("/Volumes/B/Podcasts");
        assert_eq!(settings.local_folders[0].path, "/Volumes/B/Podcasts");
        assert_eq!(settings.local_folders[1].path, "/Volumes/A/Music");

        settings.local_folders[1].track = Some("Album/a.flac".into());
        settings.local_folders[1].position = 42.0;
        settings.remember_folder("/Volumes/A/Music");
        assert_eq!(settings.local_folders[0].path, "/Volumes/A/Music");
        assert_eq!(settings.local_folders[0].position, 42.0);
        assert_eq!(settings.local_folders.len(), 2);

        for i in 0..20 {
            settings.remember_folder(&format!("/tmp/{i}"));
        }
        assert_eq!(settings.local_folders.len(), SAVED_FOLDERS_MAX);
    }

    #[test]
    fn source_fields_read_from_the_old_loose_keys() {
        let settings: Settings = serde_json::from_str(
            r#"{"source":"local","localFolder":"/Volumes/X/Music"}"#,
        )
        .unwrap();
        assert_eq!(settings.source.as_deref(), Some("local"));
        assert_eq!(settings.saved_folders()[0].path, "/Volumes/X/Music");
        assert!(!settings.extra.contains_key("source"));
    }

}
