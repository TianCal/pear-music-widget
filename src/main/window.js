'use strict';

const path = require('node:path');
const { BrowserWindow, app, screen } = require('electron');

const store = require('./store');

// Electron installs a default application menu, which binds Cmd+W to "close
// window" even though the menu itself is hidden for an accessory app. Closing
// destroys the window, and because the dropdown is still around the app does not
// quit — leaving the tray menu holding a destroyed object. A widget should hide,
// so intercept close everywhere and only really close when we are quitting.
let quitting = false;
app.on('before-quit', () => {
  quitting = true;
});

const hideInsteadOfClose = (win) => {
  win.on('close', (event) => {
    if (quitting) return;
    event.preventDefault();
    win.hide();
  });
};

// The window is exactly the card. macOS then owns the corner rounding and the
// drop shadow, which is the only way to get the vibrancy layer clipped to the
// same radius as the content — a transparent window leaves square blur corners.
//
// Every skin is a self-contained layout with its own natural size and aspect
// ratio. Dragging an edge scales whichever skin is showing; it never switches
// between them.
const BASE = {
  classic: { width: 360, height: 132 },
  compact: { width: 280, height: 103 },
  stack: { width: 330, height: 284 },
};

const SKINS = Object.keys(BASE);

const validSkin = (skin) => (BASE[skin] ? skin : 'classic');
const skinOf = () => validSkin(store.get('skin'));
const panelSkinOf = () => validSkin(store.get('panelSkin'));

const baseFor = (skin = skinOf()) => BASE[validSkin(skin)];
const aspectOf = (skin = skinOf()) => {
  const base = baseFor(skin);
  return base.width / base.height;
};

const heightFor = (width, skin = skinOf()) => Math.round(width / aspectOf(skin));

const MIN_WIDTH = 240;
const MAX_WIDTH = 760;

// Height the search panel adds, in CSS pixels. Must match `.search` in
// styles.css — the window grows by exactly what the panel occupies.
const SEARCH_PANEL_CSS = 216;

const clamp = (v, min, max) => Math.min(max, Math.max(min, v));

const defaultPosition = (size) => {
  const { workArea } = screen.getPrimaryDisplay();
  return {
    x: Math.round(workArea.x + workArea.width - size.width - 16),
    y: Math.round(workArea.y + workArea.height - size.height - 16),
  };
};

/** Keep the saved position usable if the display layout changed since last run. */
const isOnScreen = (bounds, size) =>
  screen.getAllDisplays().some((display) => {
    const a = display.workArea;
    return (
      bounds.x < a.x + a.width - 40 &&
      bounds.x + size.width > a.x + 40 &&
      bounds.y < a.y + a.height - 40 &&
      bounds.y + size.height > a.y
    );
  });

/**
 * Scale the page so the layout always fills the window. The renderer is authored
 * once at the base size; everything else is zoom.
 */
const applyZoom = (win) => {
  const { width, height } = win.getBounds();
  const base = baseFor();
  // Take the tighter of the two axes. Rounding the window height to whole pixels
  // can leave it a fraction short of the ratio, and scaling on width alone would
  // then clip the last row by a pixel.
  const zoom = clamp(Math.min(width / base.width, height / base.height), 0.4, 3);
  win.webContents.setZoomFactor(zoom);
  // The native corner radius is fixed in device pixels, so the CSS radius has to
  // shrink as the page scales up or the two stop lining up.
  win.webContents.send('zoom', zoom);
};

const savedSize = () => {
  const saved = store.get('bounds');
  const width = saved?.width ? clamp(saved.width, MIN_WIDTH, MAX_WIDTH) : null;
  if (!width) return baseFor();
  return { width, height: heightFor(width) };
};

const createWindow = () => {
  const size = savedSize();
  const saved = store.get('bounds');
  const position = saved && isOnScreen(saved, size) ? { x: saved.x, y: saved.y } : defaultPosition(size);

  const win = new BrowserWindow({
    ...size,
    ...position,
    frame: false,
    transparent: false,
    backgroundColor: '#00000000',
    roundedCorners: true,
    hasShadow: true,
    resizable: true,
    maximizable: false,
    minimizable: false,
    fullscreenable: false,
    skipTaskbar: true,
    show: false,
    vibrancy: 'under-window',
    visualEffectState: 'active',
    alwaysOnTop: store.get('alwaysOnTop'),
    webPreferences: {
      preload: path.join(__dirname, '..', 'preload', 'index.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      spellcheck: false,
    },
  });

  win.setAspectRatio(aspectOf());
  win.setMinimumSize(MIN_WIDTH, heightFor(MIN_WIDTH));
  win.setMaximumSize(MAX_WIDTH, heightFor(MAX_WIDTH));
  win.setAlwaysOnTop(store.get('alwaysOnTop'), 'floating');
  win.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true });
  win.setOpacity(store.get('opacity'));

  hideInsteadOfClose(win);
  win.loadFile(path.join(__dirname, '..', 'renderer', 'index.html'));
  win.once('ready-to-show', () => {
    applyZoom(win);
    win.showInactive();
  });
  win.webContents.on('did-finish-load', () => applyZoom(win));

  let saveTimer = null;
  const persist = () => {
    clearTimeout(saveTimer);
    saveTimer = setTimeout(() => {
      if (win.isDestroyed()) return;
      const { x, y, width, height } = win.getBounds();
      store.set({ bounds: { x, y, width, height } });
    }, 400);
  };

  win.on('move', persist);
  win.on('moved', persist);

  win.on('resize', () => {
    // Growing for the search panel is our own doing: it must not be persisted
    // as the user's preferred size, nor re-evaluated against the breakpoint.
    if (win.searchExpanded) return;

    applyZoom(win);
    persist();
  });

  return win;
};

/**
 * Grow the widget downwards to make room for the search panel, and put it back
 * afterwards. The aspect ratio has to be released while expanded or a drag would
 * snap the panel away, and the collapsed bounds are remembered rather than
 * recomputed so an odd size the user chose survives the round trip.
 */
const setSearchExpanded = (win, open) => {
  if (!win || win.isDestroyed() || !!win.searchExpanded === open) return;

  if (open) {
    const collapsed = win.getBounds();
    win.collapsedBounds = collapsed;
    win.searchExpanded = true;

    const extra = Math.round(SEARCH_PANEL_CSS * win.webContents.getZoomFactor());
    const height = collapsed.height + extra;

    // Slide up if the taller window would hang off the bottom of the screen.
    const { workArea } = screen.getDisplayMatching(collapsed);
    const maxY = workArea.y + workArea.height - height;
    const y = Math.round(Math.min(collapsed.y, Math.max(workArea.y, maxY)));

    win.setAspectRatio(0);
    win.setMinimumSize(MIN_WIDTH, 80);
    win.setMaximumSize(MAX_WIDTH, 4000);
    win.setBounds({ x: collapsed.x, y, width: collapsed.width, height }, false);
    return;
  }

  const collapsed = win.collapsedBounds;
  win.setMinimumSize(MIN_WIDTH, heightFor(MIN_WIDTH));
  win.setMaximumSize(MAX_WIDTH, heightFor(MAX_WIDTH));
  if (collapsed) win.setBounds(collapsed, false);
  win.setAspectRatio(aspectOf());

  win.searchExpanded = false;
  win.collapsedBounds = null;
};

/**
 * Jump to a layout's natural size from the menu, pinning whichever corner of the
 * window is nearest a screen corner so a widget parked bottom-right stays there.
 */
/** Resize to `next`, pinning whichever corner of the window is nearest a screen corner. */
const resizeKeepingCorner = (win, next) => {
  const current = win.getBounds();
  const { workArea } = screen.getDisplayMatching(current);

  const distLeft = Math.abs(current.x - workArea.x);
  const distRight = Math.abs(workArea.x + workArea.width - (current.x + current.width));
  const distTop = Math.abs(current.y - workArea.y);
  const distBottom = Math.abs(workArea.y + workArea.height - (current.y + current.height));

  const x = Math.round(distRight < distLeft ? current.x + current.width - next.width : current.x);
  const y = Math.round(distBottom < distTop ? current.y + current.height - next.height : current.y);

  win.setBounds({ x, y, ...next }, false);
  return { x, y, ...next };
};

/**
 * Switch layout wholesale. Each skin has its own aspect ratio, so the lock has
 * to be re-set or the next drag would snap the window back to the old shape.
 */
const applySkin = (win, skin) => {
  if (!BASE[skin]) return;
  store.set({ skin });

  win.setAspectRatio(0);
  win.setMinimumSize(MIN_WIDTH, 40);
  win.setMaximumSize(MAX_WIDTH, 4000);

  const bounds = resizeKeepingCorner(win, baseFor(skin));
  store.set({ bounds });

  win.setMinimumSize(MIN_WIDTH, heightFor(MIN_WIDTH, skin));
  win.setMaximumSize(MAX_WIDTH, heightFor(MAX_WIDTH, skin));
  win.setAspectRatio(aspectOf(skin));
  applyZoom(win);
};

const resetPosition = (win) => {
  const { width, height } = win.getBounds();
  const { x, y } = defaultPosition({ width, height });
  win.setPosition(x, y, true);
  store.set({ bounds: { x, y, width, height } });
};

module.exports = {
  createWindow,
  hideInsteadOfClose,
  skinOf,
  panelSkinOf,
  baseFor,
  resetPosition,
  applySkin,
  setSearchExpanded,
  BASE,
  SKINS,
  SEARCH_PANEL_CSS,
};
