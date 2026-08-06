# AGENTS.md

Working notes for anyone — human or agent — changing this codebase. The README is
the user-facing description; this file is the parts that will bite you.

## Layout

```
YouTube Music (API Server plugin, :26538)
        │  ws://…/api/v1/ws     push: song / position / volume / shuffle
        │  http://…/api/v1/*    pull + control
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
| `src-tauri/src/tray.rs` | Menu-bar item and the right-click menus |
| `src-tauri/src/commands.rs` | Every `invoke` the renderer can make |
| `src-tauri/src/macos.rs` | The AppKit calls Tauri does not wrap |
| `src-tauri/src/search.rs` | innertube search + queue parsing |
| `src-tauri/src/lyrics.rs` | LRCLib matching and LRC parsing |
| `src-tauri/src/store.rs` | `settings.json` |
| `src-tauri/build.rs` | Generates the app and menu-bar icons from scratch |
| `src/bridge.js` | `window.widget` on top of Tauri IPC, plus window dragging |
| `src/app.js` | Rendering, playhead interpolation, seek/volume input |
| `src/palette.js` | Accent colour extraction |

## Five rules that are easy to break

**All networking happens in Rust.** Cover art is fetched there and handed over as
a `data:` URL, which keeps the renderer's CSP closed to the network *and* lets
`<canvas>` read the pixels without cross-origin tainting.

**Nothing track-sized goes on the `state` event.** `state` carries the ~400 bytes
that change every second; `cover`, `queue` and `lyrics` are separate events sent
only when they change. Tauri's `emit` serialises the payload and then formats a
separate JS source string *per webview* to `eval`, so anything on `state` is
copied once per window per tick and parsed as JavaScript at the far end. A 187KB
base64 cover riding the position tick was the largest single cost in the app.
`widget_state` returns all four at once, for a window loading from cold.

**Queue artwork rides no event at all.** The `queue` event is text — every slot's
title, artist and duration, a few KB even for a long queue. Artwork is asked for
by video id, for the rows a surface is actually showing, and comes back in the
`queue_art` command's *reply*: a reply is formatted once, an event once per
webview, and each surface asks only for the rows it is showing. Resolving every
slot up front would also blow the cover cache, which is why `IMAGE_CACHE_MAX` is now
loose enough that `IMAGE_CACHE_BYTES` is the ceiling that binds, and why
`ImageCache::get` promotes on a hit — insertion order was fine for four rows and
evicts exactly the covers a scroll is about to come back to.

**AppKit work belongs on the main thread, and has to be queued.** See below.

**There is no npm and no bundler.** `frontendDist` points straight at `src/` and
the frontend is embedded by `tauri::generate_context!`, so **editing anything
under `src/` does nothing until you rebuild.** A stale frontend in a running
binary looks exactly like an IPC failure.

Worse, `cargo build` is not always that rebuild. The macro is expanded in
`main.rs`, so a change to `src/` alone leaves the crate looking clean and cargo
skips it — the binary is relinked with the *old* assets and nothing warns you.
`touch src-tauri/src/main.rs` first whenever only the frontend changed.

**The renderer's globals are shared.** `bridge.js` replaces the Electron preload,
but a preload had its own context and a `<script>` does not: a top-level `const`
declared in both files is a `SyntaxError` that takes down *both* before either
runs. `bridge.js` is wrapped in an IIFE for that reason; only `window.widget` is
deliberately global.

## The API surface we depend on

| Kind | Used |
| --- | --- |
| WebSocket | `GET /api/v1/ws?token=…` |
| Socket events | `PLAYER_INFO`, `VIDEO_CHANGED`, `PLAYER_STATE_CHANGED`, `POSITION_CHANGED`, `VOLUME_CHANGED`, `SHUFFLE_CHANGED`, `REPEAT_CHANGED` |
| Read | `GET /song`, `/like-state`, `/shuffle`, `/volume`, `/queue`, `/repeat-mode` |
| Write | `POST /toggle-play`, `/next`, `/previous`, `/seek-to`, `/volume`, `/toggle-mute`, `/like`, `/dislike`, `/shuffle`, `/switch-repeat` |
| Search | `POST /search`, `POST /queue`, `PATCH /queue` |
| Auth | `POST /auth/{clientId}` → JWT; `Bearer` on REST, `?token=` on the socket |

Verified against YouTube Music 3.12.0. For `/api/v2`, change `API` in `api.rs`
and the socket path in `ws.rs`.

Two quirks worked around.

**Repeat is a button press, not a mode you can set.** `POST /switch-repeat` takes
`{"iteration": n}` and clicks the player's own repeat control that many times,
cycling NONE → ALL → ONE → NONE, so a target mode has to be expressed as a press
count — `commands::command` is where that arithmetic lives, and it is why the
widget offers only ALL and ONE (ONE → ALL is two presses; a NONE the widget could
also land on would make every press look like it did nothing every third time).
`GET /repeat-mode` is nullable and answers `null` before the player bar has been
observed once, which is where the old "always returns null, so no repeat button"
note came from — verified against 3.12.0 it answers truthfully with a track
loaded. `None` is kept distinct from `NONE` all the way to the renderer for that
reason: an unknown mode draws an unlit loop rather than claiming repeat is off.
`REPEAT_CHANGED` only fires on a *change*, so `refresh_all` pulls the mode once
per connection, and the command reads it back after its own press.

**The volume you POST is not the volume reported back** — see Volume.

## Threads

Tauri delivers IPC on a worker thread and runs `async` commands on its own
runtime, so a command handler is **not** on the main thread. Touching an
`NSWindow` or building an `NSMenu` from there is undefined behaviour that does
not announce itself: the symptom is a status item that has quietly stopped
opening, or a window that ignores a resize, with no crash and nothing logged.

Everything in `macos.rs`, every `Menu::with_items` and every `popup_menu` goes
through `AppHandle::run_on_main_thread`. The hop is taken at the **command
boundary**, not inside the helpers, because the sequences matter — `apply_skin`
must release the aspect lock, resize and re-apply it in that order, and three
separate hops would interleave with Tauri's own queued window messages.
`tray::refresh` takes the hop itself, since the realtime task calls it.

**Being on the main thread is not enough.** Tauri's window setters are messages
posted to the event loop, while a raw `msg_send!` executes immediately and jumps
ahead of a resize requested before it. Everything in `macos.rs` is posted through
the same proxy so it stays FIFO with the setters.

The mirror image is on the reading side: getters (`outer_position`, `inner_size`,
`scale_factor`) run inline, so measuring a window straight after resizing it
returns the **old** geometry. Anything that has just resized calls `apply_zoom_to`
with the rect it asked for; only a real `Resized` event may measure.

## Windows

**Hidden, never closed.** `CloseRequested` is turned into a hide until
`WindowState::begin_quit` runs — Tauri exits when the last window is destroyed.

**A widget has to work unfocused.** Clicking an inactive macOS window activates it
and swallows the click, which is why a "Next tracks" row once needed clicking
twice. Both windows use `accept_first_mouse(true)`.

**One writer per window level.** `dress()` forcing `LEVEL_FLOATING` silently
overrode `always_on_top(false)`, so the widget came back floating after every
launch. Tauri's `always_on_top` owns the widget's level; `dress()` passes `None`.
The dropdown still needs an explicit level, since Tauri cannot express the
pop-up-menu level a menu-bar dropdown sits at.

Level is only half of "always on top": *collection behaviour* decides which
Spaces a window appears on, and `CAN_JOIN_ALL_SPACES | FULL_SCREEN_AUXILIARY` is
what draws it over a fullscreen app. `macos::follow_everywhere` follows the
setting rather than being set unconditionally. The dropdown always follows.

**Glass.** `transparent: true` + `decorations: false` needs `macOSPrivateApi`, or
WKWebView paints an opaque background and the vibrancy never shows.
`apply_vibrancy` takes the corner radius, and that plus `NSWindow.hasShadow`
replaces Electron's `roundedCorners`. Widget: `UnderWindowBackground`. Dropdown:
`Menu`, so it reads as part of the menu bar.

**Dragging.** WKWebView has no `-webkit-app-region`, so `bridge.js` arms on
`mousedown` outside the no-drag selector and calls `startDragging()` only after
3px of travel. The threshold is load-bearing: `startDragging` hands the mouse to
AppKit's drag loop, which swallows everything after it — arming immediately would
eat the double-click that raises YouTube Music. Resizing needs no JavaScript; a
borderless-but-resizable `NSWindow` resizes from its edges, and `.resize-edges`
is the ring the drag handler ignores. The aspect lock is
`NSWindow.contentAspectRatio`; AppKit has no call to clear it, so
`contentResizeIncrements` is the documented way.

## Skins and sizing

`window.rs` holds a flat `BASE` table of natural sizes per skin. Each skin has
its own aspect ratio, so `apply_skin` releases the lock, resizes, then re-applies
it. The two surfaces choose independently — `skin` for the widget, `panelSkin`
for the dropdown.

Whether the queue is fetched is two questions, not one: `skin_shows_queue` says
whether a skin draws it as part of its layout (Stack does, under the transport),
and `queue_wanted_by` tracks the surfaces with a queue panel open, exactly as
`lyrics_wanted_by` does. Either, on either surface, is enough. Declaring the
appetite beside `BASE` is what stops a third skin having to be remembered in
`refresh_queue` as well.

A skin's natural height must actually fit its content: the renderer does not
reflow, so too small a `BASE` silently overlaps rather than scrolling.

The renderer is authored once at each layout's natural size and **does not
reflow**: `apply_zoom` sets `webview.set_zoom(width / base_width)`, taking
whichever axis is tighter. The corner radius must *not* scale — the vibrancy
layer is rounded in fixed points — so the zoom is sent to the renderer and the
CSS radius divided by it. `sizes` stores only the width per skin; the height is
re-derived, so a stored size can never drift off the ratio.

**An open panel is a shape, not an exception.** A panel scales with the page, so
an expanded window has a constant aspect ratio — the skin's, plus the panel's
height. `shape_of` returns it and every constraint takes it, so expanding widens
the aspect lock instead of dropping it. **Limits must be applied before the
resize they allow** (`setFrame:` clamps to min/max) and the aspect ratio after.
`apply_collapsed_size` exists so changing skin or resetting with a panel open
does not shrink the window *under* it.

**The anchor is observed, never re-derived.** The skins differ by up to 174
points, so a resize moves one edge and `resize_keeping_corner` holds whichever corner was
nearest a screen corner *when the user last placed the window*. Deciding it at
resize time makes the swap irreversible and walks the widget up the screen.
`set_bounds` records what it asked for so the persist task can tell our own move
events from the user's. `reset_window` is the one place that *sets* an anchor.

Two CSS details that look arbitrary, and both apply to every skin-owned section
— `.upnext` and the panels alike: they are `position: relative`
because `.ambient` and `.scrim` are absolutely positioned siblings that would
otherwise paint over them (the symptom is text that looks washed out rather than
missing); and the reveal rule is `body.skin-X .thing:not([hidden])` because the
plain form out-specifies `.thing[hidden]` and silently defeats it.

## Panels

`queue`, `lyrics` and `search` share one slot, which keeps the height arithmetic
to a single addition. `panel_height` in `window.rs` must match the `flex-basis`
of `.queue`, `.lyrics` and `.search` in the stylesheet, and returning `Some(_)`
from it is what makes a name a panel at all — every caller filters through it.

The renderer drives all three from the `PANELS` table in `app.js`. Opening,
closing and the dropdown's blur teardown each used to enumerate every section and
every corner button by hand; with three panels that is three lists to keep in
step, and the teardown is where they drift.

**The corner bar is not the panel list.** Repeat is a fourth button that opens
nothing, so `CORNERS` — the bar in DOM order — is what decides which buttons are
drawn and how wide a gutter the titles reserve, while `PANELS` stays the three
that own the slot. `--corner-count` counts the former.

The widget's open panel is stored as `panel` and restored in `window::create`,
before the window is ever on screen — restoring it from the renderer instead puts
a visible grow into every launch. `panel_is_restorable` decides what is worth
storing: the lyrics and the queue are modes you left open, a search is a query
you have finished with, and the dropdown collapses on blur by design. Because the
window is restored before the renderer says anything, `Core::new` seeds
`queue_wanted_by` from the same store entry — otherwise a widget coming up with
the queue panel open paints an empty list and fills it a round trip later.

`panel::collapse` clears both wanted-sets itself. The renderer tears its panel
down locally when it gets `panel-collapsed` and never calls `set_panel(null)`, so
nothing else would: the dropdown would go on fetching lyrics — and a `/queue` GET
per track — for the rest of the process, for a panel dismissed once.

Growing for a panel must never be persisted as the user's preferred size — hence
the `expanded_by` guard.

## Lyrics

`lyrics.rs` is **the only code that talks to a non-localhost host**. Keep it that
way: the renderer's CSP allows no network at all.

Matching is four-tier. The first three ask LRCLib, which needs help because
YouTube Music titles carry soundtrack credits and 《…》 wrappers it will not match
on: exact with everything known, exact on a cleaned title, then free-text search
picking the closest duration. The fourth asks YouTube Music for its own timed
lyrics, covering what LRCLib has never heard of — smaller labels and much of the
Mandarin and Cantonese catalogue. That is `/next` to name the lyrics tab and
`/browse` to read it, and only the iOS Music client (`clientName: "26"`) is
served timings. Being keyed by video id, it can never return another song's
words. Empty LRC lines are kept — they are the instrumental gaps. Misses are
cached as well as hits.

The roll is a `transform` on `.lyrics-lines`, not `scrollTop`, and the active
index comes from the same interpolated playhead the progress bar uses. Three
things keep it from stuttering, all load-bearing:

- **The active line is emphasised with `scale`, never a larger font.** Growing
  the type reflows every line below it, moving the offsets the roll is already
  travelling toward. That was the stutter.
- **Geometry is measured once per render** into `lyricCentres`. Reading
  `offsetTop` inside the animation frame forces a layout flush per line.
- **The glide is a CSS transition**, with `--roll-ms` written per line. Driving
  the transform frame by frame from the rAF tick measured *worse* — it burns
  main-thread JS for what the compositor was already doing free.

The roll travels between "first line centred" and "last line centred", both ends
allowed to be negative; clamping the low end at 0 is what put the opening lines
inside the top fade. `LYRIC_RAISE` lifts the sung line slightly above centre. A
wheel over the panel scrolls the lyrics rather than the volume and parks the roll
for `LYRIC_MANUAL_MS`, with the glide duration set to zero so it tracks the hand.

**Only the middle 40% of a line is clickable**, as a `::after` band on the button
rather than a narrower button: the button is the line's layout box, so shrinking
it would re-wrap the words and move the offsets the roll is gliding toward. The
line itself is `pointer-events: none` and the band `auto`; a pseudo-element is
never an event target of its own, so `event.target` is still the line. Do not
guard the handler on `row.dataset.index` being truthy — the first line's is the
string `"0"`, which made the opening line unclickable.

**Simplified Chinese is applied on the way out, and the fetched words are kept.**
`Core` holds `lyrics_source` beside `lyrics`: the conversion is one-way, so
turning the setting *off* has to re-derive from what was fetched rather than
from what is on screen — and doing that without another round trip to LRCLib is
the whole point of keeping it. `set_lyrics` stores the source and publishes;
`restyle_lyrics` republishes from the source, which is what makes the toggle
reach the panel that is already open. `publish_lyrics` reads the store *before*
taking the lyrics lock, and still diffs before emitting: a track whose words are
already simplified converts to itself, and must not re-emit to every window on
every fetch.

Conversion is `fast2s`, and it being phrase-aware rather than a character table
is the point — 乾 is 干 in 乾淨 and stays 乾 in 乾坤, which a per-character
mapping gets wrong in the same line. Every line is converted rather than the
track being sniffed first: text that is already simplified, or not Chinese,
converts to itself.

`lyricsOffset` (seconds, positive turning the lines over earlier) nudges the roll
when the timings and the player's clock disagree. It follows `tint` exactly:
stored in `settings.json`, mirrored into `PlayerState` because the roll runs in
the renderer, and set from a **Lyrics timing** submenu in all three menus. It is
deliberately global rather than per-track. Click-to-seek subtracts it, so a
nudged roll does not seek you to a point that then lights up a different line.

## Search and the queue

`POST /search` returns ~270KB of raw innertube JSON with no stable path to the
results, so `parse_search_results` collects every
`musicResponsiveListItemRenderer` in the tree and keeps those carrying a videoId.

Playing a result inserts it with `INSERT_AFTER_CURRENT_VIDEO`, then **polls the
queue** until the slot after the playing track holds it. Do not shortcut this to
`next()`: the insert returns `204` *before* the queue has actually changed, so a
`next()` fired straight after skips onto whatever was already queued. The poll
also accepts a slot when the queue has simply grown, since the id is sometimes
re-resolved on insert.

`play_queued` searches *forward* from the playing track and jumps to it. Queueing
it instead inserts a *second copy* and jumps to that, leaving the list looking
identical with the track still in it. A jump refreshes the queue itself, because
the queue can move without the track moving, and waits for the target slot to go
`selected` first.

**Forward-only is why neither queue surface can use it.** Both the panel and
Stack's strip reach behind the playing track, and the forward search can never
get there — it would fall through to `play_result` and queue the duplicate
`play_queued` exists to avoid. So they send the slot index they drew, via
`play_queue_index`, with the video id alongside it to verify against the live
queue; a mismatch falls back to `play_queued`. `parse_queue` and `queue_entries`
are two projections over one walk for the same reason: the first drops slots that
name no track, so its positions are its own, while the second keeps every slot
because *its* positions are what `set_queue_index` takes. `QueueTrack::index` is
the bridge between them.

The jump then polls for up to 2.4s before the queue is re-read. With four
upcoming rows that was invisible; with the whole list on screen the wrong row
wearing the playing mark for that long is not, so the renderer moves `current`
optimistically and lets the push correct it — the same bargain the transport
already makes for `togglePlay`.

Stack's strip is two grid rows with `grid-auto-flow: column`, three columns to a
view, scrolled sideways. Column flow is load-bearing: with row flow, scrolling
right would step two tracks at a time through a list ordered left-to-right. It
centres on the playing track by reading a card's `offsetLeft` rather than
multiplying a column width, because the column is a percentage of the content
box less the gap and guessing at that drifts further with every column.

**The gutter belongs to the section, not the scroller.** Inside the scroller it
is part of the scrollable content, so the columns tile from 14px in while the
viewport stays 302 wide; the column pitch and the viewport never divide and every
scroll leaves the edges mid-card. On the section, three cards and two gaps come
to exactly 302 and the strip is flush at rest and after every scroll.

Three columns is a trade, not a free win: the title column drops from 101px at
two-up to 48px, so titles ellipsize hard. Six tracks visible instead of four is
what buys it.

**The strip claims the wheel.** It only overflows sideways, so a plain wheel
would do nothing to it and everything to the volume — the card's handler owns
the wheel. Its own listener calls `stopPropagation`, which is what makes
scrolling the queue never change the sound, and maps a vertical wheel onto the
axis that moves.

**And it does not snap.** Driving it means assigning `scrollLeft`, and
`scroll-snap-type` reverts any assignment smaller than its threshold. A coarse
mouse wheel clears that in one tick and looks fine; a trackpad arrives as a
stream of small deltas and moves the strip not at all. Snapping and a
hand-driven scroll do not mix.

## Volume

The most subtle part of the codebase. Three problems, all in `setVolume` in the
renderer and the `VOLUME_CHANGED` arm of `handle_message`:

1. **Echo ordering.** A drag used to put a dozen POSTs in flight and the last
   echo to land was not the newest. Sends are throttled to 70ms, and the
   released value is always flushed.
2. **Echo adoption mid-drag.** Our own value wins for `VOLUME_ECHO` (1.5s) after
   we set it, or the server fights the cursor.
3. **Scale.** The player applies an exponential curve between what you POST and
   what it reports — measured `reported = 100*(15^(sent/100)-1)/14` — so POSTing
   80 echoes back 55.

That curve comes from a plugin the user can disable, so `solve_volume_curve`
learns `b` numerically from the first (sent, echoed) pair rather than hardcoding
it. **Calibration is display-only and can never send the wrong volume**: the
command always POSTs the raw slider value. It only runs when the echo pairs with
the *latest* send, and self-corrects on the first mid-range drag. The slider is
linear in slider units — the player's curve is what makes it perceptually
exponential, so do not add a second one.

**Showing a change is per skin, and the stylesheet decides.** `peekVolume` does
both things unconditionally — peeks the popover *and* writes the readout onto the
seek bar — because a skin that hides the speaker (Stack does) had nowhere to show
a wheel change at all, while a skin that shows it would say the same thing twice.
Which one you see is `body.skin-stack .seek-vol { display: flex }`, so a new skin
declares its answer in CSS rather than in another `if` in the renderer. The seek
readout is deliberately **not** in the accent: the accent is the playhead, and
being read as position is the whole risk of drawing volume on that bar.

`.vol-pop` is anchored by its right edge rather than centred on the speaker. The
speaker is the last control in the row, so a centred popover overhangs the card
and `overflow: hidden` — which is what keeps the corner radius — clips it. The
symptom was a slider sawn in half on Classic.

## Connection state

Once a token is cached, `ensure_token()` never touches the network, so a dead API
server produces **no error there at all**. The WebSocket's close is the only
signal — which is why that path reports `offline` regardless of whether a
connection was ever established. `connecting` is shown only on the first attempt
or after a manual retry, so background reconnects do not flicker the UI.

**A retry is not free**: it tears the socket down and comes back through
`offline`, which clears the queue. `launch_music_app` only retries when the link
is not already up. `Realtime::request_retry` is a `Notify` pulse that both
cancels the backoff sleep and tears down an established socket.

The two pollers (play state, like state) stand down while neither surface is
visible, and showing either one calls `resync` first.

## Tray menu

**Register the menu handler exactly once.** `Builder::on_menu_event` already
receives the tray's menu *and* the widget's popup menu, so also passing one to
`TrayIconBuilder::on_menu_event` runs every item twice — invisible for idempotent
items, and silently cancelling every toggle.

macOS renders whatever menu was last handed to the status item, so the menu is
rebuilt on every state change *and* on `TrayIconEvent::Enter`, the last moment
before a click can open it. `show_menu_on_left_click(false)` splits the gestures:
left toggles the dropdown, right opens settings.

The tray rect arrives as an untagged `Position`/`Size` that may be either scale,
so `icon_rect` resolves it twice — guess with the primary monitor's scale, then
convert against the display the icon is actually on.

Giving the dropdown a right-click menu needs `PanelState::menu_open`: a menu
takes focus, and losing focus is what closes the dropdown, so it would otherwise
dismiss the window it was opened from.

`quit_music_app_then` addresses YouTube Music by **bundle id**, not by name —
`application "YouTube Music" is running` answers `false` even while it is
running. pear-desktop and th-ch's build ship the same appId.

## Colour from the cover

`palette.js` returns an accent plus three washes, and `.ambient` paints them as
soft pools over a base tint. The washes are **generated**, not the cover blurred:
blurring drags every dark and desaturated region into the mix, and a bright
sleeve came out brown.

It draws the cover into a 42×42 canvas, discards pixels with no hue, weights the
rest by `s² · (1 − |l−0.5|·1.2)` — squaring saturation is what stops a muddy
background winning on pixel count — bins into 24 hue buckets scored with their
neighbours at half value, then keeps *only the hue* and re-lights it at fixed
lightness so the accent stays legible on both light and dark glass. Three hues,
not one: the winner's bucket and its neighbours are zeroed before the next is
taken, and a runner-up under 12% (then 8%) of the winner is treated as absent so
a one-colour cover gets an analogous spread rather than an unrelated stripe.

**"Is this greyscale?" has to be a rate, not a total.** JPEG chroma noise leaves
a scattering of coloured pixels in any black-and-white photograph, and those
artefacts skew warm, so a total is passed by noise alone — a monochrome sleeve
picked an orange hue out of 41 pixels in 1,764 and washed the card red. Measured
over real covers, monochrome scores 0.0024 per pixel and the least colourful
cover with a genuine hue scores 0.0193; `MIN_COLOUR_RATE` sits between them.

**Transitions do not run in this webview.** `document.hidden` is `true` for the
widget — it is a non-activating panel that never becomes key — and WebKit does
not advance CSS transitions in a hidden document. A transitioned property stays
pinned at its start value for ever, which reads as "the rule is not applying" and
sends you hunting through specificity. It is the same family of problem as the
`decode()` note below. Anything that must actually change state — the corner
buttons' fade is the one that caught this — sets the property with no transition
on it. `renderProgress` and the lyric roll are unaffected: they are driven by
`transform`, written per frame from the rAF tick, not left to the compositor.

**Draw with `decode()`, not `onload`.** `onload` fires once the bytes are in;
`decode()` only once there is a bitmap. A hidden webview can have an image loaded
but not rasterised, and `drawImage` then paints nothing — indistinguishable from
greyscale artwork, so a cover resolved while the dropdown was shut came up pink
there and its real colour in the widget, from the same bytes.

**Each cover is sampled once.** The result is cached and re-mixed for tint,
appearance and visibility changes, which all want the same hues arranged
differently. Custom properties are registered with `@property` so a track change
eases between palettes; unregistered ones are unparsed strings and cannot
interpolate. `PMW_THEME=dark|light` pins the scheme for checking both.

## Settings

`~/Library/Application Support/pear-music-widget/settings.json` — the literal
path the Electron build used, so an upgrade keeps your position, skins and cached
token. Unknown keys are round-tripped rather than dropped.

`corners` is **keyed by skin** — `{ "classic": {…}, "stack": {…} }` — because how
much chrome fits above the titles is a property of the card. Files written before
1.5 carry one flat set for every skin; `corners_from_file` reads that shape into
each skin, and it has to keep doing so: `Store::load` swallows a parse error and
falls back to the defaults, which would silently take the window position, the
per-skin sizes and the cached token with it. A skin absent from the map has never
been changed and is at its defaults, so a fresh install writes nothing. The state
snapshot carries the **whole map** rather than one skin's set, since the widget
and the dropdown can be on different skins and share one snapshot — `cornersOf`
in `app.js` is where each surface picks its own. Menu item ids are
`corner:<skin>:<button>` for the same reason.

`cornersAutohide` fades the corner buttons after that many seconds of stillness,
0 to keep them up. Driven in the renderer off `mousemove`, which fires far faster
than the timer needs re-arming — hence the 200ms guard in `wakeCorners`. It runs
with a panel open as well as without. A faded button cannot be clicked, but
`mousedown` is one of the wake signals, so a press on the card brings the bar
back before the click lands and the panel's own button is never unreachable —
which is what makes the old "never while a panel is open" guard unnecessary.

## Prototyping a look

**Build the variants and launch them at once. Do not write a standalone HTML
page.** This overrides the generic advice in the `prototype` skill, which asks
for one shareable file or a throwaway route — neither shape exists here, and a
page mocked up beside the real card is judged against a card that is not the one
running: the vibrancy, the cover's tint, the real titles at the real width and a
real queue underneath are most of what there is to judge. `docs/queue-prototype.html`
is the older shape, kept for its record rather than as a pattern to copy.

Each variant is the committed app plus an appended block, built to its own
binary, and they all run side by side so a decision is a glance rather than a
memory of the last one. What makes that work:

- **Append, never edit.** Each variant is a CSS block and a JS block
  concatenated onto `src/styles.css` and `src/app.js`, so `git checkout -- src`
  is the whole cleanup and no variant can leave a fragment behind. Appended
  script sees `el`, `state` and the rest — it is the same classic script, not a
  module — and a listener added there runs *after* the app's own, which is
  usually what you want: for the volume prototypes the card's wheel handler had
  already moved `state.volume`, so each variant only had to draw it.
- **A different `identifier` per build**, patched into `tauri.conf.json`. The
  single-instance plugin keys on it, so without that only the first one launched
  survives — the rest hand off and exit.
- **A different `HOME` per instance.** Settings are read from
  `$HOME/Library/Application Support/…`, so copy the real `settings.json` into a
  scratch home per variant — the cached token comes with it, which is what saves
  re-authorising four clients — and give each one its own `bounds` so they land
  in a row instead of on top of each other.
- **`touch src-tauri/src/main.rs` before every build**, or `generate_context!`
  is not re-expanded and the binary ships the previous variant's frontend.
- **Label each card**, with a tag element in a corner. Four unlabelled copies of
  the same widget is a memory test.

Copy each binary out of `target/debug/` as you go — the next build overwrites it.
Expect one tray icon per instance while they run.

## Packaging and testing

`cargo tauri build` regenerates the icons, compiles arm64, ad-hoc signs and
writes the DMG. The ad-hoc signature is not optional on Apple Silicon and is
configured as `macOS.signingIdentity: "-"`. Not notarised: a downloaded copy
needs `xattr -dr com.apple.quarantine`. `build.rs` generates the icons from pure
maths, so no binary assets are checked in.

`cargo test` covers what can be pinned without a running player: the volume curve
solver, LRC parsing and title cleaning, the search and queue parsers, and the
artwork-size rewriting. Everything else is verified by driving the app. A debug
build opens the Web Inspector when `PMW_DEVTOOLS` is set — **console errors are
the first thing to check**, since a renderer showing static markup and never
updating is almost always a script error rather than a broken IPC channel.
`pear-music-widget --doctor` is the connectivity check.
