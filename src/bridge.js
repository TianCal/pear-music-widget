'use strict';

/**
 * The `window.widget` bridge, and the two things a WKWebView cannot do with CSS
 * alone: move the window and dismiss it with keyboard shortcuts.
 *
 * `app.js` was first written against the Electron preload; this file keeps
 * that `window.widget` surface, implemented on Tauri's IPC, and is the only
 * place the renderer touches Tauri.
 *
 * Wrapped in an IIFE because a preload ran in its own context but a plain
 * `<script>` shares one global scope with `app.js`: a top-level `const` here
 * that also exists there (`IS_PANEL` did) is a SyntaxError that takes the whole
 * page down before either file runs. Only `window.widget` is deliberately
 * global.
 */

(() => {
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { getCurrentWindow } = window.__TAURI__.window;

const IS_PANEL = new URLSearchParams(location.search).get('mode') === 'panel';

/** Register a listener and hand back an unsubscribe, like `ipcRenderer.off`.
 *  Registration is asynchronous: `off.ready` resolves once the listener is
 *  in place, which a cold load must wait for before asking for state, or an
 *  event emitted in between is lost and the reply it would have superseded
 *  stays on screen. */
const on = (event, handler) => {
  let stop = null;
  let cancelled = false;
  const ready = listen(event, ({ payload }) => handler(payload)).then((unlisten) => {
    if (cancelled) unlisten();
    else stop = unlisten;
  });
  const off = () => {
    cancelled = true;
    if (stop) stop();
  };
  off.ready = ready;
  return off;
};

window.widget = {
  getState: () => invoke('widget_state'),
  onState: (handler) => on('state', handler),
  // Artwork, the queue and the lyrics are each far larger than the state they
  // used to ride on, and each changes about once a track where the state
  // changes once a second. They get their own channels so the position tick
  // does not drag them across the IPC boundary sixty times a minute. The queue
  // carries text only; its artwork is fetched per visible row via `queueArt`.
  onCover: (handler) => on('cover', handler),
  onQueue: (handler) => on('queue', handler),
  onLyrics: (handler) => on('lyrics', handler),
  command: (name, payload) => invoke('command', { name, payload }),
  openApp: () => invoke('open_app'),
  search: (query) => invoke('search_tracks', { query }),
  playResult: (videoId) => invoke('play_result', { videoId }),
  playQueueIndex: (index, videoId) => invoke('play_queue_index', { index, videoId }),
  // The playlist picker: YouTube Music playlists and saved folders together.
  playlists: () => invoke('list_playlists'),
  playPlaylist: (kind, id) => invoke('play_playlist', { kind, id }),
  playlistArt: (urls) => invoke('playlist_art', { urls }),
  // An artist's Top songs, by the browse id on `song.artists`, or for an
  // artist YouTube Music did not link, by `query` — its own name for them.
  artistPage: (id, query) => invoke('artist_page', { id, query }),
  // Artwork for the queue rows a surface is showing. A command rather than an
  // event: a reply is formatted once, an event once per webview.
  queueArt: (ids, size) => invoke('queue_art', { ids, size }),
  // Ids-free nudge that covers which missed a `queueArt` reply are now cached:
  // ask again for whatever is on screen without art.
  onQueueArtReady: (handler) => on('queue-art-ready', handler),
  setPanel: (which) => invoke('set_panel', { which }),
  contextMenu: () => invoke('context_menu'),
  hideWidget: () => invoke('hide_widget'),
  onPanelCollapsed: (handler) => on('panel-collapsed', handler),
  onPanelRestore: (handler) => on('panel-restore', handler),
  onZoom: (handler) => on('zoom', handler),
  onOnScreen: (handler) => on('on-screen', handler),
  retry: () => invoke('retry'),
};

// ------------------------------------------------------------- window drag

/**
 * Electron let CSS declare the drag region. WKWebView has no equivalent, so the
 * page moves the window itself.
 *
 * The mirror image of the old rule: the card drags, and everything that was
 * `-webkit-app-region: no-drag` is excluded here instead. Dragging only starts
 * once the pointer has actually travelled, which is what keeps click and
 * double-click — the artwork's "bring YouTube Music forward" — working, and is
 * also why the whole card can respond to a double click rather than only the
 * artwork the way Electron forced.
 */
const NO_DRAG =
  '.art, .seek, .btn, .vol-pop, .lyrics, .corner, .search, .upnext, .queue, ' +
  '.pill, .edge, .playlists, .artist-link, input, button';

const DRAG_THRESHOLD = 3;

if (!IS_PANEL) {
  let origin = null;

  document.addEventListener('mousedown', (event) => {
    if (event.button !== 0 || event.target.closest(NO_DRAG)) return;
    origin = { x: event.clientX, y: event.clientY };
  });

  document.addEventListener('mousemove', (event) => {
    if (!origin) return;
    const travelled = Math.hypot(event.clientX - origin.x, event.clientY - origin.y);
    if (travelled < DRAG_THRESHOLD) return;
    origin = null;
    getCurrentWindow().startDragging();
  });

  const endDrag = () => {
    origin = null;
  };
  document.addEventListener('mouseup', endDrag);
  document.addEventListener('mouseleave', endDrag);
}

// -------------------------------------------------------- window shortcuts

// AppKit's standard Close action does not close a borderless window. Match
// the widget's close button directly, keeping the tray app available to reopen.
if (!IS_PANEL) {
  document.addEventListener(
    'keydown',
    (event) => {
      if (event.metaKey && !event.ctrlKey && !event.altKey && !event.shiftKey &&
          event.key.toLowerCase() === 'w') {
        event.preventDefault();
        invoke('hide_widget');
      }
    },
    true,
  );
}

// Escape closes the dropdown, like a real menu. The page's own handler closes
// whichever sub-panel is open; this closes the window itself.
if (IS_PANEL) {
  document.addEventListener(
    'keydown',
    (event) => {
      if (event.key === 'Escape') invoke('hide_panel');
    },
    true,
  );
}
})();
