//! Hand-editable settings, kept at the same path the Electron build used so an
//! upgrade keeps your position, skins and cached token:
//!
//!   ~/Library/Application Support/pear-music-widget/settings.json
//!
//! Unknown keys are round-tripped rather than dropped — the file is documented
//! as editable, and silently eating a key someone added is worse than carrying it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Position {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct StoredSize {
    pub width: f64,
}

/// Which of the three top-right toggles the card shows. All on by default —
/// this exists for people who want the chrome back off a 300×110 card, not as a
/// feature to opt into.
///
/// One struct rather than three loose booleans, so `settings.json` reads as a
/// group and the renderer gets them as one field on the state snapshot.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CornerButtons {
    pub queue: bool,
    pub lyrics: bool,
    pub search: bool,
}

impl Default for CornerButtons {
    fn default() -> Self {
        Self {
            queue: true,
            lyrics: true,
            search: true,
        }
    }
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
    "classic".into()
}
fn default_always_on_top() -> bool {
    true
}
fn default_opacity() -> f64 {
    1.0
}
fn default_tint() -> f64 {
    1.0
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
    /// Floating widget layout — see `window::SKINS`.
    #[serde(default = "default_skin")]
    pub skin: String,
    /// Menu-bar dropdown layout, chosen independently of `skin`.
    #[serde(rename = "panelSkin", default = "default_skin")]
    pub panel_skin: String,
    #[serde(rename = "alwaysOnTop", default = "default_always_on_top")]
    pub always_on_top: bool,
    #[serde(default = "default_opacity")]
    pub opacity: f64,
    /// How strongly the cover's colours wash the card, 0..=1.
    #[serde(default = "default_tint")]
    pub tint: f64,
    /// Seconds to shift the rolling lyrics by, positive meaning the lines turn
    /// over *earlier*. LRC timings and the player's clock disagree by a beat
    /// often enough to be worth a knob, and the mismatch is usually the same
    /// wherever it comes from — so this is set once and kept across tracks.
    #[serde(rename = "lyricsOffset", default)]
    pub lyrics_offset: f64,
    /// Convert the lyrics to Simplified Chinese before they are shown. Applies
    /// to the words only — everything else on the card is the player's.
    #[serde(rename = "simplifyLyrics", default)]
    pub simplify_lyrics: bool,
    /// The panel the floating widget was last left showing, restored on the
    /// next launch — see `window::panel_is_restorable`. A search is never
    /// stored: it is a query you have finished with. Nor is the dropdown's
    /// panel, which collapses on blur by design.
    #[serde(default)]
    pub panel: Option<String>,
    /// Which top-right toggles the card shows.
    #[serde(default)]
    pub corners: CornerButtons,
    /// Seconds of stillness before the corner buttons fade out, 0 to keep them
    /// up. They are chrome over someone else's artwork, and on the smallest
    /// skin they sit right on top of the title.
    #[serde(rename = "cornersAutohide", default)]
    pub corners_autohide: f64,

    /// Anything else the file carried, preserved on write.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
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
            always_on_top: default_always_on_top(),
            opacity: default_opacity(),
            tint: default_tint(),
            lyrics_offset: 0.0,
            simplify_lyrics: false,
            panel: None,
            corners: CornerButtons::default(),
            corners_autohide: 0.0,
            extra: BTreeMap::new(),
        }
    }
}

pub struct Store {
    path: PathBuf,
    settings: Mutex<Settings>,
}

/// `~/Library/Application Support/pear-music-widget` — the literal directory the
/// Electron build used (Electron derives it from the package name, not the
/// bundle id), so settings survive the port.
fn settings_path() -> PathBuf {
    // PROTOTYPE HOOK. Lets several copies run side by side without fighting
    // over one settings file. Unset in normal use; delete with the prototype.
    if let Ok(dir) = std::env::var("PMW_SETTINGS_DIR") {
        return PathBuf::from(dir).join("settings.json");
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join("Library/Application Support/pear-music-widget")
        .join("settings.json")
}

impl Store {
    pub fn load() -> Self {
        let path = settings_path();
        let settings = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
            .unwrap_or_default();
        Self {
            path,
            settings: Mutex::new(settings),
        }
    }

    /// Read a field. Cheap enough to call freely — everything is in memory.
    pub fn get<T>(&self, read: impl FnOnce(&Settings) -> T) -> T {
        read(&self.settings.lock().expect("settings lock"))
    }

    /// Mutate and persist. Failing to write is logged, never fatal: the widget
    /// stays usable on a read-only disk, it just forgets across launches.
    pub fn update(&self, edit: impl FnOnce(&mut Settings)) {
        let snapshot = {
            let mut settings = self.settings.lock().expect("settings lock");
            edit(&mut settings);
            settings.clone()
        };
        if let Err(err) = self.write(&snapshot) {
            eprintln!("[store] failed to persist settings: {err}");
        }
    }

    fn write(&self, settings: &Settings) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(settings)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        std::fs::write(&self.path, text)
    }

    pub fn http_base(&self) -> String {
        self.get(|s| format!("http://{}:{}", s.host, s.port))
    }

    pub fn ws_base(&self) -> String {
        self.get(|s| format!("ws://{}:{}", s.host, s.port))
    }
}
