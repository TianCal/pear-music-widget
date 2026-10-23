//! Covers without the network: the file's own picture or a sidecar, else
//! whatever the artwork cache already holds for the album or track.

use std::path::Path;

use lofty::prelude::*;

use crate::local::Track;

fn embedded(path: &Path) -> Option<Vec<u8>> {
    let file = lofty::read_from_path(path).ok()?;
    file.tags()
        .iter()
        .flat_map(|tag| tag.pictures())
        .next()
        .map(|picture| picture.data().to_vec())
}

fn sidecar(path: &Path) -> Option<Vec<u8>> {
    let dir = path.parent()?;
    ["cover", "folder", "front", "Cover", "Folder", "Front"]
        .iter()
        .flat_map(|name| ["jpg", "jpeg", "png"].map(|ext| dir.join(format!("{name}.{ext}"))))
        .find(|candidate| candidate.is_file())
        .and_then(|file| std::fs::read(file).ok())
}

/// A `data:` URL for `track`'s cover at `size`, or `None`. `artist` is the
/// mapped credit. Blocking IO.
pub fn cover(track: &Track, artist: &str, size: u32) -> Option<String> {
    let bytes = embedded(&track.path)
        .or_else(|| sidecar(&track.path))
        .or_else(|| {
            let wanted = crate::artwork::Wanted {
                artist: artist.to_string(),
                album: track.album.clone(),
                title: track.title.clone(),
            };
            crate::artwork::cached(&wanted).map(|(_, bytes)| bytes)
        })?;
    let sized = crate::artwork::resize(&bytes, size)?;
    Some(crate::artwork::data_url(&sized).to_string())
}
