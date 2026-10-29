//! Small file, path, clock and hashing helpers shared by the stores and caches.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The directory name under `~/Library`. It is the Electron build's (Electron
/// derived it from the package name, not the bundle id), so settings and
/// caches survive the port.
pub const APP: &str = "pear-music-widget";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// `~/Library/Application Support/<app>`: settings and anything else the user
/// would miss.
pub fn support_dir(app: &str) -> PathBuf {
    home().join("Library/Application Support").join(app)
}

/// `~/Library/Caches/pear-music-widget/<name>`: derived data that can be
/// thrown away, in the directory macOS is allowed to throw away under disk
/// pressure.
pub fn cache_dir(name: &str) -> PathBuf {
    home().join("Library/Caches").join(APP).join(name)
}

/// Seconds since the epoch. The lyrics cache stamps its records with these.
pub fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Milliseconds since the epoch. Likes stamp their entries with these, and
/// the two units are kept apart on purpose: each file keeps the unit it was
/// first written in.
pub fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Write `bytes` to `path` via a synced temporary file and a rename, so a
/// crash or a drive pulled mid-write never leaves an empty or half file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // A temp name per write, so two writes can never mix into one file — a
    // mixed file reads as broken, and a broken drive file is never replaced.
    static WRITES: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let n = WRITES.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("tmp-{}-{n}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// 64-bit FNV-1a. Track ids, folder keys and artwork cache names are built from
/// it, and all of them are stored on disk: changing it orphans the lyrics cache,
/// timing overrides, the likes fallback and every cached cover.
pub fn stable_hash(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_never_changes() {
        // Pinned: these values name files on users' disks.
        assert_eq!(stable_hash(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(stable_hash("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(stable_hash("/Volumes/X/双截棍.wav"), stable_hash("/Volumes/X/双截棍.wav"));
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("pmw-fsutil-{}", std::process::id()));
        let path = dir.join("f.json");
        write_atomic(&path, b"one").expect("first");
        write_atomic(&path, b"two").expect("second");
        assert_eq!(std::fs::read(&path).expect("read"), b"two");
        let leftovers = std::fs::read_dir(&dir).expect("dir").count();
        assert_eq!(leftovers, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
