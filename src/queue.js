'use strict';

/* The queue's two readers — Stack's Next tracks strip and the queue panel —
   and the artwork they share. Loaded after app.js; see index.html. */

// ------------------------------------------------------------------ queue

/* One channel, two readers: Stack's "Next tracks" strip and the queue panel.
   The event carries text only — artwork is asked for by id, for the rows a
   surface is actually showing, because resolving every slot would be megabytes
   for tracks nobody has scrolled to. */

/** Artwork already resolved, keyed by videoId. Survives a queue change: the
 *  same track scrolled back to is already paid for. The Rust cache has the same
 *  byte and entry ceilings; bounding this copy matters because every webview
 *  owns its own JavaScript heap. Data URLs are ASCII, so a character budget
 *  mirrors the source-byte budget even if the engine's storage differs. */
const ART_CACHE_CHARS = 4 * 1024 * 1024;
const ART_CACHE_MAX = 256;
let artChars = 0;

const artOf = (videoId) => {
  const art = state.art.get(videoId) || null;
  if (art) {
    // Map iteration order is the eviction order. Reinsert a hit to promote it.
    state.art.delete(videoId);
    state.art.set(videoId, art);
  }
  return art;
};

const cacheArt = (videoId, art) => {
  const replaced = state.art.get(videoId);
  if (replaced) {
    artChars -= videoId.length + replaced.length;
    state.art.delete(videoId);
  }

  state.art.set(videoId, art);
  artChars += videoId.length + art.length;

  while (state.art.size > ART_CACHE_MAX || artChars > ART_CACHE_CHARS) {
    const oldestId = state.art.keys().next().value;
    if (oldestId === undefined) break;
    const dropped = state.art.get(oldestId);
    state.art.delete(oldestId);
    artChars -= oldestId.length + (dropped?.length || 0);
  }
};

let artPending = null;
let artTimer = null;
/** Ids sent and not yet answered. Rust replies after up to 1.5 s with what
 *  is ready; without this, every scroll pause meanwhile re-sent the same
 *  ids. Cleared by `queueArtReady`: that nudge is Rust saying the lookups
 *  that missed a reply have landed, so asking again is the point. */
const artInFlight = new Set();

/** Must match `QUEUE_ART_MAX` in commands.rs, which answers for this many ids
 *  and drops the rest. */
const QUEUE_ART_MAX = 48;

/** Ask for artwork for `ids`, batched across a frame or two of scrolling so a
 *  drag does not fire a request per row it passes. The batch keeps the most
 *  recently asked-for ids last and sends only the newest `QUEUE_ART_MAX`: after
 *  a long drag those are the rows it stopped on, not the ones it passed. */
const wantArt = (ids, size) => {
  const missing = ids.filter((id) => id && !state.art.has(id) && !artInFlight.has(id));
  if (!missing.length) return;

  artPending = artPending || { ids: new Set(), size };
  artPending.size = Math.max(artPending.size, size);
  for (const id of missing) {
    artPending.ids.delete(id);
    artPending.ids.add(id);
  }

  clearTimeout(artTimer);
  artTimer = setTimeout(async () => {
    const batch = artPending;
    artPending = null;
    const asked = [...batch.ids].slice(-QUEUE_ART_MAX);
    asked.forEach((id) => artInFlight.add(id));
    let reply;
    try {
      reply = await window.widget.queueArt(asked, batch.size);
    } finally {
      asked.forEach((id) => artInFlight.delete(id));
    }
    if (!reply?.ok) return;

    const landed = new Set();
    for (const [id, art] of Object.entries(reply.art || {})) {
      cacheArt(id, art);
      landed.add(id);
    }
    // Written straight into whatever is on screen rather than re-rendering: a
    // reply usually lands mid-scroll, and rebuilding would fight the drag that
    // asked for it.
    if (landed.size) paintArt(landed);
  }, 120);
};

/** Fill in any still-empty `<img data-art>` among the `ids` that just arrived.
 *  Read straight from the map rather than through `artOf`: promoting every
 *  row in the DOM would make the cache's order mean nothing. An image that
 *  already has a picture keeps it — reading `src` back would only resolve a
 *  data URL to compare it with the one it already is. */
const paintArt = (ids) => {
  for (const img of document.querySelectorAll('img[data-art]')) {
    if (img.hasAttribute('src') || !ids.has(img.dataset.art)) continue;
    const art = state.art.get(img.dataset.art);
    if (art) img.src = art;
  }
};

/** Ids of the rows in `rows[from, to)` whose image is still empty. A row keeps
 *  its picture after the cache has evicted it, so asking again would fetch
 *  artwork that is already on screen. The image is each row's first child. */
const unpaintedIds = (rows, from, to) => {
  const ids = [];
  for (let at = Math.max(0, from); at < Math.min(rows.length, to); at += 1) {
    if (!rows[at].firstElementChild.hasAttribute('src')) ids.push(rows[at].dataset.videoId);
  }
  return ids;
};

/** What a list draws from each item. The playing slot is left out on purpose:
 *  it moves once a track, and rebuilding every row — up to 500 of them — to
 *  move one marker was the whole cost of a track change. The title and artist
 *  are in because a rename of an artist re-sends the queue with new text. The
 *  slot index is in because a row jumps by it (`data-slot`): when an unnamed
 *  slot drops out the indexes shift under rows that otherwise look the same,
 *  and a stale one sends the jump down the forward-search fallback. */
const queueIdentity = (items) =>
  items
    .map((item) => `${item.index}:${item.videoId}:${item.rating || ''}:${item.title}\t${item.artist}`)
    .join('\n');

/** One queue slot as a button: its art, title and artist. `kind` names the
 *  classes (`upnext-item`, `queue-item`, …), so the strip and the panel share
 *  the markup and differ only in CSS. */
const trackRow = (kind, item, playing) => {
  const row = document.createElement('button');
  row.className = `${kind}-item`;
  // Only the playing track is marked. Already-played ones are left at full
  // strength — they are as readable as anything else in the list.
  if (playing) row.className += ' now';
  if (item.rating === 'DISLIKE') row.className += ' disliked';
  row.type = 'button';
  // Both: the slot index is what a jump takes, the id is what verifies it.
  row.dataset.slot = String(item.index);
  row.dataset.videoId = item.videoId;
  // Vinyl draws only the cover, so the words are a hover away.
  if (kind === 'upnext' && skinOf(state) === 'vinyl') {
    row.title = item.artist ? `${item.title} — ${item.artist}` : item.title;
  }

  const img = document.createElement('img');
  img.alt = '';
  img.dataset.art = item.videoId;
  const art = artOf(item.videoId);
  if (art) img.src = art;

  const text = document.createElement('span');
  text.className = `${kind}-text`;
  const name = document.createElement('span');
  name.className = `${kind}-name`;
  name.textContent = item.title;
  const artist = document.createElement('span');
  artist.className = `${kind}-artist`;
  artist.textContent = item.artist;
  text.append(name, artist);

  row.append(img, text);
  return row;
};

/** What is currently drawn, so an unchanged queue is left alone. */
let upnextRendered = null;

const renderUpNext = () => {
  // Hidden whenever the setup screen is up: a queue from a previous session is
  // stale the moment YouTube Music goes away.
  const playerShowing = state.status === 'connected' && !!state.song;
  el.upnext.hidden = !playerShowing;
  if (el.upnext.hidden) return;

  const { items, current } = state.queue;

  // The queue changes about once a track. Tearing the cards down and building
  // them again — images and all — on every position tick was most of what this
  // surface cost while a track played.
  const identity = queueIdentity(items);
  if (identity === upnextRendered) {
    // Same list, so at most the playing mark moves — and only then is it worth
    // measuring anything to re-centre.
    const was = el.upnextGrid.querySelector(':scope > .now');
    const now = el.upnextGrid.children[current] || null;
    if (was === now) return;
    was?.classList.remove('now');
    now?.classList.add('now');
    centreUpNext();
    return;
  }
  upnextRendered = identity;

  el.upnextGrid.replaceChildren(...items.map((item, at) => trackRow('upnext', item, at === current)));

  centreUpNext();
};

const centreUpNext = () => {
  // Centre the playing track rather than parking it at the left edge, so what
  // has just played sits to its left and what is coming to its right — with
  // three columns that puts it in the middle one. Clamped at zero, so the start
  // of a queue sits flush left rather than leaving a gap.
  //
  // Measured off the card rather than computed from a column width: the column
  // is a percentage of the content box less the gap, and guessing at that
  // drifts further with every column.
  const now = el.upnextGrid.children[state.queue.current ?? 0];
  if (now && upnextVertical()) {
    const centred = now.offsetTop + now.offsetHeight / 2 - el.upnextGrid.clientHeight / 2;
    el.upnextGrid.scrollTop = Math.max(0, centred);
  } else if (now) {
    const centred = now.offsetLeft + now.offsetWidth / 2 - el.upnextGrid.clientWidth / 2;
    el.upnextGrid.scrollLeft = Math.max(0, centred);
  }
  upnextArtInView();
};

/** Liner lists the queue down a column; the other skins scroll it sideways. */
const upnextVertical = () => skinOf(state) === 'liner';

/** Artwork for the cards on screen, plus a screenful either side. */
const upnextArtInView = () => {
  // Liner's track list is words only.
  if (upnextVertical()) return;
  // Indexed live rather than copied: this runs on every scroll event, and only
  // a screenful of the list is wanted.
  const cards = el.upnextGrid.children;
  if (!cards.length) return;
  // Rows flowing by column: two on Stack, one on Vinyl, told apart by whether
  // the second card shares the first one's column. The column step is
  // measured rather than copied from the stylesheet's width and gap.
  const rows = cards.length > 1 && cards[1].offsetLeft === cards[0].offsetLeft ? 2 : 1;
  const step = cards.length > rows ? cards[rows].offsetLeft - cards[0].offsetLeft : cards[0].offsetWidth;
  if (!step) return;
  const view = Math.ceil(el.upnextGrid.clientWidth / step) * rows;
  const first = Math.floor(el.upnextGrid.scrollLeft / step) * rows;
  wantArt(unpaintedIds(cards, first - view, first + view * 2), 128);
};

el.upnextGrid.addEventListener('scroll', upnextArtInView, { passive: true });

/* The strip only overflows sideways, so a plain wheel would do nothing to it
   and everything to the volume — the card's own handler owns the wheel.
   Claiming the event here stops it reaching that handler at all, which is what
   makes scrolling the queue never touch the sound, and maps a vertical wheel
   onto the axis that actually moves. */
el.upnext.addEventListener(
  'wheel',
  (event) => {
    event.preventDefault();
    event.stopPropagation();
    if (upnextVertical()) el.upnextGrid.scrollTop += event.deltaY;
    else el.upnextGrid.scrollLeft += event.deltaX || event.deltaY;
  },
  { passive: false },
);

/** What the panel has drawn, so scrolling it does not rebuild it. */
let queueRendered = null;

/** The equaliser on the playing row, its duration on every other. */
const setQueueTail = (row, item, playing) => {
  row.querySelector(':scope > .queue-eq, :scope > .queue-dur')?.remove();
  if (playing) {
    const eq = document.createElement('span');
    eq.className = 'queue-eq';
    eq.append(document.createElement('i'), document.createElement('i'), document.createElement('i'));
    row.append(eq);
  } else if (item?.duration) {
    const dur = document.createElement('span');
    dur.className = 'queue-dur';
    dur.textContent = item.duration;
    row.append(dur);
  }
};

/** Scroll the playing row to the middle of the panel. */
const centreQueue = () => {
  const now = el.queueList.querySelector(':scope > .now');
  if (now) {
    el.queueList.scrollTop = now.offsetTop - el.queueList.clientHeight / 2 + now.offsetHeight / 2;
  }
};

/** Which item the panel's `.now` row is, so moving it knows what to restore. */
let queueNowAt = null;

/** `centre` asks for the playing row in the middle even if nothing changed —
 *  opening the panel does, rather than coming back wherever the last browse
 *  left it. */
const renderQueue = ({ centre = false } = {}) => {
  if (el.queue.hidden) return;

  // The note is the panel's only text now that the head is gone, and it earns
  // its height only when there is something to say. A truncated queue used to
  // say so with a `+` on the slot count up there; at the foot it sits where the
  // list actually stops.
  const { items, current } = state.queue;
  const note = !items.length ? 'Nothing queued.' : state.queue.truncated ? `First ${items.length}.` : '';
  el.queueNote.textContent = note;
  el.queueNote.hidden = !note;

  const identity = queueIdentity(items);
  if (identity === queueRendered) {
    // Same list: hand the playing mark and its equaliser over to the new row.
    // Not re-centred on a track change — the user's scroll position is theirs.
    const rows = el.queueList.children;
    if (queueNowAt !== current) {
      const was = rows[queueNowAt];
      if (was) {
        was.classList.remove('now');
        setQueueTail(was, items[queueNowAt], false);
      }
      const now = rows[current];
      if (now) {
        now.classList.add('now');
        setQueueTail(now, items[current], true);
      }
      queueNowAt = current;
    }
    if (centre) {
      centreQueue();
      queueArtInView();
    }
    return;
  }
  const first = queueRendered === null;
  queueRendered = identity;
  queueNowAt = current;

  el.queueList.replaceChildren(
    ...items.map((item, at) => {
      const row = trackRow('queue', item, at === current);
      setQueueTail(row, item, at === current);
      return row;
    }),
  );

  // Centre the playing track. On the first draw or an open only — after that
  // the user's scroll position is theirs, and a track change should not yank
  // the list out from under a browse.
  if (first || centre) centreQueue();
  queueArtInView();
};

/** Artwork for the rows on screen, plus a screenful either side. */
const queueArtInView = () => {
  // Indexed live rather than copied, as in the strip: this runs per scroll event.
  const rows = el.queueList.children;
  if (!rows.length) return;
  const row = rows[0].offsetHeight || 40;
  const first = Math.floor(el.queueList.scrollTop / row);
  const visible = Math.ceil(el.queueList.clientHeight / row);
  wantArt(unpaintedIds(rows, first - visible, first + visible * 2), 128);
};

el.queueList.addEventListener('scroll', queueArtInView, { passive: true });

/** Rust's `queue-art-ready` nudge: lookups that missed a reply have landed,
 *  so every row on screen without art asks again, in flight or not. Wired
 *  in boot.js. */
const queueArtReady = () => {
  artInFlight.clear();
  queueArtInView();
  upnextArtInView();
};

el.queueList.addEventListener('click', (event) => {
  const row = event.target.closest('.queue-item');
  if (!row) return;
  // By slot index, not just by id: the panel shows already-played rows, and a
  // forward-only search by id cannot reach those — it would queue a duplicate.
  jumpTo(Number(row.dataset.slot), row.dataset.videoId);
});

el.upnextGrid.addEventListener('click', (event) => {
  // The strip reaches behind the playing track now, and `play_queued`'s
  // forward-only search cannot serve a card from there — it would queue a
  // duplicate. Same slot-index jump the queue panel uses.
  const card = event.target.closest('.upnext-item');
  if (card) jumpTo(Number(card.dataset.slot), card.dataset.videoId);
});

/** Jump to a slot, moving our own idea of where the playhead is first.
 *
 *  The jump waits for the player to actually land before the queue is re-read,
 *  which can take a couple of seconds. With four upcoming rows that was
 *  invisible; with the whole list on screen, the wrong row wearing the playing
 *  mark for that long is not. Same bargain the transport already makes: move
 *  optimistically, and let the push correct us either way. */
const jumpTo = (slot, videoId) => {
  const at = state.queue.items.findIndex((item) => item.index === slot);
  if (at >= 0) {
    state.queue = { ...state.queue, current: at };
    renderUpNext();
    renderQueue();
  }
  window.widget.playQueueIndex(slot, videoId);
};

const toggleQueue = () => {
  if (openPanel === 'queue') {
    setPanel(null);
    return;
  }
  if (searching) resetSearch(false);
  // Re-centred on the playing track each time it is opened, rather than coming
  // back wherever the last browse left it. Only a changed list is rebuilt.
  setPanel('queue');
  renderQueue({ centre: true });
};

el.cornerQueue.addEventListener('click', (event) => {
  event.stopPropagation();
  toggleQueue();
});
