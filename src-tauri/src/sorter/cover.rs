//! Covers without the network: the file's own picture or a sidecar, else
//! whatever the artwork cache already holds for the album or track.

use crate::local::Track;

/// A `data:` URL for `track`'s cover at `size`, or `None`. `artist` is the
/// mapped credit. Blocking IO.
pub fn cover(track: &Track, artist: &str, size: u32) -> Option<String> {
    if let Some(bytes) = crate::local::art_bytes(&track.path) {
        let sized = crate::artwork::resize(&bytes, size)?;
        return Some(crate::artwork::data_url(&sized).to_string());
    }
    let wanted = crate::artwork::Wanted {
        artist: artist.to_string(),
        album: track.album.clone(),
        title: track.title.clone(),
    };
    let (key, bytes) = crate::artwork::cached(&wanted)?;
    let sized = crate::artwork::resize_cached(&key, &bytes, size)?;
    Some(crate::artwork::data_url(&sized).to_string())
}
