'use strict';

/**
 * The `window.widget` bridge, and the two things a WKWebView cannot do with CSS
 * alone: move the window and close the dropdown on Escape.
 *
 * `app.js` was written against the Electron preload and is unchanged — this
 * file re-implements that surface on Tauri's IPC so it never had to be.
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

/** Register a listener and hand back an unsubscribe, like `ipcRenderer.off`. */
const on = (event, handler) => {
  let stop = null;
  let cancelled = false;
  listen(event, ({ payload }) => handler(payload)).then((unlisten) => {
    if (cancelled) unlisten();
    else stop = unlisten;
  });
  return () => {
    cancelled = true;
    if (stop) stop();
  };
};

window.widget = {
  getState: () => invoke('widget_state'),
  onState: (handler) => on('state', handler),
  // Artwork, the queue and the lyrics are each far larger than the state they
  // used to ride on, and each changes about once a track where the state
  // changes once a second. They get their own channels so the position tick
  // does not drag them across the IPC boundary sixty times a minute.
  onCover: (handler) => on('cover', handler),
  onUpNext: (handler) => on('upnext', handler),
  onLyrics: (handler) => on('lyrics', handler),
  command: (name, payload) => invoke('command', { name, payload }),
  setSkin: (skin) => invoke('set_skin', { skin }),
  setPanelSkin: (skin) => invoke('set_panel_skin', { skin }),
  openApp: () => invoke('open_app'),
  search: (query) => invoke('search_tracks', { query }),
  playResult: (videoId) => invoke('play_result', { videoId }),
  playQueued: (videoId) => invoke('play_queued', { videoId }),
  setPanel: (which) => invoke('set_panel', { which }),
  contextMenu: () => invoke('context_menu'),
  onPanelCollapsed: (handler) => on('panel-collapsed', handler),
  onZoom: (handler) => on('zoom', handler),
  retry: () => invoke('retry'),
  quit: () => invoke('quit'),
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
  '.art, .seek, .btn, .vol-pop, .lyrics, .corner, .search, .upnext-item, .pill, .edge, input, button';

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

// ------------------------------------------------------------------ escape

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
