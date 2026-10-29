//! Covers for local tracks: the file's own art, else `artwork::find`, each
//! sized once into `Local::images`. Queue rows and the playing track share
//! the one memo, so an album's `cover.jpg` is decoded once for the album.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use lofty::prelude::*;

use super::{names, Art, Local, Track};
use crate::state::Core;

/// How long a `queue_art` reply waits for covers still being looked up. The
/// rest finish in the background and land on the next request for that row.
const QUEUE_ART_WAIT: Duration = Duration::from_millis(1500);

impl Local {
    /// Resolve the playing track's cover off the audio path and show it if the
    /// track is still the one playing when the answer arrives.
    pub(super) fn fetch_cover(&self, core: &Arc<Core>, track: Track) {
        let core = Arc::clone(core);
        tauri::async_runtime::spawn(async move {
            let local = core.local();
            let id = track.id.clone();
            let watcher = Arc::clone(&core);
            let alive: crate::artwork::Alive = Arc::new(move || {
                watcher.snapshot().song.is_some_and(|song| song.video_id == id)
            });
            let cover = local
                .cover(&core, &track, crate::api::COVER_PX, crate::artwork::Priority::Now(alive))
                .await;
            let Some(cover) = cover else {
                return;
            };
            let current = core.snapshot().song.map(|song| song.video_id);
            if current.as_deref() == Some(track.id.as_str()) {
                core.set_cover(Some(cover));
            }
        });
    }

    /// Size the next track's cover now, so skipping to it — or reaching it —
    /// shows the artwork with the first frame instead of after a decode.
    pub(super) fn warm_next(&self, core: &Arc<Core>) {
        let Some(next) = self
            .with(|s| s.order.get((s.cursor + 1) % s.order.len().max(1)).map(|&i| s.tracks[i].clone()))
            .flatten()
        else {
            return;
        };
        let core = Arc::clone(core);
        tauri::async_runtime::spawn(async move {
            let _ = core
                .local()
                .cover(&core, &next, crate::api::COVER_PX, crate::artwork::Priority::background())
                .await;
        });
    }

    pub(super) fn wanted(core: &Arc<Core>, track: &Track) -> crate::artwork::Wanted {
        crate::artwork::Wanted {
            artist: crate::store::map_artist(&names(core), &track.artist),
            album: track.album.clone(),
            title: track.title.clone(),
        }
    }

    pub(super) fn memo(&self, key: &str) -> Option<Arc<str>> {
        self.images.lock().expect("images lock").get(key)
    }

    pub(super) fn remember_art(&self, key: String, art: Arc<str>) {
        self.images.lock().expect("images lock").insert(key, art);
    }

    /// The memo key for a track's own art: per file for an embedded
    /// picture, per sidecar for a folder's, so an album shares one resize.
    fn file_key(track: &Track, size: u32) -> Option<String> {
        match &track.art {
            Art::None => None,
            Art::Embedded => Some(format!("file:{}@{size}", track.path.display())),
            Art::Sidecar(sidecar) => Some(format!("side:{}@{size}", sidecar.display())),
        }
    }

    /// The cover if it is already sized in memory. No IO at all.
    pub(super) fn cover_now(&self, core: &Arc<Core>, track: &Track, size: u32) -> Option<Arc<str>> {
        if let Some(hit) = Self::file_key(track, size).and_then(|key| self.memo(&key)) {
            return Some(hit);
        }
        crate::artwork::keys(&Self::wanted(core, track))
            .iter()
            .find_map(|key| self.memo(&format!("{key}@{size}")))
    }

    /// The file's own art, sized; blocking IO and decode, so callers run it
    /// off the async workers.
    pub(super) fn file_art(&self, track: &Track, size: u32) -> Option<Arc<str>> {
        let key = Self::file_key(track, size)?;
        if let Some(hit) = self.memo(&key) {
            return Some(hit);
        }
        let art = read_art(track, size)?;
        self.remember_art(key, art.clone());
        Some(art)
    }

    /// The track's cover at `size`: its own art, else an online cover.
    async fn cover(
        self: &Arc<Self>,
        core: &Arc<Core>,
        track: &Track,
        size: u32,
        priority: crate::artwork::Priority,
    ) -> Option<Arc<str>> {
        if let Some(hit) = self.cover_now(core, track, size) {
            return Some(hit);
        }
        if track.art != Art::None {
            // A turn at decoding first: the memo is checked again inside
            // `file_art`, so rows sharing a sidecar decode it once.
            let _turn = self.decodes.acquire().await.ok()?;
            let (local, owned) = (Arc::clone(self), track.clone());
            let art = tauri::async_runtime::spawn_blocking(move || local.file_art(&owned, size))
                .await
                .ok()
                .flatten();
            if art.is_some() {
                return art;
            }
        }
        let (key, bytes) = crate::artwork::find(core, &Self::wanted(core, track), priority).await?;
        let cached = key.clone();
        let url = tauri::async_runtime::spawn_blocking(move || {
            crate::artwork::resize_cached(&cached, &bytes, size)
                .map(|sized| crate::artwork::data_url(&sized))
        })
        .await
        .ok()
        .flatten()?;
        self.remember_art(format!("{key}@{size}"), url.clone());
        Some(url)
    }

    /// Artwork for queue rows: the file's own, else an online cover. One
    /// lookup per album — rows after the first read the sized copy. Replies
    /// after `QUEUE_ART_WAIT` with whatever is ready; lookups still running
    /// carry on and warm the cache, and the renderer asks again for any row it
    /// draws without art.
    pub async fn queue_art(core: &Arc<Core>, ids: Vec<String>, size: u32) -> Vec<(String, Arc<str>)> {
        let local = core.local();
        let tracks: Vec<Track> = ids.iter().filter_map(|id| local.track(id)).collect();
        let tasks: Vec<_> = tracks
            .into_iter()
            .map(|track| {
                let (core, local) = (Arc::clone(core), Arc::clone(&local));
                tauri::async_runtime::spawn(async move {
                    let art = local
                        .cover(&core, &track, size, crate::artwork::Priority::background())
                        .await?;
                    Some((track.id, art))
                })
            })
            .collect();
        let deadline = tokio::time::Instant::now() + QUEUE_ART_WAIT;
        let mut ready = Vec::new();
        let mut late = Vec::new();
        for mut task in tasks {
            match tokio::time::timeout_at(deadline, &mut task).await {
                Ok(Ok(Some(hit))) => ready.push(hit),
                Ok(_) => {}
                Err(_) => late.push(task),
            }
        }
        // Rows that missed the reply: once their covers are cached, say so —
        // ids only, the art itself still travels by request — and each
        // surface asks again for whatever it is showing without art.
        if !late.is_empty() {
            let app = core.app.clone();
            tauri::async_runtime::spawn(async move {
                let mut landed = false;
                for task in late {
                    landed |= matches!(task.await, Ok(Some(_)));
                }
                if landed {
                    let _ = tauri::Emitter::emit(&app, "queue-art-ready", ());
                }
            });
        }
        ready
    }
}

/// The track's own art as the scan found it, downscaled to `size` so a 3 MB
/// scan never rides an event.
fn read_art(track: &Track, size: u32) -> Option<Arc<str>> {
    let bytes = art_bytes(track)?;
    Some(crate::artwork::data_url(&crate::artwork::resize(&bytes, size)?))
}

/// The file's own picture or its folder's sidecar, as stored. Blocking IO.
pub fn art_bytes(track: &Track) -> Option<Vec<u8>> {
    match &track.art {
        Art::None => None,
        Art::Embedded => embedded_art(&track.path),
        Art::Sidecar(sidecar) => std::fs::read(sidecar).ok(),
    }
}

pub(super) fn embedded_art(path: &Path) -> Option<Vec<u8>> {
    let file = lofty::read_from_path(path).ok()?;
    file.tags()
        .iter()
        .flat_map(|tag| tag.pictures())
        .next()
        .map(|picture| picture.data().to_vec())
}
