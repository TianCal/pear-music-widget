'use strict';

/**
 * The menu-bar strip. Its own page, like Sort Songs, so it shares no globals
 * with the card's scripts. It listens to the same channels as the card
 * (`state`, `cover`, `queue`) plus `strip-shape` and `strip-hover`. The
 * window is a fixed transparent box; the page tells Rust which part of it is
 * drawn (`strip_hot`).
 */

(() => {
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const root = document.getElementById('strip');
const bar = document.getElementById('bar');
const artist = document.getElementById('artist');

let shape = { bar: 24, notch: 0, pinned: false };
let state = null;
let cover = '';
let queue = { items: [], current: null };
/** Queue artwork by video id, as `queue_art` replied: the card's bounded
 *  cache (shared/artcache.js), since this webview owns its own heap too.
 *  Read with `peek`, so insertion order stays the eviction order. */
const thumbs = window.shared.artCache();

let posBase = 0;
let posAt = performance.now();

/** `[data-bind]` elements by name. The bound markup is static, so each
 *  lookup is made once: `render` runs on every state push. */
const bound = new Map();
const binds = (name) => {
  let list = bound.get(name);
  if (!list) {
    list = [...document.querySelectorAll(`[data-bind="${name}"]`)];
    bound.set(name, list);
  }
  return list;
};
const setText = (name, text) =>
  binds(name).forEach((el) => {
    if (el.textContent !== text) el.textContent = text;
  });

// The card's clock reading (shared/format.js): an hour-long local file reads
// 1:01:40 there, so it must not read 61:40 here.
const fmt = window.shared.formatTime;

function position() {
  if (!state) return 0;
  const duration = state.song?.songDuration || 0;
  const p = posBase + (state.isPlaying ? (performance.now() - posAt) / 1000 : 0);
  return duration ? Math.min(p, duration) : p;
}

// ------------------------------------------------------------------ state

function applyShape(next) {
  shape = next;
  root.classList.toggle('notched', shape.notch > 0);
  lastFit = '';
  document.documentElement.style.setProperty('--bar', `${shape.bar}px`);
  document.documentElement.style.setProperty('--notch', `${shape.notch}px`);
  render();
  if (shape.pinned) setOpen(true);
}

function applyState(next) {
  // Also on the first state: a cover that arrived before it was tinted at
  // the default strength.
  const tintChanged = next.tint !== state?.tint;
  // While a volume change settles, our own value beats the echoes still in
  // flight, which may land out of order (see the widget's `holdingVolume`).
  if (volumeControl.holding()) next = { ...next, volume: state.volume, muted: state.muted };
  // Whole-second pushes trail the real playhead by up to a second. Keep the
  // running estimate when a push is merely behind it on the same track.
  const predicted = position();
  const same = state && state.song?.videoId === next.song?.videoId && state.isPlaying === next.isPlaying;
  const drift = predicted - next.position;
  state = next;
  if (!(same && next.isPlaying && drift >= 0 && drift < 1.2)) {
    posBase = next.position;
    posAt = performance.now();
  }
  render();
  if (tintChanged) applyPalette();
}

async function applyCover(next) {
  cover = next || '';
  render();
  applyPalette();
}

// The washes differ between light and dark glass; follow an appearance switch
// as the card does. Cheap: the cover's sampling is cached in palette.js.
matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => applyPalette());

/** The cover's colours, by the card's own algorithm (`palette.js`) and its
 *  Cover tint setting: the accent, and the washes behind the open sheet,
 *  painted on the strip's own element by the card's painter. */
const paintPalette = window.shared.palettePainter({ extract: window.palette.extract, style: root.style });

function applyPalette() {
  return paintPalette(cover, state?.tint ?? 1);
}

function render() {
  const song = state?.song;
  root.classList.toggle('playing', !!state?.isPlaying);
  root.classList.toggle('empty', !song);

  const idle = state?.status === 'offline' ? 'Music is not running' : 'Nothing playing';
  setText('title', song?.title || idle);
  setText('artist', song?.artist || '');
  renderByline(song);
  // The artist view is about the track it was opened on, and needs a
  // connection. A queue read relabelling the same track's credit (romanised
  // names becoming linked ones) is not a reason to leave it.
  if (view === 'artist' && (state?.status === 'offline' || song?.videoId !== artistsFor)) showView('main');
  // The cover opens YouTube Music; a local file has nothing to open there.
  const local = state?.source === 'local';
  if (cover3.dataset.local !== String(local)) {
    cover3.dataset.local = String(local);
    cover3.title = local ? '' : 'Open YouTube Music';
  }

  binds('cover').forEach((img) => {
    if (cover) {
      if (img.getAttribute('src') !== cover) img.src = cover;
    } else {
      img.removeAttribute('src');
    }
  });
  const glyph = state?.isPlaying ? '#i-pause' : '#i-play';
  binds('ppIcon').forEach((use) => {
    if (use.getAttribute('href') !== glyph) use.setAttribute('href', glyph);
  });
  shuffleBtn.classList.toggle('lit', !!state?.shuffle);
  // Only ALL and ONE light it; NONE and a mode not yet known both read off.
  const repeat = (state?.repeat || '').toUpperCase();
  repeatBtn.classList.toggle('lit', repeat === 'ALL' || repeat === 'ONE');
  const loop = repeat === 'ONE' ? '#i-repeat-one' : '#i-repeat';
  if (repeatGlyph.getAttribute('href') !== loop) repeatGlyph.setAttribute('href', loop);
  const liked = (state?.like || '').toUpperCase() === 'LIKE';
  likeBtns.forEach((btn) => btn.classList.toggle('on', liked));

  // The artist shows only when the whole name fits. Measuring forces layout,
  // so only when the words or the shape changed, not on every state push.
  const fitKey = `${song?.title}\u0000${song?.artist}\u0000${shape.notch}`;
  if (fitKey !== lastFit) {
    lastFit = fitKey;
    bar.classList.remove('solo');
    if (!artist.textContent || artist.scrollWidth > artist.clientWidth + 1) bar.classList.add('solo');
  }

  tick();
}

let lastFit = '';
const hairline = bar.querySelector('.line b');
const likeBtns = [...document.querySelectorAll('.btn.like')];
let hairlinePx = -1;
/** The hairline's width, kept by an observer: reading it in `tick` forced a
 *  layout four times a second. */
let lineWidth = 0;
new ResizeObserver((entries) => {
  lineWidth = entries[entries.length - 1].contentRect.width;
  hairlinePx = -1; // the cached pixel is against the old width
  tick();
}).observe(hairline.parentElement);

/**
 * Progress, written only when it would show: the hairline when it moves a
 * whole pixel, the sheet's bar and clock only while the sheet is open. Each
 * write is a style change the window server has to composite.
 */
function tick() {
  const duration = state?.song?.songDuration || 0;
  const p = position();
  const frac = duration ? Math.min(1, p / duration) : 0;
  const px = Math.round(frac * lineWidth);
  if (px !== hairlinePx) {
    hairlinePx = px;
    hairline.style.setProperty('--p', frac.toFixed(4));
  }
  if (!open) return;
  sheetBar.style.setProperty('--p', frac.toFixed(4));
  setText('elapsed', fmt(p));
  setText('remaining', duration ? `-${fmt(duration - p)}` : '');
}

// ------------------------------------------------------------- neighbours

const neighbours = [...document.querySelectorAll('.neighbour')];

/** The tracks either side of the playing one. */
function renderNeighbours() {
  const items = queue.items || [];
  const at = queue.current;
  for (const row of neighbours) {
    const track = at != null ? items[at + Number(row.dataset.at)] : null;
    row.hidden = !track;
    if (!track) continue;
    row.dataset.index = track.index;
    row.dataset.videoId = track.videoId;
    row.title = track.artist ? `${track.title} — ${track.artist}` : track.title;
    const name = row.querySelector('.name');
    if (name.textContent !== track.title) name.textContent = track.title;
    const img = row.querySelector('.thumb');
    const src = thumbs.peek(track.videoId);
    if (src) {
      if (img.getAttribute('src') !== src) img.src = src;
    } else {
      img.removeAttribute('src');
    }
  }
  askForThumbs();
}

/** Neighbours asked for and not yet answered, so a queue change during the
 *  reply does not ask for the same rows again (see `queueArtPending`). */
const neighbourArtPending = new Set();

/** Artwork is request/reply: ask only for the rows on screen without it. */
function askForThumbs() {
  if (!open) return;
  const ids = neighbours
    .filter((row) => !row.hidden && !thumbs.has(row.dataset.videoId) && !neighbourArtPending.has(row.dataset.videoId))
    .map((row) => row.dataset.videoId);
  if (!ids.length) return;
  ids.forEach((id) => neighbourArtPending.add(id));
  invoke('queue_art', { ids, size: 128 })
    .then((reply) => {
      let added = false;
      for (const [id, src] of Object.entries(reply?.art || {})) {
        if (src && !thumbs.has(id)) {
          thumbs.set(id, src);
          added = true;
        }
      }
      if (added) renderNeighbours();
    })
    .catch(() => {})
    .finally(() => ids.forEach((id) => neighbourArtPending.delete(id)));
}

function applyQueue(next) {
  queue = next || { items: [], current: null };
  // Keep only what the current queue can still show.
  const ids = new Set((queue.items || []).map((track) => track.videoId));
  for (const id of thumbs.keys()) if (!ids.has(id)) thumbs.drop(id);
  renderNeighbours();
  renderQueueList(false);
}

// Over a fullscreen app the bar is slid away (`strip-away`); nothing it
// would draw is on screen, so the hairline waits until it is back.
let away = false;

// The hairline moves a pixel every few seconds; the sheet's clock every second.
setInterval(() => {
  if (state?.isPlaying && !away) tick();
}, 250);

// --------------------------------------------------------------- hot area

const sheet = document.getElementById('sheet');
const sheetBar = sheet.querySelector('.track b');
let lastHot = '';

/**
 * Tell Rust which part of the fixed window is drawn: folded, the bar; open,
 * the sheet and the bar's row. Only that part counts as hovering. Read from
 * layout boxes, not `getBoundingClientRect`, which would include the sheet's
 * transform mid-animation.
 */
function reportHot() {
  const box = (el) => {
    const width = el.offsetWidth;
    return { x: (innerWidth - width) / 2, y: el.offsetTop, width, height: el.offsetHeight };
  };
  const boxes = open ? [box(bar), box(sheet)] : [box(bar)];
  // Open, a little slack, so brushing the edge or the shadow does not fold it.
  const slack = open ? 8 : 0;
  const left = Math.min(...boxes.map((b) => b.x)) - slack;
  const right = Math.max(...boxes.map((b) => b.x + b.width)) + slack;
  const bottom = Math.max(...boxes.map((b) => b.y + b.height)) + slack;
  const hot = { x: Math.floor(left), y: 0, width: Math.ceil(right - left), height: Math.ceil(bottom) };
  const key = JSON.stringify(hot);
  if (key === lastHot) return;
  lastHot = key;
  invoke('strip_hot', { hot });
}

new ResizeObserver(reportHot).observe(bar);
new ResizeObserver(reportHot).observe(sheet);

// ------------------------------------------------------------- hover open

let open = false;
let hoverTimer = 0;

function setOpen(want) {
  clearTimeout(hoverTimer);
  hoverTimer = setTimeout(() => {
    if (open === want) return;
    open = want;
    // The sheet's clock stands still while folded; bring it up to date first.
    if (open) tick();
    root.classList.toggle('open', open);
    // Open takes clicks and wants the queue; folded lets clicks through.
    invoke('strip_open', { open });
    if (open) {
      askForThumbs();
      warmArtists();
    } else {
      // Back to the main view once the sheet has folded out of sight.
      setTimeout(() => {
        if (!open) showView('main');
      }, 300);
    }
    reportHot();
  }, want ? 30 : 160);
}

// The page hears no hover of its own (the app is never active); Rust watches
// the pointer and says when it enters or leaves the drawn part.
listen('strip-hover', ({ payload }) => {
  if (payload || !shape.pinned) setOpen(payload);
});

// ---------------------------------------------------------------- pointer

// WebKit's own hover reaches a never-active app's page late (about half a
// second), so over the open sheet Rust says where the pointer is
// (`strip-pointer`, every 40 ms while it moves) and the page highlights what
// is under it, and brightens the queue button while it is over the tracks
// either side.

const HOVERABLE = '.neighbour, .more, .btn, .row, .back, .play-all, .artist-link, .track';
const around = document.getElementById('around');
let hovering = null;

function pointAt(x, y) {
  const under = x < 0 ? null : document.elementFromPoint(x, y);
  const target = under?.closest(HOVERABLE) ?? null;
  if (target !== hovering) {
    hovering?.classList.remove('hover');
    hovering = target;
    hovering?.classList.add('hover');
  }
  const overAround = !!under?.closest('.around');
  if (around.classList.contains('hovering') !== overAround) around.classList.toggle('hovering', overAround);
}

listen('strip-pointer', ({ payload }) => pointAt(payload.x, payload.y));

// Over a fullscreen app the bar slides away with the menu bar, and back.
listen('strip-away', ({ payload }) => {
  away = !!payload;
  root.classList.toggle('away', away);
  // Catch the hairline up on what it skipped while away.
  if (!away) tick();
});

// --------------------------------------------------------------- controls

const command = (name, payload = null) => invoke('command', { name, payload });

root.addEventListener('click', (event) => {
  const seek = event.target.closest('[data-seek]');
  if (seek) {
    const duration = state?.song?.songDuration || 0;
    const rect = seek.getBoundingClientRect();
    const seconds = duration * Math.min(1, Math.max(0, (event.clientX - rect.left) / rect.width));
    if (duration) {
      posBase = seconds;
      posAt = performance.now();
      tick();
      command('seek', { seconds });
    }
    return;
  }
  const row = event.target.closest('.neighbour');
  if (row) {
    invoke('play_queue_index', { index: Number(row.dataset.index), videoId: row.dataset.videoId });
    return;
  }
  if (event.target.closest('.record.lg .cover')) {
    if (state?.source !== 'local') invoke('open_app');
    return;
  }
  if (event.target.closest('.artist-link')) {
    openArtists();
    return;
  }
  if (event.target.closest('.more')) {
    openQueue();
    return;
  }
  if (event.target.closest('[data-view="playlists"]')) {
    openPlaylists();
    return;
  }
  if (event.target.closest('.back')) {
    showView('main');
    return;
  }
  const pick = event.target.closest('.list-scroll .row, .list-scroll .play-all');
  if (pick) {
    playFromList(pick);
    return;
  }
  const btn = event.target.closest('[data-cmd]');
  const name = btn?.dataset.cmd;
  if (!name || !state) return;
  // Optimistic, as the card is; the next push corrects it.
  if (name === 'togglePlay') {
    posBase = position();
    posAt = performance.now();
    state = { ...state, isPlaying: !state.isPlaying };
  }
  if (name === 'shuffle') state = { ...state, shuffle: !state.shuffle };
  // A press means "the other mode"; from a mode not yet known, the read-back
  // is the first honest answer (see the `repeat` command).
  if (name === 'repeat' && state.repeat) {
    state = { ...state, repeat: state.repeat.toUpperCase() === 'ALL' ? 'ONE' : 'ALL' };
  }
  render();
  command(name);
});

// Right-click: the heart dislikes, as on the card; anywhere else on the open
// sheet brings up the strip's menu.
root.addEventListener('contextmenu', (event) => {
  event.preventDefault();
  if (!open) return;
  if (event.target.closest('.btn.like')) command('dislike');
  else invoke('strip_menu');
});

// ------------------------------------------------------------------ views

// The open sheet is the main view, or a list in its place: playlists, or the
// artists on the track. The list keeps one height, so the window's fixed box
// always holds it.

const shuffleBtn = document.getElementById('shuffle');
const repeatBtn = document.getElementById('repeat');
const repeatGlyph = document.getElementById('repeat-glyph');
const byline = document.getElementById('byline');
const cover3 = sheet.querySelector('.record.lg .cover');
const mainView = sheet.querySelector('.view.main');
const listView = sheet.querySelector('.view.list');
const listTitle = document.getElementById('list-title');
const listScroll = document.getElementById('list-scroll');
let view = 'main';
/** Bumped on every view change, so a reply for a view that was left is
 *  dropped rather than drawn. */
let viewSeq = 0;

function showView(name, title = '') {
  viewSeq += 1;
  clearInterval(listArtTimer);
  if (name === 'playlists') listArtTimer = setInterval(playlists.loadArt, LIST_ART_CHECK_MS);
  if (name === 'queue') listArtTimer = setInterval(loadQueueArt, LIST_ART_CHECK_MS);
  if (view === name && name === 'main') return;
  view = name;
  mainView.hidden = name !== 'main';
  listView.hidden = name === 'main';
  listTitle.textContent = title;
  if (name === 'main') listScroll.replaceChildren();
}

let bylineKey = '';

/** "Artist · Album". On YouTube Music the whole credit is one link to the
 *  artist view; local credits have no page, so theirs is plain text. */
function renderByline(song) {
  const linked = (song?.artists || []).length > 0;
  const key = JSON.stringify([song?.artist, song?.album, (song?.artists || []).map((a) => a.id || a.query)]);
  if (key === bylineKey) return;
  bylineKey = key;
  const parts = [];
  if (song?.artist) {
    const credit = document.createElement('span');
    credit.textContent = song.artist;
    if (linked) {
      credit.className = 'artist-link';
      credit.title = 'Top songs';
    }
    parts.push(credit);
  }
  if (song?.album) parts.push(document.createTextNode(`${song.artist ? ' · ' : ''}${song.album}`));
  byline.replaceChildren(...parts);
  // New links while the pointer is on the sheet: warm them, as the card does.
  if (open) warmArtists();
}

const note = (text) => {
  const node = document.createElement('div');
  node.className = 'note';
  node.textContent = text;
  return node;
};

const group = (text) => {
  const node = document.createElement('div');
  node.className = 'group';
  node.textContent = text;
  return node;
};

/** The playing row: a band, and a badge over its cover. */
function markCurrent(row) {
  row.classList.add('current');
  const badge = document.createElement('span');
  badge.className = 'now';
  badge.innerHTML = '<i></i><i></i><i></i>';
  row.append(badge);
}

function unmarkCurrent(row) {
  row.classList.remove('current');
  row.querySelector('.now')?.remove();
}

/** A list row: a cover (or a folder tile), the title over a subtitle. */
function listRow({ kind, id, title, sub, art, current, disabled }) {
  const row = document.createElement('button');
  row.type = 'button';
  row.className = 'row';
  row.dataset.kind = kind;
  row.dataset.id = id;
  let lead;
  if (kind === 'folder') {
    lead = document.createElement('span');
    lead.className = 'tile';
    lead.innerHTML = '<svg><use href="#i-folder"/></svg>';
  } else {
    lead = document.createElement('img');
    lead.className = 'tile';
    lead.alt = '';
    if (art) lead.src = art;
  }
  const words = document.createElement('span');
  words.className = 'words';
  const t = document.createElement('span');
  t.className = 't';
  t.textContent = title;
  const s = document.createElement('span');
  s.className = 's';
  s.textContent = sub || '';
  words.append(t, s);
  row.append(lead, words);
  row.disabled = !!disabled;
  if (current) markCurrent(row);
  return row;
}

// ---- playlists: YouTube Music playlists and saved folders, as on the card.

/** The card's list (shared/lists.js), drawn as the sheet's rows and groups.
 *  Covers come from Rust, for the rows in view; an empty reply rests a
 *  minute here, where the card asks again at its next look. */
const PLAYLIST_ART_RETRY_MS = 60 * 1000;
const playlists = window.shared.playlistList({
  scroll: listScroll,
  row: '.row',
  ui: { head: group, note, row: listRow },
  fetchArt: (urls) => invoke('playlist_art', { urls }),
  retryMs: PLAYLIST_ART_RETRY_MS,
});
// Scroll events belong to the rendering steps a never-active window's page
// may skip, so while a list is up the rows in view are also rechecked on a
// timer, as the card's picker does. It asks for nothing already in flight.
const LIST_ART_CHECK_MS = 300;
let listArtTimer = 0;

async function openPlaylists() {
  showView('playlists', 'Playlists');
  const seq = viewSeq;
  listScroll.replaceChildren(note('Loading playlists…'));
  const data = await invoke('list_playlists').catch(() => null);
  if (seq !== viewSeq) return;
  playlists.render(data);
}

listScroll.addEventListener('scroll', () => {
  if (view === 'playlists') playlists.loadArt();
  if (view === 'queue') loadQueueArt();
}, { passive: true });

// ---- queue: every track, opened by the button beside the tracks either side.

let queueListKey = '';
/** Rows whose art missed a reply rest until `queue-art-ready` or a while. */
const queueArtResting = new Map();
const queueArtPending = new Set();
const QUEUE_ART_REST_MS = 5000;
const QUEUE_ART_BATCH = 48;

function openQueue() {
  showView('queue', 'Queue');
  queueListKey = '';
  renderQueueList(true);
}

/** Rebuilt only when the rows themselves change: titles and artists (a
 *  rename, a relabelled slot) and `rating` (a local like or dislike) are in
 *  the key. A track change only moves the current mark (`markQueueCurrent`),
 *  so a 500-row queue is not rebuilt on every song. */
function renderQueueList(first) {
  if (view !== 'queue') return;
  const items = queue.items || [];
  const key = items.map((t) => `${t.index}:${t.videoId}:${t.rating || ''}:${t.title}:${t.artist}`).join('|');
  if (key === queueListKey && !first) {
    markQueueCurrent(false);
    return;
  }
  const centre = first || !queueListKey;
  queueListKey = key;
  if (!items.length) {
    const offline = state?.status === 'offline';
    listScroll.replaceChildren(note(offline ? 'Music is not running.' : 'The queue is empty.'));
    queueListKey = '';
    return;
  }
  const keep = listScroll.scrollTop;
  const rows = items.map((track) => {
    const row = listRow({
      kind: 'queue',
      id: track.videoId,
      title: track.title,
      sub: track.artist,
      art: thumbs.peek(track.videoId),
    });
    row.dataset.index = track.index;
    if (track.rating === 'DISLIKE') row.classList.add('disliked');
    if (!thumbs.has(track.videoId)) row.querySelector('img').dataset.video = track.videoId;
    return row;
  });
  listScroll.replaceChildren(...rows);
  hovering = null;
  if (!centre) listScroll.scrollTop = keep;
  markQueueCurrent(centre);
  loadQueueArt();
}

/** Move the band and badge to the playing row. A local folder longer than
 *  the queue's 500 rows can be playing past the last one: then no row is
 *  marked, and the list opens at its end, nearest to it. */
function markQueueCurrent(centre) {
  const rows = listScroll.children;
  const at = queue.current;
  const target = at != null && at < rows.length ? rows[at] : null;
  const marked = listScroll.querySelector('.row.current');
  if (marked && marked !== target) unmarkCurrent(marked);
  if (target && !target.classList.contains('current')) markCurrent(target);
  if (!centre) return;
  if (target) target.scrollIntoView({ block: 'center' });
  else if (at != null) listScroll.scrollTop = listScroll.scrollHeight;
}

/** Queue artwork is request/reply, for the rows in view only. */
function loadQueueArt() {
  if (view !== 'queue') return;
  const top = listScroll.scrollTop - 60;
  const bottom = listScroll.scrollTop + listScroll.clientHeight + 60;
  const now = performance.now();
  const ids = [];
  for (const img of listScroll.querySelectorAll('img[data-video]')) {
    const row = img.closest('.row');
    // Rows are in order: nothing after the first one below the view is in it.
    if (row.offsetTop > bottom) break;
    if (row.offsetTop + row.offsetHeight < top) continue;
    const id = img.dataset.video;
    if (queueArtPending.has(id) || (queueArtResting.get(id) ?? 0) > now) continue;
    ids.push(id);
    if (ids.length >= QUEUE_ART_BATCH) break;
  }
  if (!ids.length) return;
  ids.forEach((id) => queueArtPending.add(id));
  invoke('queue_art', { ids, size: 128 })
    .then((reply) => {
      for (const id of ids) {
        const src = reply?.art?.[id];
        if (src) thumbs.set(id, src);
        else queueArtResting.set(id, performance.now() + QUEUE_ART_REST_MS);
      }
      for (const img of listScroll.querySelectorAll('img[data-video]')) {
        const src = thumbs.peek(img.dataset.video);
        if (src) {
          img.src = src;
          delete img.dataset.video;
        }
      }
    })
    .catch(() => {})
    .finally(() => ids.forEach((id) => queueArtPending.delete(id)));
}

// ---- artists: the Top songs of each artist on the track, as on the card.

/** The pages — kept replies, one section per artist filled in as each lands
 *  — are the card's (shared/lists.js); this side names the sheet's markup. */
const artistPages = window.shared.artistPages({
  fetch: (id, query) => invoke('artist_page', { id, query }),
  ui: {
    section: { head: 'section-head', name: 'section-name', playAll: 'play-all' },
    note,
    row: (song) =>
      listRow({
        kind: 'song',
        id: song.videoId,
        title: song.title,
        sub: song.subtitle,
        art: song.thumbnail,
        current: song.videoId === state?.song?.videoId,
      }),
  },
});

const songArtists = () => (state?.song?.artists || []).slice(0, artistPages.limit);

/** The sheet opening is the pointer resting on the strip: the cue the card
 *  takes to warm its artists, so a click draws them in the same frame. */
function warmArtists() {
  if (state?.status === 'connected') songArtists().forEach(artistPages.page);
}

/** The track the artist view was opened for. */
let artistsFor = null;

function openArtists() {
  const artists = songArtists();
  if (!artists.length) return;
  showView('artist', artists.map((artist) => artist.name).join(', '));
  artistsFor = state?.song?.videoId ?? null;
  const seq = viewSeq;
  artistPages.show(artists, {
    live: () => seq === viewSeq,
    draw: (sections) => listScroll.replaceChildren(...sections),
  });
}

/** One play at a time: a second click while one is starting would race it. */
let listPlaying = false;

async function playFromList(target) {
  if (target.disabled || listPlaying) return;
  const seq = viewSeq;
  listPlaying = true;
  target.classList.add('busy');
  const all = target.classList.contains('play-all');
  const kind = target.dataset.kind;
  let reply;
  try {
    if (all) reply = await invoke('play_playlist', { kind: 'youtube', id: target.dataset.id });
    else if (kind === 'song') reply = await invoke('play_result', { videoId: target.dataset.id });
    else if (kind === 'queue') {
      reply = await invoke('play_queue_index', { index: Number(target.dataset.index), videoId: target.dataset.id });
    }
    else reply = await invoke('play_playlist', { kind, id: target.dataset.id });
  } catch (err) {
    reply = { ok: false, error: String(err) };
  } finally {
    listPlaying = false;
  }
  if (seq !== viewSeq) return;
  target.classList.remove('busy');
  if (reply?.ok) {
    // The queue stays up to pick from; its current row follows the queue.
    if (kind !== 'queue') showView('main');
    else {
      // Move the mark now, as the card does; the next queue read confirms it.
      const marked = listScroll.querySelector('.row.current');
      if (marked !== target) {
        if (marked) unmarkCurrent(marked);
        markCurrent(target);
      }
    }
    return;
  }
  const sub = all ? null : target.querySelector('.s');
  if (sub) sub.textContent = reply?.error || 'Could not play that.';
}

// ----------------------------------------------------------------- wheel

// Over the open sheet (folded, the window lets everything through), the wheel
// does what it does on the widget: up and down change the volume, by the
// widget's rules. A sideways two-finger swipe skips, like swiping a
// notification away.

let peekTimer = 0;

/** The throttle (one send per 70 ms, the final value always sent), the echo
 *  hold and the wheel's carry are the card's (shared/volume.js); this side
 *  only writes the level into its state. Raising it unmutes, as there. */
const volumeControl = window.shared.volumeControl({
  apply: (volume) => {
    state = { ...state, volume, muted: state.muted && volume <= 0 };
  },
  volume: () => state.volume,
  send: (volume) => command('volume', { volume }),
});

/** Turn the progress row into the volume for a moment. */
const levelFill = document.getElementById('level-fill');
const levelText = document.getElementById('level');
const speaker = document.getElementById('speaker');

function peekVolume() {
  const level = state.muted ? 0 : state.volume;
  // On the fill alone: a property on the sheet would restyle everything in it,
  // and a trackpad sends this sixty times a second.
  levelFill.style.setProperty('--vol', (level / 100).toFixed(3));
  const text = String(Math.round(level));
  if (levelText.textContent !== text) levelText.textContent = text;
  const glyph = level > 0 ? '#i-volume' : '#i-muted';
  if (speaker.getAttribute('href') !== glyph) speaker.setAttribute('href', glyph);
  if (!sheet.classList.contains('showing-volume')) sheet.classList.add('showing-volume');
  clearTimeout(peekTimer);
  peekTimer = setTimeout(() => sheet.classList.remove('showing-volume'), 1100);
}

let swipe = 0;
let swipeAt = 0;
let wheelAt = 0;
root.addEventListener(
  'wheel',
  (event) => {
    if (!open || !state) return;
    // A list view scrolls on its own; neither volume nor skipping belongs there.
    if (view !== 'main') return;
    event.preventDefault();
    const now = performance.now();
    if (Math.abs(event.deltaX) > Math.abs(event.deltaY)) {
      // A pause between wheel events starts a new gesture.
      if (now - wheelAt > 150) swipe = 0;
      wheelAt = now;
      if (now - swipeAt < 700) return;
      swipe += event.deltaX;
      if (Math.abs(swipe) > 80) {
        command(swipe > 0 ? 'next' : 'previous');
        swipe = 0;
        swipeAt = now;
      }
      return;
    }
    if (state.status === 'offline') return;
    // The step, its carry between a trackpad's small deltas and the throttle
    // are the shared control's, as on the widget; a fresh gesture starts
    // from the level as heard.
    volumeControl.wheel(event.deltaY, state.muted ? 0 : state.volume);
    peekVolume();
  },
  { passive: false },
);

// ------------------------------------------------------------------- boot

// Listen first, and only then ask for the cold load: an event that landed
// between the reply and a listener's registration would be lost. Any live
// event heard before the reply is newer than it.
const heard = { state: false, cover: false, queue: false };
Promise.all([
  listen('state', ({ payload }) => {
    heard.state = true;
    applyState(payload);
  }),
  listen('cover', ({ payload }) => {
    heard.cover = true;
    applyCover(payload);
  }),
  listen('queue', ({ payload }) => {
    heard.queue = true;
    applyQueue(payload);
  }),
  // Lookups that missed a `queue_art` reply have landed; ask again.
  listen('queue-art-ready', () => {
    queueArtResting.clear();
    neighbourArtPending.clear();
    askForThumbs();
    loadQueueArt();
  }),
  listen('strip-shape', ({ payload }) => applyShape(payload)),
])
  .then(() => Promise.all([invoke('strip_shape'), invoke('widget_state')]))
  .then(([next, boot]) => {
    if (!heard.state) applyState(boot.state);
    if (!heard.cover) applyCover(boot.cover);
    if (!heard.queue) applyQueue(boot.queue);
    applyShape(next);
  });
})();
