'use strict';

const { exec } = require('node:child_process');
const { Menu, Tray, app, nativeImage } = require('electron');

const store = require('./store');
const TRAY_ICONS = require('./tray-icons');
const { resetPosition, applyAppearance } = require('./window');

/** Build a template image from the generated base64 pair. */
const buildIcon = (variant) => {
  const source = TRAY_ICONS[variant];
  const icon = nativeImage.createFromDataURL(`data:image/png;base64,${source.x1}`);
  icon.addRepresentation({
    scaleFactor: 2,
    dataURL: `data:image/png;base64,${source.x2}`,
  });
  icon.setTemplateImage(true);
  return icon;
};

const ICONS = {
  solid: buildIcon('solid'),
  hollow: buildIcon('hollow'),
};

/** Bring the music app forward; the bundle differs between th-ch and pear builds. */
const openMusicApp = () => {
  exec('open -a "YouTube Music" || open -a "Pear" || open -a "Pear Desktop"');
};

const STATUS_LABEL = {
  connected: 'Connected',
  connecting: 'Connecting…',
  offline: 'API server unreachable',
  unauthorized: 'Not authorised',
  denied: 'Authorisation denied',
};

const SKINS = [
  { key: 'classic', label: 'Classic' },
  { key: 'stack', label: 'Stack' },
];

/** A window can be gone by the time a menu is built; never let that throw. */
const alive = (win) => !!win && !win.isDestroyed();

const onWindow = (win, fn) => {
  if (alive(win)) fn(win);
};

// Shared between the menu-bar menu and the widget's own right-click menu, so
// the two can never drift apart.
const items = {
  skin: (setSkin) => ({
    label: 'Skin',
    submenu: SKINS.map(({ key, label }) => ({
      label,
      type: 'radio',
      checked: store.get('skin') === key,
      click: () => setSkin(key),
    })),
  }),

  size: (setAppearance) => ({
    label: 'Widget size',
    // Only the classic skin has a second variant.
    enabled: store.get('skin') === 'classic',
    submenu: [
      { key: 'normal', label: 'Normal' },
      { key: 'compact', label: 'Compact' },
    ].map(({ key, label }) => ({
      label,
      type: 'radio',
      checked: store.get('appearance') === key,
      click: () => setAppearance(key),
    })),
  }),

  opacity: (window) => ({
    label: 'Opacity',
    submenu: [100, 90, 80, 70, 60].map((pct) => ({
      label: `${pct}%`,
      type: 'radio',
      checked: Math.round(store.get('opacity') * 100) === pct,
      click: () => {
        store.set({ opacity: pct / 100 });
        onWindow(window, (w) => w.setOpacity(pct / 100));
      },
    })),
  }),

  alwaysOnTop: (window) => ({
    label: 'Always on top',
    type: 'checkbox',
    checked: store.get('alwaysOnTop'),
    click: (item) => {
      store.set({ alwaysOnTop: item.checked });
      onWindow(window, (w) => w.setAlwaysOnTop(item.checked, 'floating'));
    },
  }),

  resetPosition: (window) => ({
    label: 'Reset position',
    enabled: alive(window),
    click: () => onWindow(window, resetPosition),
  }),

  openApp: () => ({ label: 'Open YouTube Music', click: openMusicApp }),

  quit: () => ({ label: 'Quit', accelerator: 'Command+Q', click: () => app.quit() }),
};

/**
 * Right-click menu for the floating widget itself: the things you would want
 * while looking at it, minus the app-level plumbing that belongs in the tray.
 */
const buildWidgetMenu = ({ window, setSkin, setAppearance }) =>
  Menu.buildFromTemplate([
    {
      label: 'Search…',
      enabled: alive(window),
      click: () => onWindow(window, (w) => w.webContents.send('open-search')),
    },
    { type: 'separator' },
    items.skin(setSkin),
    items.size(setAppearance),
    items.opacity(window),
    items.alwaysOnTop(window),
    { type: 'separator' },
    items.resetPosition(window),
    { label: 'Hide widget', enabled: alive(window), click: () => onWindow(window, (w) => w.hide()) },
    { type: 'separator' },
    items.openApp(),
    items.quit(),
  ]);

const createTray = ({ window, realtime, getState, setSkin, setAppearance, togglePanel }) => {
  const tray = new Tray(ICONS.hollow);
  let iconVariant = 'hollow';

  const build = () => {
    const { status, song } = getState();
    const nowPlaying = song ? `${song.title} — ${song.artist}` : 'Nothing playing';

    return Menu.buildFromTemplate([
      { label: nowPlaying.slice(0, 60), enabled: false },
      { label: STATUS_LABEL[status] || status, enabled: false },
      { type: 'separator' },
      {
        // Act on the checkbox's own new state rather than re-reading the window.
        label: 'Show floating widget',
        type: 'checkbox',
        enabled: alive(window),
        checked: alive(window) && window.isVisible(),
        click: (item) => onWindow(window, (w) => (item.checked ? w.showInactive() : w.hide())),
      },
      items.alwaysOnTop(window),
      items.skin(setSkin),
      items.size(setAppearance),
      items.opacity(window),
      items.resetPosition(window),
      { type: 'separator' },
      {
        label: 'Open at login',
        type: 'checkbox',
        checked: app.getLoginItemSettings().openAtLogin,
        click: (item) => app.setLoginItemSettings({ openAtLogin: item.checked }),
      },
      { label: 'Reconnect', click: () => realtime.retry() },
      items.openApp(),
      { type: 'separator' },
      items.quit(),
    ]);
  };

  // Left click drops down the player. Right click opens settings. The menu is
  // built at popup time, so it can never show a stale checkbox.
  tray.on('click', () => togglePanel(tray));
  tray.on('right-click', () => tray.popUpContextMenu(build()));

  const refresh = () => {
    const { song, status } = getState();

    // Hollow glyph whenever we cannot reach YouTube Music, so the menu bar
    // itself tells you whether the player is up.
    const variant = status === 'connected' ? 'solid' : 'hollow';
    if (variant !== iconVariant) {
      iconVariant = variant;
      tray.setImage(ICONS[variant]);
    }

    tray.setToolTip(
      song ? `${song.title} — ${song.artist}` : `Pear Music Widget — ${STATUS_LABEL[status] || status}`,
    );
  };
  refresh();

  return { tray, refresh };
};

module.exports = { createTray, buildWidgetMenu, openMusicApp };
