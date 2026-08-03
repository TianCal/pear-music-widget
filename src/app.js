'use strict';

const $ = (id) => document.getElementById(id);

// The same document is loaded twice: once as the floating widget, once as the
// menu-bar dropdown. The dropdown is a fixed size and shows time remaining.
const IS_PANEL = new URLSearchParams(location.search).get('mode') === 'panel';
if (IS_PANEL) document.body.classList.add('panel');

/** Each surface picks its own skin: the dropdown's is configured separately. */
const skinOf = (snapshot) => (IS_PANEL ? snapshot.panelSkin : snapshot.skin) || 'classic';

const el = {
  card: $('card'),
  ambient: $('ambient'),
  player: $('player'),
  cover: $('cover'),
  title: $('title'),
  subtitle: $('subtitle'),
  seek: $('seek'),
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

/** Latest snapshot from the main process. */
let state = {
  status: 'connecting',
  skin: 'classic',
  panelSkin: 'classic',
  upnext: [],
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

const applyAccent = async (coverDataUrl) => {
  const { hex, soft } = await window.palette.extract(coverDataUrl);
  document.documentElement.style.setProperty('--accent', hex);
  document.documentElement.style.setProperty('--accent-soft', soft);
};

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

const renderStatus = () => {
  const showSetup = state.status !== 'connected' || !state.song;

  // Nothing to search against unless the API server is answering.
  const offline = state.status !== 'connected';
  el.cornerSearch.hidden = offline;
  el.cornerLyrics.hidden = offline;
  if (offline && searching) closeSearch();
  if (offline && openPanel === 'lyrics') setPanel(null);

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

const renderSong = () => {
  const song = state.song;

  if (state.cover !== lastCoverSrc) {
    lastCoverSrc = state.cover;
    if (state.cover) {
      el.cover.src = state.cover;
      el.ambient.style.backgroundImage = `url("${state.cover}")`;
      el.card.classList.add('has-art');
      applyAccent(state.cover);
    } else {
      el.card.classList.remove('has-art');
      el.ambient.style.backgroundImage = '';
      applyAccent(null);
    }
  }

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

const renderUpNext = () => {
  // Hidden whenever the setup screen is up: a queue from a previous session is
  // stale the moment YouTube Music goes away.
  const playerShowing = state.status === 'connected' && !!state.song;
  el.upnext.hidden = !playerShowing || skinOf(state) !== 'stack';
  if (el.upnext.hidden) return;

  el.upnextGrid.replaceChildren();
  for (const item of state.upnext) {
    const row = document.createElement('button');
    row.className = 'upnext-item';
    row.type = 'button';
    row.dataset.videoId = item.videoId;

    const img = document.createElement('img');
    img.alt = '';
    if (item.thumbnail) img.src = item.thumbnail;

    const text = document.createElement('span');
    text.className = 'upnext-text';

    const name = document.createElement('span');
    name.className = 'upnext-name';
    name.textContent = item.title;

    const artist = document.createElement('span');
    artist.className = 'upnext-artist';
    artist.textContent = item.artist;

    text.append(name, artist);
    row.append(img, text);
    el.upnextGrid.append(row);
  }
};

const renderControls = () => {
  el.playIcon.firstElementChild.setAttribute('href', state.isPlaying ? '#i-pause' : '#i-play');
  el.play.title = state.isPlaying ? 'Pause' : 'Play';

  el.shuffle.classList.toggle('on', !!state.shuffle);

  const like = (state.like || '').toUpperCase();
  el.like.classList.toggle('on', like === 'LIKE');
  el.like.title = like === 'LIKE' ? 'Remove like' : 'Like';

  // Volume 0 reads as muted even when the player's own mute flag is off.
  const silent = state.muted || state.volume === 0;
  el.volFill.style.width = `${clamp(state.muted ? 0 : state.volume, 0, 100)}%`;
  el.volumeIcon.firstElementChild.setAttribute('href', silent ? '#i-muted' : '#i-volume');
  el.mute.classList.toggle('on', silent);
  el.mute.title = `Volume ${Math.round(state.muted ? 0 : state.volume)}%`;
};

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

  el.fill.style.width = `${ratio * 100}%`;
  el.knob.style.left = `${ratio * 100}%`;

  rollLyrics(position);
};

const tick = () => {
  renderProgress();
  requestAnimationFrame(tick);
};

// ------------------------------------------------------------------ state

const applyState = (next) => {
  const positionChanged = next.position !== state.position;
  const playingChanged = next.isPlaying !== state.isPlaying;
  const songChanged = next.song?.videoId !== state.song?.videoId;
  const skin = skinOf(next);

  // Keep the level the user is actually setting while our own echoes drain.
  const heldVolume = holdingVolume() ? { volume: state.volume, muted: state.muted } : null;

  state = next;

  if (heldVolume) {
    state.volume = heldVolume.volume;
    state.muted = heldVolume.muted;
  }

  // Tracked separately from `state` so the very first push applies the class too.
  if (skin !== skinApplied) {
    skinApplied = skin;
    document.body.classList.remove('skin-classic', 'skin-stack');
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

  renderStatus();
  renderSong();
  renderControls();
  renderUpNext();
  renderLyrics();
  renderProgress();
};

el.upnextGrid.addEventListener('click', (event) => {
  const row = event.target.closest('.upnext-item');
  if (row?.dataset.videoId) window.widget.playResult(row.dataset.videoId);
});

// ------------------------------------------------------------------ lyrics

let lyricLines = [];       // [{ time, text }]
let lyricSynced = false;
let lyricActive = -1;
let lyricOffset = 0;
let lyricManualUntil = 0;
let lyricManualTimer = null;

// How long a hand scroll holds the roll still before playback takes it back.
const LYRIC_MANUAL_MS = 5000;

/** Furthest we let a hand scroll go: content height, less half a viewport so
 *  the final lines can still sit centred. */
const lyricMaxOffset = () =>
  Math.max(0, el.lyricsLines.offsetHeight - el.lyricsScroll.clientHeight / 2);

const applyLyricOffset = (offset) => {
  lyricOffset = clamp(offset, 0, lyricMaxOffset());
  el.lyricsLines.style.transform = `translateY(${-lyricOffset}px)`;
};

/** Scrolling over the lyrics moves the lyrics, not the volume. */
const scrollLyrics = (deltaY) => {
  if (!lyricSynced) return; // unsynced is a plain block that scrolls natively

  lyricManualUntil = performance.now() + LYRIC_MANUAL_MS;
  el.lyrics.classList.add('manual');
  applyLyricOffset(lyricOffset + deltaY);

  clearTimeout(lyricManualTimer);
  lyricManualTimer = setTimeout(() => {
    el.lyrics.classList.remove('manual');
    lyricActive = -1; // force the next tick to re-centre on the playing line
  }, LYRIC_MANUAL_MS);
};
let lyricRendered = null;  // identity of what is currently in the DOM

const LYRIC_NOTE = {
  idle: '',
  loading: 'Looking for lyrics…',
  none: 'No lyrics found for this track.',
};

const renderLyrics = () => {
  const data = state.lyrics;
  const identity = `${state.song?.videoId || ''}:${state.lyricsState}:${data?.lines?.length || 0}`;
  if (identity === lyricRendered) return;
  lyricRendered = identity;

  lyricLines = data?.lines || [];
  lyricSynced = !!data?.synced;
  lyricActive = -1;
  lyricOffset = 0;
  lyricManualUntil = 0;
  clearTimeout(lyricManualTimer);
  el.lyrics.classList.remove('manual');

  el.lyrics.classList.toggle('plain', !!data && !lyricSynced);
  el.lyricsLines.replaceChildren();
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

  const index = lyricIndexAt(position);
  if (index === lyricActive) return;
  lyricActive = index;

  const rows = el.lyricsLines.children;
  for (let i = 0; i < rows.length; i += 1) {
    rows[i].classList.toggle('active', i === index);
    rows[i].classList.toggle('past', i < index);
  }

  // Leave the roll where the user parked it until their scroll times out.
  if (performance.now() < lyricManualUntil) return;

  // Centre the active line in the viewport.
  const row = rows[index];
  applyLyricOffset(
    row ? row.offsetTop + row.offsetHeight / 2 - el.lyricsScroll.clientHeight / 2 : 0,
  );
};

const toggleLyrics = () => {
  if (openPanel === 'lyrics') {
    setPanel(null);
    return;
  }
  if (searching) resetSearch(false);
  lyricActive = -1;
  setPanel('lyrics');
};

el.cornerLyrics.addEventListener('click', (event) => {
  event.stopPropagation();
  toggleLyrics();
});

// Clicking a line seeks to it — the timestamps are right there.
el.lyricsLines.addEventListener('click', (event) => {
  const row = event.target.closest('.lyric');
  if (!row || !lyricSynced || !row.dataset.index) return;
  const line = lyricLines[Number(row.dataset.index)];
  if (!line) return;
  clock.position = line.time;
  clock.at = performance.now();
  ignorePositionUntil = performance.now() + 1200;
  send('seek', { seconds: line.time });
});

// ------------------------------------------------------------------ search

let openPanel = null; // 'search' | 'lyrics' | null

const setPanel = (which) => {
  openPanel = which;
  document.body.classList.toggle('panel-open', !!which);
  el.search.hidden = which !== 'search';
  el.lyrics.hidden = which !== 'lyrics';
  el.cornerLyrics.classList.toggle('on', which === 'lyrics');
  el.cornerSearch.classList.toggle('on', which === 'search');
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
    document.body.classList.remove('panel-open');
    el.search.hidden = true;
    el.lyrics.hidden = true;
    el.cornerLyrics.classList.remove('on');
    el.cornerSearch.classList.remove('on');
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
    document.body.classList.remove('panel-open');
    el.lyrics.hidden = true;
    el.cornerLyrics.classList.remove('on');
    el.cornerSearch.classList.remove('on');
  }
});

// Double-clicking brings YouTube Music forward. The artwork is the guaranteed
// target because it opts out of the drag region; the document-level listener
// covers the rest of the card for the case where the events do get through.
// Controls are excluded — a double click on one would fire it twice as well.
const INTERACTIVE = 'button, input, .seek, .vol-hit, .result, .upnext-item';

const openMusicApp = () => window.widget.openApp();

document.addEventListener('dblclick', (event) => {
  if (IS_PANEL || event.target.closest(INTERACTIVE)) return;
  openMusicApp();
});

// Right-click anywhere on the widget for a subset of the menu-bar menu. Inputs
// keep their own menu, and the heart already uses right-click for dislike.
document.addEventListener('contextmenu', (event) => {
  if (IS_PANEL || event.target.closest('input')) return;
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
  renderProgress();
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
window.widget.getState().then(applyState);

window.addEventListener('resize', () => {
  setupMarquee(el.title);
  setupMarquee(el.subtitle);
});

requestAnimationFrame(tick);
