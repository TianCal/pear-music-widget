'use strict';

const $ = (id) => document.getElementById(id);

// The same document is loaded twice: once as the floating widget, once as the
// menu-bar dropdown. The dropdown is a fixed size and shows time remaining.
const IS_PANEL = new URLSearchParams(location.search).get('mode') === 'panel';
if (IS_PANEL) document.body.classList.add('panel');

// PROTOTYPE HOOK: ?variant= picks a Next-tracks layout. Delete with it.
const PROTO = new URLSearchParams(location.search).get('variant');
if (PROTO) document.body.classList.add(`proto-${PROTO}`);

/** Must match `SKINS` in src-tauri/src/window.rs. */
const SKINS = ['classic', 'stack'];

/* The three panels that share the one slot, in corner-bar order. A table rather
   than three enumerations: opening, closing and the dropdown's blur teardown
   each used to list every panel and every corner button by hand, and a third
   panel would have made that three near-identical lists to keep in step.
   `corner` also names the flag in `state.corners` that hides the button. */
const PANELS = [
  { key: 'queue', section: 'queue', corner: 'cornerQueue' },
  { key: 'lyrics', section: 'lyrics', corner: 'cornerLyrics' },
  { key: 'search', section: 'search', corner: 'cornerSearch' },
];

/** Each surface picks its own skin: the dropdown's is configured separately. */
const skinOf = (snapshot) => (IS_PANEL ? snapshot.panelSkin : snapshot.skin) || 'classic';

const el = {
  card: $('card'),
  player: $('player'),
  cover: $('cover'),
  title: $('title'),
  subtitle: $('subtitle'),
  seek: $('seek'),
  rail: document.querySelector('.rail'),
  fill: $('fill'),
  knob: $('knob'),
  upnext: $('upnext'),
  upnextGrid: $('upnext-grid'),
  play: $('btn-play'),
  playIcon: $('play-icon'),
  shuffle: $('btn-shuffle'),
  like: $('btn-like'),
  mute: $('btn-mute'),
  volumeIcon: $('volume-icon'),
  volHit: $('vol-hit'),
  volRail: $('vol-rail'),
  volFill: $('vol-fill'),
  volPop: document.querySelector('.vol-pop'),
  cornerSearch: $('btn-search'),
  cornerLyrics: $('btn-lyrics'),
  cornerQueue: $('btn-queue'),
  cornerBar: document.querySelector('.corner-bar'),
  queue: $('queue'),
  queueList: $('queue-list'),
  queueCount: $('queue-count'),
  queueNote: $('queue-note'),
  lyrics: $('lyrics'),
  lyricsScroll: $('lyrics-scroll'),
  lyricsLines: $('lyrics-lines'),
  lyricsNote: $('lyrics-note'),
  search: $('search'),
  searchInput: $('search-input'),
  searchClose: $('search-close'),
  results: $('results'),
  searchNote: $('search-note'),
  setup: $('setup'),
  setupTitle: $('setup-title'),
  setupBody: $('setup-body'),
  setupRetry: $('setup-retry'),
  setupOpen: $('setup-open'),
};

/** Latest snapshot from the main process.
 *
 *  `status` down is pushed on the `state` event roughly once a second; `cover`,
 *  `queue` and `lyrics` arrive on their own events and are merged in here, so
 *  everything below still reads from one object. */
let state = {
  status: 'connecting',
  skin: 'classic',
  panelSkin: 'classic',
  tint: 1,
  lyricsOffset: 0,
  corners: { queue: true, lyrics: true, search: true },
  cornersAutohide: 0,
  /* The whole queue, text only. Artwork arrives separately, by id, for the rows
     a surface is actually showing — see `wantArt`. */
  queue: { items: [], current: null, truncated: false },
  art: new Map(),
  lyrics: null,
  lyricsState: 'idle',
  song: null,
  cover: null,
  isPlaying: false,
  position: 0,
  volume: 100,
  muted: false,
  shuffle: false,
  like: null,
};

// Local playhead: the server only pushes a position roughly once a second, so
// we extrapolate between updates to keep the bar moving at 60fps.
const clock = { position: 0, at: performance.now(), playing: false };

let seeking = false;
let seekPreview = 0;
let ignorePositionUntil = 0;

let skinApplied = null;
let lastCoverSrc = null;
let lastTitle = null;
let lastSubtitle = null;

// ------------------------------------------------------------------ format

const formatTime = (seconds) => {
  const s = Math.max(0, Math.floor(seconds || 0));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, '0');
  return h > 0 ? `${h}:${String(m).padStart(2, '0')}:${sec}` : `${m}:${sec}`;
};

const clamp = (v, min, max) => Math.min(max, Math.max(min, v));

// ----------------------------------------------------------------- accent

let paletteToken = 0;

/** The whole card is tinted from the cover, not just the transport. */
const applyPalette = async (coverDataUrl) => {
  const token = ++paletteToken;
  const palette = await window.palette.extract(coverDataUrl, state.tint ?? 1);
  if (token !== paletteToken) return; // a newer cover won the race

  const root = document.documentElement.style;
  root.setProperty('--accent', palette.accent);
  root.setProperty('--accent-soft', palette.accentSoft);
  root.setProperty('--wash-1', palette.wash1);
  root.setProperty('--wash-2', palette.wash2);
  root.setProperty('--wash-3', palette.wash3);
  root.setProperty('--wash-base', palette.washBase);
};

// A surface that was hidden when the track changed may have had nothing to
// sample, so take the colours again once it is back on screen. Cheap, and it is
// the only thing that can correct a cover resolved while the dropdown was shut.
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState !== 'visible') return;
  if (state.cover) applyPalette(state.cover);
  // The progress loop stops while the surface is off screen, so the playhead is
  // stale by however long it was away. Catch it up and start ticking again.
  bumpProgress();
});

// Dark and light get different washes from the same artwork, so they have to be
// rebuilt when the system flips rather than just restyled.
matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
  applyPalette(state.cover);
});

// --------------------------------------------------------------- marquee

/** Scroll long text only when it actually overflows. */
const setupMarquee = (span) => {
  const box = span.parentElement;
  box.classList.remove('scroll');
  span.style.removeProperty('--drift-distance');

  const overflow = span.scrollWidth - box.clientWidth;
  if (overflow <= 4) return;

  // Overshoot by the width of the fade so the tail clears the mask entirely.
  const distance = overflow + 18;
  span.style.setProperty('--drift-distance', `${-distance}px`);
  span.style.setProperty('--drift-duration', `${clamp(distance / 16, 5, 18)}s`);
  box.classList.add('scroll');
};

// ---------------------------------------------------------------- render

const STATUS_COPY = {
  connecting: {
    title: 'Connecting…',
    body: 'Looking for the YouTube Music API server.',
  },
  offline: {
    title: 'API server unreachable',
    body:
      'Open YouTube Music, then enable <code>API Server</code> under Plugins. ' +
      'The widget listens on port <code>26538</code>.',
  },
  unauthorized: {
    title: 'Waiting for approval',
    body: 'YouTube Music is asking whether to allow <code>PearMusicWidget</code>. Click Allow in that dialog.',
  },
  denied: {
    title: 'Access denied',
    body: 'The authorisation request was denied in YouTube Music. Retry to ask again.',
  },
};

/* Auto-hide. The buttons are chrome over someone else's artwork, and on Classic
   they sit right on top of the title, so they can be asked to fade once the
   pointer has been still for a while. */
let cornerIdleTimer = null;
let cornerWokeAt = 0;
/** The delay the running timer was armed with, so a state push that did not
 *  change it leaves the countdown alone. */
let cornerFadeArmedFor = null;

const armCornerFade = () => {
  clearTimeout(cornerIdleTimer);
  cornerIdleTimer = null;
  const after = state.cornersAutohide || 0;
  cornerFadeArmedFor = after;
  // Never while a panel is open — its lit button is the way back out of it.
  if (!after || openPanel) return;
  cornerIdleTimer = setTimeout(() => el.cornerBar.classList.add('idle'), after * 1000);
};

/** The pointer moved, so start the countdown over. */
const wakeCorners = () => {
  const now = performance.now();
  const asleep = el.cornerBar.classList.contains('idle');
  // A mousemove fires far faster than this needs to run, and re-arming a
  // timeout per event is pure churn while the pointer is travelling.
  if (!asleep && now - cornerWokeAt < 200) return;
  cornerWokeAt = now;
  el.cornerBar.classList.remove('idle');
  armCornerFade();
};

/* Four signals, because a widget is meant to be used without focusing it first
   and pointer *position* is the one thing a background window cannot count on:
   AppKit delivers `mouseMoved` to the key window alone, and this one is usually
   not it. `mouseover` covers hover, which tracking areas still fire in the
   background; `wheel` and `mousedown` cover the interactions that always
   arrive, and are the guaranteed way back to a faded bar. */
for (const signal of ['mousemove', 'mouseover', 'mousedown', 'wheel']) {
  document.addEventListener(signal, wakeCorners, { passive: true });
}

const renderStatus = () => {
  const showSetup = state.status !== 'connected' || !state.song;

  // Nothing to search, read along with or queue against unless the API server
  // is answering — and each button can also be turned off from the menu.
  const offline = state.status !== 'connected';
  const corners = state.corners || {};
  for (const panel of PANELS) {
    el[panel.corner].hidden = offline || corners[panel.key] === false;
  }
  // The gutter the titles reserve follows the buttons that are actually there.
  document.body.style.setProperty(
    '--corner-count',
    String(offline ? 0 : PANELS.filter((panel) => corners[panel.key] !== false).length),
  );

  // Only when the setting itself moved. `renderStatus` runs on every state
  // push — about once a second — and re-arming the countdown each time is what
  // made it never expire at all.
  if (cornerFadeArmedFor !== (state.cornersAutohide || 0)) {
    el.cornerBar.classList.remove('idle');
    armCornerFade();
  }

  // A panel whose button has just gone is a panel with no way back out of it.
  const orphaned = openPanel && corners[openPanel] === false;
  if ((offline || orphaned) && searching) closeSearch();
  else if ((offline || orphaned) && openPanel) setPanel(null);

  if (!showSetup) {
    el.setup.hidden = true;
    el.player.hidden = false;
    return;
  }

  el.player.hidden = true;
  el.setup.hidden = false;

  if (state.status === 'connected') {
    el.setupTitle.textContent = 'Nothing playing';
    el.setupBody.innerHTML = 'Start a track in YouTube Music and it will show up here.';
    el.setupRetry.hidden = true;
    el.setupOpen.hidden = true;
    return;
  }

  const copy = STATUS_COPY[state.status] || STATUS_COPY.connecting;
  el.setupTitle.textContent = copy.title;
  el.setupBody.innerHTML = copy.body;
  el.setupRetry.hidden = state.status === 'connecting';
  // Launching the app is the fix for 'offline'; the other states need a retry.
  el.setupOpen.hidden = state.status !== 'offline';
};

const renderCover = () => {
  if (state.cover === lastCoverSrc) return;
  lastCoverSrc = state.cover;

  if (state.cover) {
    el.cover.src = state.cover;
    el.card.classList.add('has-art');
  } else {
    el.card.classList.remove('has-art');
  }
  applyPalette(state.cover);
};

const renderSong = () => {
  const song = state.song;
  const title = song?.title || 'Nothing playing';
  const subtitle = song ? [song.artist, song.album].filter(Boolean).join(' — ') : '';

  if (title !== lastTitle) {
    lastTitle = title;
    el.title.textContent = title;
    requestAnimationFrame(() => setupMarquee(el.title));
  }
  if (subtitle !== lastSubtitle) {
    lastSubtitle = subtitle;
    el.subtitle.textContent = subtitle;
    requestAnimationFrame(() => setupMarquee(el.subtitle));
  }

};

// ------------------------------------------------------------------ queue

/* One channel, two readers: Stack's "Next tracks" strip and the queue panel.
   The event carries text only — artwork is asked for by id, for the rows a
   surface is actually showing, because resolving every slot would be megabytes
   for tracks nobody has scrolled to. */

/** Artwork already resolved, keyed by videoId. Survives a queue change: the
 *  same track scrolled back to is already paid for. */
const artOf = (videoId) => state.art.get(videoId) || null;

let artPending = null;
let artTimer = null;

/** Ask for artwork for `ids`, batched across a frame or two of scrolling so a
 *  drag does not fire a request per row it passes. */
const wantArt = (ids, size) => {
  const missing = ids.filter((id) => id && !state.art.has(id));
  if (!missing.length) return;

  artPending = artPending || { ids: new Set(), size };
  artPending.size = Math.max(artPending.size, size);
  for (const id of missing) artPending.ids.add(id);

  clearTimeout(artTimer);
  artTimer = setTimeout(async () => {
    const batch = artPending;
    artPending = null;
    const reply = await window.widget.queueArt([...batch.ids], batch.size);
    if (!reply?.ok) return;

    let landed = false;
    for (const [id, art] of Object.entries(reply.art || {})) {
      state.art.set(id, art);
      landed = true;
    }
    // Written straight into whatever is on screen rather than re-rendering: a
    // reply usually lands mid-scroll, and rebuilding would fight the drag that
    // asked for it.
    if (landed) paintArt();
  }, 120);
};

/** Fill in any `<img data-art>` whose artwork has since arrived. */
const paintArt = () => {
  for (const img of document.querySelectorAll('img[data-art]')) {
    const art = artOf(img.dataset.art);
    if (art && img.src !== art) img.src = art;
  }
};

/** What is currently drawn, so an unchanged queue is left alone. */
let upnextRendered = null;

const renderUpNext = () => {
  // Hidden whenever the setup screen is up: a queue from a previous session is
  // stale the moment YouTube Music goes away.
  const playerShowing = state.status === 'connected' && !!state.song;
  el.upnext.hidden = !playerShowing || skinOf(state) !== 'stack';
  if (el.upnext.hidden) return;

  const { items, current } = state.queue;

  // The queue changes about once a track. Tearing the cards down and building
  // them again — images and all — on every position tick was most of what this
  // surface cost while a track played.
  const identity = `${current}\n${items.map((item) => item.videoId).join('\n')}`;
  if (identity === upnextRendered) {
    paintArt();
    return;
  }
  upnextRendered = identity;

  el.upnextGrid.replaceChildren();
  items.forEach((item, at) => {
    const card = document.createElement('button');
    card.className = 'upnext-item';
    // Only the playing track is marked. Already-played ones are left at full
    // strength — they are as readable as anything else in the list, and dimming
    // them made the strip look half broken.
    if (at === current) card.className += ' now';
    card.type = 'button';
    // Both: the slot index is what a jump takes, the id is what verifies it.
    card.dataset.slot = String(item.index);
    card.dataset.videoId = item.videoId;

    const img = document.createElement('img');
    img.alt = '';
    img.dataset.art = item.videoId;
    const art = artOf(item.videoId);
    if (art) img.src = art;

    const text = document.createElement('span');
    text.className = 'upnext-text';

    const name = document.createElement('span');
    name.className = 'upnext-name';
    name.textContent = item.title;

    const artist = document.createElement('span');
    artist.className = 'upnext-artist';
    artist.textContent = item.artist;

    text.append(name, artist);
    card.append(img, text);
    el.upnextGrid.append(card);
  });

  // Centre the playing track rather than parking it at the left edge, so what
  // has just played sits to its left and what is coming to its right — with
  // three columns that puts it in the middle one. Clamped at zero, so the start
  // of a queue simply sits flush left rather than leaving a gap.
  // Measured off the card rather than computed from a column width: the column
  // is a percentage of the content box less the gap, and guessing at that
  // drifts further with every column.
  const now = el.upnextGrid.children[current ?? 0];
  if (now) {
    const centred = now.offsetLeft + now.offsetWidth / 2 - el.upnextGrid.clientWidth / 2;
    el.upnextGrid.scrollLeft = Math.max(0, centred);
  }
  upnextArtInView();
};

/** Artwork for the cards on screen, plus a screenful either side. */
const upnextArtInView = () => {
  const cards = [...el.upnextGrid.children];
  if (!cards.length) return;
  // Two cards to a column, so a screenful is four and the step is one card's
  // width plus the gap.
  const step = (cards[0].offsetWidth || 140) + 8;
  const first = Math.floor(el.upnextGrid.scrollLeft / step) * 2;
  wantArt(
    cards.slice(Math.max(0, first - 4), first + 12).map((card) => card.dataset.videoId),
    128,
  );
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
    el.upnextGrid.scrollLeft += event.deltaX || event.deltaY;
  },
  { passive: false },
);

/** What the panel has drawn, so scrolling it does not rebuild it. */
let queueRendered = null;

const renderQueue = () => {
  if (el.queue.hidden) return;

  const { items, current } = state.queue;
  el.queueCount.textContent = items.length
    ? `${(current ?? -1) + 1 || '–'} of ${items.length}${state.queue.truncated ? '+' : ''}`
    : '';
  el.queueNote.textContent = items.length ? '' : 'Nothing queued.';
  el.queueNote.hidden = !!items.length;

  const identity = `${current}\n${items.map((item) => item.videoId).join('\n')}`;
  if (identity === queueRendered) {
    paintArt();
    return;
  }
  const first = queueRendered === null;
  queueRendered = identity;

  el.queueList.replaceChildren();
  items.forEach((item, at) => {
    const row = document.createElement('button');
    row.className = 'queue-item';
    if (at === current) row.className += ' now';
    row.type = 'button';
    // Both: the slot index is what the jump takes, the id is what verifies it.
    row.dataset.slot = String(item.index);
    row.dataset.videoId = item.videoId;

    const img = document.createElement('img');
    img.alt = '';
    img.dataset.art = item.videoId;
    const art = artOf(item.videoId);
    if (art) img.src = art;

    const text = document.createElement('span');
    text.className = 'queue-text';

    const name = document.createElement('span');
    name.className = 'queue-name';
    name.textContent = item.title;

    const artist = document.createElement('span');
    artist.className = 'queue-artist';
    artist.textContent = item.artist;

    text.append(name, artist);
    row.append(img, text);

    if (at === current) {
      const eq = document.createElement('span');
      eq.className = 'queue-eq';
      eq.append(document.createElement('i'), document.createElement('i'), document.createElement('i'));
      row.append(eq);
    } else if (item.duration) {
      const dur = document.createElement('span');
      dur.className = 'queue-dur';
      dur.textContent = item.duration;
      row.append(dur);
    }

    el.queueList.append(row);
  });

  // Centre the playing track. On the first draw only — after that the user's
  // scroll position is theirs, and a track change should not yank the list out
  // from under a browse.
  const now = el.queueList.querySelector('.queue-item.now');
  if (now && first) {
    el.queueList.scrollTop = now.offsetTop - el.queueList.clientHeight / 2 + now.offsetHeight / 2;
  }
  queueArtInView();
};

/** Artwork for the rows on screen, plus a screenful either side. */
const queueArtInView = () => {
  const rows = [...el.queueList.children];
  if (!rows.length) return;
  const row = rows[0].offsetHeight || 40;
  const first = Math.floor(el.queueList.scrollTop / row);
  const visible = Math.ceil(el.queueList.clientHeight / row);
  wantArt(
    rows
      .slice(Math.max(0, first - visible), first + visible * 2)
      .map((node) => node.dataset.videoId),
    128,
  );
};

el.queueList.addEventListener('scroll', queueArtInView, { passive: true });

el.queueList.addEventListener('click', (event) => {
  const row = event.target.closest('.queue-item');
  if (!row) return;
  // By slot index, not just by id: the panel shows already-played rows, and a
  // forward-only search by id cannot reach those — it would queue a duplicate.
  jumpTo(Number(row.dataset.slot), row.dataset.videoId);
});

/* Written on every push, but almost never actually different. `classList.toggle`
   is already a no-op when the class is where it should be; `setAttribute` on a
   `<use>` is not — it re-resolves the reference — and neither is a style write. */
const written = { play: null, like: null, volume: null };

const renderControls = () => {
  if (written.play !== state.isPlaying) {
    written.play = state.isPlaying;
    el.playIcon.firstElementChild.setAttribute('href', state.isPlaying ? '#i-pause' : '#i-play');
    el.play.title = state.isPlaying ? 'Pause' : 'Play';
  }

  el.shuffle.classList.toggle('on', !!state.shuffle);

  const like = (state.like || '').toUpperCase();
  if (written.like !== like) {
    written.like = like;
    el.like.classList.toggle('on', like === 'LIKE');
    el.like.title = like === 'LIKE' ? 'Remove like' : 'Like';
  }

  // Volume 0 reads as muted even when the player's own mute flag is off.
  const level = clamp(state.muted ? 0 : state.volume, 0, 100);
  if (written.volume !== level) {
    written.volume = level;
    const silent = state.muted || state.volume === 0;
    el.volFill.style.width = `${level}%`;
    el.volumeIcon.firstElementChild.setAttribute('href', silent ? '#i-muted' : '#i-volume');
    el.mute.classList.toggle('on', silent);
    el.mute.title = `Volume ${Math.round(level)}%`;
  }
};

/* The rail's pixel width, cached. Measuring it inside the tick would force a
   layout flush every frame, which is most of what this whole path is trying to
   avoid. A skin switch and a window resize both change it. */
let railWidth = 0;
let seekX = -1; // last `--seek-x` written, in whole pixels

const renderProgress = () => {
  const duration = state.song?.songDuration || 0;
  let position;

  if (seeking) {
    position = seekPreview;
  } else if (clock.playing) {
    position = clock.position + (performance.now() - clock.at) / 1000;
  } else {
    position = clock.position;
  }

  position = clamp(position, 0, duration || position);
  const ratio = duration > 0 ? clamp(position / duration, 0, 1) : 0;

  // A four-minute track moves the playhead about two pixels a second, so at
  // display refresh all but a couple of frames a second land on the pixel that
  // is already on screen. Writing the same position back anyway is what made
  // this loop cost a full layout, paint and shadow re-blur 120 times a second.
  const x = Math.round(ratio * railWidth);
  if (x !== seekX) {
    seekX = x;
    el.seek.style.setProperty('--seek-x', `${x}px`);
  }

  // Not behind the pixel guard: lyric lines turn over on their own timetable,
  // and this is a binary search that touches the DOM only when the line
  // actually changes.
  rollLyrics(position);
};

new ResizeObserver((entries) => {
  railWidth = entries[entries.length - 1].contentRect.width;
  seekX = -1; // the cached pixel is against the old width, so force a rewrite
  renderProgress();
}).observe(el.rail);

// The playhead only moves under its own steam while the track is playing, and
// only matters while the window is on screen. Outside of that the loop is not
// smoothing anything — it is redrawing a still frame — so it stops, and the
// events that can move the playhead restart it.
let ticking = false;

const shouldTick = () => !document.hidden && (clock.playing || seeking);

const tick = () => {
  renderProgress();
  ticking = shouldTick();
  if (ticking) requestAnimationFrame(tick);
};

/** Render now, and resume the loop if there is anything left to animate. */
const bumpProgress = () => {
  if (ticking) return; // the running loop will pick the change up next frame
  if (shouldTick()) {
    ticking = true;
    requestAnimationFrame(tick);
  } else {
    renderProgress();
  }
};

// ------------------------------------------------------------------ state

const applyState = (next) => {
  const positionChanged = next.position !== state.position;
  const playingChanged = next.isPlaying !== state.isPlaying;
  const songChanged = next.song?.videoId !== state.song?.videoId;
  const tintChanged = next.tint !== state.tint;
  const skin = skinOf(next);

  // Keep the level the user is actually setting while our own echoes drain.
  const heldVolume = holdingVolume() ? { volume: state.volume, muted: state.muted } : null;

  // Merged rather than replaced: artwork, the queue and the lyrics live on this
  // same object but arrive on their own events, and this push does not carry them.
  Object.assign(state, next);

  if (heldVolume) {
    state.volume = heldVolume.volume;
    state.muted = heldVolume.muted;
  }

  // Tracked separately from `state` so the very first push applies the class too.
  if (skin !== skinApplied) {
    skinApplied = skin;
    // Removed from a list rather than by name: a skin added to the table in
    // window.rs and not here leaves both classes on the body, which reads as a
    // stuck skin with no error anywhere.
    document.body.classList.remove(...SKINS.map((name) => `skin-${name}`));
    document.body.classList.add(`skin-${skin}`);
    // Column widths and font sizes both changed, so the drift maths is stale.
    requestAnimationFrame(() => {
      setupMarquee(el.title);
      setupMarquee(el.subtitle);
    });
  }

  // A seek we just issued takes a moment to be reflected; ignore stale positions
  // in that window so the bar does not snap backwards.
  const stale = performance.now() < ignorePositionUntil;
  if (songChanged || ((positionChanged || playingChanged) && !stale && !seeking)) {
    clock.position = next.position || 0;
    clock.at = performance.now();
  }
  clock.playing = !!next.isPlaying;

  // The cover has not changed, so renderCover will not re-mix the wash.
  if (tintChanged) applyPalette(state.cover);

  renderStatus();
  renderSong();
  renderControls();
  renderUpNext();
  renderQueue();
  bumpProgress();
};

/** Artwork, the queue and the lyrics: each pushed only when it changes. */
const applyCover = (cover) => {
  state.cover = cover ?? null;
  renderCover();
};

const applyQueue = (view) => {
  state.queue = view || { items: [], current: null, truncated: false };
  renderUpNext();
  renderQueue();
};

const applyLyrics = (view) => {
  state.lyrics = view?.lyrics ?? null;
  state.lyricsState = view?.state || 'idle';
  renderLyrics();
  bumpProgress();
};

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

// ------------------------------------------------------------------ lyrics

let lyricLines = [];       // [{ time, text }]
let lyricSynced = false;
let lyricActive = -1;
let lyricManualUntil = 0;
let lyricManualTimer = null;

// How long a hand scroll holds the roll still before playback takes it back.
const LYRIC_MANUAL_MS = 5000;

/* What made the roll stutter was never the transition — it was that the target
   moved underneath it. The active line used to grow from 12px to 13.5px, which
   reflowed every line below it *while* the glide to it was still running, and
   `offsetTop` was read back inside the animation frame, forcing a layout flush
   on every line change.

   So: the emphasis is a transform now (nothing below an active line moves), the
   geometry is measured once per render, and the glide itself stays a CSS
   transition — the compositor runs it with no per-frame JavaScript at all. All
   this code does is write a destination and a duration, twice a verse. */
let lyricCentres = [];     // the offset that puts each line in the middle
let lyricRange = [0, 0];   // how far the roll may travel, first line to last
let lyricMeasured = false;
/** False until the roll has been put somewhere. The first placement after
 *  opening the panel or changing track snaps: gliding there would mean watching
 *  the words swoop up from the top every time you open the panel. */
let lyricSettled = false;
let lyricOffset = 0;

const LYRIC_GLIDE_MIN_MS = 260;
const LYRIC_GLIDE_MAX_MS = 620;
/** Milliseconds per pixel travelled, between those two bounds. */
const LYRIC_GLIDE_PACE = 5;

/** The line being sung sits this far above the true middle. Dead centre reads
 *  as slightly low, because what you want to see next is underneath it — the
 *  lines already sung need less room than the ones coming. */
const LYRIC_RAISE = 14;

/** Where every line would have to sit to be centred. Costs one layout, taken
 *  when the panel is open and the lines have just changed shape. */
const measureLyrics = () => {
  const rows = el.lyricsLines.children;
  const height = el.lyricsScroll.clientHeight;
  // Nothing laid out yet — the panel is mid-open. Stay unmeasured and try again
  // on the next frame rather than caching a viewport of zero.
  if (!height) return false;
  const middle = height / 2;

  lyricCentres = [];
  for (let i = 0; i < rows.length; i += 1) {
    lyricCentres.push(rows[i].offsetTop + rows[i].offsetHeight / 2 - middle + LYRIC_RAISE);
  }

  // The roll travels between "first line centred" and "last line centred", and
  // no further. Clamping the low end at 0 instead is what put the opening lines
  // up in the top fade and let the closing ones drift down into the bottom one:
  // a line was being sung while it sat outside the readable band. Both ends are
  // allowed to be negative — a short lyric simply sits in the middle.
  const first = lyricCentres[0] ?? 0;
  const last = lyricCentres[lyricCentres.length - 1] ?? 0;
  lyricRange = [Math.min(first, last), Math.max(first, last)];
  lyricMeasured = true;
  return true;
};

/** Send the roll to `target`, over a distance-appropriate glide. Two style
 *  writes, and the compositor does the rest. */
const glideLyricsTo = (target, { instant = false } = {}) => {
  const to = clamp(target, lyricRange[0], lyricRange[1]);
  // A line-to-line step should feel immediate; a jump across a seek should not
  // race. Distance picks where between those the glide lands.
  const ms =
    instant ? 0
    : clamp(Math.abs(to - lyricOffset) * LYRIC_GLIDE_PACE, LYRIC_GLIDE_MIN_MS, LYRIC_GLIDE_MAX_MS);

  lyricOffset = to;
  el.lyricsLines.style.setProperty('--roll-ms', `${ms}ms`);
  el.lyricsLines.style.transform = `translateY(${(-to).toFixed(2)}px)`;
};

/** Scrolling over the lyrics moves the lyrics, not the volume. */
const scrollLyrics = (deltaY) => {
  if (!lyricSynced) return; // unsynced is a plain block that scrolls natively
  if (!lyricMeasured) measureLyrics();

  lyricManualUntil = performance.now() + LYRIC_MANUAL_MS;
  // Follows the wheel exactly: an eased glide would lag behind the fingers.
  glideLyricsTo(lyricOffset + deltaY, { instant: true });

  clearTimeout(lyricManualTimer);
  lyricManualTimer = setTimeout(() => {
    lyricActive = -1; // force the next tick to re-centre on the playing line
    bumpProgress();
  }, LYRIC_MANUAL_MS);
};

// A resize or a skin change rewraps the lines, so every measurement taken
// against the old shape is stale.
new ResizeObserver(() => {
  lyricMeasured = false;
  lyricSettled = false;
  if (openPanel === 'lyrics') {
    lyricActive = -1;
    bumpProgress();
  }
}).observe(el.lyricsScroll);

const LYRIC_NOTE = {
  idle: '',
  loading: 'Looking for lyrics…',
  none: 'No lyrics found for this track.',
};

/* No dirty check here any more: the main process pushes the `lyrics` event only
   when the words or the state actually change, so being called at all is the
   signal that this has to be rebuilt. */
const renderLyrics = () => {
  const data = state.lyrics;
  lyricLines = data?.lines || [];
  lyricSynced = !!data?.synced;
  lyricActive = -1;
  lyricManualUntil = 0;
  clearTimeout(lyricManualTimer);

  lyricOffset = 0;
  lyricMeasured = false;
  lyricSettled = false;
  lyricRange = [0, 0];

  el.lyrics.classList.toggle('plain', !!data && !lyricSynced);
  el.lyricsLines.replaceChildren();
  el.lyricsLines.style.removeProperty('--roll-ms');
  el.lyricsLines.style.transform = '';

  if (!lyricLines.length) {
    el.lyricsNote.textContent = LYRIC_NOTE[state.lyricsState] || LYRIC_NOTE.none;
    el.lyricsNote.hidden = !el.lyricsNote.textContent;
    return;
  }

  el.lyricsNote.textContent = lyricSynced ? '' : 'Unsynced lyrics — these do not follow along.';
  el.lyricsNote.hidden = lyricSynced;

  lyricLines.forEach((line, index) => {
    const row = document.createElement(lyricSynced ? 'button' : 'p');
    row.className = line.text ? 'lyric' : 'lyric gap';
    if (lyricSynced) {
      row.type = 'button';
      row.dataset.index = String(index);
    }
    row.textContent = line.text;
    el.lyricsLines.append(row);
  });
};

/** Seconds the roll runs ahead of the playhead, from the Lyrics timing menu.
 *  Positive turns the lines over earlier. Set once and kept across tracks: the
 *  drift is between the timings and the player's clock, not a property of any
 *  one song. */
const lyricShift = () => state.lyricsOffset || 0;

/** Index of the last line whose timestamp has passed. */
const lyricIndexAt = (position) => {
  let lo = 0;
  let hi = lyricLines.length - 1;
  let found = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (lyricLines[mid].time <= position) {
      found = mid;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  return found;
};

/** Called from the rAF tick with the interpolated playhead. */
const rollLyrics = (position) => {
  if (openPanel !== 'lyrics' || !lyricSynced || !lyricLines.length) return;
  // Deferred to here rather than done at render: the panel may well have been
  // closed then, and a hidden panel has no height to measure against.
  if (!lyricMeasured && !measureLyrics()) return;

  const index = lyricIndexAt(position + lyricShift());
  if (index === lyricActive) return;
  const previous = lyricActive;
  lyricActive = index;

  // Only the rows whose state actually changed. Repainting the class on all of
  // them meant a hundred style invalidations per line on a long lyric, to move
  // one word's worth of emphasis.
  const rows = el.lyricsLines.children;
  rows[previous]?.classList.remove('active');
  rows[index]?.classList.add('active');
  for (let i = Math.min(previous, index); i < Math.max(previous, index); i += 1) {
    rows[i]?.classList.toggle('past', i < index);
  }

  // Leave the roll where the user parked it until their scroll times out.
  if (performance.now() < lyricManualUntil) return;

  // Centre the active line. Before the first cue there is no active line, so
  // the roll waits on the opening one rather than pinning the list to the top
  // and leaving it half inside the fade.
  glideLyricsTo(lyricCentres[index] ?? lyricRange[0], { instant: !lyricSettled });
  lyricSettled = true;
};

const toggleLyrics = () => {
  if (openPanel === 'lyrics') {
    setPanel(null);
    return;
  }
  if (searching) resetSearch(false);
  // Opening lands on the line being sung rather than gliding to it from
  // wherever the roll happened to be left.
  lyricActive = -1;
  lyricMeasured = false;
  lyricSettled = false;
  setPanel('lyrics');
};

el.cornerLyrics.addEventListener('click', (event) => {
  event.stopPropagation();
  toggleLyrics();
});

const toggleQueue = () => {
  if (openPanel === 'queue') {
    setPanel(null);
    return;
  }
  if (searching) resetSearch(false);
  // Re-centred on the playing track each time it is opened, rather than coming
  // back wherever the last browse left it.
  queueRendered = null;
  setPanel('queue');
  renderQueue();
};

el.cornerQueue.addEventListener('click', (event) => {
  event.stopPropagation();
  toggleQueue();
});

// Clicking a line seeks to it — the timestamps are right there. `dataset.index`
// is not tested for truthiness: the first line's is the string "0", which is
// falsy, and guarding on it is what used to make the opening line unclickable.
// A `<p>` from an unsynced block has no index at all, so the lookup misses.
el.lyricsLines.addEventListener('click', (event) => {
  const row = event.target.closest('.lyric');
  if (!row || !lyricSynced) return;
  const line = lyricLines[Number(row.dataset.index)];
  if (!line) return;

  // Land the audio where this line *sounds*, which is the same correction the
  // roll is already applying — otherwise a nudged roll would seek you to a
  // point that then lights up a different line.
  const seconds = Math.max(0, line.time - lyricShift());
  clock.position = seconds;
  clock.at = performance.now();
  ignorePositionUntil = performance.now() + 1200;
  bumpProgress();
  send('seek', { seconds });
});

// ------------------------------------------------------------------ search

let openPanel = null; // 'queue' | 'lyrics' | 'search' | null

/** Write the open panel to the DOM. Shared by `setPanel` and the teardown the
 *  dropdown's blur pushes at us, so the two can never drift apart. */
const paintPanel = (which) => {
  document.body.classList.toggle('panel-open', !!which);
  for (const panel of PANELS) {
    el[panel.section].hidden = which !== panel.key;
    el[panel.corner].classList.toggle('on', which === panel.key);
  }
};

const setPanel = (which) => {
  openPanel = which;
  paintPanel(which);
  // Opening one parks the fade; closing one starts it counting again.
  el.cornerBar.classList.remove('idle');
  armCornerFade();
  return window.widget.setPanel(which);
};

let searching = false;
let searchSeq = 0;
let searchTimer = null;
let results = [];
let activeResult = -1;

const SEARCH_DEBOUNCE_MS = 400;

const setNote = (text) => {
  el.searchNote.textContent = text || '';
  el.searchNote.hidden = !text;
};

const renderResults = () => {
  el.results.replaceChildren();

  results.forEach((item, index) => {
    const row = document.createElement('button');
    row.className = 'result';
    row.type = 'button';
    row.dataset.index = String(index);
    if (index === activeResult) row.classList.add('active');

    if (item.thumbnail) {
      const img = document.createElement('img');
      img.src = item.thumbnail;
      img.alt = '';
      row.append(img);
    } else {
      const blank = document.createElement('img');
      blank.alt = '';
      row.append(blank);
    }

    const text = document.createElement('span');
    text.className = 'result-text';

    const title = document.createElement('span');
    title.className = 'result-title';
    title.textContent = item.title;

    const sub = document.createElement('span');
    sub.className = 'result-sub';
    sub.textContent = item.subtitle;

    text.append(title, sub);
    row.append(text);
    el.results.append(row);
  });
};

const highlight = (index) => {
  if (!results.length) return;
  activeResult = Math.min(results.length - 1, Math.max(0, index));
  [...el.results.children].forEach((row, i) => row.classList.toggle('active', i === activeResult));
  el.results.children[activeResult]?.scrollIntoView({ block: 'nearest' });
};

const runSearch = async (query) => {
  const seq = ++searchSeq;
  const trimmed = query.trim();

  if (!trimmed) {
    results = [];
    activeResult = -1;
    renderResults();
    setNote('Type to search.');
    return;
  }

  setNote('Searching…');
  const res = await window.widget.search(trimmed);
  if (seq !== searchSeq || !searching) return; // a newer query won, or we closed

  if (!res?.ok) {
    results = [];
    renderResults();
    setNote(res?.error || 'Search failed.');
    return;
  }

  results = res.results || [];
  activeResult = results.length ? 0 : -1;
  renderResults();
  setNote(results.length ? '' : 'No results.');
};

const openSearch = async () => {
  if (searching || state.status !== 'connected') return;
  searching = true;
  results = [];
  activeResult = -1;
  renderResults();
  setNote('Type to search.');
  await setPanel('search');
  el.searchInput.focus();
  el.searchInput.select();
};

/** Local teardown; `pushed` is true when main already shrank the window for us. */
const resetSearch = (pushed) => {
  searching = false;
  searchSeq += 1;
  clearTimeout(searchTimer);
  el.searchInput.value = '';
  results = [];
  activeResult = -1;
  el.results.replaceChildren();
  if (pushed) {
    openPanel = null;
    paintPanel(null);
  } else {
    setPanel(null);
  }
};

const closeSearch = () => {
  if (!searching) return;
  resetSearch(false);
};

el.cornerSearch.addEventListener('click', (event) => {
  event.stopPropagation();
  if (searching) closeSearch();
  else openSearch();
});

el.searchClose.addEventListener('click', (event) => {
  event.stopPropagation();
  closeSearch();
});

el.searchInput.addEventListener('input', () => {
  clearTimeout(searchTimer);
  const query = el.searchInput.value;
  searchTimer = setTimeout(() => runSearch(query), SEARCH_DEBOUNCE_MS);
});

el.results.addEventListener('click', (event) => {
  const row = event.target.closest('.result');
  if (!row) return;
  const item = results[Number(row.dataset.index)];
  if (!item) return;
  window.widget.playResult(item.videoId);
  closeSearch();
});

// The dropdown collapses itself when it loses focus; mirror that here.
window.widget.onPanelCollapsed(() => {
  if (searching) resetSearch(true);
  else if (openPanel) {
    openPanel = null;
    paintPanel(null);
  }
});

// Double-clicking brings YouTube Music forward. The artwork is the guaranteed
// target because it opts out of the drag region; the document-level listener
// covers the rest of the card for the case where the events do get through.
// Controls are excluded — a double click on one would fire it twice as well.
const INTERACTIVE =
  'button, input, .seek, .vol-hit, .result, .upnext-item, .queue-item';

const openMusicApp = () => window.widget.openApp();

document.addEventListener('dblclick', (event) => {
  if (IS_PANEL || event.target.closest(INTERACTIVE)) return;
  openMusicApp();
});

// Right-click either surface for a subset of the menu-bar menu — the widget
// gets its own layout and chrome, the dropdown gets its layout. Inputs keep
// their own menu, and the heart already uses right-click for dislike.
document.addEventListener('contextmenu', (event) => {
  if (event.target.closest('input')) return;
  event.preventDefault();
  window.widget.contextMenu();
});

// ---------------------------------------------------------------- commands

const send = (name, payload) => window.widget.command(name, payload);

document.querySelectorAll('[data-cmd]').forEach((button) => {
  button.addEventListener('click', (event) => {
    event.stopPropagation();
    const cmd = button.dataset.cmd;

    // Optimistic feedback: the round trip through YouTube Music is ~100ms.
    if (cmd === 'togglePlay') {
      clock.position = seeking ? seekPreview : clock.position + (clock.playing ? (performance.now() - clock.at) / 1000 : 0);
      clock.at = performance.now();
      clock.playing = !clock.playing;
      state.isPlaying = clock.playing;
      renderControls();
      bumpProgress();
    }
    if (cmd === 'shuffle') {
      state.shuffle = !state.shuffle;
      renderControls();
    }
    if (cmd === 'toggleMute') {
      state.muted = !state.muted;
      renderControls();
    }

    send(cmd);
  });
});

// Right-clicking the heart flips to dislike, which YouTube Music has no other affordance for here.
el.like.addEventListener('contextmenu', (event) => {
  event.preventDefault();
  send('dislike');
});

// -------------------------------------------------------------- seek drag

const ratioFromEvent = (event, node) => {
  const rect = node.getBoundingClientRect();
  return clamp((event.clientX - rect.left) / rect.width, 0, 1);
};

/** Pointer capture throws if the id is no longer active; a failed capture just
 *  means the drag ends when the pointer leaves the element. */
const capturePointer = (node, pointerId) => {
  try {
    node.setPointerCapture(pointerId);
  } catch {
    /* not capturable */
  }
};

el.seek.addEventListener('pointerdown', (event) => {
  const duration = state.song?.songDuration || 0;
  if (!duration) return;

  seeking = true;
  el.seek.classList.add('dragging');
  capturePointer(el.seek, event.pointerId);
  seekPreview = ratioFromEvent(event, el.seek) * duration;
  bumpProgress();
});

el.seek.addEventListener('pointermove', (event) => {
  if (!seeking) return;
  seekPreview = ratioFromEvent(event, el.seek) * (state.song?.songDuration || 0);
  renderProgress();
});

const endSeek = (event) => {
  if (!seeking) return;
  seeking = false;
  el.seek.classList.remove('dragging');
  try {
    el.seek.releasePointerCapture(event.pointerId);
  } catch {
    /* pointer already released */
  }

  clock.position = seekPreview;
  clock.at = performance.now();
  ignorePositionUntil = performance.now() + 1200;
  bumpProgress();
  send('seek', { seconds: seekPreview });
};

el.seek.addEventListener('pointerup', endSeek);
el.seek.addEventListener('pointercancel', endSeek);

// ------------------------------------------------------------ volume drag

let volumeDragging = false;
let volumeSendTimer = null;
let volumePeekTimer = null;

// Every volume we POST comes back as a VOLUME_CHANGED echo. While the user is
// dragging there are several in flight at once, and the last ones to land are
// not necessarily the newest — that is what made the slider jump on release.
// So: throttle what we send, always send the final value, and ignore echoes
// until the server has had time to settle on it.
let ignoreVolumeUntil = 0;
const VOLUME_SETTLE_MS = 900;
const VOLUME_THROTTLE_MS = 70;

const holdingVolume = () => volumeDragging || performance.now() < ignoreVolumeUntil;

/** Briefly reveal the slider so wheel and keyboard changes are visible. */
const peekVolume = () => {
  el.volPop.classList.add('peek');
  clearTimeout(volumePeekTimer);
  volumePeekTimer = setTimeout(() => el.volPop.classList.remove('peek'), 1100);
};

const flushVolume = () => {
  clearTimeout(volumeSendTimer);
  volumeSendTimer = null;
  send('volume', { volume: Math.round(state.volume) });
};

const setVolume = (value, { immediate = false } = {}) => {
  const volume = clamp(value, 0, 100);
  if (state.muted && volume > 0) state.muted = false;
  state.volume = volume;
  ignoreVolumeUntil = performance.now() + VOLUME_SETTLE_MS;
  renderControls();

  if (immediate) {
    flushVolume();
    return;
  }
  if (volumeSendTimer) return;
  volumeSendTimer = setTimeout(() => {
    volumeSendTimer = null;
    send('volume', { volume: Math.round(state.volume) });
  }, VOLUME_THROTTLE_MS);
};

// Events land on the padded hit area, but the ratio is measured against the
// visible rail so the ends of the slider map to 0 and 100.
const volumeFromEvent = (event) => ratioFromEvent(event, el.volRail) * 100;

el.volHit.addEventListener('pointerdown', (event) => {
  volumeDragging = true;
  el.volPop.classList.add('dragging');
  capturePointer(el.volHit, event.pointerId);
  setVolume(volumeFromEvent(event));
});

el.volHit.addEventListener('pointermove', (event) => {
  if (volumeDragging) setVolume(volumeFromEvent(event));
});

const endVolume = (event) => {
  if (!volumeDragging) return;
  volumeDragging = false;
  el.volPop.classList.remove('dragging');
  try {
    el.volHit.releasePointerCapture(event.pointerId);
  } catch {
    /* pointer already released */
  }
  // Whatever the throttle last sent, the value under the cursor wins.
  setVolume(state.volume, { immediate: true });
};

el.volHit.addEventListener('pointerup', endVolume);
el.volHit.addEventListener('pointercancel', endVolume);

// ------------------------------------------------- wheel / trackpad / keys

// A trackpad emits many small deltas where a mouse wheel emits few large ones.
// Carrying a float between events lets both feel proportional instead of the
// trackpad being ignored to rounding.
let wheelVolume = null;
let wheelResetTimer = null;
const WHEEL_SENSITIVITY = 0.12;

el.card.addEventListener(
  'wheel',
  (event) => {
    if (searching) return; // the wheel belongs to the result list
    if (openPanel === 'lyrics' && event.target.closest('#lyrics')) {
      scrollLyrics(event.deltaY);
      return;
    }
    // These scroll on their own; the wheel belongs to them, not to volume.
    if (event.target.closest('#queue, #upnext')) return;
    if (state.status !== 'connected') return;
    // With macOS natural scrolling, swiping up reports a positive deltaY, so
    // adding it is what makes "scroll up" raise the volume.
    const from = wheelVolume ?? (state.muted ? 0 : state.volume);
    wheelVolume = clamp(from + event.deltaY * WHEEL_SENSITIVITY, 0, 100);

    clearTimeout(wheelResetTimer);
    wheelResetTimer = setTimeout(() => {
      wheelVolume = null;
    }, 400);

    setVolume(wheelVolume);
    peekVolume();
  },
  { passive: true },
);

const VOLUME_STEP = 5;
const VOLUME_STEP_FINE = 1;

// While the search panel is open the arrows belong to the result list, and
// Escape closes it. Volume only gets the keys once search is out of the way.
document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape' && !searching && openPanel) {
    event.preventDefault();
    setPanel(null);
    return;
  }
  if (!searching) return;

  if (event.key === 'Escape') {
    event.preventDefault();
    closeSearch();
    return;
  }
  if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
    event.preventDefault();
    highlight(activeResult + (event.key === 'ArrowDown' ? 1 : -1));
    return;
  }
  if (event.key === 'Enter') {
    event.preventDefault();
    clearTimeout(searchTimer);
    const item = results[activeResult];
    if (item) {
      window.widget.playResult(item.videoId);
      closeSearch();
    } else {
      runSearch(el.searchInput.value);
    }
  }
});

// Up/down adjust volume whenever the widget or the dropdown has focus.
document.addEventListener('keydown', (event) => {
  if (searching) return;
  if (event.key !== 'ArrowUp' && event.key !== 'ArrowDown') return;
  if (state.status !== 'connected') return;

  event.preventDefault();
  const step = event.shiftKey ? VOLUME_STEP_FINE : VOLUME_STEP;
  const from = state.muted ? 0 : state.volume;
  wheelVolume = null;
  setVolume(from + (event.key === 'ArrowUp' ? step : -step), { immediate: true });
  peekVolume();
});

// ------------------------------------------------------------------- boot

const showConnecting = () => {
  el.setupTitle.textContent = 'Connecting…';
  el.setupBody.textContent = 'Looking for the YouTube Music API server.';
  el.setupRetry.hidden = true;
  el.setupOpen.hidden = true;
};

el.setupRetry.addEventListener('click', () => {
  showConnecting();
  window.widget.retry();
});

el.setupOpen.addEventListener('click', () => {
  el.setupTitle.textContent = 'Starting YouTube Music…';
  el.setupBody.textContent = 'The widget will connect as soon as the app is up.';
  el.setupRetry.hidden = true;
  el.setupOpen.hidden = true;
  window.widget.openApp();
});

// The page is authored at a fixed base size and scaled by the main process, so
// the CSS corner radius has to shrink as the zoom grows to stay flush with the
// window's own native rounding.
window.widget.onZoom((zoom) => {
  document.documentElement.style.setProperty('--radius', `${(12 / zoom).toFixed(2)}px`);
});

window.widget.onState(applyState);
window.widget.onCover(applyCover);
window.widget.onQueue(applyQueue);
window.widget.onLyrics(applyLyrics);

// One reply carries all four channels: a window loading from cold cannot wait a
// track for the artwork to change.
window.widget.getState().then((boot) => {
  applyCover(boot.cover);
  applyQueue(boot.queue);
  applyLyrics(boot.lyrics);
  applyState(boot.state);

  // The widget reopens the panel it was left showing. The main process has
  // already sized the window for it; this is what tells the page to draw it and
  // the main process that somebody is watching, so the lyrics get fetched.
  if (!IS_PANEL && boot.panel === 'lyrics') toggleLyrics();
  else if (!IS_PANEL && boot.panel === 'queue') toggleQueue();
});

window.addEventListener('resize', () => {
  setupMarquee(el.title);
  setupMarquee(el.subtitle);
});

bumpProgress();

