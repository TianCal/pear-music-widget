//! `Core`'s lyrics channel: the view the renderers draw, the lookup that
//! fills it while a lyrics panel is open, and the display-only treatments
//! (Simplified Chinese, Jyutping) derived from the fetched words.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde::Serialize;
use tauri::Emitter;

use crate::jyutping::Jyutping;
use crate::lyrics::{self, LyricLine, Lyrics};

use super::Core;

/// The lyrics panel's whole world, pushed on the `lyrics` event. The state
/// string travels with the words so the note under the roll can never describe
/// a different fetch than the one showing.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsView {
    /// idle | loading | ready | none
    pub state: String,
    pub lyrics: Option<Arc<Lyrics>>,
}

impl LyricsView {
    pub(super) fn idle() -> Self {
        Self {
            state: "idle".into(),
            lyrics: None,
        }
    }

    /// Derive every display-only lyric treatment from the fetched text. The
    /// Jyutping lookup deliberately happens before Simplified Chinese is
    /// assigned to `text`: 心裏 may be shown as 心里, but its reading must
    /// remain `sam1 leoi5`, not the reading of the different character 里.
    ///
    /// Text that is already simplified, or not Chinese at all, comes back
    /// unchanged, so this is safe to run over every line rather than trying to
    /// guess whether a track needs it.
    fn styled(&self, simplify: bool, jyutping: Option<&Jyutping>) -> Self {
        let Some(lyrics) = &self.lyrics else {
            return self.clone();
        };
        Self {
            state: self.state.clone(),
            lyrics: Some(Arc::new(Lyrics {
                synced: lyrics.synced,
                how: lyrics.how,
                lines: lyrics
                    .lines
                    .iter()
                    .map(|line| LyricLine {
                        time: line.time,
                        text: if simplify {
                            fast2s::convert(&line.text)
                        } else {
                            line.text.clone()
                        },
                        jyutping: jyutping.and_then(|engine| engine.annotate(&line.text)),
                    })
                    .collect(),
            })),
        }
    }

    #[cfg(test)]
    fn simplified(&self) -> Self {
        self.styled(true, None)
    }
}

impl Core {
    pub(super) fn set_lyrics(self: &Arc<Self>, view: LyricsView) {
        *self.lyrics_source.lock().expect("lyrics lock") = view.clone();
        self.restyle_lyrics();
    }

    /// Commit a fully styled view, still only on a real change.
    fn publish_lyrics(&self, view: LyricsView) {
        {
            let mut held = self.lyrics.lock().expect("lyrics lock");
            if *held == view {
                return;
            }
            *held = view.clone();
        }
        let _ = self.app.emit("lyrics", &view);
    }

    /// Re-derive the shown words from the fetched ones. Jyutping is dictionary
    /// work, so keep it off the AppKit/menu thread and reject the result if a
    /// newer track or toggle superseded it while it was running.
    pub fn restyle_lyrics(self: &Arc<Self>) {
        let source = self.lyrics_source.lock().expect("lyrics lock").clone();
        let (simplify, show_jyutping) = self
            .store
            .get(|settings| (settings.simplify_lyrics, settings.jyutping_lyrics));
        let token = self.lyrics_style_token.fetch_add(1, Ordering::SeqCst) + 1;

        if !show_jyutping || source.lyrics.is_none() {
            self.publish_lyrics(source.styled(simplify, None));
            return;
        }

        let core = Arc::clone(self);
        tauri::async_runtime::spawn_blocking(move || {
            let view = source.styled(simplify, Some(&core.jyutping));
            if token == core.lyrics_style_token.load(Ordering::SeqCst) {
                core.publish_lyrics(view);
            }
        });
    }

    /// Lyrics come from several providers (see `lyrics.rs`) and are only
    /// fetched while a lyrics panel is actually open.
    pub async fn refresh_lyrics(self: &Arc<Self>) {
        let open = self.lyrics_wanted_by();
        // A lyrics panel left open on a hidden widget: nobody is reading, so
        // no lookup — the panel catches up when the widget shows again.
        if !open.is_empty() && !self.any_on_screen(&open) {
            self.lyrics_demand.mark_stale();
            return;
        }
        let wanted = !open.is_empty();
        let song = self.player.lock().expect("player lock").song.clone();
        let names = self.store.get(|s| s.artist_names.clone());
        let song = song.map(|song| song.for_lyrics(&names));

        let token = self.lyrics_demand.bump();
        self.lyrics_superseded.notify_waiters();
        let Some(song) = song.filter(|_| wanted) else {
            self.set_lyrics(LyricsView::idle());
            return;
        };

        self.set_lyrics(LyricsView {
            state: "loading".into(),
            lyrics: None,
        });

        // Created before the lookup starts, so a newer request's
        // `notify_waiters` reaches it even before its first poll.
        let superseded = self.lyrics_superseded.notified();
        let result = tokio::select! {
            result = lyrics::fetch_lyrics(self.api.client(), &song) => result,
            () = superseded => return,
        };

        // A newer request, or the track moved on while we were waiting.
        if !self.lyrics_demand.current(token) {
            return;
        }
        let still_playing = self
            .player
            .lock()
            .expect("player lock")
            .song
            .as_ref()
            .map(|s| s.video_id.clone())
            == Some(song.video_id.clone());
        if !still_playing {
            return;
        }

        self.set_lyrics(LyricsView {
            state: if result.is_some() { "ready" } else { "none" }.into(),
            lyrics: result.map(Arc::new),
        });
    }

    /// A cache-menu retry should not replace lyrics already on screen.
    pub async fn retry_missing_lyrics(self: &Arc<Self>) {
        let missing = self.lyrics_source.lock().expect("lyrics lock").state == "none";
        if missing {
            self.refresh_lyrics().await;
        }
    }

    /// Surfaces that want the lyrics: an open lyrics panel, or a skin that
    /// draws them in its own layout. The skin half is read from the store, as
    /// for the queue (`queue_wanted`).
    fn lyrics_wanted_by(&self) -> HashSet<String> {
        let mut open = self.lyrics_demand.wanted_by();
        if crate::window::skin_shows_lyrics(&crate::window::skin_of(&self.store)) {
            open.insert(crate::window::WIDGET.to_string());
        }
        if crate::window::skin_shows_lyrics(&crate::window::panel_skin_of(&self.store)) {
            open.insert(crate::window::DROPDOWN.to_string());
        }
        open
    }

    pub fn set_lyrics_wanted(&self, label: &str, wanted: bool) {
        self.lyrics_demand.set_wanted(label, wanted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simplifies_the_words_and_leaves_the_rest_alone() {
        let view = LyricsView {
            state: "ready".into(),
            lyrics: Some(Arc::new(Lyrics {
                synced: true,
                how: crate::lyrics::How::Exact,
                lines: vec![
                    LyricLine {
                        time: Some(0.0),
                        text: "後來我總算學會了如何去愛".into(),
                        jyutping: None,
                    },
                    // The reason this is not a character table: 乾 is 干 in
                    // 乾淨 and stays 乾 in 乾坤. Any per-character mapping gets
                    // one of those two wrong, in the same line.
                    LyricLine {
                        time: Some(1.0),
                        text: "乾淨的乾坤".into(),
                        jyutping: None,
                    },
                    // Already simplified, and not Chinese at all: both have to
                    // survive untouched, since every line is converted rather
                    // than the track being sniffed first.
                    LyricLine {
                        time: Some(2.0),
                        text: "Baby, already 简体 and ASCII".into(),
                        jyutping: None,
                    },
                ],
            })),
        };

        let converted = view.simplified();
        let lyrics = converted.lyrics.as_ref().expect("lyrics");
        assert_eq!(lyrics.lines[0].text, "后来我总算学会了如何去爱");
        assert_eq!(lyrics.lines[1].text, "干净的乾坤");
        assert_eq!(lyrics.lines[2].text, "Baby, already 简体 and ASCII");

        // Timings and the fetch's own description belong to the fetch, not the
        // text — losing a timestamp here would stop the roll following along.
        assert_eq!(lyrics.lines[0].time, Some(0.0));
        assert_eq!(lyrics.lines[2].time, Some(2.0));
        assert!(lyrics.synced);
        assert_eq!(lyrics.how, crate::lyrics::How::Exact);
        assert_eq!(converted.state, "ready");
    }

    #[test]
    fn a_track_with_no_lyrics_converts_to_itself() {
        let idle = LyricsView::idle();
        assert_eq!(idle.simplified(), idle);
    }

    #[test]
    fn jyutping_is_derived_before_simplification() {
        let view = LyricsView {
            state: "ready".into(),
            lyrics: Some(Arc::new(Lyrics {
                synced: true,
                how: crate::lyrics::How::Exact,
                lines: vec![LyricLine {
                    time: Some(1.0),
                    text: "我心裏".into(),
                    jyutping: None,
                }],
            })),
        };
        let engine = Jyutping::from_dir(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/jyutping"),
        );

        let converted = view.styled(true, Some(&engine));
        let line = &converted.lyrics.as_ref().expect("lyrics").lines[0];
        assert_eq!(line.text, "我心里");
        assert_eq!(line.jyutping.as_deref(), Some("ngo sam leoi"));
        assert_eq!(
            view.lyrics.as_ref().expect("source").lines[0].text,
            "我心裏"
        );
    }
}
