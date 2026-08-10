# AGENTS.md

Implementation notes for contributors. The README explains the product; this
file records the contracts that are easy to break.

## Architecture

```text
YouTube Music API Server (:26538)
  WebSocket push + REST control
              ↓
src-tauri/src/ws.rs → state.rs → Tauri IPC → src/app.js
                         ↑                     src/palette.js
                    src/api.rs
```

| File | Responsibility |
| --- | --- |
| `src-tauri/src/main.rs` | Setup, lifecycle, pollers, window events, `--doctor` |
| `src-tauri/src/state.rs` | Player state, realtime messages, volume calibration |
| `src-tauri/src/api.rs` | REST, authentication, cover fetching |
| `src-tauri/src/ws.rs` | WebSocket connection, backoff, re-authentication |
| `src-tauri/src/window.rs` | Widget sizing, zoom, anchoring, persistence |
| `src-tauri/src/panel.rs` | Menu-bar dropdown and blur handling |
| `src-tauri/src/tray.rs` | Status item and menus |
| `src-tauri/src/commands.rs` | Renderer commands |
| `src-tauri/src/macos.rs` | AppKit integration |
| `src-tauri/src/search.rs` | Search and queue parsing |
| `src-tauri/src/lyrics.rs` | Lyrics lookup, matching, and LRC parsing |
| `src-tauri/src/lyrics_cache.rs` | Memory and disk lyrics cache |
| `src-tauri/src/store.rs` | `settings.json` |
| `src/bridge.js` | `window.widget` IPC wrapper and dragging |
| `src/app.js` | Rendering and interaction |
| `src/palette.js` | Cover colour extraction |

## Non-negotiable rules

1. **All network access belongs in Rust.** The renderer's CSP intentionally
   blocks it. Rust returns covers as `data:` URLs so canvas sampling is not
   cross-origin tainted. `lyrics.rs` is the only code allowed to contact a
   non-localhost host.
2. **Keep the `state` event small.** It is emitted every second and must not
   contain cover, queue, or lyrics data. Those use separate change-only events;
   `widget_state` combines them only for a cold window load.
3. **Queue artwork is request/reply data, not an event.** Each surface calls
   `queue_art` only for visible rows. Keep `ImageCache::get` promotion and the
   byte cap so scrolling does not evict the artwork it is returning to.
4. **Run AppKit and menu work on the main thread.** Take one
   `AppHandle::run_on_main_thread` hop at the command boundary so ordered
   sequences cannot interleave. `tray::refresh` takes its own hop because
   realtime tasks call it.
5. **Queue raw AppKit calls.** Tauri setters post to the event loop; immediate
   `msg_send!` calls can overtake them. `macos.rs` uses the same proxy to keep
   operations FIFO.
6. **Frontend files are embedded without a bundler.** After changing only
   `src/`, run `touch src-tauri/src/main.rs` before rebuilding or Cargo may reuse
   stale embedded assets.
7. **Renderer globals are shared.** Keep `bridge.js` inside its IIFE; duplicate
   top-level declarations across scripts can prevent both scripts from loading.

## API contract

The app targets `/api/v1`, verified with YouTube Music 3.12.0.

- WebSocket: `GET /api/v1/ws?token=…`
- Events: `PLAYER_INFO`, `VIDEO_CHANGED`, `PLAYER_STATE_CHANGED`,
  `POSITION_CHANGED`, `VOLUME_CHANGED`, `SHUFFLE_CHANGED`, `REPEAT_CHANGED`
- Reads: `/song`, `/like-state`, `/shuffle`, `/volume`, `/queue`, `/repeat-mode`
- Writes: `/toggle-play`, `/next`, `/previous`, `/seek-to`, `/volume`,
  `/toggle-mute`, `/like`, `/dislike`, `/shuffle`, `/switch-repeat`
- Search/queue: `POST /search`, `POST /queue`, `PATCH /queue`
- Auth: `POST /auth/{clientId}`; Bearer token for REST and query token for WS

Repeat is a button cycle, not a setter. `/switch-repeat` accepts an iteration
count for `NONE → ALL → ONE → NONE`; `commands::command` calculates the presses.
`/repeat-mode` may be `null` before the player bar is observed, so preserve the
difference between unknown and `NONE`. Refresh it on connection and after a
repeat command because `REPEAT_CHANGED` fires only when the value changes.

## State and connection

The WebSocket close path is the reliable offline signal: a cached token lets
`ensure_token()` succeed without touching the server. Show `connecting` only on
the first attempt or a manual retry; background reconnects should not flicker.

Retrying tears down the socket and clears the queue, so `launch_music_app`
requests a retry only when disconnected. `Realtime::request_retry` must wake
backoff and close an established socket. Pollers stand down while both surfaces
are hidden, and showing either surface calls `resync` first.

## macOS windows

- Close requests hide windows until `WindowState::begin_quit`; otherwise Tauri
  exits after the last window is destroyed.
- Both windows use `accept_first_mouse(true)` so inactive controls respond to the
  first click.
- Tauri's `always_on_top` exclusively owns the widget level. The dropdown uses
  its explicit menu level. `follow_everywhere` follows the widget setting; the
  dropdown always joins all Spaces.
- Transparent borderless windows require `macOSPrivateApi`. Use vibrancy plus a
  fixed AppKit corner radius and shadow.
- `bridge.js` begins dragging only after 3 px and outside the no-drag selector.
  Starting immediately consumes clicks and double-clicks. The dropdown only
  drags from its artwork.
- Resize getters are synchronous and can report old geometry immediately after
  a setter. Pass the requested rectangle to `apply_zoom_to`; only a real
  `Resized` event should measure the window.

## Skins, sizing, and panels

`window.rs::BASE` defines each skin's natural size. The renderer does not reflow;
it zooms by the tighter `width/base_width` ratio. Stored sizes contain only
width, and height is derived from the skin ratio. The AppKit corner radius stays
in points, so CSS divides its radius by the zoom.

When switching skins, release the aspect lock, resize, then restore the lock.
Apply min/max limits before the resize they permit. A visible panel adds its
height to the window's shape; `shape_of` must be used by every constraint.
Changing skins or resetting with a panel open must use `apply_collapsed_size`.
Never persist the temporary panel expansion (`expanded_by`).

Resizing must preserve the corner recorded when the user last positioned the
window. Do not recompute the anchor during a resize: different skin sizes will
make the window walk across the screen. `set_bounds` records programmatic moves;
only `reset_window` chooses a new anchor.

Queue, lyrics, and search share one panel slot. `window.rs::panel_height` must
match the CSS `flex-basis`; its `Some` result is also the panel-name whitelist.
Use `PANELS` for panel ownership and `CORNERS` for the four visible buttons,
because Repeat does not open a panel. Reveal rules must retain `:not([hidden])`,
and skin sections need `position: relative` so ambient layers do not cover them.

Only queue and lyrics are restored. Seed `queue_wanted_by` from stored state
before the first render. `panel::collapse` clears both wanted sets because the
renderer handles `panel-collapsed` locally and does not call `set_panel(null)`.
Queue demand comes from either a queue-owning skin or a surface with its queue
panel open.

## Lyrics

Lookup order:

1. YouTube Music timed lyrics, keyed by video ID, using the iOS Music client.
2. LRCLib exact match with full metadata.
3. LRCLib exact match with a cleaned title.
4. LRCLib search, ranked by duration.

Keep unsynced YouTube lyrics as a fallback while looking for a synced LRCLib
result. Preserve empty LRC lines because they represent instrumental gaps.

The cache holds 60 entries in memory and JSON files under
`~/Library/Caches/pear-music-widget/lyrics`. Disk hits return to memory. Evict
oldest-written files to 90% of `lyricsCacheMb`; `0` disables new caching without
deleting existing files. Hits do not expire, while misses expire after one week.
Corrupt records behave as cache misses. Keep `how` mapped to the known static
source labels.

The lyric roll depends on three rules:

- Emphasise the active line with `scale`, not font-size, to avoid reflow.
- Measure line centres once per render, not during animation frames.
- Move `.lyrics-lines` with a CSS transform transition and per-line `--roll-ms`.

Do not clamp the transform at zero; the first and last lines must both be able to
reach centre. Manual wheel scrolling pauses auto-roll for `LYRIC_MANUAL_MS`.
Only the middle 40% of a line is clickable; keep the full button as the layout
box and use its pseudo-element as the hit band. Do not treat index `"0"` as
false. Click-to-seek subtracts `lyricsOffset` so seeking matches the adjusted
display timing.

`Core` keeps both `lyrics_source` and displayed `lyrics`. Simplified Chinese is
derived on output with phrase-aware `fast2s`; turning the option off must restore
the unmodified source without another request. `publish_lyrics` reads settings
before locking lyrics and emits only when the display value changed.

## Search and queue

Search responses have no stable result path. `parse_search_results` walks the
JSON and keeps `musicResponsiveListItemRenderer` nodes with a `videoId`.

Playing a search result inserts after the current video, then polls until the
queue reflects the insertion before advancing. The insert returns `204` before
the queue changes, so calling `next()` immediately can play the wrong track.

`play_queued` searches forward and is unsuitable for queue UIs that show earlier
tracks. Those UIs send the rendered slot plus video ID to `play_queue_index`;
the command verifies the live queue and falls back to `play_queued` on mismatch.
Keep `parse_queue` and `queue_entries` as separate projections: the former drops
unnamed slots, while the latter preserves API indexes. `QueueTrack::index`
connects them. Update the renderer's current row optimistically while polling.

Stack's queue uses two rows, column flow, and three columns per view. Keep the
gutter on the section rather than the scroller. Centre from each card's
`offsetLeft`; percentage columns plus gaps make multiplication drift. Its wheel
handler must stop propagation and map vertical deltas to horizontal movement so
queue scrolling never changes volume. Do not enable scroll snapping; trackpad
deltas are too small to cross snap thresholds reliably.

## Volume

The renderer's `setVolume` and Rust's `VOLUME_CHANGED` handler jointly enforce:

1. throttle sends to 70 ms and always flush the released value;
2. prefer the local drag value for `VOLUME_ECHO` (1.5 s);
3. learn the server's optional exponential reporting curve.

The command always posts the raw slider value. Curve calibration is display-only
and pairs only with the latest send. The slider itself stays linear.

`peekVolume` updates both the speaker popover and the progress-bar readout; CSS
chooses which one a skin displays. Stack shows `.seek-vol` because it has no
speaker. Keep that readout neutral rather than accent-coloured. `.vol-pop` is
right-anchored to avoid clipping against the card edge.

## Tray and menus

Register menu handling once with `Builder::on_menu_event`; adding a tray-local
handler executes toggles twice. Rebuild the status menu on state changes and on
`TrayIconEvent::Enter`, because macOS displays the last attached menu.

Left click toggles the dropdown, right click opens settings, and a detected
double click shows the widget. macOS does not emit the documented tray
`DoubleClick`, so pair clicks using the live `NSEvent.doubleClickInterval`
clamped to 0.2–0.6 s. Do not delay the first click: the brief dropdown flash on
double-click is intentional. The second click must `dismiss` the panel rather
than toggle it, then call `show_widget` rather than the checkbox-style menu arm.

Resolve tray icon coordinates against the display that contains the icon; input
positions may be logical or physical. While a dropdown context menu is open,
`PanelState::menu_open` prevents blur from dismissing its parent. Address
YouTube Music by bundle ID when quitting it; name-based AppleScript is unreliable.

## Cover palette

`palette.js` samples each decoded cover once at 42×42, rejects low-colour pixels,
weights saturated mid-lightness colours, and selects separated hue buckets. It
then assigns fixed lightness for legibility and derives three ambient washes.
Greyscale detection must use a coloured-pixel rate; JPEG noise can otherwise
produce a false warm palette.

Use `img.decode()` before canvas drawing. Hidden WKWebViews may fire `onload`
before a bitmap is available. Also avoid relying on CSS transitions for required
state changes: the non-activating widget reports `document.hidden` and WebKit may
never advance them. Registered colour custom properties are the exception used
for palette interpolation. `PMW_THEME=dark|light` pins appearance for testing.

## Settings

Settings live at
`~/Library/Application Support/pear-music-widget/settings.json`, preserving the
legacy Electron path and unknown keys.

`corners` is keyed by skin. `corners_from_file` must continue accepting the old
flat shape; a parse failure falls back to defaults and would also lose position,
sizes, and token. The state snapshot includes the whole map because the two
surfaces can use different skins. Menu IDs follow `corner:<skin>:<button>`.

`cornersAutohide` is renderer-driven. Keep the 200 ms `wakeChrome` guard, allow
mousedown to wake faded controls before click, and run it even with a panel
open. The floating close button shares wake timing but is not in `CORNERS`, does
not consume title gutter, and is always absent from the dropdown.

## Build, test, and prototype

```bash
cargo test
cargo tauri build
cargo run -- --doctor
./scripts/reinstall-local.sh
./scripts/publish-release.sh small|minor|major
```

The release build generates icons, compiles arm64, and ad-hoc signs with
`macOS.signingIdentity: "-"`. It is not notarised. Debug builds open Web
Inspector when `PMW_DEVTOOLS` is set; check console errors first when markup
loads but state does not update.

Prototype visual variants in the real app, not standalone HTML. Append temporary
CSS/JS blocks, give each build a unique Tauri `identifier`, use a separate
scratch home and window bounds, touch `main.rs` before each build, label each
variant, and copy each binary before the next build overwrites it. Do not point a
variant at the real settings directory.
