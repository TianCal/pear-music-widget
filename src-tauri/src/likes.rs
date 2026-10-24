//! Like and dislike for local files.
//!
//! Ratings belong to the library, and a library on an external drive moves
//! between Macs, so they live **on the drive**: `<root>/.pear-music/likes.json`.
//! When the drive cannot be written (read-only, unplugged mid-session) a copy
//! goes to `~/Library/Application Support/pear-music-widget/likes/<folder key>.json`.
//!
//! **Both files are always read and merged entry by entry, newest `at` wins**,
//! so a rating saved to the fallback while the drive was away is not lost when
//! the drive file is preferred again. Clearing a rating writes a dated
//! tombstone rather than deleting the entry, so the merge cannot resurrect it.
//! Every save tries the drive first; once the drive takes a write the fallback
//! is removed. A drive file that exists but cannot be read or parsed is never
//! overwritten — saves go to the fallback until it is readable again.
//!
//! Entries are keyed by path relative to the root, with a fingerprint (size,
//! duration, title, artist) beside it. A file renamed or moved inside the folder
//! loses its key but keeps its fingerprint, and `load` re-links it instead of
//! forgetting the rating.
//!
//! States use YouTube Music's strings — `LIKE`, `DISLIKE`, and no rating for
//! `INDIFFERENT` — so the renderer's heart needs no second vocabulary.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::fsutil::write_atomic;
use crate::local::Track;

const DIR: &str = ".pear-music";
const FILE: &str = "likes.json";
const VERSION: u32 = 1;
/// Entries for files gone from the folder — deleted outside the app, or a
/// tombstone — are kept this long in case the file comes back, then dropped.
const ORPHAN_DAYS: u64 = 180;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Rating {
    Like,
    Dislike,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Entry {
    /// `None` is a tombstone: cleared at `at`.
    #[serde(default)]
    state: Option<Rating>,
    size: u64,
    duration: f64,
    title: String,
    artist: String,
    at: u64,
}

impl Entry {
    /// Same file content, whatever it is called now. Size alone collides
    /// rarely; with duration and tags it does not in practice.
    fn matches(&self, track: &Track) -> bool {
        self.size == track.size
            && (self.duration - track.duration).abs() < 1.0
            && self.title == track.title
            && self.artist == track.artist
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    version: u32,
    tracks: BTreeMap<String, Entry>,
}

/// What reading one of the two files found.
enum Read {
    Missing,
    Found(File),
    /// Present but unreadable or unparseable: keep it, never overwrite it.
    Broken,
}

fn read(path: &Path) -> Read {
    match std::fs::read(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Read::Missing,
        Err(_) => Read::Broken,
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(file) => Read::Found(file),
            Err(_) => Read::Broken,
        },
    }
}

/// One folder's ratings, keyed in memory by track id.
pub struct Likes {
    root: PathBuf,
    entries: BTreeMap<String, Entry>,
    by_id: HashMap<String, Rating>,
    /// The drive's file exists but could not be read: do not write over it.
    drive_broken: bool,
}

/// A pending write, produced under the session lock and performed outside it.
pub struct Save {
    json: Vec<u8>,
    drive: PathBuf,
    fallback: PathBuf,
    drive_broken: bool,
}

impl Save {
    /// Drive first; on failure, or when the drive's file is broken, the
    /// fallback. A successful drive write retires the fallback, whose entries
    /// were merged in at load. Blocking IO.
    pub fn write(self) {
        if !self.drive_broken && write_atomic(&self.drive, &self.json).is_ok() {
            let _ = std::fs::remove_file(&self.fallback);
            return;
        }
        if let Err(err) = write_atomic(&self.fallback, &self.json) {
            eprintln!("likes: cannot save ratings: {err}");
        }
    }
}

fn drive_file(root: &Path) -> PathBuf {
    root.join(DIR).join(FILE)
}

fn fallback_file(root: &Path) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let name = crate::local::folder_key(root);
    PathBuf::from(home)
        .join("Library/Application Support/pear-music-widget/likes")
        .join(format!("{name}.json"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn relative(root: &Path, track: &Track) -> Option<String> {
    track
        .path
        .strip_prefix(root)
        .ok()
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
}

/// Newest `at` wins, per key.
fn merge(into: &mut BTreeMap<String, Entry>, from: BTreeMap<String, Entry>) {
    for (key, entry) in from {
        match into.get(&key) {
            Some(held) if held.at >= entry.at => {}
            _ => {
                into.insert(key, entry);
            }
        }
    }
}

impl Likes {
    /// Read both copies, merge them, attach ratings to `tracks` and re-link any
    /// whose file has moved. Blocking IO.
    pub fn load(root: &Path, tracks: &[Track]) -> Self {
        let mut entries = BTreeMap::new();
        let mut drive_broken = false;
        let mut stale_drive = false;
        match read(&drive_file(root)) {
            Read::Found(file) => entries = file.tracks,
            Read::Missing => stale_drive = true,
            Read::Broken => drive_broken = true,
        }
        if let Read::Found(file) = read(&fallback_file(root)) {
            // Anything in the fallback is something the drive missed.
            stale_drive |= !file.tracks.is_empty();
            merge(&mut entries, file.tracks);
        }

        let mut likes = Self {
            root: root.to_path_buf(),
            entries,
            by_id: HashMap::new(),
            drive_broken,
        };
        let changed = likes.attach(tracks);
        if (changed || stale_drive) && !likes.entries.is_empty() {
            likes.save().write();
        }
        likes
    }

    /// Map entries onto tracks, re-linking moved files and pruning long-gone
    /// ones. Returns whether the stored entries changed.
    fn attach(&mut self, tracks: &[Track]) -> bool {
        let rel: Vec<Option<String>> = tracks.iter().map(|t| relative(&self.root, t)).collect();
        let present: HashSet<&str> = rel.iter().flatten().map(String::as_str).collect();
        let orphans: Vec<String> = self
            .entries
            .keys()
            .filter(|key| !present.contains(key.as_str()))
            .cloned()
            .collect();

        // Each orphan re-links to at most one unrated file, and each file
        // takes at most one orphan; existing keys are never overwritten.
        let mut moved: Vec<(String, String)> = Vec::new();
        if !orphans.is_empty() {
            for (track, key) in tracks.iter().zip(&rel) {
                let Some(key) = key else { continue };
                if self.entries.contains_key(key) || moved.iter().any(|(_, to)| to == key) {
                    continue;
                }
                let free = |o: &&String| !moved.iter().any(|(from, _)| from == *o);
                if let Some(orphan) = orphans
                    .iter()
                    .filter(free)
                    .find(|o| self.entries[*o].state.is_some() && self.entries[*o].matches(track))
                {
                    moved.push((orphan.clone(), key.clone()));
                }
            }
        }
        for (from, to) in &moved {
            if let Some(entry) = self.entries.remove(from) {
                self.entries.insert(to.clone(), entry);
            }
        }

        let cutoff = now().saturating_sub(ORPHAN_DAYS * 24 * 60 * 60);
        let before = self.entries.len();
        self.entries.retain(|key, entry| present.contains(key.as_str()) || entry.at >= cutoff);
        let pruned = self.entries.len() != before;

        self.by_id = tracks
            .iter()
            .zip(&rel)
            .filter_map(|(track, key)| {
                let state = self.entries.get(key.as_ref()?)?.state?;
                Some((track.id.clone(), state))
            })
            .collect();
        !moved.is_empty() || pruned
    }

    pub fn get(&self, id: &str) -> Option<Rating> {
        self.by_id.get(id).copied()
    }

    pub fn is_disliked(&self, id: &str) -> bool {
        self.get(id) == Some(Rating::Dislike)
    }

    pub fn is_liked(&self, id: &str) -> bool {
        self.get(id) == Some(Rating::Like)
    }

    pub fn liked_count(&self) -> usize {
        self.by_id.values().filter(|r| **r == Rating::Like).count()
    }

    /// Set or clear (`None`) a rating in memory. Returns the write to perform
    /// — outside whatever lock holds `self`.
    #[must_use]
    pub fn set(&mut self, track: &Track, rating: Option<Rating>) -> Option<Save> {
        let key = relative(&self.root, track)?;
        self.entries.insert(
            key,
            Entry {
                state: rating,
                size: track.size,
                duration: track.duration,
                title: track.title.clone(),
                artist: track.artist.clone(),
                at: now(),
            },
        );
        match rating {
            Some(state) => self.by_id.insert(track.id.clone(), state),
            None => self.by_id.remove(&track.id),
        };
        Some(self.save())
    }

    /// Follow a track whose file is gone (trashed): clear its rating.
    #[must_use]
    pub fn forget(&mut self, track: &Track) -> Option<Save> {
        let key = relative(&self.root, track)?;
        if self.entries.get(&key)?.state.is_none() {
            return None;
        }
        self.set(track, None)
    }

    fn save(&self) -> Save {
        let file = File {
            version: VERSION,
            tracks: self.entries.clone(),
        };
        Save {
            json: serde_json::to_vec_pretty(&file).unwrap_or_default(),
            drive: drive_file(&self.root),
            fallback: fallback_file(&self.root),
            drive_broken: self.drive_broken,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(root: &Path, rel: &str, title: &str, size: u64) -> Track {
        Track {
            id: format!("local-{rel}"),
            path: root.join(rel),
            title: title.into(),
            artist: "林俊杰".into(),
            album: String::new(),
            duration: 200.0,
            has_art: false,
            size,
        }
    }

    /// A scratch folder and its own fallback file, removed again on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("pmw-likes-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("Album")).unwrap();
            let _ = std::fs::remove_file(fallback_file(&dir));
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(fallback_file(&self.0));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rate(likes: &mut Likes, track: &Track, rating: Option<Rating>) {
        likes.set(track, rating).unwrap().write();
    }

    #[test]
    fn ratings_round_trip_through_the_drive_file() {
        let root = Scratch::new("roundtrip");
        let a = track(&root.0, "Album/a.flac", "A", 4);

        let mut likes = Likes::load(&root.0, std::slice::from_ref(&a));
        assert_eq!(likes.get(&a.id), None);
        rate(&mut likes, &a, Some(Rating::Like));
        assert!(drive_file(&root.0).exists());
        assert_eq!(Likes::load(&root.0, std::slice::from_ref(&a)).get(&a.id), Some(Rating::Like));

        rate(&mut likes, &a, None);
        assert_eq!(Likes::load(&root.0, &[a.clone()]).get(&a.id), None);
    }

    #[test]
    fn a_renamed_file_keeps_its_rating() {
        let root = Scratch::new("rename");
        let old = track(&root.0, "Album/old.flac", "Song", 10);
        let mut likes = Likes::load(&root.0, std::slice::from_ref(&old));
        rate(&mut likes, &old, Some(Rating::Dislike));

        let new = track(&root.0, "Album/new.flac", "Song", 10);
        let likes = Likes::load(&root.0, std::slice::from_ref(&new));
        assert!(likes.is_disliked(&new.id));
        assert!(likes.entries.contains_key("Album/new.flac"));
        assert!(!likes.entries.contains_key("Album/old.flac"));
    }

    #[test]
    fn a_different_file_does_not_inherit_a_rating() {
        let root = Scratch::new("different");
        let old = track(&root.0, "Album/old.flac", "Song", 3);
        let mut likes = Likes::load(&root.0, std::slice::from_ref(&old));
        rate(&mut likes, &old, Some(Rating::Like));

        let other = track(&root.0, "Album/other.flac", "Other", 14);
        assert_eq!(Likes::load(&root.0, &[other.clone()]).get(&other.id), None);
    }

    #[test]
    fn ratings_saved_to_the_fallback_survive_the_drive_returning() {
        let root = Scratch::new("fallback");
        let a = track(&root.0, "Album/a.flac", "A", 1);
        let b = track(&root.0, "Album/b.flac", "B", 2);
        let mut likes = Likes::load(&root.0, &[a.clone(), b.clone()]);
        rate(&mut likes, &a, Some(Rating::Like));

        // The drive was away for this one: it only reached the fallback.
        let save = likes.set(&b, Some(Rating::Dislike)).unwrap();
        write_atomic(&save.fallback, &save.json).unwrap();
        let mut drive = read_file(&drive_file(&root.0));
        drive.tracks.remove("Album/b.flac");
        write_atomic(&drive_file(&root.0), &serde_json::to_vec(&drive).unwrap()).unwrap();

        let back = Likes::load(&root.0, &[a.clone(), b.clone()]);
        assert_eq!(back.get(&a.id), Some(Rating::Like));
        assert_eq!(back.get(&b.id), Some(Rating::Dislike));
        // Merged onto the drive, and the fallback retired.
        assert!(read_file(&drive_file(&root.0)).tracks.contains_key("Album/b.flac"));
        assert!(!fallback_file(&root.0).exists());
    }

    #[test]
    fn a_cleared_rating_is_not_resurrected_by_an_older_copy() {
        let root = Scratch::new("tombstone");
        let a = track(&root.0, "Album/a.flac", "A", 1);
        let mut likes = Likes::load(&root.0, std::slice::from_ref(&a));
        let liked = likes.set(&a, Some(Rating::Like)).unwrap();
        let mut old = read_json(&liked.json);
        liked.write();
        // An older copy in the fallback still says LIKE.
        for entry in old.tracks.values_mut() {
            entry.at -= 100;
        }
        rate(&mut likes, &a, None);
        write_atomic(&fallback_file(&root.0), &serde_json::to_vec(&old).unwrap()).unwrap();

        assert_eq!(Likes::load(&root.0, &[a.clone()]).get(&a.id), None);
    }

    #[test]
    fn a_broken_drive_file_is_never_overwritten() {
        let root = Scratch::new("broken");
        let a = track(&root.0, "Album/a.flac", "A", 1);
        write_atomic(&drive_file(&root.0), b"{ not json").unwrap();
        let mut likes = Likes::load(&root.0, std::slice::from_ref(&a));
        rate(&mut likes, &a, Some(Rating::Like));
        assert_eq!(std::fs::read(drive_file(&root.0)).unwrap(), b"{ not json");
        assert!(fallback_file(&root.0).exists());
    }

    fn read_file(path: &Path) -> File {
        match read(path) {
            Read::Found(file) => file,
            _ => panic!("no readable file at {}", path.display()),
        }
    }

    fn read_json(bytes: &[u8]) -> File {
        serde_json::from_slice(bytes).unwrap()
    }
}
