# AGENTS.md

Working notes for anyone — human or agent — changing this codebase. The README is
the user-facing description; this file is the parts that will bite you.

## Layout

```
YouTube Music (API Server plugin, :26538)
        │  ws://…/api/v1/ws     push: song / position / volume / shuffle
        │  http://…/api/v1/*    pull: song, like-state, shuffle, volume
        │                       push: play, pause, next, seek-to, volume, like, …
        ▼
src/main/ws.js  ──▶  src/main/index.js  ──IPC──▶  src/renderer/app.js
src/main/api.js                                   src/renderer/palette.js
```

| File | Role |
| --- | --- |
| `src/main/index.js` | State machine, IPC handlers, app lifecycle, volume curve |
| `src/main/api.js` | REST client, token handling, cover fetching |
| `src/main/ws.js` | WebSocket client with backoff and re-auth |
| `src/main/window.js` | Floating widget: sizes, zoom, position persistence |
| `src/main/panel.js` | Menu-bar dropdown: anchoring, blur-to-close, toggle guard |
| `src/main/tray.js` | Menu-bar item and settings menu |
| `src/main/tray-icons.js` | **Generated** by `scripts/make-icon.js` — do not hand-edit |
| `src/preload/index.js` | `window.widget` bridge |
| `src/renderer/app.js` | Rendering, playhead interpolation, seek/volume input |
| `src/renderer/palette.js` | Accent colour extraction |
| `scripts/doctor.js` | Connectivity diagnostics |
| `scripts/adhoc-sign.js` | electron-builder `afterPack` signing hook |

**All networking happens in the main process.** Cover art is fetched there and
handed to the renderer as a `data:` URL, which keeps the renderer's CSP closed to
the network *and* lets `<canvas>` read the pixels for colour extraction without
cross-origin tainting. Do not fetch from the renderer.

**One renderer, several presentations.** `index.html` is loaded twice — plain for
the widget, with `?mode=panel` for the dropdown. `app.js` branches on `IS_PANEL`
only to pick which skin applies. Everything else is a `body.skin-*` class.

## The API surface we depend on

| Kind | Used |
| --- | --- |
| WebSocket | `GET /api/v1/ws?token=…` |
| Socket events | `PLAYER_INFO`, `VIDEO_CHANGED`, `PLAYER_STATE_CHANGED`, `POSITION_CHANGED`, `VOLUME_CHANGED`, `SHUFFLE_CHANGED` |
| Read | `GET /song`, `/like-state`, `/shuffle`, `/volume`, `/queue` |
| Write | `POST /play`, `/pause`, `/toggle-play`, `/next`, `/previous`, `/seek-to`, `/volume`, `/toggle-mute`, `/like`, `/dislike`, `/shuffle` |
| Search | `POST /search`, `POST /queue`, `PATCH /queue` |
| Auth | `POST /auth/{clientId}` → JWT; `Authorization: Bearer` on REST, `?token=` on the socket |

If the plugin bumps to `/api/v2`: change `API` in `src/main/api.js` and the socket
path in `src/main/ws.js`. Verified against YouTube Music 3.12.0.

### Plugin quirks worked around

- **`GET /repeat-mode` always returns `{"mode": null}`.** `POST /switch-repeat`
  is accepted (204) but the state can never be read back, so there is no repeat
  button. `/shuffle` and `/like-state` return real values, so this is specific to
  repeat. Re-check after a plugin upgrade.
- **The volume you POST is not the volume reported back** — see below.

## Skins

`src/main/window.js` holds a flat `BASE` table of natural sizes per skin — there
are no variants and no automatic switching, so dragging an edge only ever scales
the skin that is showing. Each skin has its own aspect ratio, so `applySkin` must
release the lock, resize, then re-apply it, or the next drag snaps the window
back to the old shape.

The two surfaces choose independently: `skin` drives the floating widget,
`panelSkin` the dropdown. The renderer picks with `skinOf(snapshot)`, which keys
off `IS_PANEL`. `refreshUpNext` therefore has to fetch the queue when *either* is
`stack`, and the panel's natural size follows `panelSkin` rather than a constant.

`.upnext` is hidden whenever the setup screen is up — a queue from a previous
session is stale the moment YouTube Music goes away. The skin rule is written
`body.skin-stack .upnext:not([hidden])` on purpose: as a plain
`body.skin-stack .upnext` it out-specifies `.upnext[hidden]` and the hidden
attribute silently does nothing.

The renderer expresses both from **one DOM** via `body.skin-*` classes. The
trick that makes that possible: the seek bar is a flex child of `.controls`
rather than a sibling, so each skin places it with `order` alone — above the
buttons (classic, via `flex-basis: 100%` and wrapping) or below them (stack).

`.upnext` is `position: relative` for a reason: `.ambient` and `.scrim` are
absolutely positioned siblings, and an unpositioned block paints *underneath*
them, so the scrim washed the queue out until it was given a position.

Stack's "Next tracks" comes from `GET /queue`, parsed by `parseQueueUpcoming`.
It has its own `--well` backdrop because the ambient artwork layer bleeds
through the whole card — without it, queue legibility depends on whatever cover
happens to be playing.

## Search

`POST /search` returns ~270KB of raw innertube JSON with no stable path to the
results, so `parseSearchResults` collects every `musicResponsiveListItemRenderer`
in the tree and keeps the ones carrying a videoId. Songs, albums and videos all
use that renderer.

Playing a result inserts it with `INSERT_AFTER_CURRENT_VIDEO`, then **polls the
queue** until the slot after the playing track holds it, and jumps to that index.

Do not shortcut this to `next()`. The insert returns `204` *before* YouTube Music
has actually mutated its queue, so a `next()` fired straight after skips onto
whatever was already queued — you click A, an unrelated track plays, and A only
starts when you click something else. The poll accepts the slot either when it
carries our videoId or when the queue has simply grown, since the id is
occasionally re-resolved on insert.

Opening the panel grows the window by `SEARCH_PANEL_CSS` (216, matching `.search`
in the stylesheet). That growth must not be persisted as the user's preferred
size — hence the `win.searchExpanded` guard at the top of the `resize` handler.

## Volume

The single most subtle part of the codebase. Three separate problems, all in
`setVolume` in the renderer and the `VOLUME_CHANGED` case in main:

1. **Echo ordering.** Every `pointermove` used to POST, so a drag put a dozen
   requests in flight and the last echo to land was not necessarily the newest.
   Sends are throttled to 70ms and the released value is always flushed.
2. **Echo adoption mid-drag.** Our own value wins for `VOLUME_ECHO_MS` (1.5s)
   after we set it, otherwise the server fights the cursor.
3. **Scale.** The player applies an exponential curve between what you POST and
   what it reports: measured `reported = 100*(15^(sent/100)-1)/14` on 3.12.0, so
   POSTing 80 echoes back 55.

That curve comes from a plugin the user can disable, so `solveVolumeCurve`
learns `b` numerically from the first (sent, echoed) pair instead of hardcoding
it, and `reportedToSlider` maps reported values back before display.

**Calibration is display-only and can never send the wrong volume.**
`commands.volume` always POSTs the raw slider value. Calibration can fail — you
only ever set volume at the extremes (endpoints are fixed for every `b`), no echo
arrives, or the pair does not match the expected shape. The consequence is
limited to volume changed *outside* the widget (or a very late echo) showing on
the player's raw gain scale. It self-corrects on the first mid-range drag.

Calibration only runs when the echo pairs with the *latest* send
(`volumeAwaitingEcho.seq === volumeSendSeq`); mid-drag there are several echoes in
flight and none of them pairs with anything.

The slider is linear in slider units — the player's curve is what makes it
perceptually exponential. Do not add a second curve in the widget.

## Resizing

Dragging an edge scales the active skin; it never switches skins.

The renderer is authored once at each layout's natural size and **does not
reflow**. `applyZoom` sets `webContents.setZoomFactor(width / baseWidth)`. The
zoom is derived from whichever axis is tighter, because rounding the window height
to whole pixels can leave it a fraction under the ratio and scaling on width alone
would clip the last row.

The corner radius must *not* scale: macOS rounds the window in fixed device
pixels, so main sends the zoom to the renderer and the CSS radius is divided by
it to stay flush.

A frameless window resizes from its edges, but `-webkit-app-region: drag` on the
card swallows them — hence the 5px no-drag ring (`.resize-edges`) in the markup.

The breakpoint has hysteresis (compact below 392px, normal above 412px) so a slow
drag does not flap.

## Connection state

Once a token is cached, `ensureToken()` returns without touching the network, so
a dead API server produces **no error there at all**. The only signal is the
WebSocket's `close` event — which is why that handler reports `offline`
regardless of whether a connection was ever established. Getting this wrong left
the widget stuck on "Connecting…" forever.

`connecting` is shown only on the first attempt or after a manual retry, so
background reconnects do not flicker the UI.

## Windows are hidden, never closed

Electron installs a **default application menu** even though an accessory app
never shows one, and its File menu binds Cmd+W to "Close Window". Closing
destroys the window; because the dropdown still existed, `window-all-closed`
never fired and the app kept running with a destroyed widget — so the next tray
right-click threw `Object has been destroyed` from `window.isVisible()` and put
up a crash dialog.

`hideInsteadOfClose` in `src/main/window.js` intercepts `close` on both windows
and hides instead, releasing only once `before-quit` has fired. The menus are
independently hardened: `alive(win)` guards every window access, so a dead
window greys those items out rather than throwing.

## Tray menu

macOS renders whatever menu was last handed to `Tray.setContextMenu`, so a
checkbox built at the wrong moment stays wrong. The menu is now built at popup
time (`popUpContextMenu(build())`), which makes staleness structurally
impossible. The show/hide item still acts on the checkbox's own new state rather
than re-reading the window — the menu used to be built before `ready-to-show` and
inverted the action.

## Accent colour

`palette.js` draws the cover into a 42×42 canvas, discards pixels with no hue
(`l < 0.14`, `l > 0.93`, `s < 0.16`), weights the rest by `s² · (1 − |l−0.5|·1.2)`
— squaring saturation is what stops the muddy background from winning on sheer
pixel count — bins into 24 hue buckets scored with their neighbours at half
value, then keeps *only the hue* and re-lights it at fixed lightness 66 so the
accent stays legible on both light and dark glass.

## Packaging

`npm run build` regenerates the icons, packages arm64, ad-hoc signs, and writes
the DMG.

The ad-hoc signing is not optional. With `identity: null` electron-builder skips
signing altogether, leaving the bundle seal broken (`spctl`: *"code has no
resources but signature indicates they must be present"*), and Apple Silicon
requires at least an ad-hoc signature. `scripts/adhoc-sign.js` signs frameworks
and helpers before the outer bundle, then verifies.

Not notarised — a downloaded copy needs `xattr -dr com.apple.quarantine`.

## Testing

There is no test suite. Verification so far has been done by driving the running
app over CDP: launch with `--remote-debugging-port=9333 --inspect=9334`, then
`Runtime.evaluate` against the renderer for UI state and against the main process
(`require('electron')` is available in the inspector context with
`includeCommandLineAPI: true`) for window state.

`Page.captureScreenshot` on the renderer captures only the app's own window and
never the user's desktop — prefer it over `screencapture` for layout checks. It
does not include the vibrancy layer, so it cannot verify the glass background.
