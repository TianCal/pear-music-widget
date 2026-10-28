'use strict';

const $ = (id) => document.getElementById(id);

// The same document is loaded twice: once as the floating widget, once as the
// menu-bar dropdown. The dropdown is a fixed size and shows time remaining.
const IS_PANEL = new URLSearchParams(location.search).get('mode') === 'panel';
if (IS_PANEL) document.body.classList.add('panel');

/** Must match `SKINS` in src-tauri/src/window.rs. */
const SKINS = ['stack', 'vinyl', 'liner'];

/** Skins whose own layout carries the lyrics, so they have no lyrics panel.
 *  Must match `skin_shows_lyrics` in window.rs. (Every skin draws the queue.) */
const LYRIC_SKINS = ['vinyl', 'liner'];

/* The three panels that share the one slot, in corner-bar order. A table rather
   than three enumerations: opening, closing and the dropdown's blur teardown
   each used to list every panel and every corner button by hand, and a third
   panel would have made that three near-identical lists to keep in step.
   `corner` also names the flag in the skin's `corners` set that hides the
   button. */
const PANELS = [
  { key: 'queue', section: 'queue', corner: 'cornerQueue' },
  { key: 'lyrics', section: 'lyrics', corner: 'cornerLyrics' },
  { key: 'search', section: 'search', corner: 'cornerSearch' },
];

/* Every button in the bar, in DOM order. The panels are a subset: shuffle,
   repeat and delete open nothing, so they are here and not in `PANELS`, and this is the
   list that decides what is drawn and how wide a gutter the titles reserve.
   `local` buttons exist only while a file from a folder is playing. */
const CORNERS = [
  { key: 'trash', node: 'cornerTrash', local: true },
  { key: 'shuffle', node: 'cornerShuffle' },
  { key: 'repeat', node: 'cornerRepeat' },
  ...PANELS.map((panel) => ({ key: panel.key, node: panel.corner })),
  // Opens the playlist picker, an overlay rather than a panel.
  { key: 'playlists', node: 'cornerPlaylists' },
];

/** Each surface picks its own skin: the dropdown's is configured separately. */
const skinOf = (snapshot) => (IS_PANEL ? snapshot.panelSkin : snapshot.skin) || 'stack';

const skinShowsLyrics = (snapshot) => LYRIC_SKINS.includes(skinOf(snapshot));

/** The corner buttons for *this* surface's skin. One snapshot carries every
 *  skin's set, because the widget and the dropdown can be on different ones. A
 *  skin with no entry has never been changed, so everything is on. */
const cornersOf = (snapshot) => (snapshot.corners || {})[skinOf(snapshot)] || {};

/** Whether a local folder is the source, rather than YouTube Music. */
const isLocal = (snapshot) => snapshot.source === 'local' || snapshot.song?.mediaType === 'LOCAL';

/** The corner buttons this snapshot actually draws. */
const shownCorners = (snapshot) => {
  const corners = cornersOf(snapshot);
  return CORNERS.filter(
    (button) =>
      corners[button.key] !== false &&
      (!button.local || isLocal(snapshot)) &&
      !(button.key === 'lyrics' && skinShowsLyrics(snapshot)),
  );
};

const el = {
  card: $('card'),
  player: $('player'),
  cover: $('cover'),
  art: $('art'),
  titles: document.querySelector('.titles'),
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
  seekVolFill: $('seek-vol-fill'),
  seekVolLevel: $('seek-vol-level'),
  seekVolIcon: $('seek-vol-icon'),
  cornerSearch: $('btn-search'),
  cornerLyrics: $('btn-lyrics'),
  cornerQueue: $('btn-queue'),
  cornerRepeat: $('btn-repeat'),
  cornerShuffle: $('btn-shuffle-corner'),
  cornerTrash: $('btn-trash'),
  cornerPlaylists: $('btn-playlists'),
  playlists: $('playlists'),
  playlistsScroll: $('playlists-scroll'),
  artistPop: $('artist-pop'),
  artistPopScroll: $('artist-pop-scroll'),
  repeatIcon: $('repeat-icon'),
  cornerBar: document.querySelector('.corner-bar'),
  closeWidget: $('btn-close'),
  queue: $('queue'),
  queueList: $('queue-list'),
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

/* What is open on the card. Read across the scripts (status, input, the
   panels and overlays), so declared here, in the first of them. */
/** Whether the playlist picker is showing. */
let pickerOpen = false;
/** The same, for the artist popup. */
let artistOpen = false;
/** 'queue' | 'lyrics' | 'search' | null — see `setPanel` in panels.js. */
let openPanel = null;
/** Whether the search panel is up; it owns the keyboard while it is. */
let searching = false;

/** Latest snapshot from the main process.
 *
 *  `status` down is pushed on the `state` event roughly once a second; `cover`,
 *  `queue` and `lyrics` arrive on their own events and are merged in here, so
 *  everything below still reads from one object. */
let state = {
  source: 'youtube',
  status: 'connecting',
  statusMessage: '',
  skin: 'stack',
  panelSkin: 'stack',
  liquidGlass: true,
  tint: 1,
  lyricsOffset: 0,
  /** Per skin — see `cornersOf`. Empty means every skin is at its defaults. */
  corners: {},
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
  /** 'ALL' | 'ONE' | 'NONE' | null — null until the player has told us. */
  repeat: null,
  like: null,
};

// Local playhead: the server only pushes a position roughly once a second, so
// we extrapolate between updates to keep the bar moving between pushes.
const clock = { position: 0, at: performance.now(), playing: false };

/** Where the playhead is now, interpolated since the last push. */
const playhead = () => clock.position + (clock.playing ? (performance.now() - clock.at) / 1000 : 0);

let seeking = false;
let seekPreview = 0;
let ignorePositionUntil = 0;

/** How long after a seek we issued the pushes still carrying the old position
 *  are ignored, so the bar does not snap back before the player catches up. */
const SEEK_ECHO_MS = 1200;

/** Move our playhead to `seconds` now and ask the player to follow. */
const seekLocally = (seconds) => {
  clock.position = seconds;
  clock.at = performance.now();
  ignorePositionUntil = clock.at + SEEK_ECHO_MS;
  bumpProgress();
  send('seek', { seconds });
};

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
  renderTurning();
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

/** Measure long text, optionally giving newly displayed text one complete pass. */
const setupMarquee = (span, { intro = false } = {}) => {
  const box = span.parentElement;
  box.classList.remove('scroll', 'intro');
  span.style.removeProperty('--drift-distance');

  const overflow = span.scrollWidth - box.clientWidth;
  if (overflow <= 4) return;

  // Overshoot by the width of the fade so the tail clears the mask entirely.
  const distance = overflow + 18;
  const duration = clamp(distance / 16, 5, 18);
  span.style.setProperty('--drift-distance', `${-distance}px`);
  span.style.setProperty('--drift-duration', `${duration}s`);
  span.style.setProperty('--drift-cycle-duration', `${duration * 2}s`);
  box.classList.add('scroll');

  // Hover owns the animation while the pointer is already here; starting the
  // intro afterwards would make the text move again when the pointer leaves.
  if (intro && !el.titles.matches(':hover')) {
    void box.offsetWidth;
    box.classList.add('intro');
  }
};

/** Recalculate the resting title width without replaying its intro pass.
 *  Coalesced to one frame: each pass writes then measures, so a burst of
 *  calls — a window resize fires many — would force a layout per call. */
let marqueesPending = false;
const refreshMarquees = () => {
  if (marqueesPending) return;
  marqueesPending = true;
  requestAnimationFrame(() => {
    marqueesPending = false;
    setupMarquee(el.title);
    setupMarquee(el.subtitle);
  });
};

// Entering the shared title region switches both lines to hover control. An
// unfinished intro must not resume after the pointer leaves.
el.titles.addEventListener('mouseenter', () => {
  el.title.parentElement.classList.remove('intro');
  el.subtitle.parentElement.classList.remove('intro');
});

for (const span of [el.title, el.subtitle]) {
  span.addEventListener('animationend', (event) => {
    if (event.animationName === 'drift-once') span.parentElement.classList.remove('intro');
  });
}

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

/* Auto-hide. The buttons are chrome over someone else's artwork, so they can be
   asked to fade once the pointer has been still for a while. */
let cornerIdleTimer = null;
let cornerWokeAt = 0;
/** The delay the running timer was armed with, so a state push that did not
 *  change it leaves the countdown alone. */
let cornerFadeArmedFor = null;

/** Give hidden corner controls' gutter back to the titles, and remeasure it. */
const setCornerIdle = (idle) => {
  if (!IS_PANEL) el.closeWidget.classList.toggle('awake', !idle);
  if (el.cornerBar.classList.contains('idle') === idle) return;
  el.cornerBar.classList.toggle('idle', idle);
  refreshMarquees();
};

const armCornerFade = () => {
  clearTimeout(cornerIdleTimer);
  cornerIdleTimer = null;
  const after = state.cornersAutohide || 0;
  cornerFadeArmedFor = after;
  // The countdown runs whether or not a panel is open. A faded button cannot
  // be clicked, but any press or scroll on the card wakes the bar before the
  // click lands, so the panel's own button is never out of reach. (Escape
  // closes a panel too, but only once the widget has focus, which a widget
  // usually does not.)
  if (!after) return;
  cornerIdleTimer = setTimeout(() => {
    if (!pickerOpen) setCornerIdle(true);
  }, after * 1000);
};

/** The pointer moved, so start the countdowns over. */
const wakeChrome = () => {
  const now = performance.now();
  const asleep = el.cornerBar.classList.contains('idle');
  // A mousemove fires far faster than this needs to run, and re-arming a
  // timeout per event is pure churn while the pointer is travelling.
  if (!asleep && now - cornerWokeAt < 200) return;
  cornerWokeAt = now;
  setCornerIdle(false);
  armCornerFade();
};

/* Four signals, because a widget is meant to be used without focusing it first
   and pointer *position* is the one thing a background window cannot count on:
   AppKit delivers `mouseMoved` to the key window alone, and this one is usually
   not it. `mouseover` covers hover, which tracking areas still fire in the
   background; `wheel` and `mousedown` cover the interactions that always
   arrive, and are the guaranteed way back to a faded bar. */
for (const signal of ['mousemove', 'mouseover', 'mousedown', 'wheel']) {
  document.addEventListener(signal, wakeChrome, { passive: true });
}

// The idle bar can remain CSS-visible while the pointer rests over the card.
// Remeasure once it actually leaves and the reclaimed gutter becomes real.
el.card.addEventListener('mouseleave', () => {
  if (el.cornerBar.classList.contains('idle')) refreshMarquees();
});

/* Same dismissal as "Hide widget" in the menu — the tray's "Show floating
   widget" is the way back, and nothing else about the app changes. */
if (!IS_PANEL) {
  el.closeWidget.addEventListener('click', (event) => {
    event.stopPropagation();
    window.widget.hideWidget();
  });
}

const renderStatus = () => {
  const showSetup = state.status !== 'connected' || !state.song;

  // Nothing to search, read along with or queue against unless the API server
  // is answering — and each button can also be turned off from the menu.
  const offline = state.status !== 'connected';
  const corners = cornersOf(state);
  const shown = offline ? [] : shownCorners(state);
  for (const button of CORNERS) {
    el[button.node].hidden = !shown.includes(button);
  }
  // The gutter the titles reserve follows the buttons that are actually there.
  document.body.style.setProperty('--corner-count', String(shown.length));

  // Only when the setting itself moved. Other chrome changes can call this
  // renderer too, and re-arming the countdown for any of them would extend the
  // fade for reasons unrelated to pointer activity.
  if (cornerFadeArmedFor !== (state.cornersAutohide || 0)) {
    setCornerIdle(false);
    armCornerFade();
  }

  // A panel whose button has just gone is a panel with no way back out of it.
  const orphaned = openPanel && corners[openPanel] === false;
  if ((offline || orphaned) && searching) closeSearch();
  else if ((offline || orphaned) && openPanel) setPanel(null);
  if (pickerOpen && (offline || corners.playlists === false)) closePicker();
  if (artistOpen && (offline || isLocal(state))) closeArtist();
  // The double-click promise only holds while YouTube Music is the source.
  el.art.title = isLocal(state) ? '' : 'Double-click to open YouTube Music';

  if (!showSetup) {
    el.setup.hidden = true;
    el.player.hidden = false;
    return;
  }

  el.player.hidden = true;
  el.setup.hidden = false;

  if (state.status === 'connected') {
    el.setupTitle.textContent = 'Nothing playing';
    el.setupBody.textContent = isLocal(state)
      ? 'Pick a folder from Source in the menu to play it here.'
      : 'Start a track in YouTube Music and it will show up here.';
    el.setupRetry.hidden = true;
    el.setupOpen.hidden = true;
    return;
  }

  showStatusCopy(state.status, state.statusMessage);
  el.setupRetry.hidden = state.status === 'connecting';
  // Launching the app is the fix for 'offline'; the other states need a retry.
  el.setupOpen.hidden = state.status !== 'offline';
};

/** The setup screen's words for `status`, plus what actually went wrong when
 *  Rust said (`statusMessage`): the specific error is what tells a wrong port
 *  from a server that is not running. */
const showStatusCopy = (status, detail = '') => {
  const copy = STATUS_COPY[status] || STATUS_COPY.connecting;
  el.setupTitle.textContent = copy.title;
  el.setupBody.innerHTML = copy.body;
  if (detail) {
    const line = document.createElement('span');
    line.className = 'setup-detail';
    line.textContent = detail;
    el.setupBody.append(line);
  }
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

/** What the subtitle's links depend on, so a change to them redraws it. */
const artistsKey = (song) => (song?.artists || []).map(({ id, name }) => `${id}:${name}`).join(',');

/** "Artist — Album". On YouTube Music the whole credit is one link that
 *  drops the Top songs of everyone in it (`song.artists`). */
const subtitleNodes = (song, album) => {
  const nodes = [];
  const credit = song?.artist || '';
  if (credit && song.artists?.length) {
    const link = document.createElement('span');
    link.className = 'artist-link';
    link.setAttribute('role', 'button');
    link.title = 'Top songs on YouTube Music';
    link.textContent = credit;
    nodes.push(link);
  } else if (credit) {
    nodes.push(credit);
  }
  if (album) nodes.push(credit ? ` — ${album}` : album);
  return nodes;
};

const renderSong = ({ restartMarquee = false } = {}) => {
  const song = state.song;
  const title = song?.title || 'Nothing playing';
  const releaseName = (value) =>
    (value || '')
      .trim()
      .replace(/\s*[-–—]\s*(single|ep)\s*$/i, '')
      // YouTube Music can spell the same version label in different Chinese
      // scripts (for example `(粤语版)` on the song and `(粵語版)` on the EP).
      .replace(/\s*(?:\([^)]*\)|（[^）]*）|\[[^\]]*\]|【[^】]*】)\s*$/u, '')
      .trim()
      .toLocaleLowerCase();
  const album = song && releaseName(song.album) !== releaseName(song.title) ? song.album : '';
  // Text plus links: a relabel can change the links and not the words.
  const subtitle = song ? [song.artist, album, artistsKey(song)].join('\n') : '';

  const titleChanged = title !== lastTitle;
  if (titleChanged) {
    lastTitle = title;
    el.title.textContent = title;
  }
  if (titleChanged || restartMarquee) {
    requestAnimationFrame(() => setupMarquee(el.title, { intro: true }));
  }

  const subtitleChanged = subtitle !== lastSubtitle;
  if (subtitleChanged) {
    lastSubtitle = subtitle;
    el.subtitle.replaceChildren(...subtitleNodes(song, album));
    // The popup belongs to the credit it dropped from.
    if (artistOpen) closeArtist();
    if (pointerOnCard) warmArtists();
  }
  if (subtitleChanged || restartMarquee) {
    requestAnimationFrame(() => setupMarquee(el.subtitle, { intro: true }));
  }

};

/* Several player fields share this renderer. `classList.toggle` is already a
   no-op when the class is where it should be; `setAttribute` on a `<use>` is
   not — it re-resolves the reference — and neither is a style write. */
const written = { play: null, like: null, volume: null, repeat: null };

/** Only ALL and ONE are reachable from here — see the `repeat` command. The
 *  glyph tells them apart and nothing else does, so the title is where NONE —
 *  set inside YouTube Music — and a mode we have not been told yet are named. */
const REPEAT_TITLE = {
  ALL: 'Repeat: queue',
  ONE: 'Repeat: this track',
  NONE: 'Repeat: off',
};

/** Volume 0 reads as muted even when the player's own mute flag is off. */
const silent = () => state.muted || state.volume === 0;
const volumeLevel = () => clamp(state.muted ? 0 : state.volume, 0, 100);
const volumeGlyph = () => (silent() ? '#i-muted' : '#i-volume');

const renderControls = () => {
  if (written.play !== state.isPlaying) {
    written.play = state.isPlaying;
    el.playIcon.firstElementChild.setAttribute('href', state.isPlaying ? '#i-pause' : '#i-play');
    el.play.title = state.isPlaying ? 'Pause' : 'Play';
    // The queue's equaliser holds still while paused.
    document.body.classList.toggle('is-playing', !!state.isPlaying);
    renderTurning();
  }

  el.shuffle.classList.toggle('on', !!state.shuffle);
  // `lit`, not `on`: a disc up in the corner bar means "that panel is open".
  el.cornerShuffle.classList.toggle('lit', !!state.shuffle);
  el.cornerShuffle.title = state.shuffle ? 'Shuffle: on' : 'Shuffle: off';

  const repeat = (state.repeat || '').toUpperCase();
  if (written.repeat !== repeat) {
    written.repeat = repeat;
    // The glyph is the difference between the two modes: the same loop, with a
    // 1 through the middle for a single track.
    el.repeatIcon.firstElementChild.setAttribute(
      'href',
      repeat === 'ONE' ? '#i-repeat-one' : '#i-repeat',
    );
    // No `on` class, unlike the panel toggles: the glyph is the state, and a lit
    // disc up here means "that panel is open".
    el.cornerRepeat.title = REPEAT_TITLE[repeat] || 'Repeat';
  }

  const like = (state.like || '').toUpperCase();
  if (written.like !== like) {
    written.like = like;
    el.like.classList.toggle('on', like === 'LIKE');
    el.like.title = like === 'LIKE' ? 'Remove like' : 'Like';
  }

  const level = volumeLevel();
  if (written.volume !== level) {
    written.volume = level;
    el.volFill.style.width = `${level}%`;
    el.volumeIcon.firstElementChild.setAttribute('href', volumeGlyph());
    el.mute.classList.toggle('on', silent());
    el.mute.title = `Volume ${Math.round(level)}%`;
  }
};

/* The rail's pixel width, cached. Measuring it inside the tick would force a
   layout flush every frame, which is most of what this whole path is trying to
   avoid. A skin switch and a window resize both change it; the observer that
   measures it is in boot.js. */
let railWidth = 0;
let seekX = -1; // last `--seek-x` written, in whole pixels

const renderProgress = () => {
  const duration = state.song?.songDuration || 0;
  let position;

  if (seeking) {
    position = seekPreview;
  } else {
    position = playhead();
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

// The playhead is quantised to whole pixels, so display-refresh cadence spends
// almost every callback rediscovering the same pixel. Ten updates a second are
// comfortably faster than the rail can move, while keeping lyric cues within
// 100 ms. Direct state and pointer changes still render immediately below.
// The loop stops as soon as playback pauses or the surface is hidden. The
// dropdown can ask `document.hidden`; the widget is a non-activating window
// whose document reports hidden even while it is on screen (gating on it left
// the playhead and synced lyrics stepping once a second), so Rust tells it.
const PROGRESS_TICK_MS = 100;
let ticking = false;
/** Whether the widget window is on screen, told by Rust (`on-screen`) since
 *  its `document.hidden` cannot say. Unused by the dropdown. */
let widgetOnScreen = true;

/** Vinyl's record turns only while it plays on a surface that is on screen:
 *  a running animation has the window server redraw the window every frame,
 *  so a hidden one must not keep asking. */
const renderTurning = () => {
  const onScreen = IS_PANEL ? !document.hidden : widgetOnScreen;
  document.body.classList.toggle('turning', !!state.isPlaying && onScreen);
};

const shouldTick = () =>
  (IS_PANEL ? !document.hidden : widgetOnScreen) && (clock.playing || seeking);

const tick = () => {
  renderProgress();
  ticking = shouldTick();
  if (ticking) setTimeout(tick, PROGRESS_TICK_MS);
};

/** Render now, and resume the loop if there is anything left to animate. */
const bumpProgress = () => {
  renderProgress();
  if (!ticking && shouldTick()) {
    ticking = true;
    setTimeout(tick, PROGRESS_TICK_MS);
  }
};

// ------------------------------------------------------------------ state

const applyState = (next) => {
  const firstState = skinApplied === null;
  const positionChanged = next.position !== state.position;
  const playingChanged = next.isPlaying !== state.isPlaying;
  const songChanged = next.song?.videoId !== state.song?.videoId;
  const songTextChanged =
    next.song?.title !== state.song?.title ||
    next.song?.artist !== state.song?.artist ||
    next.song?.album !== state.song?.album ||
    artistsKey(next.song) !== artistsKey(state.song);
  const tintChanged = next.tint !== state.tint;
  const liquidGlassChanged = next.liquidGlass !== state.liquidGlass;
  const statusChanged = next.status !== state.status;
  const skin = skinOf(next);
  const skinChanged = skin !== skinOf(state);
  const songPresenceChanged = !!next.song !== !!state.song;
  const previousCorners = cornersOf(state);
  const nextCorners = cornersOf(next);
  const chromeChanged =
    statusChanged ||
    next.statusMessage !== state.statusMessage ||
    songPresenceChanged ||
    skinChanged ||
    next.cornersAutohide !== state.cornersAutohide ||
    isLocal(next) !== isLocal(state) ||
    CORNERS.some(({ key }) => nextCorners[key] !== previousCorners[key]);

  // Keep the level the user is actually setting while our own echoes drain.
  const heldVolume = holdingVolume() ? { volume: state.volume, muted: state.muted } : null;
  const controlsChanged =
    playingChanged ||
    next.shuffle !== state.shuffle ||
    next.repeat !== state.repeat ||
    next.like !== state.like ||
    (!heldVolume && (next.volume !== state.volume || next.muted !== state.muted));

  // Merged rather than replaced: artwork, the queue and the lyrics live on this
  // same object but arrive on their own events, and this push does not carry them.
  Object.assign(state, next);

  if (firstState || liquidGlassChanged) {
    document.body.classList.toggle('liquid-glass', !!state.liquidGlass);
  }

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
    // The lyrics are part of this skin's layout now; the panel would be a
    // second copy of them, with its button already gone. Rust has already
    // collapsed the window (and says so with `panel-collapsed`); this is only
    // the page's half, and it deliberately skips `setPanel(null)`, which would
    // forget the panel the user left open on the skin they came from.
    if (openPanel === 'lyrics' && skinShowsLyrics(state)) {
      openPanel = null;
      paintPanel(null);
    }
    // The strip changed shape and maybe axis, so it is drawn and centred anew.
    upnextRendered = null;
    // Column widths and font sizes both changed, so the drift maths is stale.
    requestAnimationFrame(() => {
      setupMarquee(el.title);
      setupMarquee(el.subtitle);
    });
  }

  // A seek we just issued takes a moment to be reflected; ignore stale positions
  // in that window so the bar does not snap backwards.
  const stale = performance.now() < ignorePositionUntil;
  // Positions arrive in whole seconds, rounded down, so a playhead running up
  // to a second ahead of one is on time, not wrong. Snapping back to each push
  // dragged the clock back by up to a second every second, which Vinyl's
  // sung-line fill showed as a stutter or a fill that ran ahead and retreated.
  // The same holds across a pause and a resume: the push is our own moment,
  // rounded down. Only a push the playhead cannot be reconciled with moves it.
  const behind = playhead() - (next.position || 0);
  const onTime = behind >= 0 && behind < 1.25;
  if (songChanged || ((positionChanged || playingChanged) && !stale && !seeking && !onTime)) {
    clock.position = next.position || 0;
    clock.at = performance.now();
  }
  clock.playing = !!next.isPlaying;

  // The cover has not changed, so renderCover will not re-mix the wash.
  if (tintChanged) applyPalette(state.cover);

  // A position push is by far the common case. Leave every unrelated DOM tree
  // alone: queue data has its own event, and the remaining renderers name the
  // small slice of player state they actually depend on here.
  if (firstState || chromeChanged) renderStatus();
  if (firstState || songChanged || songTextChanged) {
    renderSong({ restartMarquee: songChanged });
  }
  if (firstState || controlsChanged) renderControls();
  if (firstState || statusChanged || songPresenceChanged || skinChanged) {
    renderUpNext();
  }
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

// ---------------------------------------------------------------- commands

const send = (name, payload) => window.widget.command(name, payload);

document.querySelectorAll('[data-cmd]').forEach((button) => {
  button.addEventListener('click', (event) => {
    event.stopPropagation();
    const cmd = button.dataset.cmd;

    // Optimistic feedback: the round trip through YouTube Music is ~100ms.
    if (cmd === 'togglePlay') {
      clock.position = seeking ? seekPreview : playhead();
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

/* Asks first, natively, in Rust: the dialog names the file and the command
   refuses if the track changed before the answer. Nothing optimistic here — a
   file only leaves the queue once it is really in the Trash. */
el.cornerTrash.addEventListener('click', (event) => {
  event.stopPropagation();
  if (isLocal(state)) send('trash');
});

/* Not a panel: it presses the player's repeat button and the bar stays as it
   was. The press means "the other mode", which is the same rule the command
   applies in Rust — and like it, a mode we have not been told yet is left alone
   rather than guessed at, since the round trip is the only thing that knows
   where the player's own cycle will land. */
el.cornerRepeat.addEventListener('click', (event) => {
  event.stopPropagation();
  if (state.repeat) {
    state.repeat = (state.repeat || '').toUpperCase() === 'ALL' ? 'ONE' : 'ALL';
    renderControls();
  }
  send('repeat');
});
