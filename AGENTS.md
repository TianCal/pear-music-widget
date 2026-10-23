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
| `src-tauri/src/local.rs` | Local-folder scanning and playback |
| `src-tauri/src/artwork.rs` | Online covers for local files, disk cache |
| `src-tauri/src/likes.rs` | Local like/dislike, stored on the drive |
| `src-tauri/src/store.rs` | `settings.json` |
| `src/bridge.js` | `window.widget` IPC wrapper and dragging |
| `src/app.js` | Rendering and interaction |
| `src/palette.js` | Cover colour extraction |

## Non-negotiable rules

1. **All network access belongs in Rust.** The renderer's CSP intentionally
   blocks it. Rust returns covers as `data:` URLs so canvas sampling is not
   cross-origin tainted. `lyrics.rs` and `artwork.rs` are the only code
   allowed to contact a non-localhost host.
2. **Keep the `state` event small.** It is emitted every second and must not
   contain cover, queue, or lyrics data. Those use separate change-only events;
   `widget_state` combines them only for a cold window load. The renderer
   skips any channel of that reply which has already heard a live event, since
   the event is newer.
3. **Queue artwork is request/reply data, not an event.** Each surface calls
   `queue_art` only for visible rows. The only queue-art event is the
   payload-free `queue-art-ready` nudge, which makes surfaces ask again. Keep `ImageCache::get` promotion and the
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
8. **Every feature works in both sources.** YouTube Music and Local Music are
   one card, so any control, menu item, panel or setting must be designed and
   tested for both, or must say on screen why it is source-specific (Delete
   is local only). The state you see should survive a switch or a relaunch in
   both. A YouTube-only command reaching the local player silently doing
   nothing is a bug, not a gap. Example: repeat-one is a YouTube Music mode
   that the player itself keeps, so locally it has to replay at a natural end
   *and* be saved per folder (`SavedFolder.repeat`) or a relaunch quietly
   turns it off. Check `commands::local_command`, the Source-specific menus,
   and `Core::is_local` paths before calling a feature done.

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

`window.rs::BASE` defines each skin's natural size. Stack is the only skin;
Classic was removed; stored `"classic"` values fall back to Stack
through `valid_skin`. The skin plumbing stays so a new skin is a table row. The renderer does not reflow;
it zooms by the tighter `width/base_width` ratio. Stored sizes contain only
width, and height is derived from the skin ratio. The AppKit corner radius stays
in points, so CSS divides its radius by the zoom.

Apply min/max limits before the resize they permit. A visible panel adds its
height to the window's shape; `shape_of` must be used by every constraint.
Resetting with a panel open must use `apply_collapsed_size`.
Never persist the temporary panel expansion (`expanded_by`).

`set_bounds` records programmatic moves so move/resize events can be told
apart from the user's.

Queue, lyrics, and search share one panel slot. `window.rs::panel_height` must
match the CSS `flex-basis`; its `Some` result is also the panel-name whitelist.
Use `PANELS` for panel ownership and `CORNERS` for the visible buttons,
because Shuffle, Repeat and Delete do not open a panel. Shuffle lights with
the accent colour only (`lit`); the `on` disc means a panel is open. Reveal rules must retain `:not([hidden])`,
and skin sections need `position: relative` so ambient layers do not cover them.

Only queue and lyrics are restored. The widget stores `panel` and reopens it at
launch; the dropdown stores `dropdownPanel`. The dropdown keeps queue/lyrics
open while hidden (only search collapses) and clears its wanted sets; showing
it calls `panel::restore`, which re-marks them or, if nothing is open, pre-sizes
for the remembered panel and emits `panel-restore`. Remembered panels change
only while connected, so the offline teardown does not forget them. Seed
`queue_wanted_by` from stored state before the first render. `panel::collapse` clears both wanted sets because the
renderer handles `panel-collapsed` locally and does not call `set_panel(null)`.
`set_panel` from a hidden dropdown never marks data as wanted.
Queue demand comes from either a queue-owning skin or a surface with its queue
panel open.

## Lyrics

Two orders, chosen by `lyrics::fetch_lyrics` from the track id. The cache,
expiry and miss handling are shared.

**Local files** (`lookup_local`) have the real album and exact duration but
no video id:

1. LRCLib exact match with full tags and duration.
2. Kugeci, only when the title or artist contains Han characters.
3. LRCLib exact match with a cleaned title.
4. LRCLib search, rejecting hits more than 3 s from the file's duration.
5. YouTube Music: an InnerTube song search (direct, not through the API
   server, so YouTube Music need not be running), matched on artist and title
   with `artwork`'s folding and on duration within 3 s, then that video's timed
   lyrics.

Each tier continues past unsynced words, and the first plain block found is
the fallback.

**YouTube Music tracks** (`lookup_streamed`):

1. YouTube Music timed lyrics, keyed by video ID, using the iOS Music client.
2. LRCLib exact match with full metadata.
3. LRCLib exact match with a cleaned title.
4. LRCLib search, ranked by duration.
5. Kugeci title search, verified against title and artist on both the result row
   and song page. Preserve live/remix qualifiers and try original plus simplified
   Chinese titles. The site supplies LRC in `#lyricsContainer`, without duration
   or album metadata; treat failed requests or changed markup as misses.
   Pass the full artist credit to Kugeci. Match literal names first, then require
   every collaborator (including names joined with `和`) in the singer list.
   Try a trailing parenthesized subtitle without the subtitle only after the
   full title. Keep recording qualifiers such as Live, remix, and 粤语版 intact.
   Recognized official-video, lyric-video, and CCTV song/performer title formats
   can supply credits when the API artist is an uploader. Recheck extracted
   title and performers against Kugeci. Preserve version annotations, and keep
   the verified 华云龙 ↔ 华云龙KLE alias specific rather than stripping arbitrary
   artist suffixes. Segment collaborations using provider names so `和` inside
   a name (such as 楊和蘇) remains intact.

Keep unsynced lyrics as a fallback while looking for a synced result from later
sources. Preserve empty LRC lines because they represent instrumental gaps.

The cache holds 60 entries in memory and JSON files under
`~/Library/Caches/pear-music-widget/lyrics`. Disk hits return to memory. Evict
oldest-written files to 90% of `lyricsCacheMb`; `0` disables new caching without
deleting existing files. Hits do not expire, while misses expire after one week.
Corrupt records behave as cache misses. Keep `how` mapped to the known static
source labels.
Lyrics → Cache → Clear negative cache removes remembered misses from disk and
memory, preserving synced and unsynced hits. Retry a current `none` result after
clearing; keep cache expiry independent of provider/matching changes.

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

## Local folders

`local.rs` is a second source behind the same `Core` channels. While a folder
is active, `Core::is_local` makes `handle_message`, `set_status`, `refresh_all`
and the pollers stand down rather than closing the socket; leaving calls
`leave_local` and `ws::retry`. YouTube-side writes go through
`Core::update_remote`, which checks `is_local` inside the player lock;
`Local::open` sets `active` inside that same lock. Checking only before an
`.await` is not enough, because a reply already in flight lands after the
folder took over. `commands.rs` routes every renderer command to
the folder player first.

Settings keep `source` (`"local"` or YouTube), `localFolder` (the current
folder) and `localFolders` (up to eight, most recent first, each with a resume
track relative to the folder and a position). `saved_folders()` folds in an old
lone `localFolder`. The Source submenu in the tray, dropdown and widget menus
offers YouTube Music and Local Music, then a Recent Folders section. Local
Music opens the last folder whose drive is mounted. A folder whose drive is not
mounted stays listed but disabled. Menu IDs are `source:youtube`,
`source:local`, `source:folder:<path>`, `source:forget:<path>` and
`source:choose`. `Local::open` scans off-thread, then `swap_in` replaces the
session under `loading`, remembering where the old folder was and resuming the
new one paused. The resume point is saved on every track change, on pause, when
switching away, and on quit (`quit` and `RunEvent::ExitRequested`), but only
for a track that actually started and while the drive is mounted. A failed
load must not overwrite the real place. The menus read mount state from
`Local::folder_present`, which the ticker refreshes; never `stat` saved paths
on the main thread. `rate` holds `loading`.

Audio is decoded in Rust (rodio/symphonia). **Never call `Sink::clear` or
`Sink::try_seek`**: both wait for the audio callback, which a stream on a
vanished device never runs again. Each track gets a fresh `Sink` and the old one
is dropped; seeking reopens the file at the new position. The `OutputStream`
lives on its own thread, opens on first use, and is reopened when its error
callback fires or the default device changes (checked every two seconds
and on each load). Loads do file IO: run them on blocking threads or the
`local-tick` thread, never on the AppKit main thread or an async worker. `loading`
is held across every load and seek; the ticker only `try_lock`s it, so it never
sees the gap between sinks and advances twice. `close` and `toggle_play` take
it too. Advancing tries each track at most once and publishes only the track
that started (or the last failure), never each file it skipped. If the root is
no longer a directory it stops at the first failure. Position is pushed on
`state` once per whole second, like YouTube Music's.

`scan` lists the folder (cheap) and reads tags only for files whose size or
mtime differs from the index at
`~/Library/Caches/pear-music-widget/library/<hash of root>.json`. Reading
every tag on a cold USB drive takes several seconds. Track ids are
`local-<hash of path>` so lyrics cache entries and timing overrides survive
relaunches, and YouTube timed lyrics are skipped for them. Skip `._*`
AppleDouble files. Untagged files take the first folder under the root as artist
(not for files in the root itself) and the parent folder (minus a `YYYY - `
prefix) as album; album tags containing `.com` or `www.` are rip-site
watermarks and ignored.

Tracks keep raw credits. `artistNames` maps credits for display, lyrics and
cover lookups, for YouTube Music and local files alike; apply it on output
(`Song::raw_artist` keeps the original) so a rename needs no rescan.
`map_artist` tries the whole credit, then each separated credit. A rename calls
`Core::remap_artists`, which forgets the current track's lyrics miss and the
artwork misses made under the old name.

Covers: embedded or sidecar art first, then `artwork::find` — iTunes album
(TW, HK, US stores; TW credits Mandopop in Chinese), Deezer album, iTunes song,
then YouTube Music song search through the API server (square
`googleusercontent` art only). Accept a result only when artist and name match
after `fold`; exact names outrank names that match only once bracketed notes
are dropped. Look up asynchronously so playback never waits. **Cache a miss only
when every provider answered**; a timeout, 403 or 429 says nothing about whether
a cover exists. iTunes allows about 20 requests a minute: requests are spaced
3.2 s apart, and only the TW and US stores are tried. One caller looks up an
album at a time, and re-reads the cache once it has the slot. The playing track
is `Priority::Now`; queue rows and next-track prefetch are `Background`, run one
at a time, and yield their next iTunes slot while a `Now` lookup runs. Count a
`Now` lookup as active only after it holds the album slot, or the two
priorities deadlock. `queue_art` replies after 1.5 s with what is ready; lookups
still running warm the cache for the renderer's next request. The disk cache is
`~/Library/Caches/pear-music-widget/artwork`: `<key>.jpg` hits at 640 px,
`<key>.miss` misses for a week (a day if YouTube Music could not be asked, an
hour if a provider failed). `Priority::Now` carries an `Alive` check, tested
before the slot, the permit and every request, so lookups for tracks you
skipped past stop instead of queueing ahead of the current one. `queue_art`
emits `queue-art-ready` when lookups that missed its reply have landed.
Sized `data:` URLs share one byte-capped `ImageCache`, keyed by artwork key, so
an album is resized once. Search rows use the cache only, never the network.

Likes for local files live on the drive at `<root>/.pear-music/likes.json`, so
they travel with the library. If the drive refuses a write they go to
`~/Library/Application Support/pear-music-widget/likes/<folder key>.json`.
**Always read both and merge by entry `at`, newest wins**; clearing a rating
writes a dated tombstone so the merge cannot bring it back. Every save tries
the drive first and retires the fallback once the drive accepts a write. A
drive file that exists but will not parse is never overwritten. Writes are
temp file + `sync_all` + rename, and happen outside the session lock
(`Likes::set` returns a `Save`). Entries are keyed by relative path,
with a size/duration/title/artist fingerprint that re-links a renamed or moved
file. States reuse YouTube Music's `LIKE`/`DISLIKE`/`INDIFFERENT`, so the
heart, its right-click and the menus work unchanged. Pressing the current state
clears it. Advancing skips disliked tracks, but picking one from the queue still
plays it. Disliking the playing track moves on. Queue rows carry `rating` (local
only) so disliked rows render dimmed; keep `rating` in the renderers' identity
strings or a change will not redraw.

Liked Songs (`source:liked`) is a scope of Local Music, not a playlist file.
`Session.liked_only` limits `order` to liked tracks through `Session::rescope`,
which keeps the playing track pinned so switching never cuts a song off.
`Session::reachable` is the one predicate for advancing: it excludes dislikes,
and under Liked Songs anything not liked. `publish` rescopes, which drops a
pinned track once another is playing. `rate` rescopes so a new like joins at
once; an unliked track stays until it ends. Shuffle stays within the scope. The
scope is saved per folder (`SavedFolder.liked_only`) and is ignored when a
folder has no likes (opening it as Liked Songs then says so). Switching scope
never cuts a song off: `set_scope` only rescopes, and an out-of-scope playing
track finishes. `play_id` slots a search result from outside the scope in
after the current track. Trashing the last song in Liked Songs falls back to
the whole folder, and only a folder with no tracks left closes. Shuffle and
repeat are saved per folder with the resume point.

Delete is a local-only corner button (`CORNERS` entries marked `local`). It asks
through a native `NSAlert` and moves the file and its `._` shadow to the Trash
with `NSFileManager`, never a permanent delete. The confirmation captures the
track id, so an answer given after playback moved on trashes the file the
dialog named, never whatever is playing by then. The confirm callback runs on
the main thread and hands the move and the next load to a blocking thread.

Debug builds optimise the decoder and image crates (`[profile.dev.package]`
in `Cargo.toml`). Unoptimised, a cover resize takes ~270 ms and 24/96 FLAC
decodes slowly enough to risk underruns.

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

Left click toggles the dropdown, right click opens settings, a detected
double click shows and focuses the widget, and Option-click or middle click
toggles playback. Check Option (`macos::option_held`) before
`is_double_click` so a play/pause click never arms a double click. macOS does not emit the documented tray
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
legacy Electron path and unknown keys. `Store::write` serialises the latest
settings under its write lock, skips unchanged text, and writes via temp file,
`sync_all` and rename: a truncated file would load as defaults and lose the
token, sizes and saved folders.

`corners` is keyed by skin. `corners_from_file` must continue accepting the old
flat shape; a parse failure falls back to defaults and would also lose position,
sizes, and token. The state snapshot includes the whole map because the two
surfaces could use different skins. Menu IDs follow `corner:<skin>:<button>`.

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
