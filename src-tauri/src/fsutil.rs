//! Small file and hashing helpers shared by the stores and caches.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

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
