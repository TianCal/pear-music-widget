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
    volume: typeof volume?.state === 'number' ? volume.state : undefined,
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
    case 'VOLUME_CHANGED':
      update({ volume: msg.volume ?? state.volume, muted: !!msg.muted });
      break;
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
  volume: ({ volume }) => api.actions.setVolume(Math.min(100, Math.max(0, Math.round(volume)))),
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
