'use strict';

const { app, ipcMain } = require('electron');

const api = require('./api');
const store = require('./store');
const { RealtimeClient } = require('./ws');
const { createWindow, applyAppearance } = require('./window');
const { createPanel } = require('./panel');
const { createTray, openMusicApp } = require('./tray');

if (!app.requestSingleInstanceLock()) {
  app.quit();
  return;
}

app.commandLine.appendSwitch('disable-features', 'HardwareMediaKeyHandling');

let win = null;
let panel = null;
let tray = null;
let setAppearance = () => {};
const realtime = new RealtimeClient();

// ------------------------------------------------------------- player state

// Repeat is deliberately absent: POST /switch-repeat is accepted but
// GET /repeat-mode always answers null on the shipped api-server plugin, so the
// widget can never show a truthful repeat state.
const state = {
  status: 'connecting',
  statusMessage: '',
  appearance: store.get('appearance'),
  song: null,
  cover: null,
  isPlaying: false,
  position: 0,
  volume: 100,
  muted: false,
  shuffle: false,
  like: null,
};

// The floating widget and the menu-bar panel are two views of the same state.
const send = () => {
  for (const view of [win, panel]) {
    if (view && !view.isDestroyed()) view.webContents.send('state', state);
  }
};

/** Merge a patch and push to the renderer only when something actually changed. */
const update = (patch) => {
  let dirty = false;
  for (const [key, value] of Object.entries(patch)) {
    if (value === undefined) continue;
    if (state[key] !== value) {
      state[key] = value;
      dirty = true;
    }
  }
  if (dirty) send();
};

const normaliseSong = (song) => {
  if (!song || !song.videoId) return null;
  return {
    videoId: song.videoId,
    title: song.title || 'Unknown title',
    artist: song.artist || '',
    album: song.album || '',
    songDuration: song.songDuration || 0,
    imageSrc: song.imageSrc || null,
    url: song.url || null,
    mediaType: song.mediaType || null,
  };
};

let coverToken = 0;

// YouTube Music applies its own curve between the volume we POST and the volume
// it reports back — measured on 3.12.0 as reported = 100*(15^(sent/100)-1)/14,
// so POSTing 80 echoes back as 55. Rather than hardcode that curve (it comes
// from a plugin the user can turn off), treat the value we set as the truth for
// a moment afterwards. Without this the slider visibly snaps to the curved
// number about a second after the user lets go of the drag.
const VOLUME_ECHO_MS = 1500;
let volumeEchoUntil = 0;
let volumeWeSet = null;

const ownVolumeEcho = () => Date.now() < volumeEchoUntil && volumeWeSet !== null;

// The reported value follows 100*(b^(x/100)-1)/(b-1) — measured at b≈15 on
// 3.12.0, but the curve comes from a plugin the user can turn off, so learn b
// from the first (sent, echoed) pair instead of hardcoding it. Every reported
// value is then mapped back onto the scale the slider actually uses, which is
// what keeps a late echo, or a volume change made inside YouTube Music, from
// yanking the slider onto a different scale.
let volumeCurveBase = null; // null = not calibrated yet, 1 = no curve applied
let volumeSendSeq = 0;
let volumeAwaitingEcho = null;

/** Numerically solve b for a single observed (sent → echoed) pair. */
const solveVolumeCurve = (sent, echoed) => {
  const u = sent / 100;
  const v = echoed / 100;
  if (u <= 0.05 || u >= 0.95) return null; // endpoints are fixed for every b
  if (Math.abs(sent - echoed) <= 2) return 1; // identity
  if (v <= 0 || v >= u) return null; // not the shape we expect

  // (b^u - 1)/(b - 1) decreases monotonically in b, so bisect in log space.
  let lo = 1.0001;
  let hi = 1e6;
  for (let i = 0; i < 60; i += 1) {
    const mid = Math.sqrt(lo * hi);
    if ((Math.pow(mid, u) - 1) / (mid - 1) > v) lo = mid;
    else hi = mid;
  }
  return Math.sqrt(lo * hi);
};

const reportedToSlider = (reported) => {
  if (typeof reported !== 'number') return undefined;
  const b = volumeCurveBase;
  if (!b || b <= 1.001) return reported;
  const slider = (100 * Math.log(1 + ((b - 1) * reported) / 100)) / Math.log(b);
  return Math.round(Math.min(100, Math.max(0, slider)));
};

/** Swap in a new song, then resolve its artwork and like state out of band. */
const applySong = async (raw) => {
  const song = normaliseSong(raw);
  const changed = song?.videoId !== state.song?.videoId;
  if (!changed) {
    // Same track: metadata may still have been enriched (album, duration).
    if (song) update({ song });
    return;
  }

  update({ song, cover: null, like: null });
  if (!song) return;

  const token = ++coverToken;
  const [cover, like] = await Promise.all([
    api.fetchCover(song.imageSrc),
    api.queries.likeState().catch(() => null),
  ]);
  if (token !== coverToken) return; // a newer song won the race
  update({ cover, like: like?.state ?? null });
};

/** Ground truth pull — the websocket's initial values for shuffle/volume are
 *  optimistic defaults until the player emits its first change event. */
const refreshAll = async () => {
  const [song, shuffle, volume] = await Promise.all([
    api.queries.song().catch(() => null),
    api.queries.shuffleState().catch(() => null),
    api.queries.volumeState().catch(() => null),
  ]);

  if (song) await applySong(song);
  update({
    shuffle: shuffle?.state ?? undefined,
    volume: ownVolumeEcho() ? volumeWeSet : reportedToSlider(volume?.state),
    muted: typeof volume?.isMuted === 'boolean' ? volume.isMuted : undefined,
    isPlaying: song ? !song.isPaused : undefined,
    position: typeof song?.elapsedSeconds === 'number' ? song.elapsedSeconds : undefined,
  });
};

// ----------------------------------------------------------------- realtime

realtime.on('status', ({ state: status, message }) => {
  update({ status, statusMessage: message || '' });
  if (status === 'connected') {
    refreshAll().catch(() => {});
  } else if (status !== 'connecting') {
    update({ isPlaying: false });
  }
  tray?.refresh();
});

realtime.on('message', (msg) => {
  switch (msg.type) {
    case 'PLAYER_INFO':
      applySong(msg.song).catch(() => {});
      update({
        isPlaying: !!msg.isPlaying,
        position: msg.position ?? 0,
        volume: typeof msg.volume === 'number' ? msg.volume : undefined,
        muted: !!msg.muted,
        shuffle: !!msg.shuffle,
      });
      break;
    case 'VIDEO_CHANGED':
      applySong(msg.song).catch(() => {});
      update({ position: msg.position ?? 0 });
      break;
    case 'PLAYER_STATE_CHANGED':
      update({ isPlaying: !!msg.isPlaying, position: msg.position ?? state.position });
      break;
    case 'POSITION_CHANGED':
      update({ position: msg.position ?? 0 });
      break;
    case 'VOLUME_CHANGED': {
      const reported = typeof msg.volume === 'number' ? msg.volume : null;

      // Only calibrate against the most recent send: mid-drag there are several
      // echoes in flight and none of them pairs with the value we last set.
      if (reported !== null && volumeAwaitingEcho?.seq === volumeSendSeq) {
        const base = solveVolumeCurve(volumeAwaitingEcho.value, reported);
        if (base) volumeCurveBase = base;
        volumeAwaitingEcho = null;
      }

      update({
        volume: ownVolumeEcho() ? volumeWeSet : reportedToSlider(reported) ?? state.volume,
        muted: !!msg.muted,
      });
      break;
    }
    case 'SHUFFLE_CHANGED':
      update({ shuffle: !!msg.shuffle });
      break;
    default:
      break;
  }
});

// Like state has no push channel, so poll it while we are connected.
setInterval(() => {
  if (state.status !== 'connected' || !state.song) return;
  api.queries
    .likeState()
    .then((res) => update({ like: res?.state ?? null }))
    .catch(() => {});
}, 20000);

// -------------------------------------------------------------------- input

const commands = {
  togglePlay: () => api.actions.togglePlay(),
  next: () => api.actions.next(),
  previous: () => api.actions.previous(),
  seek: ({ seconds }) => api.actions.seekTo(Math.max(0, Math.round(seconds))),
  volume: ({ volume }) => {
    const value = Math.min(100, Math.max(0, Math.round(volume)));
    volumeWeSet = value;
    volumeEchoUntil = Date.now() + VOLUME_ECHO_MS;
    volumeSendSeq += 1;
    volumeAwaitingEcho = { value, seq: volumeSendSeq };
    update({ volume: value });
    return api.actions.setVolume(value);
  },
  toggleMute: () => api.actions.toggleMute(),
  shuffle: () => api.actions.shuffle(),
  like: async () => {
    await api.actions.like();
    update({ like: (await api.queries.likeState().catch(() => null))?.state ?? null });
  },
  dislike: async () => {
    await api.actions.dislike();
    update({ like: (await api.queries.likeState().catch(() => null))?.state ?? null });
  },
};

// Pressing play when YouTube Music is not up should start it, not fail silently.
const PLAYBACK_COMMANDS = new Set(['togglePlay', 'next', 'previous']);

const launchMusicApp = () => {
  openMusicApp();
  // Reset the backoff so the widget picks the app up as soon as it is listening.
  realtime.retry();
};

ipcMain.handle('widget:state', () => state);

ipcMain.handle('widget:open-app', () => {
  launchMusicApp();
  return { ok: true };
});

ipcMain.handle('widget:command', async (_event, name, payload) => {
  if (PLAYBACK_COMMANDS.has(name) && state.status === 'offline') {
    launchMusicApp();
    return { ok: true, launched: true };
  }

  const fn = commands[name];
  if (!fn) return { ok: false, error: `Unknown command ${name}` };
  try {
    await fn(payload || {});
    return { ok: true };
  } catch (err) {
    if (err.code === 'OFFLINE' || err.code === 'UNAUTHORIZED') {
      update({ status: err.code === 'OFFLINE' ? 'offline' : 'unauthorized', statusMessage: err.message });
      realtime.retry();
    }
    return { ok: false, error: err.message };
  }
});

ipcMain.handle('widget:appearance', (_event, appearance) => {
  if (appearance !== 'normal' && appearance !== 'compact') return { ok: false };
  setAppearance(appearance);
  return { ok: true };
});

ipcMain.handle('widget:retry', () => {
  realtime.retry();
  return { ok: true };
});

ipcMain.handle('widget:quit', () => app.quit());

// ---------------------------------------------------------------- lifecycle

app.whenReady().then(() => {
  app.dock?.hide();
  // Dragging the widget across the breakpoint switches layout; the window is
  // already the right size by then, so only the renderer needs telling.
  win = createWindow({ onAppearance: (appearance) => update({ appearance }) });

  const panelView = createPanel();
  panel = panelView.panel;

  setAppearance = (appearance) => {
    applyAppearance(win, appearance);
    update({ appearance });
    tray?.refresh();
  };

  tray = createTray({
    window: win,
    realtime,
    getState: () => state,
    setAppearance: (appearance) => setAppearance(appearance),
    togglePanel: (trayInstance) => panelView.toggle(trayInstance),
  });
  realtime.start();
});

app.on('window-all-closed', () => app.quit());
app.on('before-quit', () => realtime.stop());
app.on('second-instance', () => win?.showInactive());
