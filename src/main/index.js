'use strict';

const { app, ipcMain, BrowserWindow } = require('electron');

const api = require('./api');
const store = require('./store');
const { parseSearchResults, parseQueueUpcoming, queueEntries } = require('./search');
const { RealtimeClient } = require('./ws');
const {
  createWindow,
  applySkin,
  setSearchExpanded,
  skinOf,
  panelSkinOf,
  SKINS,
} = require('./window');
const { createPanel } = require('./panel');
const { createTray, buildWidgetMenu, openMusicApp } = require('./tray');

if (!app.requestSingleInstanceLock()) {
  app.quit();
  return;
}

app.commandLine.appendSwitch('disable-features', 'HardwareMediaKeyHandling');

let win = null;
let panel = null;
let tray = null;
let setSkin = () => {};
let setPanelSkin = () => {};
const realtime = new RealtimeClient();

// ------------------------------------------------------------- player state

// Repeat is deliberately absent: POST /switch-repeat is accepted but
// GET /repeat-mode always answers null on the shipped api-server plugin, so the
// widget can never show a truthful repeat state.
const state = {
  status: 'connecting',
  statusMessage: '',
  skin: skinOf(),
  panelSkin: panelSkinOf(),
  song: null,
  cover: null,
  upnext: [],
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
  refreshUpNext().catch(() => {});
};

/**
 * "Next tracks" for the stack skin. Only fetched when that skin is showing —
 * it is an extra request plus artwork on every track change.
 */
let upnextToken = 0;

const refreshUpNext = async () => {
  if (skinOf() !== 'stack' && panelSkinOf() !== 'stack') {
    if (state.upnext.length) update({ upnext: [] });
    return;
  }

  const token = ++upnextToken;
  const queue = await api.queries.queue().catch(() => null);
  if (token !== upnextToken || !queue) return;

  const items = parseQueueUpcoming(queue, 4);
  const covers = await Promise.all(
    items.map((item) => api.fetchCover(item.thumbnail).catch(() => null)),
  );
  if (token !== upnextToken) return;

  const upnext = items.map((item, i) => ({ ...item, thumbnail: covers[i] }));
  // Arrays are never === so compare identity by the ids they carry.
  const key = (list) => list.map((i) => i.videoId).join(',');
  if (key(upnext) !== key(state.upnext)) update({ upnext });
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
    // Drop the queue too: it belongs to a player we can no longer see.
    update({ isPlaying: false, upnext: [] });
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

// -------------------------------------------------------------------- search

// Polling budget for a queued track to show up (see widget:play-result).
const PLAY_POLL_ATTEMPTS = 20;
const PLAY_POLL_MS = 150;
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

let panelView = null;

ipcMain.handle('widget:search-open', (event, open) => {
  const sender = BrowserWindow.fromWebContents(event.sender);
  if (!sender) return { ok: false };

  if (panel && sender.id === panel.id) panelView?.setSearchExpanded(!!open);
  else setSearchExpanded(sender, !!open);

  return { ok: true };
});

ipcMain.handle('widget:search', async (_event, query) => {
  if (typeof query !== 'string' || !query.trim()) return { ok: true, results: [] };
  try {
    const payload = await api.actions.search(query.trim());
    const results = parseSearchResults(payload);

    // Artwork goes through the main process for the same reason cover art does:
    // the renderer's CSP allows data: URLs only.
    const thumbnails = await Promise.all(
      results.map((item) => api.fetchCover(item.thumbnail).catch(() => null)),
    );
    return {
      ok: true,
      results: results.map((item, i) => ({ ...item, thumbnail: thumbnails[i] })),
    };
  } catch (err) {
    return { ok: false, error: err.message || 'Search failed' };
  }
});

ipcMain.handle('widget:play-result', async (_event, videoId) => {
  if (typeof videoId !== 'string' || !videoId) return { ok: false };
  try {
    const before = queueEntries(await api.queries.queue().catch(() => null));

    await api.actions.addToQueue(videoId, 'INSERT_AFTER_CURRENT_VIDEO');

    // The 204 comes back before YouTube Music has actually mutated its queue, so
    // poll for the new slot instead of pressing next — next would skip onto
    // whatever was already queued and play the wrong track.
    for (let attempt = 0; attempt < PLAY_POLL_ATTEMPTS; attempt += 1) {
      await delay(PLAY_POLL_MS);

      const entries = queueEntries(await api.queries.queue().catch(() => null));
      const current = entries.findIndex((entry) => entry.selected);
      if (current < 0 || current + 1 >= entries.length) continue;

      // INSERT_AFTER_CURRENT_VIDEO always lands in the slot after the playing
      // track. Accept it once that slot holds our id, or — since the id is
      // occasionally re-resolved on insert — once the queue has simply grown.
      const landed =
        entries[current + 1].videoId === videoId || entries.length > before.length;
      if (!landed) continue;

      await api.actions.setQueueIndex(current + 1);
      return { ok: true, index: current + 1 };
    }

    return { ok: false, error: 'Queued, but it did not appear in time' };
  } catch (err) {
    return { ok: false, error: err.message };
  }
});

// Right-clicking the floating widget gets a focused subset of the tray menu.
// The dropdown is left alone: popping a menu over it would blur it shut.
ipcMain.on('widget:context-menu', (event) => {
  const sender = BrowserWindow.fromWebContents(event.sender);
  if (!win || !sender || sender.id !== win.id) return;
  buildWidgetMenu({ window: win, setSkin, setPanelSkin }).popup({ window: win });
});

ipcMain.handle('widget:skin', (_event, skin) => {
  if (!SKINS.includes(skin)) return { ok: false };
  setSkin(skin);
  return { ok: true };
});

ipcMain.handle('widget:panel-skin', (_event, skin) => {
  if (!SKINS.includes(skin)) return { ok: false };
  setPanelSkin(skin);
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
  win = createWindow();

  panelView = createPanel();
  panel = panelView.panel;

  setSkin = (skin) => {
    applySkin(win, skin);
    update({ skin });
    refreshUpNext().catch(() => {});
    tray?.refresh();
  };

  setPanelSkin = (skin) => {
    panelView.applyPanelSkin(skin);
    update({ panelSkin: skin });
    refreshUpNext().catch(() => {});
    tray?.refresh();
  };

  tray = createTray({
    window: win,
    realtime,
    getState: () => state,
    setSkin: (skin) => setSkin(skin),
    setPanelSkin: (skin) => setPanelSkin(skin),
    togglePanel: (trayInstance) => panelView.toggle(trayInstance),
  });
  realtime.start();
});

app.on('window-all-closed', () => app.quit());
app.on('before-quit', () => realtime.stop());
app.on('second-instance', () => win?.showInactive());
