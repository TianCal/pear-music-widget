//! The artist popup's data: the Top songs shelf of an artist's YouTube Music
//! page, read through the API server's `/browse` (the fork's; stock
//! pear-desktop has none) with each row's cover resolved to a `data:` URL.
//!
//! A page costs ~0.4 s through the API server plus its covers, which is too
//! slow for a click. The renderer warms the playing track's artists once the
//! pointer rests on the card, as it warms the playlist picker; nothing is
//! fetched just because a track started. Pages are cached by artist for
//! `FRESH`, and a click shares any fetch already in flight rather than
//! starting its own.
//!
//! An artist YouTube Music did not link in the credit has no id, only a name.
//! The id is found by searching for the name (`search::find_artist`, exact
//! names only) behind any search the user is waiting on, and kept apart from
//! the page so a page that failed does not cost the search again.
//!
//! Failures are kept too, for less time: the warm-up runs on every hover, and
//! an API server without `/browse` would otherwise be asked again each time.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::sync::OnceCell;

use crate::api::{Api, ApiError, ErrorCode, THUMB_PX};
use crate::search::{self, ArtistPage};

/// How long a page or a name lookup is reused. Top songs move over weeks.
const FRESH: Duration = Duration::from_secs(30 * 60);
/// How long a failed request is remembered before the next hover tries again.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// How long an API server without `/browse` is believed. It changes only when
/// YouTube Music is updated or replaced.
const NO_BROWSE_FOR: Duration = Duration::from_secs(10 * 60);
/// Entries kept in each cache: an evening of different artists. A page is a
/// few rows of text plus five small covers.
const MAX_ENTRIES: usize = 24;
/// A cover that has not come by then is left blank rather than holding the
/// page, and everyone waiting on it, until the cover timeout.
const COVER_WAIT: Duration = Duration::from_secs(2);

/// Why there is no page, and how long that answer is kept.
#[derive(Clone, Debug)]
pub struct Failed {
    pub message: String,
    keep: Duration,
}

impl Failed {
    fn request(err: ApiError) -> Self {
        let keep = if err.code == ErrorCode::NoBrowse { NO_BROWSE_FOR } else { RETRY_AFTER };
        Self { message: err.message, keep }
    }
}

type Answer<T> = Arc<OnceCell<Result<T, Failed>>>;

struct Entry<T> {
    at: Instant,
    answer: Answer<T>,
}

impl<T> Entry<T> {
    /// In flight, or answered and still within that answer's lifetime.
    fn fresh(&self) -> bool {
        let keep = match self.answer.get() {
            None => return true,
            Some(Ok(_)) => FRESH,
            Some(Err(failed)) => failed.keep,
        };
        self.at.elapsed() < keep
    }
}

/// Answers by key, each worked out once however many callers ask at the same
/// time.
struct Memo<T>(Mutex<HashMap<String, Entry<T>>>);

impl<T: Clone> Memo<T> {
    fn new() -> Self {
        Self(Mutex::default())
    }

    async fn get<F, Fut>(&self, key: &str, work: F) -> Result<T, Failed>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, Failed>>,
    {
        let answer = {
            let mut entries = self.0.lock().expect("artist cache");
            match entries.get(key) {
                Some(entry) if entry.fresh() => Arc::clone(&entry.answer),
                _ => {
                    if entries.len() >= MAX_ENTRIES {
                        let oldest = entries
                            .iter()
                            .min_by_key(|(_, entry)| entry.at)
                            .map(|(key, _)| key.clone());
                        if let Some(oldest) = oldest {
                            entries.remove(&oldest);
                        }
                    }
                    let answer = Answer::default();
                    entries.insert(key.to_string(), Entry { at: Instant::now(), answer: Arc::clone(&answer) });
                    answer
                }
            }
        };
        answer.get_or_init(work).await.clone()
    }
}

static PAGES: LazyLock<Memo<Arc<ArtistPage>>> = LazyLock::new(Memo::new);
static IDS: LazyLock<Memo<String>> = LazyLock::new(Memo::new);

/// The artist's page by browse id, or by YouTube Music's own name for them
/// when `id` is empty, from the cache or fetched now.
pub async fn page(api: &Api, id: &str, name: &str) -> Result<Arc<ArtistPage>, Failed> {
    let id = if id.is_empty() {
        IDS.get(&crate::text::fold(name), || resolve(api, name)).await?
    } else {
        id.to_string()
    };
    PAGES.get(&id, || load(api, &id)).await
}

async fn resolve(api: &Api, name: &str) -> Result<String, Failed> {
    let payload = api
        .search_background(name)
        .await
        .map_err(Failed::request)?;
    search::find_artist(&payload, name).ok_or_else(|| Failed {
        message: format!("YouTube Music has no artist page for {name}"),
        keep: FRESH,
    })
}

async fn load(api: &Api, id: &str) -> Result<Arc<ArtistPage>, Failed> {
    let payload = api.browse(id).await.map_err(Failed::request)?;
    let mut page = search::parse_artist_page(&payload);
    // All at once: a shelf is five rows, and the slowest cover sets the wait.
    let covers = page.songs.len().max(1);
    page.songs = futures_util::stream::iter(page.songs)
        .map(|song| async move {
            let cover = api.fetch_cover(song.thumbnail.as_deref(), THUMB_PX);
            let thumbnail = tokio::time::timeout(COVER_WAIT, cover).await.ok().flatten();
            search::SearchResult { thumbnail, ..song }
        })
        .buffered(covers)
        .collect()
        .await;
    Ok(Arc::new(page))
}
