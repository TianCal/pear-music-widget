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
  classic: { width: 300, height: 110 },
  stack: { width: 330, height: 284 },
};

const SKINS = Object.keys(BASE);

// The settings file is hand-editable, so never trust the value blindly.
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

// Height each expanding panel adds, in CSS pixels. Must match `.search` and
// `.lyrics` in styles.css — the window grows by exactly what the panel occupies.
// Only one can be open at a time, which keeps the height arithmetic trivial.
const PANEL_HEIGHT = { search: 216, lyrics: 240 };

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

/**
 * The size the user last left this skin at, falling back to its natural size.
 * Only the width is stored back — the height is always re-derived, so a size
 * can never drift off the skin's aspect ratio.
 */
const sizeForSkin = (skin = skinOf()) => {
  const width = store.get('sizes')?.[skin]?.width;
  if (!width) return baseFor(skin);

  const clamped = clamp(width, MIN_WIDTH, MAX_WIDTH);
  return { width: clamped, height: heightFor(clamped, skin) };
};

/** Remember the current width against the skin that is showing. */
const rememberSize = (skin, width) => {
  store.set({ sizes: { ...(store.get('sizes') || {}), [skin]: { width } } });
};

const createWindow = () => {
  const size = sizeForSkin();
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
      const { x, y, width } = win.getBounds();
      store.set({ bounds: { x, y } });
      rememberSize(skinOf(), width);
    }, 400);
  };

  win.on('move', persist);
  win.on('moved', persist);

  win.on('resize', () => {
    // Growing for a panel is our own doing: it must not be persisted as the
    // user's preferred size.
    if (win.expandedBy) return;

    applyZoom(win);
    persist();
  });

  return win;
};

/**
 * Grow the widget downwards to make room for a panel, and put it back
 * afterwards. The aspect ratio has to be released while expanded or a drag would
 * snap the panel away, and the collapsed bounds are remembered rather than
 * recomputed so an odd size the user chose survives the round trip.
 *
 * `panel` is a key of PANEL_HEIGHT, or null to collapse.
 */
const setExpanded = (win, panel) => {
  if (!win || win.isDestroyed()) return;
  if ((win.expandedBy || null) === (panel || null)) return;

  if (panel && PANEL_HEIGHT[panel]) {
    // Switching straight from one panel to another: measure from the collapsed
    // size we already remembered, not from the currently expanded window.
    const collapsed = win.collapsedBounds || win.getBounds();
    win.collapsedBounds = collapsed;
    win.expandedBy = panel;

    const extra = Math.round(PANEL_HEIGHT[panel] * win.webContents.getZoomFactor());
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

  win.expandedBy = null;
  win.collapsedBounds = null;
};

/**
 * Jump to a layout's natural size from the menu, pinning whichever corner of the
 * window is nearest a screen corner so a widget parked bottom-right stays there.
 */
/**
 * Resize to `next`, pinning whichever corner of the window is nearest a screen
 * corner. Measures against the collapsed bounds when a panel is open, so the
 * panel's extra height does not skew which corner looks nearest.
 */
const resizeKeepingCorner = (win, next) => {
  const current = win.collapsedBounds || win.getBounds();
  const { workArea } = screen.getDisplayMatching(current);

  const distLeft = Math.abs(current.x - workArea.x);
  const distRight = Math.abs(workArea.x + workArea.width - (current.x + current.width));
  const distTop = Math.abs(current.y - workArea.y);
  const distBottom = Math.abs(workArea.y + workArea.height - (current.y + current.height));

  const x = Math.round(distRight < distLeft ? current.x + current.width - next.width : current.x);
  const y = Math.round(distBottom < distTop ? current.y + current.height - next.height : current.y);

  applyCollapsedSize(win, { x, y, ...next });
  return { x, y };
};

/**
 * Set the window to a collapsed size, re-adding the open panel's height if one
 * is showing. Without this, changing skin or resetting while lyrics or search
 * are open would shrink the window under the panel and the two would overlap.
 */
function applyCollapsedSize(win, bounds) {
  const panel = win.expandedBy;
  if (!panel || !PANEL_HEIGHT[panel]) {
    win.setBounds(bounds, false);
    return;
  }

  win.collapsedBounds = bounds;
  const extra = Math.round(PANEL_HEIGHT[panel] * win.webContents.getZoomFactor());
  win.setBounds({ ...bounds, height: bounds.height + extra }, false);
}

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

  // Come back to whatever size the user last left this skin at.
  const position = resizeKeepingCorner(win, sizeForSkin(skin));
  store.set({ bounds: position });

  win.setMinimumSize(MIN_WIDTH, heightFor(MIN_WIDTH, skin));
  win.setMaximumSize(MAX_WIDTH, heightFor(MAX_WIDTH, skin));
  win.setAspectRatio(aspectOf(skin));
  applyZoom(win);
};

/** Back to the skin's natural size, in the default corner, forgetting the
 *  size the user had chosen for it. */
const resetWindow = (win) => {
  const skin = skinOf();
  const size = baseFor(skin);
  const { x, y } = defaultPosition(size);

  const sizes = { ...(store.get('sizes') || {}) };
  delete sizes[skin];
  store.set({ sizes, bounds: { x, y } });

  applyCollapsedSize(win, { x, y, ...size });
  applyZoom(win);
};

module.exports = {
  createWindow,
  hideInsteadOfClose,
  skinOf,
  panelSkinOf,
  baseFor,
  resetWindow,
  applySkin,
  setExpanded,
  BASE,
  SKINS,
  PANEL_HEIGHT,
};
