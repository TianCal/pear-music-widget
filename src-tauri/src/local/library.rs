//! The folder on disk: listing it, reading tags into `Track`s and the index
//! that remembers them between launches, so a relaunch lists a 360-file drive
//! in a quarter second instead of re-reading every tag cold.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use lofty::prelude::*;
use serde::{Deserialize, Serialize};

use crate::state::Song;

/// What the enabled symphonia features decode: FLAC, WAV, MP3, AAC and ALAC in
/// MP4, Vorbis in Ogg, AIFF.
const EXTENSIONS: [&str; 7] = ["flac", "wav", "mp3", "m4a", "aac", "ogg", "aiff"];

/// Prefix for local ids. Also keeps them inside `lyrics_cache::safe_name`.
pub const ID_PREFIX: &str = "local-";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    /// As tagged, or the top folder's name when untagged — never mapped.
    pub artist: String,
    pub album: String,
    pub duration: f64,
    /// Embedded picture or sidecar image found at scan time. Without one the
    /// file is never reopened for art — `artwork::find` is asked instead.
    /// The index holds only `None` or `Embedded`; the sidecar is found per
    /// scan, since it can come and go without the audio file changing.
    #[serde(default, alias = "has_art", deserialize_with = "art_field")]
    pub art: Art,
    /// Bytes on disk, from the listing. Part of the likes fingerprint.
    #[serde(default)]
    pub size: u64,
}

/// What a track's own cover is, so one `cover.jpg` shared by an album is
/// decoded once for the album rather than once per row.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Art {
    #[default]
    None,
    /// A picture in the file's own tags.
    Embedded,
    /// A `cover.jpg` (or the like) beside the file.
    Sidecar(PathBuf),
}

/// An index written before `Art` stored `has_art`, a bool that meant
/// embedded — the sidecar was always layered on at scan time. Read it as
/// such so those files parse and need no tag re-read.
fn art_field<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Art, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Field {
        Flag(bool),
        Kind(Art),
    }
    Ok(match Field::deserialize(d)? {
        Field::Flag(true) => Art::Embedded,
        Field::Flag(false) => Art::None,
        Field::Kind(art) => art,
    })
}

impl Track {
    /// As `Core` shows it: raw credit kept, `artistNames` applied.
    pub fn song(&self, names: &BTreeMap<String, String>) -> Song {
        Song {
            video_id: self.id.clone(),
            title: self.title.clone(),
            artist: crate::store::map_artist(names, &self.artist),
            raw_artist: self.artist.clone(),
            album: self.album.clone(),
            song_duration: self.duration,
            image_src: None,
            url: None,
            media_type: Some("LOCAL".into()),
            ..Song::default()
        }
    }
}

/// Stable across launches so lyrics-cache entries and timing overrides stick.
fn id_for(path: &Path) -> String {
    let hash = crate::fsutil::stable_hash(&path.to_string_lossy());
    format!("{ID_PREFIX}{hash:016x}")
}

/// `2001 - 范特西` → `范特西`: the organiser's year prefix is not album text.
fn album_from_dir(name: &str) -> String {
    match name.split_once(" - ") {
        Some((year, rest)) if year.len() == 4 && year.chars().all(|c| c.is_ascii_digit()) => {
            rest.trim().to_string()
        }
        _ => name.trim_start_matches('_').to_string(),
    }
}

/// Tag readers working in parallel on a cold scan. A USB SSD answers several
/// small reads at once far faster than one after another.
const SCAN_THREADS: usize = 4;

/// A file as listed: enough to tell whether its indexed tags are still good.
struct Listed {
    path: PathBuf,
    size: u64,
    modified: u64,
}

/// One entry of the on-disk library index.
#[derive(Serialize, Deserialize)]
struct Indexed {
    size: u64,
    modified: u64,
    track: Track,
}

/// `~/Library/Caches/pear-music-widget/library/<hash of root>.json`.
/// A stable file-name-safe name for a folder, shared by the library index and
/// the likes fallback.
pub fn folder_key(root: &Path) -> String {
    id_for(root).trim_start_matches(ID_PREFIX).to_string()
}

fn index_path(root: &Path) -> PathBuf {
    crate::fsutil::cache_dir("library").join(format!("{}.json", folder_key(root)))
}

/// Every audio file under `root`, tags read only for files the index does not
/// already know at this size and modification time. Listing a 360-file drive
/// is a quarter second; reading every tag cold is ten or more, which is the
/// difference between the widget being ready and not at launch.
pub fn scan(root: &Path) -> Vec<Track> {
    let mut files = Vec::new();
    collect(root, &mut files);
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let index_file = index_path(root);
    let mut index: HashMap<PathBuf, Indexed> = std::fs::read(&index_file)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<Indexed>>(&bytes).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|entry| (entry.track.path.clone(), entry))
        .collect();
    let known = |file: &Listed, index: &HashMap<PathBuf, Indexed>| {
        index
            .get(&file.path)
            .is_some_and(|entry| entry.size == file.size && entry.modified == file.modified)
    };

    // Sidecars are per folder: look once per folder, not once per file.
    let sidecars: HashMap<PathBuf, PathBuf> = files
        .iter()
        .filter_map(|file| file.path.parent())
        .collect::<HashSet<_>>()
        .into_iter()
        .filter_map(|dir| Some((dir.to_path_buf(), sidecar_in(dir)?)))
        .collect();

    let stale: Vec<&Listed> = files.iter().filter(|file| !known(file, &index)).collect();
    // Each result travels with its own file, and a tag reader that panics on
    // one odd file costs that file its tags rather than shifting every later
    // track onto the wrong path.
    let fresh: Vec<(&Listed, Track)> = std::thread::scope(|scope| {
        let chunk = stale.len().div_ceil(SCAN_THREADS).max(1);
        let workers: Vec<_> = stale
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|file| {
                            let track = std::panic::catch_unwind(|| read_track(root, &file.path))
                                .unwrap_or_else(|_| untagged_track(root, &file.path));
                            (*file, track)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap_or_default())
            .collect()
    });
    let changed = !fresh.is_empty() || index.len() != files.len();
    for (file, track) in fresh {
        index.insert(
            file.path.clone(),
            Indexed {
                size: file.size,
                modified: file.modified,
                track,
            },
        );
    }

    let tracks: Vec<Track> = files
        .iter()
        .filter_map(|file| index.get(&file.path))
        .map(|entry| {
            let mut track = entry.track.clone();
            if track.art == Art::None {
                if let Some(sidecar) = track.path.parent().and_then(|dir| sidecars.get(dir)) {
                    track.art = Art::Sidecar(sidecar.clone());
                }
            }
            track.size = entry.size;
            track
        })
        .collect();

    if changed {
        let entries: Vec<&Indexed> = files.iter().filter_map(|file| index.get(&file.path)).collect();
        if let Ok(json) = serde_json::to_vec(&entries) {
            // The sorter and the player can scan one folder at once; unique
            // temp names keep their writes from tearing each other.
            let _ = crate::fsutil::write_atomic(&index_file, &json);
        }
    }
    tracks
}

fn collect(dir: &Path, out: &mut Vec<Listed>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        // `._*` are AppleDouble shadows on exFAT drives, not audio.
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            collect(&path, out);
            continue;
        }
        let audio = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
        if !audio {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let modified = meta
            .modified()
            .ok()
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(Listed {
            path,
            size: meta.len(),
            modified,
        });
    }
}

/// What the path alone says about a file: title, folder artist and album.
fn untagged_track(root: &Path, path: &Path) -> Track {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // The first folder under the root is the artist in an Artist/Album tree —
    // but only a folder: a file sitting in the root has none.
    let top = path
        .strip_prefix(root)
        .ok()
        .filter(|rel| rel.components().count() >= 2)
        .and_then(|rel| rel.components().next())
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .unwrap_or_default();

    Track {
        id: id_for(path),
        path: path.to_path_buf(),
        title: stem,
        artist: top,
        album: album_from_dir(&parent),
        duration: 0.0,
        art: Art::None,
        size: 0,
    }
}

fn read_track(root: &Path, path: &Path) -> Track {
    let mut track = untagged_track(root, path);
    if let Ok(file) = lofty::read_from_path(path) {
        track.duration = file.properties().duration().as_secs_f64();
        if file.tags().iter().any(|tag| !tag.pictures().is_empty()) {
            track.art = Art::Embedded;
        }
        if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
            if let Some(title) = tag.title().filter(|t| !t.trim().is_empty()) {
                track.title = title.trim().to_string();
            }
            if let Some(artist) = tag.artist().filter(|t| !t.trim().is_empty()) {
                track.artist = artist.trim().to_string();
            }
            // Rip-site watermarks ("…-MOOFEEL.COM") are not album names.
            let watermark = |t: &str| {
                let t = t.to_ascii_lowercase();
                t.contains(".com") || t.contains("www.")
            };
            if let Some(album) = tag
                .album()
                .filter(|t| !t.trim().is_empty() && !watermark(t))
            {
                track.album = album.trim().to_string();
            }
        }
    }
    track
}

fn sidecar_in(dir: &Path) -> Option<PathBuf> {
    ["cover", "folder", "front", "Cover", "Folder", "Front"]
        .iter()
        .flat_map(|name| ["jpg", "jpeg", "png"].map(|ext| dir.join(format!("{name}.{ext}"))))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::art::embedded_art;

    #[test]
    fn strips_year_prefix() {
        assert_eq!(album_from_dir("2001 - 范特西"), "范特西");
        assert_eq!(album_from_dir("_单曲与合作"), "单曲与合作");
        assert_eq!(album_from_dir("Live - 2004"), "Live - 2004");
    }

    #[test]
    fn ids_are_cache_safe() {
        let id = id_for(Path::new("/Volumes/X/双截棍.wav"));
        assert!(id.starts_with(ID_PREFIX));
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn an_old_index_with_has_art_still_parses() {
        let entry = |art: &str| {
            format!(
                r#"{{"size":1,"modified":2,"track":{{"id":"local-a","path":"/m/a.flac","title":"A","artist":"B","album":"C","duration":1.0,{art}}}}}"#
            )
        };
        let old: Indexed = serde_json::from_str(&entry(r#""has_art":true"#)).unwrap();
        assert_eq!(old.track.art, Art::Embedded);
        let old: Indexed = serde_json::from_str(&entry(r#""has_art":false"#)).unwrap();
        assert_eq!(old.track.art, Art::None);
        let new: Indexed = serde_json::from_str(&entry(r#""art":"embedded""#)).unwrap();
        assert_eq!(new.track.art, Art::Embedded);
        let json = serde_json::to_string(&new).unwrap();
        assert!(json.contains(r#""art":"embedded""#), "{json}");
    }

    /// `PMW_SCAN=/path cargo test scan_folder -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn scan_folder() {
        let root = std::env::var("PMW_SCAN").expect("PMW_SCAN");
        let tracks = scan(Path::new(&root));
        for track in tracks.iter().take(400) {
            let art = embedded_art(&track.path).map(|b| b.len()).unwrap_or(0);
            println!(
                "{} | {} | {} | {:.0}s | art {}",
                track.title, track.artist, track.album, track.duration, art
            );
        }
        println!("{} tracks", tracks.len());
    }
}

/// `PMW_SCAN=/path cargo test local_costs -- --ignored --nocapture`
#[cfg(test)]
mod costs {
    use super::*;
    use crate::local::art::embedded_art;
    use rodio::Source;
    use std::time::Instant;

    #[test]
    #[ignore]
    fn local_costs() {
        let root = std::env::var("PMW_SCAN").expect("PMW_SCAN");
        let at = Instant::now();
        let tracks = scan(Path::new(&root));
        println!("scan {} tracks: {:?}", tracks.len(), at.elapsed());

        let at = Instant::now();
        let with_art = tracks.iter().take(60).filter(|t| embedded_art(&t.path).is_some()).count();
        println!("embedded_art x60 ({with_art} hits): {:?}", at.elapsed());

        let hires = tracks.iter().find(|t| t.title.contains("LP24")).expect("24/96 track");
        let at = Instant::now();
        let decoder = rodio::Decoder::try_from(std::fs::File::open(&hires.path).unwrap()).unwrap();
        let (rate, channels) = (decoder.sample_rate(), decoder.channels());
        let samples = decoder.take((rate as usize) * channels as usize * 10).count();
        println!("decode 10s of {} Hz x{}: {:?} ({samples} samples)", rate, channels, at.elapsed());

        if let Some(bytes) = tracks.iter().find_map(|t| embedded_art(&t.path)) {
            let at = Instant::now();
            let _ = crate::artwork::resize(&bytes, 320);
            println!("resize embedded {} KB to 320: {:?}", bytes.len() / 1024, at.elapsed());
        }
    }
}
