//! `Core`'s queue channel: who wants the queue, the one-at-a-time fetch
//! that answers them, and the relabelling of the playing track from its
//! slot once the queue has arrived.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::Emitter;

use crate::search::{self, QueueView, QUEUE_MAX};

use super::Core;

/// A queue fetched longer ago than this is refetched when any surface comes on
/// screen. The queue has no push channel, so edits made inside YouTube Music
/// would otherwise wait for the next track change while the widget stays up.
pub(super) const QUEUE_SHOW_TTL: Duration = Duration::from_secs(5);

/// Waits before each queue read after a shuffle change, in ms: the first
/// catches YouTube Music's new order, the second an unshuffle's restored
/// opening tracks, which need a whole-playlist fetch first.
const REORDER_REFRESH_MS: [u64; 2] = [700, 900];

impl Core {
    pub(crate) fn set_queue(&self, view: QueueView) {
        {
            let mut held = self.queue.lock().expect("queue lock");
            if *held == view {
                return;
            }
            *held = view.clone();
        }
        let _ = self.app.emit("queue", &view);
    }

    /// The artwork source URL each row was parsed with, for `queue_art` to
    /// resolve. Looked up by video id rather than by index so a request that
    /// crosses a queue change is applied to the right track or to none. One
    /// walk of the queue for the whole request, not one per id.
    pub fn queue_art_srcs(&self, video_ids: &[String]) -> Vec<(String, Arc<str>)> {
        let queue = self.queue.lock().expect("queue lock");
        let mut by_id: HashMap<&str, &Arc<str>> = HashMap::new();
        for track in queue.items.iter() {
            if let Some(art) = track.art.as_ref() {
                by_id.entry(track.video_id.as_str()).or_insert(art);
            }
        }
        video_ids
            .iter()
            .filter_map(|id| Some((id.clone(), Arc::clone(by_id.get(id.as_str())?))))
            .collect()
    }

    /// The queue changed: relabel the playing track from its slot.
    fn relabel_song(&self) {
        let Some(current) = self.player.lock().expect("player lock").song.clone() else {
            return;
        };
        let dressed = self.dress(current.clone());
        if dressed == current {
            return;
        }
        // Only onto the song it was dressed from: an `apply_song` in between
        // (a later album or duration) wins, and relabels itself.
        self.update_remote(|state| {
            if state.song.as_ref() == Some(&current) {
                state.song = Some(dressed);
            }
        });
    }

    /// Whether any surface is looking at the queue — a skin whose layout draws
    /// it, or a queue panel open on that surface. The same bargain as the
    /// lyrics: a request on every track change is not work to do for nobody.
    ///
    /// The skin half is answered from the store, because a skin is a property of
    /// the surface rather than something the renderer has to report.
    ///
    /// Either way only while a surface is on screen: a hidden Stack card draws
    /// nothing, and `set_on_screen` refreshes it when it comes back.
    pub(super) fn queue_wanted(&self) -> bool {
        if !self.showing() {
            return false;
        }
        // A skin's own queue counts only while its surface is the one on
        // screen: the menu-bar strip alone must not fetch the widget's queue.
        let drawn = |label: &str, skin: String| {
            crate::window::skin_shows_queue(&skin)
                && self.on_screen.lock().expect("screen lock").contains(label)
        };
        if drawn(crate::window::WIDGET, crate::window::skin_of(&self.store))
            || drawn(crate::window::DROPDOWN, crate::window::panel_skin_of(&self.store))
        {
            return true;
        }
        let open = self.queue_demand.wanted_by();
        self.any_on_screen(&open)
    }

    pub fn set_queue_wanted(&self, label: &str, wanted: bool) {
        self.queue_demand.set_wanted(label, wanted);
    }

    /// A surface just started or stopped showing queue rows. Starting fetches
    /// the queue unless what is held is recent: while nobody wanted it, track
    /// changes left it empty (`queue_visible`).
    pub fn want_queue(self: &Arc<Self>, label: &str, wanted: bool) {
        self.set_queue_wanted(label, wanted);
        if !wanted {
            return;
        }
        let held = !self.queue.lock().expect("queue lock").items.is_empty();
        let fresh = self
            .queue_fetched_at
            .lock()
            .expect("time lock")
            .is_some_and(|at| at.elapsed() < QUEUE_SHOW_TTL);
        if held && fresh && !self.is_local() {
            return;
        }
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move { core.refresh_queue().await });
    }

    /// Refresh the queue once YouTube Music has finished reordering it. The
    /// shuffle event fires when the button flips, but the new order arrives
    /// from YouTube after that, and an unshuffle can land in two steps — the
    /// API server puts back a long playlist's opening tracks once the queue
    /// is rebuilt. Two timed reads cover both without polling a queue that can
    /// be thousands of items. A newer toggle supersedes an older one, and
    /// nothing is fetched while no surface shows the queue.
    pub(super) fn refresh_queue_after_reorder(self: &Arc<Self>) {
        let token = self.reorder_token.fetch_add(1, Ordering::SeqCst) + 1;
        if !self.queue_visible() {
            return;
        }
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            for delay in REORDER_REFRESH_MS {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                if core.reorder_token.load(Ordering::SeqCst) != token || core.is_local() {
                    return;
                }
                core.refresh_queue().await;
            }
        });
    }

    /// The whole queue, for the queue panel and Stack's "Next tracks" strip
    /// alike — one fetch, one channel, two readers.
    ///
    /// No artwork is resolved here. That is the difference from the four-item
    /// version this replaced: a full queue's worth of `data:` URLs would be
    /// megabytes on an event, and the two surfaces want different sizes anyway.
    ///
    /// Each `/queue` is a full round trip into the YouTube Music renderer, so
    /// overlapping refreshes merge: one in flight, at most one more queued
    /// behind it (see `Gate`).
    pub async fn refresh_queue(self: &Arc<Self>) {
        if !self.queue_gate.enter(true) {
            return;
        }
        loop {
            self.refresh_queue_once().await;
            if !self.queue_gate.leave() {
                break;
            }
        }
    }

    /// Whether a queue view would be shown right now; marks it stale if not.
    fn queue_visible(self: &Arc<Self>) -> bool {
        if !self.showing() {
            self.queue_demand.mark_stale();
            return false;
        }
        if !self.queue_wanted() {
            self.set_queue(QueueView::empty());
            return false;
        }
        true
    }

    /// Publish a `/queue` reply someone else fetched (the jump path polls the
    /// queue itself), superseding any refresh still in flight.
    pub fn adopt_queue(self: &Arc<Self>, queue: &Value) {
        if self.is_local() || !self.queue_visible() {
            return;
        }
        self.queue_demand.bump();
        *self.queue_fetched_at.lock().expect("time lock") = Some(Instant::now());
        self.publish_queue(queue);
    }

    fn publish_queue(&self, queue: &Value) {
        let mut view = search::parse_queue(Some(queue), QUEUE_MAX);
        let names = self.store.get(|s| s.artist_names.clone());
        if !names.is_empty() {
            let items: Vec<_> = view
                .items
                .iter()
                .cloned()
                .map(|mut track| {
                    track.artist = crate::store::map_artist(&names, &track.artist);
                    track
                })
                .collect();
            view.items = Arc::from(items);
        }
        self.set_queue(view);
        self.relabel_song();
    }

    async fn refresh_queue_once(self: &Arc<Self>) {
        // Hidden: keep what is drawn rather than flash it empty, and refetch
        // when a surface shows.
        if !self.queue_visible() {
            return;
        }
        if self.is_local() {
            self.publish_local_queue();
            return;
        }

        let token = self.queue_demand.bump();
        *self.queue_fetched_at.lock().expect("time lock") = Some(Instant::now());
        let Ok(Some(queue)) = self.api.queue().await else {
            return;
        };
        if self.is_local() {
            return;
        }
        // One await inside the guarded region, so one check: if a newer refresh
        // started while this response was in flight, its result is the truth.
        if !self.queue_demand.current(token) {
            return;
        }
        self.publish_queue(&queue);
    }
}
