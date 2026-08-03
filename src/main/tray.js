'use strict';

const { exec } = require('node:child_process');
const { Menu, Tray, app, nativeImage } = require('electron');

const store = require('./store');
const TRAY_ICONS = require('./tray-icons');
const { resetWindow } = require('./window');

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

// The bundle differs between th-ch and pear builds, so both helpers try each
// name in turn.
const MUSIC_APPS = ['YouTube Music', 'Pear', 'Pear Desktop'];

const openMusicApp = () => {
  exec(MUSIC_APPS.map((name) => `open -a "${name}"`).join(' || '));
};

// pear-desktop and th-ch's build ship the same appId, so one id covers both.
const MUSIC_APP_ID = 'com.github.th-ch.youtube-music';

/**
 * Ask the music app to quit, then run `done` — whether or not it worked.
 *
 * Addressed by bundle id, not by name: `application "YouTube Music" is running`
 * answers false even while it is running, which silently turns the guard into a
 * no-op. By id it is accurate, and a guarded quit on a closed app does nothing
 * rather than launching it.
 *
 * `quit` is an Apple event, so the first use raises the one-time "wants to
 * control" prompt. If that is denied the script just fails and we still quit
 * ourselves.
 */
const quitMusicApp = (done) => {
  let settled = false;
  const finish = () => {
    if (settled) return;
    settled = true;
    done();
  };

  const target = `application id "${MUSIC_APP_ID}"`;
  exec(`osascript -e 'if ${target} is running then quit ${target}'`, finish);

  // Never hang the quit on a blocked or slow Apple event.
  setTimeout(finish, 4000);
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

  panelSkin: (setPanelSkin) => ({
    label: 'Dropdown skin',
    submenu: SKINS.map(({ key, label }) => ({
      label,
      type: 'radio',
      checked: store.get('panelSkin') === key,
      click: () => setPanelSkin(key),
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

  resetWindow: (window) => ({
    label: 'Reset size and position',
    enabled: alive(window),
    click: () => onWindow(window, resetWindow),
  }),

  openApp: () => ({ label: 'Open YouTube Music', click: openMusicApp }),

  quit: () => ({ label: 'Quit', accelerator: 'Command+Q', click: () => app.quit() }),

  quitWithMusicApp: () => ({
    label: 'Quit with YouTube Music',
    click: () => quitMusicApp(() => app.quit()),
  }),
};

/**
 * Right-click menu for the floating widget itself: the things you would want
 * while looking at it, minus the app-level plumbing that belongs in the tray.
 */
const buildWidgetMenu = ({ window, setSkin, setPanelSkin }) =>
  Menu.buildFromTemplate([
    {
      label: 'Search…',
      enabled: alive(window),
      click: () => onWindow(window, (w) => w.webContents.send('open-search')),
    },
    { type: 'separator' },
    items.skin(setSkin),
    items.panelSkin(setPanelSkin),
    items.opacity(window),
    items.alwaysOnTop(window),
    { type: 'separator' },
    items.resetWindow(window),
    { label: 'Hide widget', enabled: alive(window), click: () => onWindow(window, (w) => w.hide()) },
    { type: 'separator' },
    items.openApp(),
    items.quit(),
    items.quitWithMusicApp(),
  ]);

const createTray = ({ window, realtime, getState, setSkin, setPanelSkin, togglePanel }) => {
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
      items.panelSkin(setPanelSkin),
      items.opacity(window),
      items.resetWindow(window),
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
      items.quitWithMusicApp(),
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
