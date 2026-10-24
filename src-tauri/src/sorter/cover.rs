//! Covers without the network: the file's own picture or a sidecar, else
//! whatever the artwork cache already holds for the album or track.

use crate::local::Track;

/// A `data:` URL for `track`'s cover at `size`, or `None`. `artist` is the
/// mapped credit. Blocking IO.
pub fn cover(track: &Track, artist: &str, size: u32) -> Option<String> {
    let bytes = crate::local::art_bytes(&track.path).or_else(|| {
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
