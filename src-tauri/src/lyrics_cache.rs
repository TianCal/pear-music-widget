//! The words, kept on disk between launches, under a size the user sets.
//!
//! `lyrics.rs` already remembers the last 60 lookups in memory, which covers an
//! evening. This covers everything after it: relaunching used to re-ask LRCLib
//! for the same track, and the fourth matching tier — YouTube Music's own timed
//! lyrics — is three requests deep before it answers. A hit here costs one file
//! read and no network at all.
//!
//! ```text
//!   ~/Library/Caches/pear-music-widget/lyrics/<videoId>.json
//! ```
//!
//! `~/Library/Caches` rather than the Application Support directory the settings
//! live in, deliberately: this is derived data that can be thrown away, and that
//! is the directory macOS is allowed to throw away under disk pressure.
//!
//! **Misses are cached too, and they expire.** A track LRCLib has never heard of
//! should not cost four requests on every launch — but the answer can change, so
//! a stored miss is ignored once it is a week old. A stored *hit* never expires:
//! lyrics do not change, and the file is the only copy that costs nothing.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::lyrics::{how_from, LyricLine, Lyrics};

/// Megabytes, and what the menu offers. `0` is off — nothing is read and
/// nothing is written, and what is already there stays until it is emptied.
pub const SIZES: [(u32, &str); 5] = [
    (0, "Off"),
    (10, "10 MB"),
    (50, "50 MB"),
    (200, "200 MB"),
    (500, "500 MB"),
];

pub const DEFAULT_MB: f64 = 50.0;

const MISS_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Evicting down to exactly the cap would mean a sweep on almost every write
/// once the cache is full. Going under it buys a few hundred writes of quiet.
const EVICT_TO: f64 = 0.9;

/// Bumped if the stored shape changes; an older record is treated as absent and
/// fetched again, which is all a cache owes anyone.
const VERSION: u8 = 1;

static CAP_BYTES: AtomicU64 = AtomicU64::new(0);
/// What the directory holds, tracked rather than measured: the tray menu is
/// rebuilt on every state change and cannot go counting files each time.
static TOTAL_BYTES: AtomicU64 = AtomicU64::new(0);
static DIR: OnceLock<PathBuf> = OnceLock::new();

// ------------------------------------------------------------------- records

#[derive(Serialize, Deserialize)]
struct Record {
    v: u8,
    /// Unix seconds, for the miss expiry.
    at: u64,
    /// `None` is a miss — a track none of the four tiers could match.
    lyrics: Option<Stored>,
}

/// `Lyrics` with `how` widened to a `String`: the live type carries a
/// `&'static str`, which is worth keeping (it is one of four known values, not
/// arbitrary text) and cannot be deserialised into.
#[derive(Serialize, Deserialize)]
struct Stored {
    synced: bool,
    how: String,
    lines: Vec<LyricLine>,
}

// ----------------------------------------------------------------- the store

pub fn dir() -> PathBuf {
    DIR.get_or_init(|| {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join("Library/Caches/pear-music-widget/lyrics")
    })
    .clone()
}

/// Set the cap and take stock of what is already on disk. Called at startup and
/// whenever the menu changes the size; the sweep it may trigger is why callers
/// hand it to a background task rather than the setup path.
pub fn configure(cap_mb: f64) {
    let cap = (cap_mb.max(0.0) * 1024.0 * 1024.0) as u64;
    CAP_BYTES.store(cap, Ordering::Relaxed);
    if cap == 0 {
        // Left on disk rather than deleted: turning the cache off is not the
        // same as asking for it to be emptied, and there is a menu item for
        // that. Measured anyway, so the menu can still say how much is there.
        TOTAL_BYTES.store(measure(), Ordering::Relaxed);
        return;
    }
    enforce();
}

pub fn size_bytes() -> u64 {
    TOTAL_BYTES.load(Ordering::Relaxed)
}

fn enabled() -> bool {
    CAP_BYTES.load(Ordering::Relaxed) > 0
}

/// Video ids are `[A-Za-z0-9_-]{11}`, but this is a filename built from
/// something the player handed us, so anything else is refused rather than
/// escaped — `..` included, which is the one that matters.
fn safe_name(video_id: &str) -> Option<String> {
    if video_id.is_empty()
        || video_id.len() > 64
        || !video_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(format!("{video_id}.json"))
}

fn path_for(video_id: &str) -> Option<PathBuf> {
    Some(dir().join(safe_name(video_id)?))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `Some(hit)` — including `Some(None)` for a remembered miss — or `None` when
/// this track is not in the cache and has to be fetched.
pub fn load(video_id: &str) -> Option<Option<Lyrics>> {
    if !enabled() {
        return None;
    }
    let path = path_for(video_id)?;
    let record: Record = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
    if record.v != VERSION {
        return None;
    }

    match record.lyrics {
        Some(stored) => Some(Some(Lyrics {
            synced: stored.synced,
            how: how_from(&stored.how),
            lines: stored.lines,
        })),
        // A miss worth re-asking about. Left on disk — the write that follows
        // the refetch replaces it, and deleting it here would only make the
        // total wrong if that fetch never happened.
        None if now().saturating_sub(record.at) > MISS_TTL.as_secs() => None,
        None => Some(None),
    }
}

pub fn store(video_id: &str, lyrics: &Option<Lyrics>) {
    if !enabled() {
        return;
    }
    let Some(path) = path_for(video_id) else {
        return;
    };
    let record = Record {
        v: VERSION,
        at: now(),
        lyrics: lyrics.as_ref().map(|words| Stored {
            synced: words.synced,
            how: words.how.to_string(),
            lines: words.lines.clone(),
        }),
    };
    let Ok(text) = serde_json::to_string(&record) else {
        return;
    };

    if fs::create_dir_all(&dir()).is_err() {
        return;
    }
    // A torn write parses as garbage on the way back in, which `load` reads as
    // "not cached" and fixes by fetching over the top of it. That is the whole
    // failure mode, so no temp file and rename.
    let replacing = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    if fs::write(&path, &text).is_err() {
        return;
    }

    // Tracked rather than re-measured, and only approximately: two fetches
    // landing at once can lose an update, and the next sweep — or the next
    // launch — counts the directory again and puts it right.
    let total = TOTAL_BYTES
        .load(Ordering::Relaxed)
        .saturating_add(text.len() as u64)
        .saturating_sub(replacing);
    TOTAL_BYTES.store(total, Ordering::Relaxed);
    if total > CAP_BYTES.load(Ordering::Relaxed) {
        enforce();
    }
}

pub fn clear() {
    let dir = dir();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if is_record(&entry.path()) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    TOTAL_BYTES.store(0, Ordering::Relaxed);
}

// ------------------------------------------------------------------ eviction

fn is_record(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("json")
}

fn measure() -> u64 {
    measure_in(&dir())
}

fn measure_in(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| is_record(&entry.path()))
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .sum()
}

/// Delete oldest-written first until the directory is comfortably under the
/// cap, and leave `TOTAL_BYTES` describing what is left.
///
/// By write time rather than by last use: keeping a true LRU would mean
/// touching a file on every cache *hit*, which is a write per track played for
/// a cache that, at the default size, holds something like ten thousand of them
/// and will never fill.
fn enforce() {
    let total = enforce_in(&dir(), CAP_BYTES.load(Ordering::Relaxed));
    TOTAL_BYTES.store(total, Ordering::Relaxed);
}

/// The sweep itself, against a directory it is handed — the globals are the
/// caller's business, which is also what makes this testable without one.
/// Returns what the directory holds afterwards.
fn enforce_in(dir: &Path, cap: u64) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };

    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_record(path))
        .filter_map(|path| {
            let meta = fs::metadata(&path).ok()?;
            Some((meta.modified().unwrap_or(UNIX_EPOCH), meta.len(), path))
        })
        .collect();

    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    if cap == 0 || total <= cap {
        return total;
    }

    files.sort_by_key(|(modified, _, _)| *modified);
    let target = (cap as f64 * EVICT_TO) as u64;
    for (_, len, path) in files {
        if total <= target {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
    total
}

/// "12.4 MB", for the menu.
pub fn human(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb < 0.1 && bytes > 0 {
        return "under 0.1 MB".into();
    }
    format!("{mb:.1} MB")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cache that grows past its cap has to come back under it, and the
    /// oldest file is the one that goes. Against a directory of its own, not
    /// the process-wide one: `DIR` is a `OnceLock` and the tests share a
    /// process, so whichever test touched it first would decide for all of them.
    #[test]
    fn evicts_oldest_first_until_it_is_under_the_cap() {
        let dir = std::env::temp_dir().join(format!("pmw-lyrics-evict-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");

        // Five 1KB files, written oldest first with a gap the filesystem's
        // timestamp resolution can actually see.
        for name in ["a", "b", "c", "d", "e"] {
            fs::write(dir.join(format!("{name}.json")), "x".repeat(1024)).expect("write");
            std::thread::sleep(std::time::Duration::from_millis(12));
        }

        // 5KB against a 4KB cap, and it evicts to 90% of that — 3686 — rather
        // than to the cap itself, so two of the five go and not just one.
        let left = enforce_in(&dir, 4096);

        assert!(!dir.join("a.json").exists());
        assert!(!dir.join("b.json").exists());
        assert!(dir.join("c.json").exists());
        assert!(dir.join("e.json").exists());
        assert_eq!(3072, left);
        assert_eq!(3072, measure_in(&dir));

        fs::remove_dir_all(&dir).ok();
    }

    /// Nothing else in the directory is ours, and a sweep must not touch it.
    #[test]
    fn counts_and_evicts_only_its_own_records() {
        let dir = std::env::temp_dir().join(format!("pmw-lyrics-others-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        fs::write(dir.join("a.json"), "x".repeat(1024)).expect("write");
        fs::write(dir.join("notes.txt"), "x".repeat(4096)).expect("write");

        assert_eq!(1024, measure_in(&dir));
        enforce_in(&dir, 512);
        assert!(!dir.join("a.json").exists());
        assert!(dir.join("notes.txt").exists());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuses_a_video_id_that_would_leave_the_directory() {
        assert!(safe_name("../../etc/passwd").is_none());
        assert!(safe_name("a/b").is_none());
        assert!(safe_name("").is_none());
        assert_eq!(Some("dQw4w9WgXcQ.json".to_string()), safe_name("dQw4w9WgXcQ"));
    }
}
