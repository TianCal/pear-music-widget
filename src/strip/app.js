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
/** Queue artwork by video id, as `queue_art` replied. */
const thumbs = new Map();
let posBase = 0;
let posAt = performance.now();

const binds = (name) => document.querySelectorAll(`[data-bind="${name}"]`);
const setText = (name, text) =>
  binds(name).forEach((el) => {
    if (el.textContent !== text) el.textContent = text;
  });

const fmt = (seconds) => {
  const s = Math.max(0, Math.floor(seconds));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
};

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
}

async function applyCover(next) {
  cover = next || '';
  render();
  if (!cover) return;
  const accent = await accentOf(cover).catch(() => null);
  if (cover === next) root.style.setProperty('--accent', accent || '#8e8e93');
}

function render() {
  const song = state?.song;
  root.classList.toggle('playing', !!state?.isPlaying);
  root.classList.toggle('empty', !song);

  const idle = state?.status === 'offline' ? 'Music is not running' : 'Nothing playing';
  setText('title', song?.title || idle);
  setText('artist', song?.artist || '');
  setText('byline', [song?.artist, song?.album].filter(Boolean).join(' · '));

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
  const liked = (state?.like || '').toUpperCase() === 'LIKE';
  document.querySelectorAll('.btn.like').forEach((btn) => btn.classList.toggle('on', liked));

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
let hairlinePx = -1;

/**
 * Progress, written only when it would show: the hairline when it moves a
 * whole pixel, the sheet's bar and clock only while the sheet is open. Each
 * write is a style change the window server has to composite.
 */
function tick() {
  const duration = state?.song?.songDuration || 0;
  const p = position();
  const frac = duration ? Math.min(1, p / duration) : 0;
  const px = Math.round(frac * hairline.parentElement.offsetWidth);
  if (px !== hairlinePx) {
    hairlinePx = px;
    hairline.style.setProperty('--p', frac.toFixed(4));
  }
  if (!open) return;
  sheet.querySelector('.track b').style.setProperty('--p', frac.toFixed(4));
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
    const src = thumbs.get(track.videoId);
    if (src) {
      if (img.getAttribute('src') !== src) img.src = src;
    } else {
      img.removeAttribute('src');
    }
  }
  askForThumbs();
}

/** Artwork is request/reply: ask only for the rows on screen without it. */
function askForThumbs() {
  if (!open) return;
  const ids = neighbours
    .filter((row) => !row.hidden && !thumbs.has(row.dataset.videoId))
    .map((row) => row.dataset.videoId);
  if (!ids.length) return;
  invoke('queue_art', { ids, size: 128 }).then((reply) => {
    let added = false;
    for (const [id, src] of Object.entries(reply?.art || {})) {
      if (src && !thumbs.has(id)) {
        thumbs.set(id, src);
        added = true;
      }
    }
    if (added) renderNeighbours();
  });
}

function applyQueue(next) {
  queue = next || { items: [], current: null };
  // Keep only what the current queue can still show.
  const ids = new Set((queue.items || []).map((track) => track.videoId));
  for (const id of thumbs.keys()) if (!ids.has(id)) thumbs.delete(id);
  renderNeighbours();
}

// The hairline moves a pixel every few seconds; the sheet's clock every second.
setInterval(() => {
  if (state?.isPlaying) tick();
}, 250);

// --------------------------------------------------------------- hot area

const sheet = document.getElementById('sheet');
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
    if (open) askForThumbs();
    reportHot();
  }, want ? 30 : 160);
}

// The page hears no hover of its own (the app is never active); Rust watches
// the pointer and says when it enters or leaves the drawn part.
listen('strip-hover', ({ payload }) => {
  if (payload || !shape.pinned) setOpen(payload);
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
  const btn = event.target.closest('[data-cmd]');
  const name = btn?.dataset.cmd;
  if (!name) return;
  if (name === 'togglePlay' && state) {
    // Optimistic, as the card does; the next push corrects it.
    posBase = position();
    posAt = performance.now();
    state = { ...state, isPlaying: !state.isPlaying };
    render();
  }
  command(name);
});

// A sideways two-finger swipe over the open sheet skips, like swiping a
// notification away. Folded, the window lets everything through.
let swipe = 0;
let swipeAt = 0;
let wheelAt = 0;
root.addEventListener(
  'wheel',
  (event) => {
    if (Math.abs(event.deltaX) <= Math.abs(event.deltaY)) return;
    event.preventDefault();
    const now = performance.now();
    // A pause between wheel events starts a new gesture.
    if (now - wheelAt > 150) swipe = 0;
    wheelAt = now;
    if (now - swipeAt < 700) return;
    swipe += event.deltaX;
    if (Math.abs(swipe) > 80) {
      command(swipe > 0 ? 'next' : 'previous');
      swipe = 0;
      swipeAt = performance.now();
    }
  },
  { passive: false },
);

// ------------------------------------------------------------ cover accent

/** The cover's most present vivid hue, at a lightness readable on the bar. */
async function accentOf(src) {
  const img = new Image();
  img.src = src;
  await img.decode();
  const size = 24;
  const canvas = document.createElement('canvas');
  canvas.width = canvas.height = size;
  const ctx = canvas.getContext('2d', { willReadFrequently: true });
  ctx.drawImage(img, 0, 0, size, size);
  const data = ctx.getImageData(0, 0, size, size).data;

  const buckets = Array.from({ length: 12 }, () => ({ w: 0, h: 0, s: 0 }));
  for (let i = 0; i < data.length; i += 4) {
    const [h, s, l] = hsl(data[i], data[i + 1], data[i + 2]);
    const weight = s * Math.max(0, 1 - Math.abs(l - 0.5) * 1.8);
    if (s < 0.2 || weight <= 0) continue;
    const bucket = buckets[Math.floor(h * 12) % 12];
    bucket.w += weight;
    bucket.h += h * weight;
    bucket.s += s * weight;
  }
  const best = buckets.reduce((a, b) => (b.w > a.w ? b : a));
  if (best.w < 2) return null;
  const dark = matchMedia('(prefers-color-scheme: dark)').matches;
  const hue = Math.round((best.h / best.w) * 360);
  const sat = Math.round(Math.min(0.85, best.s / best.w) * 100);
  return `hsl(${hue} ${sat}% ${dark ? 64 : 48}%)`;
}

function hsl(r, g, b) {
  r /= 255;
  g /= 255;
  b /= 255;
  const max = Math.max(r, g, b);
  const min = Math.min(r, g, b);
  const l = (max + min) / 2;
  if (max === min) return [0, 0, l];
  const d = max - min;
  const s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
  const h = max === r ? (g - b) / d + (g < b ? 6 : 0) : max === g ? (b - r) / d + 2 : (r - g) / d + 4;
  return [h / 6, s, l];
}

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
  listen('queue-art-ready', () => askForThumbs()),
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
