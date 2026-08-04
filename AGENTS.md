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
src-tauri/src/ws.rs  ──▶  src-tauri/src/state.rs  ──IPC──▶  src/app.js
src-tauri/src/api.rs                                        src/palette.js
```

| File | Role |
| --- | --- |
| `src-tauri/src/main.rs` | Setup, window events, pollers, lifecycle, `--doctor` |
| `src-tauri/src/state.rs` | Player state machine, volume curve, realtime handlers |
| `src-tauri/src/api.rs` | REST client, token handling, cover fetching |
| `src-tauri/src/ws.rs` | WebSocket client with backoff and re-auth |
| `src-tauri/src/window.rs` | Floating widget: sizes, zoom, position persistence |
| `src-tauri/src/panel.rs` | Menu-bar dropdown: anchoring, blur-to-close, toggle guard |
| `src-tauri/src/tray.rs` | Menu-bar item, settings menu, widget right-click menu |
| `src-tauri/src/commands.rs` | Every `invoke` the renderer can make |
| `src-tauri/src/macos.rs` | The AppKit calls Tauri does not wrap |
| `src-tauri/src/search.rs` | innertube search + queue parsing |
| `src-tauri/src/lyrics.rs` | LRCLib matching and LRC parsing |
| `src-tauri/src/store.rs` | `settings.json` |
| `src-tauri/build.rs` | Generates the app icon and menu-bar icons from scratch |
| `src/bridge.js` | `window.widget` on top of Tauri IPC, plus window dragging |
| `src/app.js` | Rendering, playhead interpolation, seek/volume input |
| `src/palette.js` | Accent colour extraction |

**All networking happens in Rust.** Cover art is fetched there and handed to the
renderer as a `data:` URL, which keeps the renderer's CSP closed to the network
*and* lets `<canvas>` read the pixels for colour extraction without cross-origin
tainting. Do not fetch from the renderer.

**One renderer, several presentations.** `index.html` is loaded twice — plain for
the widget, with `?mode=panel` for the dropdown. `app.js` branches on `IS_PANEL`
only to pick which skin applies. Everything else is a `body.skin-*` class.

**There is no npm and no bundler.** `frontendDist` points straight at `src/`, and
the frontend is embedded into the binary by `tauri::generate_context!`. That last
part catches everyone: **editing anything under `src/` does nothing until you
rebuild.** A stale frontend in a running debug binary looks exactly like an IPC
failure.

## The API surface we depend on

| Kind | Used |
| --- | --- |
| WebSocket | `GET /api/v1/ws?token=…` |
| Socket events | `PLAYER_INFO`, `VIDEO_CHANGED`, `PLAYER_STATE_CHANGED`, `POSITION_CHANGED`, `VOLUME_CHANGED`, `SHUFFLE_CHANGED` |
| Read | `GET /song`, `/like-state`, `/shuffle`, `/volume`, `/queue` |
| Write | `POST /play`, `/pause`, `/toggle-play`, `/next`, `/previous`, `/seek-to`, `/volume`, `/toggle-mute`, `/like`, `/dislike`, `/shuffle` |
| Search | `POST /search`, `POST /queue`, `PATCH /queue` |
| Auth | `POST /auth/{clientId}` → JWT; `Authorization: Bearer` on REST, `?token=` on the socket |

If the plugin bumps to `/api/v2`: change `API` in `src-tauri/src/api.rs` and the
socket path in `src-tauri/src/ws.rs`. Verified against YouTube Music 3.12.0.

### Plugin quirks worked around

- **`GET /repeat-mode` always returns `{"mode": null}`.** `POST /switch-repeat`
  is accepted (204) but the state can never be read back, so there is no repeat
  button. `/shuffle` and `/like-state` return real values, so this is specific to
  repeat. Re-check after a plugin upgrade.
- **The volume you POST is not the volume reported back** — see below.

## Threads: AppKit work belongs on the main thread

The single sharpest edge in the port. Tauri delivers IPC on a worker thread and
runs `async` commands on its own runtime, so a command handler is **not** on the
main thread. Anything that touches an `NSWindow` or builds an `NSMenu` from
there is undefined behaviour, and it does not announce itself as one: the
symptom is a status item that has quietly stopped opening its menu, or a window
that ignores a resize, with no crash and nothing in the log.

Everything in `macos.rs`, every `Menu::with_items`, and every `popup_menu` must
therefore be reached through `AppHandle::run_on_main_thread`. The hop is taken at
the **command boundary**, not inside the helpers, because the sequences matter:
`apply_skin` has to release the aspect lock, resize, and re-apply the lock in
that order, and splitting those across three separate hops would interleave them
with Tauri's own queued window messages.

`tray::refresh` takes the hop itself, because it is called from the realtime task
on every status change.

Tauri's own getters (`outer_position`, `inner_size`, `scale_factor`) are safe
from either thread — they run inline when already on the main thread and post a
message otherwise.

### …and it has to be *queued*, not just on the main thread

Being on the main thread is not enough. Tauri's window **setters** (`set_size`,
`set_position`, `set_min_size`) are messages posted to the event loop, while a
raw `msg_send!` executes immediately — so a direct AppKit call always jumps
ahead of a resize requested before it. Re-applying the aspect ratio at the end
of `apply_skin` therefore landed *before* the resize it was meant to follow, and
the window settled at a size matching neither skin.

Everything in `macos.rs` is posted through `run_on_main_thread` for that reason:
same proxy, same queue, FIFO with the setters.

The mirror image of the same trap is on the reading side. `bounds_of` runs
inline, so measuring a window straight after resizing it returns the **old**
geometry. `apply_zoom` did exactly that and scaled the page for the previous
skin — a 330×284 dropdown rendering its layout at 0.4 zoom in the top corner.
Anything that has just resized a window calls `apply_zoom_to` with the rect it
asked for; only a real `Resized` event may measure.

## The renderer's globals are shared

`bridge.js` replaces the Electron preload, but a preload ran in its own context
and a `<script>` does not. A top-level `const` in `bridge.js` that also exists in
`app.js` is a `SyntaxError` that takes down **both** files before either runs —
`IS_PANEL` was declared in both, and the result was a widget that rendered its
static markup and never updated. `bridge.js` is wrapped in an IIFE for exactly
this reason; only `window.widget` is deliberately global.

## Dragging and resizing a frameless window

WKWebView has no `-webkit-app-region`, so the drag region is implemented in
`bridge.js`: a `mousedown` outside the no-drag selector arms a drag, and
`startDragging()` fires only once the pointer has travelled 3px.

The threshold is not cosmetic. `startDragging` hands the mouse to AppKit's window
drag loop, which swallows everything after it — arming on `mousedown` alone would
eat the click and the double-click, and double-clicking the card is how you bring
YouTube Music forward. Because the handoff is deferred, the *whole card* now
responds to a double click, where Electron could only manage the artwork.

Resizing needs no JavaScript at all: a borderless-but-resizable `NSWindow` still
resizes from its edges, and the window server sees those events before the
webview does. `.resize-edges` survives from the Electron build as the ring the
drag handler ignores — without it, a press near the edge would start a window
move instead of a resize.

The aspect lock is `NSWindow.contentAspectRatio` (`macos::set_aspect_ratio`).
AppKit has no call to clear it; setting `contentResizeIncrements` is the
documented way, since the two are mutually exclusive.

## Skins

`window.rs` holds a flat `BASE` table of natural sizes per skin — there are no
variants and no automatic switching, so dragging an edge only ever scales the
skin that is showing. Each skin has its own aspect ratio, so `apply_skin` must
release the lock, resize, then re-apply it, or the next drag snaps the window
back to the old shape.

The two surfaces choose independently: `skin` drives the floating widget,
`panelSkin` the dropdown. The renderer picks with `skinOf(snapshot)`, which keys
off `IS_PANEL`. `refresh_upnext` therefore has to fetch the queue when *either* is
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

Stack's "Next tracks" comes from `GET /queue`, parsed by `parse_queue_upcoming`.
It has its own `--well` backdrop because the ambient artwork layer bleeds
through the whole card — without it, queue legibility depends on whatever cover
happens to be playing.

## Panels

`search` and `lyrics` share one slot — only one can be open at a time, which is
what keeps the height arithmetic to a single addition. `panel_height` in
`window.rs` must match the `flex-basis` of `.search` and `.lyrics` in the
stylesheet; the window grows by exactly what the panel occupies, times the
current zoom.

`apply_collapsed_size` exists because changing skin or resetting while a panel is
open would otherwise shrink the window to the collapsed size *under* the open
panel, and the panel would overlap whatever is above it. Anything that resizes
the window has to go through it.

## Lyrics

`src-tauri/src/lyrics.rs` is **the only code that talks to a non-localhost host**
(LRCLib). Keep it that way: the renderer's CSP allows no network at all, so
anything fetched has to come through Rust.

Matching is three-tier, because YouTube Music titles carry soundtrack credits
and 《…》 wrappers that LRCLib will not match on: exact with everything we know,
exact on a cleaned title, then free-text search picking the hit whose duration
is closest. Empty LRC lines are kept — they are the instrumental gaps and the
roll needs them.

Misses are cached as well as hits, so reopening the panel on a track with no
lyrics does not re-query.

A wheel over the panel scrolls the lyrics, not the volume, and parks the roll
for `LYRIC_MANUAL_MS` so auto-centring does not fight the user's hand. The eased
transition is dropped while scrolling or the roll lags behind the wheel.

The roll is a `transform` on `.lyrics-lines`, not `scrollTop`: it animates on the
compositor and lands on sub-pixel offsets, which is what makes it glide. The
active index comes from the same interpolated playhead the progress bar uses, so
it stays smooth between the server's ~1/sec position pushes.

## Search

`POST /search` returns ~270KB of raw innertube JSON with no stable path to the
results, so `parse_search_results` collects every `musicResponsiveListItemRenderer`
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

Opening the panel grows the window; that growth must not be persisted as the
user's preferred size — hence the `expanded_by` guard at the top of the `Resized`
handler in `main.rs`.

## Volume

The single most subtle part of the codebase. Three separate problems, all in
`setVolume` in the renderer and the `VOLUME_CHANGED` arm of `handle_message`:

1. **Echo ordering.** Every `pointermove` used to POST, so a drag put a dozen
   requests in flight and the last echo to land was not necessarily the newest.
   Sends are throttled to 70ms and the released value is always flushed.
2. **Echo adoption mid-drag.** Our own value wins for `VOLUME_ECHO` (1.5s)
   after we set it, otherwise the server fights the cursor.
3. **Scale.** The player applies an exponential curve between what you POST and
   what it reports: measured `reported = 100*(15^(sent/100)-1)/14` on 3.12.0, so
   POSTing 80 echoes back 55.

That curve comes from a plugin the user can disable, so `solve_volume_curve`
learns `b` numerically from the first (sent, echoed) pair instead of hardcoding
it, and `reported_to_slider` maps reported values back before display. The unit
tests in `state.rs` pin both directions against the measured shape.

**Calibration is display-only and can never send the wrong volume.** The `volume`
command always POSTs the raw slider value. Calibration can fail — you only ever
set volume at the extremes (endpoints are fixed for every `b`), no echo arrives,
or the pair does not match the expected shape. The consequence is limited to
volume changed *outside* the widget (or a very late echo) showing on the player's
raw gain scale. It self-corrects on the first mid-range drag.

Calibration only runs when the echo pairs with the *latest* send
(`awaiting_echo.seq == send_seq`); mid-drag there are several echoes in flight
and none of them pairs with anything.

The slider is linear in slider units — the player's curve is what makes it
perceptually exponential. Do not add a second curve in the widget.

## Sizing

`sizes` in the store holds the width the user last left **each skin** at, so
switching back to a skin restores it. Only the width is stored — the height is
always re-derived from the skin's aspect ratio, so a stored size can never drift
off it. `reset_window` forgets the current skin's entry and returns it to the
natural size in the default corner.

The renderer is authored once at each layout's natural size and **does not
reflow**. `apply_zoom` sets `webview.set_zoom(width / base_width)`. The zoom is
derived from whichever axis is tighter, because rounding the window height to
whole pixels can leave it a fraction under the ratio and scaling on width alone
would clip the last row.

The corner radius must *not* scale: the vibrancy layer is rounded in fixed
points, so `apply_zoom` sends the zoom to the renderer and the CSS radius is
divided by it to stay flush.

The breakpoint has hysteresis (compact below 392px, normal above 412px) so a slow
drag does not flap.

## Connection state

Once a token is cached, `ensure_token()` returns without touching the network, so
a dead API server produces **no error there at all**. The only signal is the
WebSocket's close — which is why that path reports `offline` regardless of
whether a connection was ever established. Getting this wrong left the widget
stuck on "Connecting…" forever.

`connecting` is shown only on the first attempt or after a manual retry, so
background reconnects do not flicker the UI.

A manual retry (`Realtime::request_retry`) is a `Notify` pulse that both cancels
the backoff sleep and tears down an established socket, so Reconnect is immediate
whatever state the link is in.

## Windows are hidden, never closed

`CloseRequested` is intercepted on both windows and turned into a hide, released
only once `WindowState::begin_quit` has been called. Tauri exits when the last
window is destroyed, so a real close would take the app down and leave the tray
icon holding nothing.

## Glass, corners and shadow

The window is `transparent: true` + `decorations: false`, which needs
`macOSPrivateApi` in `tauri.conf.json` — without it WKWebView paints an opaque
background and the vibrancy never shows.

`window_vibrancy::apply_vibrancy` is given a corner radius, and that rounding
plus `NSWindow.hasShadow` is what replaces Electron's `roundedCorners`. The
widget uses `UnderWindowBackground`; the dropdown uses `Menu`, so it reads as
part of the menu bar it hangs from.

`macos::join_all_spaces` sets `canJoinAllSpaces | fullScreenAuxiliary` so the
widget follows you between Spaces and stays visible over a fullscreen app.

## Tray menu

**Register the menu handler exactly once.** `Builder::on_menu_event` already
receives the tray's menu *and* the widget's popup menu, so also passing one to
`TrayIconBuilder::on_menu_event` runs every item twice. That is invisible for
the idempotent items — setting a skin or an opacity twice is the same as once —
and silently cancels every toggle, which is how "Show floating widget", "Always
on top" and "Open at login" all came to do nothing at all.

macOS renders whatever menu was last handed to the status item, so a checkbox
built at the wrong moment stays wrong. The menu is rebuilt on every state change
*and* on `TrayIconEvent::Enter` — the last moment before a click can open it.

`show_menu_on_left_click(false)` is what splits the two gestures: left click
toggles the dropdown, right click opens the settings menu (tray-icon's
`menu_on_right_click` defaults to true and Tauri does not expose it).

The tray rect arrives as an untagged `Position`/`Size` that may be either scale,
so `icon_rect` resolves it twice — guess the display with the primary monitor's
scale, then convert against the display the icon is actually on. On one display
the passes agree; with the menu bar on a secondary display of a different
density, the second is what keeps the dropdown under the icon.

## Double click opens the music app

The listener is on `.art`, plus a document-level `dblclick`. Both work now that
the drag handler defers to a movement threshold — see the dragging section.

## Quitting the music app

`quit_music_app_then` addresses YouTube Music by **bundle id**, not by name.
`application "YouTube Music" is running` answers `false` even while it is
running, which silently turns the guard into a no-op — by id it is accurate.
pear-desktop and th-ch's build ship the same appId, so one id covers both.

The guard is still worth keeping: an unguarded `quit` is harmless for a closed
app, but the check keeps us from raising the Apple-event permission prompt for
no reason. `quit` is an Apple event, so the first use prompts; if it is denied
the script fails and the 4s timeout makes sure we quit ourselves anyway.

## Accent colour

**Draw with `decode()`, not `onload`.** The two mean different things: `onload`
fires once the bytes are in, `decode()` only once there is a bitmap ready to
draw. A webview that is hidden — the dropdown, most of the time — can have an
image loaded but not yet rasterised, and `drawImage` then paints nothing at all.
That is indistinguishable from greyscale artwork, so the extractor fell back to
its default pink: a track whose cover was resolved while the dropdown was shut
came up pink there and its real colour in the widget, from the same bytes.

`applyAccent` also re-runs on `visibilitychange`, which is the only thing that
can correct a surface that had nothing to sample while it was off screen.

`palette.js` draws the cover into a 42×42 canvas, discards pixels with no hue
(`l < 0.14`, `l > 0.93`, `s < 0.16`), weights the rest by `s² · (1 − |l−0.5|·1.2)`
— squaring saturation is what stops the muddy background from winning on sheer
pixel count — bins into 24 hue buckets scored with their neighbours at half
value, then keeps *only the hue* and re-lights it at fixed lightness 66 so the
accent stays legible on both light and dark glass.

## Settings

`~/Library/Application Support/pear-music-widget/settings.json` — the literal
path the Electron build used, because Electron derives it from the package name
rather than the bundle id. Keeping it means an upgrade keeps your position,
skins and cached token. Unknown keys are round-tripped rather than dropped.

## Packaging

`cargo tauri build` regenerates the icons, compiles arm64, ad-hoc signs and
writes the DMG.

The ad-hoc signing is not optional — Apple Silicon requires at least an ad-hoc
signature — and is configured as `macOS.signingIdentity: "-"`, which replaces the
`afterPack` hook the Electron build needed.

Not notarised: a downloaded copy needs `xattr -dr com.apple.quarantine`.

`build.rs` generates `icons/` and the menu-bar template images from pure maths,
so no binary assets are checked in. The tray images land in `OUT_DIR` and are
pulled in with `include_bytes!`; only the app icons are written back into the
source tree, where `.gitignore` covers them.

## Testing

`cargo test` covers the parts worth pinning without a running player: the volume
curve solver against the measured `b≈15` shape, LRC parsing and title cleaning,
and the innertube search and queue parsers.

Everything else is verified by driving the running app. The frontend has no
devtools by default; a debug build opens the Web Inspector when `PMW_DEVTOOLS` is
set in the environment. **Console errors are the first thing to check** — a
renderer that shows its static markup and never updates is almost always a script
error, not a broken IPC channel.

`pear-music-widget --doctor` is the connectivity check; run it when the widget
sits on a setup screen.
