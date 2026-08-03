'use strict';

const $ = (id) => document.getElementById(id);

// The same document is loaded twice: once as the floating widget, once as the
// menu-bar dropdown. The dropdown is a fixed size and shows time remaining.
const IS_PANEL = new URLSearchParams(location.search).get('mode') === 'panel';
if (IS_PANEL) document.body.classList.add('panel');

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
  elapsed: $('elapsed'),
  duration: $('duration'),
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
  setup: $('setup'),
  setupTitle: $('setup-title'),
  setupBody: $('setup-body'),
  setupRetry: $('setup-retry'),
  setupOpen: $('setup-open'),
};

/** Latest snapshot from the main process. */
let state = {
  status: 'connecting',
  appearance: 'normal',
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

  // In panel mode the right-hand readout counts down, so renderProgress owns it.
  if (!IS_PANEL) el.duration.textContent = formatTime(song?.songDuration || 0);
};

const renderControls = () => {
  el.playIcon.firstElementChild.setAttribute('href', state.isPlaying ? '#i-pause' : '#i-play');
  el.play.title = state.isPlaying ? 'Pause' : 'Play';

  el.shuffle.classList.toggle('on', !!state.shuffle);

  const like = (state.like || '').toUpperCase();
  el.like.classList.toggle('on', like === 'LIKE');
  el.like.title = like === 'LIKE' ? 'Remove like' : 'Like';

  const volume = state.muted ? 0 : state.volume;
  el.volFill.style.width = `${clamp(volume, 0, 100)}%`;
  el.volumeIcon.firstElementChild.setAttribute('href', state.muted ? '#i-muted' : '#i-volume');
  el.mute.classList.toggle('on', state.muted);
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
  el.elapsed.textContent = formatTime(position);

  if (IS_PANEL) {
    el.duration.textContent = duration > 0 ? `−${formatTime(duration - position)}` : '0:00';
  }
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
  const appearanceChanged = next.appearance !== state.appearance;

  state = next;

  // The dropdown has its own fixed size; only the floating widget follows the
  // Normal/Compact setting.
  if (appearanceChanged && !IS_PANEL) {
    document.body.classList.toggle('compact', next.appearance === 'compact');
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
  renderProgress();
};

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

el.seek.addEventListener('pointerdown', (event) => {
  const duration = state.song?.songDuration || 0;
  if (!duration) return;

  seeking = true;
  el.seek.classList.add('dragging');
  el.seek.setPointerCapture(event.pointerId);
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

// Events land on the padded hit area, but the ratio is measured against the
// visible rail so the ends of the slider map to 0 and 100.
const applyVolumeFromEvent = (event) => {
  const volume = Math.round(ratioFromEvent(event, el.volRail) * 100);
  state.volume = volume;
  state.muted = volume === 0;
  renderControls();
  send('volume', { volume });
};

el.volHit.addEventListener('pointerdown', (event) => {
  volumeDragging = true;
  el.volPop.classList.add('dragging');
  el.volHit.setPointerCapture(event.pointerId);
  applyVolumeFromEvent(event);
});

el.volHit.addEventListener('pointermove', (event) => {
  if (volumeDragging) applyVolumeFromEvent(event);
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
};

el.volHit.addEventListener('pointerup', endVolume);
el.volHit.addEventListener('pointercancel', endVolume);

// Scrolling anywhere on the card nudges the volume.
el.card.addEventListener(
  'wheel',
  (event) => {
    if (state.status !== 'connected') return;
    const delta = event.deltaY > 0 ? -3 : 3;
    const volume = clamp(Math.round((state.muted ? 0 : state.volume) + delta), 0, 100);
    if (volume === state.volume && !state.muted) return;
    state.volume = volume;
    state.muted = false;
    renderControls();
    send('volume', { volume });
  },
  { passive: true },
);

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
