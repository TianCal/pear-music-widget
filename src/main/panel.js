'use strict';

const path = require('node:path');
const { BrowserWindow, screen } = require('electron');

const store = require('./store');
const { hideInsteadOfClose, panelSkinOf, baseFor } = require('./window');

// The dropdown renders whichever skin is configured for it, independently of the
// floating widget, so its size follows that skin rather than being fixed.
const sizeOf = () => baseFor(panelSkinOf());
const EDGE_MARGIN = 8;
const GAP_BELOW_MENU_BAR = 6;

// Clicking the tray icon while the panel has focus fires 'blur' (which hides it)
// and then 'click' (which would immediately reopen it). Ignore a toggle that
// arrives right after a blur-close so the icon behaves like a real toggle.
const REOPEN_GUARD_MS = 250;

// Must match `.search` in styles.css. The dropdown never zooms, so this is 1:1.
const SEARCH_PANEL_CSS = 216;

const createPanel = () => {
  const panel = new BrowserWindow({
    ...sizeOf(),
    show: false,
    frame: false,
    transparent: false,
    backgroundColor: '#00000000',
    roundedCorners: true,
    hasShadow: true,
    resizable: false,
    maximizable: false,
    minimizable: false,
    fullscreenable: false,
    skipTaskbar: true,
    vibrancy: 'menu',
    visualEffectState: 'active',
    webPreferences: {
      preload: path.join(__dirname, '..', 'preload', 'index.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      spellcheck: false,
    },
  });

  hideInsteadOfClose(panel);
  panel.setAlwaysOnTop(true, 'pop-up-menu');
  panel.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true });

  panel.loadFile(path.join(__dirname, '..', 'renderer', 'index.html'), {
    query: { mode: 'panel' },
  });

  let hiddenAt = 0;
  let searchExpanded = false;

  /** Grow downwards for the search panel, clamped to the screen. */
  const resize = (expanded) => {
    const size = sizeOf();
    const bounds = panel.getBounds();
    const height = size.height + (expanded ? SEARCH_PANEL_CSS : 0);
    const { workArea } = screen.getDisplayMatching(bounds);
    const maxY = workArea.y + workArea.height - height;
    const y = Math.round(Math.min(bounds.y, Math.max(workArea.y, maxY)));

    panel.setBounds({ x: bounds.x, y, width: size.width, height }, false);
  };

  const setSearchExpanded = (open) => {
    if (open === searchExpanded) return;
    searchExpanded = open;
    resize(open);
  };

  /** Switching the dropdown's skin changes its natural size. */
  const applyPanelSkin = (skin) => {
    store.set({ panelSkin: skin });
    resize(searchExpanded);
  };

  const collapse = () => {
    if (!searchExpanded) return;
    setSearchExpanded(false);
    panel.webContents.send('search-collapsed');
  };

  panel.on('blur', () => {
    if (!panel.isVisible()) return;
    panel.hide();
    // A dropdown that reopens mid-search would be showing a stale query.
    collapse();
    hiddenAt = Date.now();
  });

  // Escape closes it, like a real menu.
  panel.webContents.on('before-input-event', (_event, input) => {
    if (input.type === 'keyDown' && input.key === 'Escape') panel.hide();
  });

  /** Park the panel under the tray icon, clamped to the screen it sits on. */
  const position = (tray) => {
    const size = sizeOf();
    const icon = tray.getBounds();
    const anchorX = icon.x + icon.width / 2;
    const { workArea } = screen.getDisplayNearestPoint({
      x: Math.round(anchorX),
      y: Math.round(icon.y),
    });

    const minX = workArea.x + EDGE_MARGIN;
    const maxX = workArea.x + workArea.width - size.width - EDGE_MARGIN;
    const x = Math.round(Math.min(Math.max(anchorX - size.width / 2, minX), maxX));
    const y = Math.round(icon.y + icon.height + GAP_BELOW_MENU_BAR);

    panel.setBounds({ x, y, ...size }, false);
  };

  const toggle = (tray) => {
    if (panel.isVisible()) {
      panel.hide();
      collapse();
      hiddenAt = Date.now();
      return;
    }
    if (Date.now() - hiddenAt < REOPEN_GUARD_MS) return;
    position(tray);
    panel.show();
    panel.focus();
  };

  return { panel, toggle, setSearchExpanded, applyPanelSkin };
};

module.exports = { createPanel };
