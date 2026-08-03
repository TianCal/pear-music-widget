'use strict';

const path = require('node:path');
const { BrowserWindow, screen } = require('electron');

const store = require('./store');

// The window is exactly the card. macOS then owns the corner rounding and the
// drop shadow, which is the only way to get the vibrancy layer clipped to the
// same radius as the content — a transparent window leaves square blur corners.
//
// Both layouts share one aspect ratio, so dragging any edge is a pure scale and
// crossing the breakpoint never makes the window jump shape. What changes at the
// breakpoint is the layout (and therefore the zoom factor the content is drawn
// at): compact shows fewer controls, so it can render them larger in less space.
const BASE = {
  normal: { width: 420, height: 142 },
  compact: { width: 340, height: 115 },
};

const ASPECT = BASE.normal.width / BASE.normal.height;
const heightFor = (width) => Math.round(width / ASPECT);

// Hysteresis, so a slow drag across the threshold does not flap.
const TO_COMPACT_BELOW = 392;
const TO_NORMAL_ABOVE = 412;

const MIN_WIDTH = 240;
const MAX_WIDTH = 760;

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
  const base = BASE[store.get('appearance')] || BASE.normal;
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
  if (!width) return BASE[store.get('appearance')] || BASE.normal;
  return { width, height: heightFor(width) };
};

const createWindow = ({ onAppearance } = {}) => {
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

  win.setAspectRatio(ASPECT);
  win.setMinimumSize(MIN_WIDTH, heightFor(MIN_WIDTH));
  win.setMaximumSize(MAX_WIDTH, heightFor(MAX_WIDTH));
  win.setAlwaysOnTop(store.get('alwaysOnTop'), 'floating');
  win.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true });
  win.setOpacity(store.get('opacity'));

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
    const { width } = win.getBounds();
    const current = store.get('appearance');

    let next = current;
    if (current === 'normal' && width < TO_COMPACT_BELOW) next = 'compact';
    else if (current === 'compact' && width > TO_NORMAL_ABOVE) next = 'normal';

    if (next !== current) {
      store.set({ appearance: next });
      onAppearance?.(next);
    }
    applyZoom(win);
    persist();
  });

  return win;
};

/**
 * Jump to a layout's natural size from the menu, pinning whichever corner of the
 * window is nearest a screen corner so a widget parked bottom-right stays there.
 */
const applyAppearance = (win, appearance) => {
  const next = BASE[appearance] || BASE.normal;
  const current = win.getBounds();
  const { workArea } = screen.getDisplayMatching(current);

  const distLeft = Math.abs(current.x - workArea.x);
  const distRight = Math.abs(workArea.x + workArea.width - (current.x + current.width));
  const distTop = Math.abs(current.y - workArea.y);
  const distBottom = Math.abs(workArea.y + workArea.height - (current.y + current.height));

  const x = Math.round(distRight < distLeft ? current.x + current.width - next.width : current.x);
  const y = Math.round(distBottom < distTop ? current.y + current.height - next.height : current.y);

  store.set({ appearance, bounds: { x, y, ...next } });
  win.setBounds({ x, y, ...next }, false);
  applyZoom(win);
};

const resetPosition = (win) => {
  const size = { width: win.getBounds().width, height: win.getBounds().height };
  const { x, y } = defaultPosition(size);
  win.setPosition(x, y, true);
  store.set({ bounds: { x, y, ...size } });
};

module.exports = { createWindow, resetPosition, applyAppearance, BASE, ASPECT };
