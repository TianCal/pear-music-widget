# Pear Music Widget

A floating now-playing widget for macOS, driven by the **API Server** plugin in
[pear-desktop](https://github.com/pear-devs/pear-desktop) (and its predecessor
[th-ch/youtube-music](https://github.com/th-ch/youtube-music)).

Inspired by [YoutubeMusicCoverWidget](https://github.com/rafailpapastamou/YoutubeMusicCoverWidget),
which polls the same API server every two seconds from an Übersicht widget. This
one is a standalone Electron app instead, and it holds the API server's
**WebSocket** open so track, position, volume, repeat and shuffle changes arrive
as push events rather than on a timer.

![The floating widget on the desktop](docs/widget.png)

## Two surfaces

**A menu-bar dropdown.** Click the play icon in the menu bar and a 400×148
popover drops down under it — artwork, title, artist, transport, a scrubable
progress bar and a countdown to the end of the track. It closes on click-away or
Escape. This is meant to replace the system Now Playing menu-bar item.

**A floating widget.** An always-available panel you can park anywhere on the
desktop, in two sizes.

Both are the same renderer driven by the same state, so they never disagree.

| Tray icon | |
| --- | --- |
| **Left click** | Open/close the player dropdown |
| **Right click** | Settings menu (widget visibility, size, opacity, always on top, open at login, reconnect, quit) |
| **Solid glyph** | Connected to YouTube Music |
| **Hollow glyph** | YouTube Music is not running, or its API server is off |

The settings menu is built the moment you open it, so its checkboxes can never
be stale.

If YouTube Music is not running, pressing play — in either surface — launches it
instead of failing, and the widget connects as soon as the app is up. The setup
screen grows an **Open YouTube Music** button for the same reason.

## What it looks like

A frameless panel with real macOS vibrancy, native rounded corners and shadow.
The album art is bled behind the content as a blurred ambient layer, and the
accent colour — play button, progress fill, active toggles — is extracted from
the cover art on every track change.

### Resizing

Drag any edge of the floating widget. The aspect ratio is locked, so width and
height always move together, and the whole layout scales with the window rather
than leaving the content stranded in whitespace.

Two layouts, chosen automatically as you drag past the breakpoint:

| | Natural size | Controls |
| --- | --- | --- |
| **Normal** | 420×142 | shuffle, previous, play/pause, next, like, volume, elapsed/duration |
| **Compact** | 340×115 | previous, play/pause, next, like, volume |

Both keep the artwork, titles and the scrubable progress bar. Anything from
240px to 760px wide is allowed. The breakpoint has hysteresis — it drops to
compact below 392px and returns to normal above 412px — so a slow drag across
it does not flap.

The menu bar's **Widget size** items jump straight to a layout's natural size,
pinning whichever screen corner the window is already nearest, so a widget
parked bottom-right stays bottom-right.

## Features

- **Realtime**, not polled: `VIDEO_CHANGED`, `PLAYER_STATE_CHANGED`,
  `POSITION_CHANGED`, `VOLUME_CHANGED`, `SHUFFLE_CHANGED`
- Full transport: play/pause, next, previous, **scrubable** progress bar
- Shuffle and like (right-click the heart to dislike)
- Volume three ways: drag the popover on the speaker icon, scroll anywhere on
  the card, or press **↑ / ↓** while the widget or dropdown has focus (hold
  **Shift** for 1% steps). Scroll and keyboard flash the slider so you can see
  the level
- Cover art, title, artist and album, with the title drifting when it overflows
- Accent colour derived from the artwork
- Light and dark appearance
- Drag anywhere to move; position is remembered across launches
- Menu-bar item: show/hide, always on top, appearance, opacity, reset position,
  reconnect, open at login, quit. The app has no Dock icon.

### No repeat button

`POST /api/v1/switch-repeat` is accepted (204), but `GET /api/v1/repeat-mode`
returns `{"mode": null}` unconditionally on the shipped api-server plugin —
`/shuffle` and `/like-state` return real values, so this is specific to repeat.
The widget could send the click but never show whether repeat was on, off or
one, so the button was dropped rather than shipped lying. If a later build
starts reporting the mode, re-adding it is `queries.repeatMode` in
`src/main/api.js` plus a button in the controls row.

## Download

Grab the DMG from [the latest release](https://github.com/TianCal/pear-music-widget/releases/latest),
open it, and drag the app onto Applications.

It is ad-hoc signed but not notarised, so a downloaded copy arrives quarantined —
right-click → Open the first time, or:

```bash
xattr -dr com.apple.quarantine "/Applications/Pear Music Widget.app"
```

## Requirements

- macOS
- Node 18+ (built and tested on Node 26)
- pear-desktop or th-ch/youtube-music, with the **API Server** plugin enabled

## Compatibility

This talks to the `api-server` plugin, which is versioned independently of the
host app and can change its protocol. Pinned here so a future break is easy to
diagnose:

| | |
| --- | --- |
| API surface | `/api/v1` (the plugin's `API_VERSION`) |
| Verified against | YouTube Music **3.12.0**, bundle `com.github.th-ch.youtube-music` |
| Default endpoint | `http://127.0.0.1:26538` |
| Auth | `POST /auth/{clientId}` → JWT; `Authorization: Bearer` on REST, `?token=` on the socket |

Everything the widget depends on:

| Kind | Used |
| --- | --- |
| WebSocket | `GET /api/v1/ws` |
| Socket events | `PLAYER_INFO`, `VIDEO_CHANGED`, `PLAYER_STATE_CHANGED`, `POSITION_CHANGED`, `VOLUME_CHANGED`, `SHUFFLE_CHANGED` |
| Read | `GET /song`, `/like-state`, `/shuffle`, `/volume` |
| Write | `POST /play`, `/pause`, `/toggle-play`, `/next`, `/previous`, `/seek-to`, `/volume`, `/toggle-mute`, `/like`, `/dislike`, `/shuffle` |

If the plugin bumps to `/api/v2`, change `API` in [src/main/api.js](src/main/api.js)
and the socket path in [src/main/ws.js](src/main/ws.js). `npm run doctor` reports
the server's own OpenAPI title and version and will fail loudly on a protocol
change rather than leaving the widget silently blank.

Two quirks of the current plugin the widget works around, both worth re-checking
after an upgrade:

- **`GET /repeat-mode` always returns `{"mode": null}`**, which is why there is
  no repeat button. `/shuffle` and `/like-state` return real values, so this is
  specific to repeat.
- **The volume you POST is not the volume reported back.** The player applies an
  exponential curve — measured at `reported = 100*(15^(sent/100)-1)/14` on
  3.12.0, so POSTing 80 echoes back 55. The widget learns the curve at runtime
  from the first (sent, echoed) pair rather than hardcoding it, since it comes
  from a plugin that can be turned off. See `solveVolumeCurve` in
  [src/main/index.js](src/main/index.js).

## Assets

There are no third-party assets and nothing to attribute. The transport glyphs
are hand-authored SVG paths in the `<symbol>` sprite at the top of
[src/renderer/index.html](src/renderer/index.html); the menu-bar icons and the
app icon are computed procedurally at build time by
[scripts/make-icon.js](scripts/make-icon.js). The only binary in the repo is the
screenshot.

## Setup

```bash
cd ~/projs/pear-music-widget && npm install
```

Enable the API server in YouTube Music: open the app menu → **Plugins** →
**API Server**. The default port is `26538`.

Then:

```bash
cd ~/projs/pear-music-widget && npm start
```

On first run the widget asks the API server for an access token. If the plugin's
auth strategy is `AUTH_AT_FIRST` (the default), a dialog appears **inside
YouTube Music** asking whether to allow `PearMusicWidget` — click Allow. The
token is cached afterwards, so this happens once.

If the widget sits on a setup screen, run the connectivity check:

```bash
cd ~/projs/pear-music-widget && npm run doctor
```

## Packaging a DMG

```bash
cd ~/projs/pear-music-widget && npm run build
```

This regenerates the app icon (`scripts/make-icon.js` draws it from scratch — no
binary assets in the repo), packages an arm64 bundle, ad-hoc signs it, and
writes `dist/Pear Music Widget-<version>-arm64.dmg`.

The ad-hoc signing is not optional. With `identity: null` electron-builder skips
signing altogether, which leaves the bundle seal broken — `spctl` reports *"code
has no resources but signature indicates they must be present"* — and Apple
Silicon requires at least an ad-hoc signature. `scripts/adhoc-sign.js` runs as an
`afterPack` hook, signs the frameworks and helpers before the outer bundle, and
verifies the result.

It is still not *notarised*, so a copy that arrives with a quarantine attribute
(downloaded, AirDropped) needs right-click → Open once, or:

```bash
xattr -dr com.apple.quarantine "/Applications/Pear Music Widget.app"
```

To install:

```bash
open "dist/Pear Music Widget-1.0.0-arm64.dmg"
```

Then drag the app onto the Applications alias. Use the tray's **Open at login**
item to start it with macOS.

Both the installed app and `npm start` share one settings file and hold a single
instance lock, so launching one while the other runs will just quit the newcomer.

## Configuration

Settings live at
`~/Library/Application Support/pear-music-widget/settings.json`:

| Key | Default | Meaning |
| --- | --- | --- |
| `host` | `127.0.0.1` | API server host |
| `port` | `26538` | API server port |
| `clientId` | `PearMusicWidget` | Identity shown in the authorisation dialog |
| `token` | `null` | Cached JWT; delete it to re-authorise |
| `bounds` | `null` | Last window position and size |
| `appearance` | `normal` | `normal` or `compact`; also set by dragging past the breakpoint |
| `alwaysOnTop` | `true` | Float above other windows; toggled from the menu bar |
| `opacity` | `1` | Window opacity |

Quit the widget before editing, or your changes will be overwritten.

## How it fits together

```
YouTube Music (API Server plugin, :26538)
        │  ws://…/api/v1/ws     push: song / position / volume / repeat / shuffle
        │  http://…/api/v1/*    pull: song, like-state, shuffle, repeat, volume
        │                       push: play, pause, next, seek-to, volume, like, …
        ▼
src/main/ws.js  ──▶  src/main/index.js  ──IPC──▶  src/renderer/app.js
src/main/api.js                                   src/renderer/palette.js
```

All networking happens in the main process. Cover art is fetched there too and
handed to the renderer as a `data:` URL, which keeps the renderer's CSP closed
to the network and lets `<canvas>` read the pixels for colour extraction without
cross-origin tainting.

| File | Role |
| --- | --- |
| `src/main/index.js` | State machine, IPC handlers, app lifecycle |
| `src/main/api.js` | REST client, token handling, cover fetching |
| `src/main/ws.js` | WebSocket client with backoff and re-auth |
| `src/main/window.js` | Floating widget: creation, vibrancy, sizes, position persistence |
| `src/main/panel.js` | Menu-bar dropdown: anchoring, blur-to-close, toggle guard |
| `src/main/tray.js` | Menu-bar item and settings menu |
| `src/preload/index.js` | `window.widget` bridge |
| `src/renderer/app.js` | Rendering, playhead interpolation, seek/volume drag |
| `src/renderer/palette.js` | Accent colour extraction from artwork |
| `scripts/doctor.js` | Connectivity diagnostics |
| `scripts/make-icon.js` | Draws `build/icon.icns` from scratch |
| `scripts/adhoc-sign.js` | electron-builder `afterPack` signing hook |

The renderer is loaded twice — once plain, once with `?mode=panel` — and
`src/renderer/app.js` branches on that for the two differences that are not pure
CSS: the dropdown ignores the Normal/Compact setting, and its right-hand readout
counts down instead of showing total duration.

### Playhead interpolation

The server emits a position roughly once per second. The renderer keeps a local
clock (`position` + elapsed wall time) and re-renders on `requestAnimationFrame`,
so the bar moves smoothly and snaps back to the truth on each server frame.
During a scrub, and for 1.2s after it, incoming positions are ignored so the bar
does not jump backwards while the seek is in flight.

### Scaling instead of reflowing

The renderer is authored once at each layout's natural size. Resizing does not
reflow it — the main process sets `webContents.setZoomFactor(width / baseWidth)`
so the same layout is simply drawn larger or smaller. The zoom is derived from
whichever axis is tighter, because rounding the window height to whole pixels
can leave it a fraction under the ratio and scaling on width alone would then
clip the last row.

The one thing that must not scale is the corner radius: macOS rounds the window
in fixed device pixels, so the CSS radius is divided by the zoom to stay flush.

A frameless window resizes from its edges, but `-webkit-app-region: drag` on the
card would swallow those edges — hence the 5px no-drag ring in the markup.

### Why the volume slider holds its position

Three separate things conspired to make a released drag land somewhere arbitrary,
and all three are handled in `setVolume` / the `VOLUME_CHANGED` case:

1. Every `pointermove` used to POST, so a drag put a dozen requests in flight and
   the last echo to arrive was not necessarily the newest. Sends are now
   throttled to 70ms, and the value under the cursor is always flushed on
   release.
2. The player echoes each change back, and adopting the echo mid-drag fought the
   user. Our own value wins for 1.5s after we set it.
3. The echo is on a different scale entirely (the exponential curve above), so
   even a correctly-ordered echo moved the slider. Reported values are mapped
   back through the learned curve before they are displayed.

### Reporting "offline"

Worth knowing if you touch `src/main/ws.js`: once a token is cached,
`ensureToken()` returns without touching the network, so a dead API server
produces no error there at all. The only signal is the WebSocket's `close`
event, which is why that handler reports `offline` regardless of whether a
connection was ever established. `connecting` is only shown on the first attempt
or after a manual retry, so background reconnects do not flicker the UI.

### Tray menu freshness

macOS renders whatever menu was last handed to `Tray.setContextMenu`, so a
checkbox built at the wrong moment stays wrong. The menu is built before the
window's `ready-to-show`, which is why every source of truth it reflects pushes
a rebuild (`show`, `hide`, connection status, appearance, always-on-top), and
why the show/hide item acts on the checkbox's own new state rather than
re-reading the window.

## Notes

- `npm audit` reports vulnerabilities in `electron-builder`'s transitive
  dependencies. They are devDependencies and are not part of the running app;
  drop `electron-builder` from `package.json` if you never need the `.app`.
- The API server's `authStrategy` can be set to `NONE` in the plugin settings,
  in which case no dialog appears and any local client can control playback.
