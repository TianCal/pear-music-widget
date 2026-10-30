# AGENTS.md

Implementation notes for contributors. The README explains the product; this
file records the contracts that are easy to break.

## Architecture

```text
YouTube Music API Server (:26538)
  WebSocket push + REST control
              ↓
src-tauri/src/ws.rs → state/ → Tauri IPC → src/app.js
                         ↑                     src/palette.js
                    src/api.rs
```

| File | Responsibility |
| --- | --- |
| `src-tauri/src/main.rs` | Setup, lifecycle, pollers, window events, `--doctor` |
| `src-tauri/src/state/` | Player state, realtime messages, volume calibration (`mod.rs`); the queue channel (`queue.rs`); the lyrics channel and `LyricsView` (`lyrics.rs`) |
| `src-tauri/src/api.rs` | REST, authentication, cover fetching |
| `src-tauri/src/ws.rs` | WebSocket connection, backoff, re-authentication |
| `src-tauri/src/window.rs` | Widget sizing, zoom, anchoring, persistence |
| `src-tauri/src/panel.rs` | Menu-bar dropdown and blur handling |
| `src-tauri/src/strip.rs` | Now playing strip at the top centre of the menu bar |
| `src-tauri/src/tray.rs` | Status item and menus |
| `src-tauri/src/commands.rs` | Renderer commands |
| `src-tauri/src/macos.rs` | AppKit integration |
| `src-tauri/src/search.rs` | Search, queue and artist page parsing |
| `src-tauri/src/artist.rs` | Artist popup pages: fetch and cache |
| `src-tauri/src/lyrics/` | Lookup orders, caching and LRC parsing (`mod.rs`); one provider per file: `lrclib.rs`, `ytmusic.rs`, `kugeci.rs` |
| `src-tauri/src/lyrics_cache.rs` | Memory and disk lyrics cache |
| `src-tauri/src/local/` | Local-folder playback (`mod.rs`); folder scan, tags and index (`library.rs`); covers for local tracks (`art.rs`) |
| `src-tauri/src/audio.rs` | Output streams and decoders shared by both local players |
| `src-tauri/src/fsutil.rs` | Atomic file writes, the stable on-disk hash, `~/Library` paths, clocks |
| `src-tauri/src/text.rs` | Folding, bracket stripping, credit splitting, Han detection, URL encoding |
| `src-tauri/src/artwork.rs` | Online covers for local files, disk cache |
| `src-tauri/src/likes.rs` | Local like/dislike, stored on the drive |
| `src-tauri/src/store.rs` | `settings.json` |
| `src-tauri/src/sorter/` | Sort Songs window: chorus finding, clip player, ratings |
| `src/bridge.js` | `window.widget` IPC wrapper and dragging |
| `src/app.js` | Shared state, card rendering, commands (loads first) |
| `src/queue.js` | Next tracks strip, queue panel, their artwork |
| `src/lyrics.js` | Lyrics panel and roll |
| `src/overlays.js` | Playlist picker and artist popup |
| `src/panels.js` | Panel slot, search, card gestures |
| `src/input.js` | Seek, volume, wheel and keys |
| `src/boot.js` | Rust event wiring and first state (loads last) |
| `src/palette.js` | Cover colour extraction |
| `src/shared/format.js` | Clock reading and clamp, shared by the three pages |
| `src/shared/artcache.js` | `artCache`: the bounded artwork LRU of the card and the strip |
| `src/shared/tint.js` | `palettePainter`: writes a cover's palette onto a style |
| `src/shared/volume.js` | `volumeControl`: throttle, final send, echo hold, wheel carry |
| `src/shared/lists.js` | `playlistList` and `artistPages`, drawn with each page's own markup |
| `scripts/*.test.cjs` | Renderer tests (`node --test 'scripts/*.test.cjs'`) |
| `src/sorter/` | Sort Songs page (its own HTML, CSS and JS) |
| `src/strip/` | Menu-bar strip page (its own HTML, CSS and JS) |

## Non-negotiable rules

1. **All network access belongs in Rust.** The renderer's CSP intentionally
   blocks it. Rust returns covers as `data:` URLs so canvas sampling is not
   cross-origin tainted. `lyrics/` and `artwork.rs` are the only code
   allowed to contact a non-localhost host.
2. **Keep the `state` event small.** It is emitted every second and must not
   contain cover, queue, or lyrics data. Those use separate change-only events;
   `widget_state` combines them only for a cold window load. The renderer
   skips any channel of that reply which has already heard a live event, since
   the event is newer. Pages listen through `getCurrentWindow().listen`, never
   the global `event.listen`: a global listener hears every `emit_to` whatever
   its target, so hidden surfaces got the `state` meant for the one on screen
   and redrew their playhead every second. Each surface also hears `state`
   under its own name, `state:<label>` (`state::state_event`): Tauri runs an
   event's script in every page that listens for that name, even when
   `emit_to` names another page, so hidden pages woke every second to run
   it for nothing. Ticks of steady play (`steady_tick`: playing, only the
   position moved, forward by at most 2 s) reach a `slow_ticks` surface (the
   folded strip) only every other second: each push makes WebKit redraw,
   which runs the display link even when nothing on the page changed. A
   seek, or anything while paused, goes at once, and leaving slow ticks
   sends the latest state if a tick was held back.
3. **Queue artwork is request/reply data, not an event.** Each surface calls
   `queue_art` only for visible rows. The only queue-art event is the
   payload-free `queue-art-ready` nudge, which makes surfaces ask again. Keep `ImageCache::get` promotion and the
   byte cap so scrolling does not evict the artwork it is returning to. The
   cache is a lazy LRU: a use stamps the entry and pushes the stamp onto
   `order`, and eviction skips stale stamps (`compact` trims them), so a hit
   is O(1). Each surface keeps a pending set of the ids it has asked for
   (`artInFlight` in `queue.js`, `neighbourArtPending` and `queueArtPending`
   in the strip) and does not ask for them again until the reply lands;
   `queue-art-ready` clears those sets. `queue_art` resolves YouTube Music
   covers `QUEUE_ART_CONCURRENCY` (8) at a time and reads their sources in
   one pass over the queue (`Core::queue_art_srcs`).
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
   The renderer is several classic scripts in one scope, in `index.html`
   order. State read by more than one file is declared in `app.js`. Anything
   whose callback can run before the last script loads (`window.widget.on*`,
   `ResizeObserver`s, the first state) is wired in `boot.js`. Code shared by
   the card, the strip and Sort Songs lives in `src/shared/`: classic scripts
   that each attach to `window.shared` inside an IIFE (the `palette.js`
   pattern), read no page globals and declare nothing else at the top level.
   Each HTML page loads only the shared files it uses, before the scripts
   that call them.
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
- Fork only (TianCal/pear-desktop): `GET /library/playlists`,
  `POST /library/playlists/{id}/play`, `GET /browse/{browseId}`,
  `POST /queue/play`. A 404 means
  stock pear-desktop; say so on screen rather than failing silently. A 404
  arrives as `ErrorCode::NotFound`; decide on the code, never on the message.
- Auth: `POST /auth/{clientId}`; Bearer token for REST and query token for WS.
  One authorisation at a time (`auth_slot`): concurrent 401s share it, and a
  late 401 clears the token only if it is still the one that was rejected.

Status, source, repeat and like reach the renderer as their wire strings but
are enums in Rust (`state::Status`, `Source`, `Repeat`, `Like`). Settings keep
a folder's repeat as a string so an unknown value cannot fail the file's parse.

Repeat is a button cycle, not a setter. `/switch-repeat` accepts an iteration
count for `NONE → ALL → ONE → NONE`; `commands::command` calculates the presses.
`/repeat-mode` may be `null` before the player bar is observed, so preserve the
difference between unknown and `NONE`. Refresh it on connection and after a
repeat command because `REPEAT_CHANGED` fires only when the value changes.
`PLAYER_INFO` carries `repeat` too, but the server starts it at `NONE` before
the player reports, so only `ALL`/`ONE` are taken from it.

The server answers `/search` and `/queue` by asking the renderer and taking the
first reply, so overlapping calls can swap answers. `Api` serialises each
behind its own async mutex; every caller (user search, artwork's YouTube Music
lookup, the artist popup's name lookups) goes through it.

## State and connection

The WebSocket close path is the reliable offline signal: a cached token lets
`ensure_token()` succeed without touching the server. Show `connecting` only on
the first attempt or a manual retry; background reconnects should not flicker.

`Core::set_on_screen` records which surfaces are showing; every `show`/`hide`
of the widget or dropdown must call `crate::on_screen`. `state` goes only to
surfaces on screen (`emit_to`); while nothing shows, queue/lyrics refreshes
only mark themselves stale. Coming on screen emits the latest snapshot to that
surface and runs the stale refreshes. The
widget learns its own visibility from the `on-screen` event, since its
`document.hidden` is always true; a widget built hidden (`showWidget` off,
which every hide and show path writes) learns it from `widget_state`'s
`onScreen` instead, as it hears no event until it is first shown. A superseded lyrics lookup is dropped at its
next await (it caches nothing). Settings writes from the main thread or an
async worker use `Store::update_soon`: the edit applies at once, and one
background writer per burst (a `dirty` flag; the writer goes round again
while it is set) writes the latest settings, so several quick edits are one
write. `before_exit` calls `Store::flush` so a queued write survives
quitting. `Core::update` clones the state once (it is held as
`Arc<PlayerState>`); read a field with `Core::with_player` and take
`snapshot` only when the whole state is needed.

Retrying tears down the socket and clears the queue, so `launch_music_app`
requests a retry only when disconnected. `Realtime::request_retry` must wake
backoff and close an established socket. Pollers stand down while both surfaces
are hidden, and the first surface to come on screen runs `refresh_all(false)`.
`refresh_all` runs one at a time and skips itself within 2.5 s of the last
(a new connection forces it, still reading REST for the placeholder
shuffle/volume/repeat). The queue and lyrics channels are `state/queue.rs`
and `state/lyrics.rs`, separate `impl Core` blocks; `Gate` and `refresh_all`
stay in `state/mod.rs`. Overlapping `refresh_queue` calls merge into one fetch
plus at most one rerun (`Gate`). Any surface coming on screen refetches the
queue if the last fetch is older than 5 s (`QUEUE_SHOW_TTL`), since the queue
has no push channel. `handle_message` never awaits: `apply_song`
publishes the track at once and fetches cover and like state in a task guarded
by `cover_token`. Like state is read on each track change (and again
1.5 s later, as the page's like button can lag) and when the first surface
comes on screen; its poll is a slow fallback. The play-state poll asks `/song` every 30 s, or every
10 s while position ticks disagree with the play/pause glyph.

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
  `Resized` event should measure the window. `on_geometry_changed` takes
  `resized`: only a `Resized` event re-zooms, from the rect the handler
  measured; a `Moved` event only records geometry and schedules persistence,
  which is one debounce task per burst (`persist_pending` + `persist_seq`),
  never one per event.

## Skins, sizing, and panels

`window.rs::BASE` defines each skin's natural size. The skins are Stack,
Vinyl and Liner Notes (`liner`), picked from Skin / Dropdown skin
(`skin:<name>`, `panelSkin:<name>`); a switch keeps the corner the user
anchored the widget to. Classic was removed; stored `"classic"` values fall
back to Stack through `valid_skin`. `skin_shows_queue` and
`skin_shows_lyrics` say what a skin draws itself, and Rust reads them for
demand, so a skin that shows lyrics fetches them with no panel open. Those
skins have no lyrics panel: the corner button and its menu item are gone, and
`panel_fits_skin` keeps a remembered lyrics panel from reopening over them.
Vinyl and Liner lay the card's own children out directly (`.player` and
`.meta` are `display: contents`); Liner's grid gives the queue and search
panels explicit heights that must match `panel_height`. Vinyl's karaoke fill
is a CSS animation of the registered `--sung`, started and resynced by
`singAlong`; its record turns only under `turning`
(playing and on screen, `renderTurning`), because a running animation makes
the window server redraw the window every frame. The renderer does not reflow;
it zooms by the tighter `width/base_width` ratio. Stored sizes contain only
width, and height is derived from the skin ratio. The AppKit corner radius stays
in points, so CSS divides its radius by the zoom; the `zoom` event carries both
(`{ zoom, radius }`) so the radius lives only in `window.rs`. The dropdown's
window label is `DROPDOWN` ("panel" on disk and in its URL); in code "panel"
otherwise means the section that opens inside a card. While a panel is open,
persistence saves the collapsed rectangle, never the slid-up one.

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

Two orders, chosen by `lyrics::fetch_lyrics` (`lyrics/mod.rs`) from the track
id; each provider is its own file under `lyrics/`. The cache,
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
Corrupt records behave as cache misses. `fetch_lyrics` reads and writes the
disk cache on blocking threads; the write is awaited, so a `forget_miss` made
right after still finds the record. `fetch_lyrics_traced` also says where the
answer came from (`Origin`: memory, disk or network). `how` is the `lyrics::How` enum; an
unknown stored value reads back as `cached`.
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

## Track labels and artists

`/song` gives YouTube Music's canonical words, often romanised ("Free Soul"
by "Ronghao Li"). The track's queue slot has them in the account's language
("纵横四海" by "李荣浩") and a browse id for each artist. `Core::dress`
relabels the playing `Song` from its slot whenever the queue holds it: in
`apply_song` (the next track is usually already queued, so there is no flash),
after every `publish_queue` (`relabel_song`) and in `remap_artists`. It
rebuilds from `api_title`/`api_artist` each time, so it can be re-run. The
two later callers dress a copy outside the locks and write it back only if
the song is still that copy, so a newer `apply_song` is never overwritten.
An `artistNames` entry for the whole canonical credit is the user's choice
and beats the slot's credit (`store::renames_whole`); a rename of one name
inside it does not. Lyrics look up with `Song::for_lyrics`: the API's words,
so a lookup and its cached miss never depend on whether the queue had
arrived, except that a rename of the credit as shown (what the Rename menu
offers) still steers the artist.

`Song.artists` comes from the slot's byline. A run whose page type is
`MUSIC_PAGE_TYPE_ARTIST` gives its browse id. YouTube Music links only the
artists it has matched, and sometimes none: "MC HotDog熱狗/艾怡良" is one plain
run. So plain text is split with `store::split_credit` into names with an
empty id, and the joiners it writes as their own runs ("、", "和") are dropped.
Each link keeps YouTube Music's own name in `query`; `name` is the
`artistNames` rename for display. A name without an id is found when needed:
`artist::page` searches for `query` and takes only an artist whose name folds
equal (`search::find_artist`). Uploader channels are skipped because they
have no Top songs, and for uploads only linked artists are kept. Without a
slot the credit is romanised, so it is linked only when it names one artist
and `/song`'s `artistUrl` channel (kept for `AUDIO` and
`ORIGINAL_MUSIC_VIDEO`) can stand in; the slot follows within a queue fetch.
The channel also fills a slot's lone unlinked name. `QueueTrack` keeps
`credit` and `artists` unserialised; only the playing track's links reach the
renderer.

The whole credit under the title is one link. It opens the artist popup,
which has one section per artist (up to four), each with the Top songs shelf
of `GET /browse/{UC…}` and its own Play all. Sections fill in as their pages
land. This is YouTube Music only: local credits have no page, so local
subtitles have no link. A page takes about 0.4 s through the API server,
plus its covers, so it must never be fetched on the click:

- Warming is on hover only, like the playlist picker: never on a track
  change. Resting the pointer on the card for 150 ms warms every artist the
  popup would show, and again when the links change while the pointer is
  still there.
- `artist::page` caches pages (covers already resolved to `data:` URLs) by
  id, and name lookups by folded name, separately, so a failed page does not
  repeat its search. Both keep 24 entries for 30 min, and concurrent callers
  share one fetch. Failures are kept too, because warming runs on every
  hover: 10 min for an API server without `/browse` (`api::NO_BROWSE`), 60 s
  for other failed requests, and 30 min for a name with no artist.
- Name lookups use `Api::search_background`, which takes the search slot only
  while no `search_if` is waiting (it waits on `searches_idle`, a `Notify`
  fired when the last waiting search takes the slot, not a poll), so they
  never delay the user's search.
- A page's covers are fetched all at once, each given 2 s, so one slow
  thumbnail cannot hold the page.
- The renderer keeps replies for 10 min and shows "Loading" only after
  150 ms, so a warm open draws in the same frame. `artist_page` replies with
  a typed struct rather than `json!`, which would copy the covers again.

Rows play through `play_result` (after the current track). **Play all** plays
the shelf's "VL…" playlist through `play_playlist`. The popup is an overlay like
the picker. It closes for panels, search, the picker, Escape, an outside click,
going offline, switching to a folder and a change of credit, and it blocks
the playback shortcuts while open. A drifting subtitle pauses while the
pointer is on the credit.

## Menu-bar strip

Tray → **Show now playing in menu bar** (`menuBarStrip`, a boolean) puts the
playing song at the top centre of the display with the menu bar: the cover
with a record peeking out, the song title, the artist only if the whole name
fits (never an ellipsis), and a hairline of progress. Hovering drops a sheet
with controls and the queue's previous and next tracks: over the bar on a
flat display, under the menu bar beside a notch. Clicking the bar does
nothing. It is one design, not a skin: it has no skin setting and no lyrics.

The open sheet has the card's controls: like (right-click dislikes),
shuffle, repeat (the same "other mode" press as the card), wheel volume by
the card's rules (70 ms throttle, final value always sent, 900 ms echo hold)
with the progress row turning into the volume, a sideways swipe to skip, a
right-click menu, the cover opening YouTube Music (not for local files), and
two list views in its place: playlists (`list_playlists`) and the track's
artists (`artist_page`, YouTube Music only; local credits are plain text).
Both lists are `shared/lists.js`, the strip supplying its own row and note
markup.

- **Its own page and window** (`strip.rs`, label `strip`, `src/strip/`), like
  Sort Songs, so it shares no globals with the card. The window exists only
  while the setting is on: `show` builds it, `hide` destroys it. It is a
  surface like the others: `crate::on_screen` on every show and hide, and it
  reads `state`, `cover` and `queue`. While it is up something is on screen,
  so the pollers keep running. It joins every desktop but not fullscreen
  Spaces. On top of that, the watcher asks Quartz whether another app's
  ordinary window reaches the menu-bar display's top edge (after a Space
  change, an app coming forward or a display change, `macos::watch_screens`,
  every 250 ms while it settles; otherwise every 5 s as a fallback),
  where in a normal Space only the menu bar can be
  (`macos::top_of_main_display`: a fullscreen app, Split View, whose windows
  do not span the display, a game). `visibleFrame` cannot tell: it keeps the
  menu bar's height in fullscreen. The same pass sees whether the menu bar's
  own window (level 24) is down across the top: in fullscreen it slides in
  when the pointer reaches the top, and the strip comes with it, checked
  every 100 ms while in fullscreen with the pointer near the top or the menu
  bar down. The strip stays while its sheet is in use, though moving into it
  puts the menu bar away. It comes as soon as the menu bar starts down and
  goes as soon as it starts up (the menu bar's `y` rising or falling between
  looks). Otherwise, in fullscreen, the strip is away (`set_covered`): the
  window stays, takes no clicks and is off screen for `on_screen`, and the
  page slides the bar up out of sight (`strip-away`). Hiding and showing the
  window drew a stale frame on the way back, a visible twitch.
- **The window never resizes.** A webview shows its old picture for a frame
  after a resize, pinned to the corner, which reads as a jump or a flash. The
  window is one fixed transparent box (`WINDOW`) centred on the top edge at
  `NSStatusWindowLevel` (over the menu bar, under its menus). It lets clicks
  through except while the sheet is open *and* the pointer is on it, so the
  menu bar beside the sheet never loses a click to the fold delay. Position
  it at build time or through `macos::set_frame`; a `set_frame` posted right
  after building loses to the builder's own position. The watcher re-checks
  the menu bar's display on the same notifications and fallback, and
  re-centres and re-shapes after a dock, unplug or resolution change.
- **Animate transform and opacity only.** The sheet is always laid out
  (`visibility: hidden` when folded) and opens by transform and opacity, which
  the compositor runs without WebKit laying out each frame. Animating height,
  width or grid rows dropped frames. Progress is a `scaleX`, written only when
  the hairline moves a whole pixel and, in steady play, at most every 1.5 s
  (`HAIRLINE_MS`, paired with Rust's `SLOW_TICK`, so folded it moves on
  every other push; a seek, new track or pause writes at once): each write
  keeps WebKit's display link running for about 200 ms, so a pixel a second
  kept the app drawing frames half the time. The sheet's bar and clock are
  written only while it is open; a transition on it would run every frame
  while playing. The
  hairline's width comes from a `ResizeObserver`, never `offsetWidth` in
  `tick`. The 250 ms clock runs only while the sheet is open and the bar
  is not `away`; folded, the `state` pushes tick the hairline.
- **Hover comes from Rust.** The app is never active, so WebKit sends the
  page no hover. The page reports the drawn part of the window (`strip_hot`:
  the bar, or the sheet plus slack while open). While the strip is on, a
  thread reads the pointer through Quartz (`macos::pointer_location`, no
  main-thread hop) when it moves, at most every 40 ms near the top of the
  screen and every 150 ms elsewhere, and emits `strip-hover` when it crosses
  the drawn part. NSEvent global and local monitors (`macos::watch_pointer`,
  installed by `show`, removed by `hide`) wake it through
  `StripState::nudge`, as do `strip_hot` and `strip_open`; a still pointer
  costs nothing, where polling woke the app seven times a second. A moving
  pointer does wake the main thread for each event the monitor sees; the
  callback only sets a flag. Off, the thread parks. It reads whether the strip is on from `StripState.on`
  (written with the setting by `set_shown`), not from the settings lock, and
  `macos::top_of_main_display` takes its dictionary keys from `window_keys`,
  built once.
- **Views.** A list view replaces the main view inside the sheet, at a fixed
  height so the fixed window always holds it. `viewSeq` drops replies for a
  view that was left; the sheet returns to the main view 300 ms after it
  folds. The artist view leaves on a new track or going offline, not on a
  relabelled credit, and artists are warmed when the sheet opens and again
  when the links change while it is open. Playlist covers are request/reply
  for the rows in view, rechecked on a 300 ms timer while the list is up
  (scroll events may not come), and an empty reply rests a minute. In a list
  the wheel scrolls the list only.
- **Hover inside the sheet comes from Rust too.** WebKit's own `:hover`
  reaches a never-active app's page about half a second late. While the
  sheet is open the watcher also emits `strip-pointer` (window coordinates,
  when the pointer moves; `-1,-1` when it leaves), and the page sets a
  `.hover` class on what is under it (`pointAt`). Every hover rule in the
  strip's CSS has a `.hover` twin. Beside Previous and Up next, inside the
  column's fixed width, a queue button (brighter while the pointer is over
  them) opens the queue view: every queue row, centred on the playing one, played
  through `play_queue_index`, art by `queue_art` for the rows in view (a
  missed row rests until `queue-art-ready` or 5 s), and `rating` in the
  rows' identity so a local dislike redraws them dimmed. The playing row
  in any list is marked by a band and a bar badge on its cover, not by
  accent-coloured text alone, which read as disabled when the accent came
  out muted. A track change only moves that mark (`markQueueCurrent`); the
  rows are rebuilt only when their own key (index, id, rating, title,
  artist) changes. The badge's bars
  bounce by `scaleY` only while playing with the sheet open; nothing else in
  the strip runs an animation.
- **Colour.** The strip loads the card's `palette.js` and paints through the
  same `shared/tint.js` painter: the accent and
  the three washes behind the open sheet come from the same algorithm and
  the same Cover tint setting as the widget and dropdown, and are redone when
  `state.tint` changes. The washes are painted once per cover, never animated.
  The folded bar's words take **Text colour** from the strip's menu
  (`stripInk`: `auto`, `cover`, or a `#rrggbb` from the system Colours panel
  via `macos::pick_colour`; `strip::valid_ink`), carried on `state`
  and applied as `data-ink` on `#strip`. It recolours only the words in
  `.bar` (the hairline keeps the cover's tint), since the menu bar's own text
  follows the wallpaper, not the appearance.
- **Right-click menu.** `strip_menu` pops `tray::build_strip_menu` (which
  carries Cover tint) under the
  pointer. It is modal, so `menu_open` makes the watcher stand down while it
  is up; otherwise the pointer moving into the menu would fold the sheet.
- **Queue only while open.** `strip_open` registers queue demand for the open
  sheet (`Core::want_queue`, which fetches unless a recent queue is held), so
  a strip that is merely showing never fetches `/queue`. A queue-owning skin
  counts only while its own surface is on screen (`queue_wanted`), or the
  strip would keep the widget's queue fetching. Neighbour artwork is
  `queue_art` request/reply, like every other queue surface.
- **Notch.** `macos::menu_bar_shape` reads the menu bar height and the camera
  housing's width; with a notch the song sits left of it and the artist right,
  in equal columns so the camera stays centred. `PMW_NOTCH=<points>` fakes a
  notch on a flat display and `PMW_STRIP_OPEN=1` pins the sheet open.
- **Settings.** Test builds stored a design name in `menuBarStrip`;
  `strip_from_file` reads any name but `off` as shown, so those files still
  parse.

## Search and queue

Search responses have no stable result path. `parse_search_results` walks the
JSON and keeps `musicResponsiveListItemRenderer` nodes with a `videoId`.

Playing a search result or an artist's top song (`play_result`) uses the
fork's `POST /queue/play`, which inserts after the current video and jumps to
it inside YouTube Music in one request (~0.1 s, YouTube's `get_queue`). It
replies with the slot and the video now in it, which YouTube Music may have
resolved to another id; `refresh_queue_after_jump` watches that one, and skips
its confirming queue read since the slot was selected in the same step. 422
(`ErrorCode::Rejected`) means YouTube Music gave nothing to queue, 503 that it
could not be asked; the fork gives up rather than play the song after its
request timed out. Plays go one at a time (`Api::play_slot`): each reads the
playing slot only once YouTube has answered, so two in flight could finish in
the wrong order. Only a 404 (stock pear-desktop) falls back, because any
other reply may already have played the song. The fallback, `insert_and_jump`,
inserts and then polls until the queue reflects the insertion before advancing. That insert
returns `204` before the queue changes, so calling `next()` immediately can
play the wrong track. Polls back off (150 ms doubling, five tries). After a jump that changes the
track, VIDEO_CHANGED → `apply_song` refreshes the queue; `refresh_queue_after_jump`
only watches `/song` as a fallback. A jump to another copy of the playing video
has no VIDEO_CHANGED, so that case polls `/queue` until the slot is selected and
publishes that reply. A search superseded by a newer query in the same window
stops before parsing; thumbnails load four at a time.

`play_queued` searches forward and is unsuitable for queue UIs that show earlier
tracks. Those UIs send the rendered slot plus video ID to `play_queue_index`;
the command verifies the live queue and falls back to `play_queued` on mismatch.
Keep `parse_queue` and `queue_entries` as separate projections: the former drops
unnamed slots, while the latter preserves API indexes. `QueueTrack::index`
connects them. Update the renderer's current row optimistically while polling.

Search results, queue rows and Up next rows are one `.track-row` with a kind
modifier (`track-row--result|queue|upnext`) that sets `--row-art`, `--row-gap`
and `--row-font`; the old `.result`/`.queue-item`/`.upnext-item` classes stay
on the elements for the handlers and the skin rules.

Stack's queue uses two rows, column flow, and three columns per view. Keep the
gutter on the section rather than the scroller. Centre from each card's
`offsetLeft`; percentage columns plus gaps make multiplication drift. Its wheel
handler must stop propagation and map vertical deltas to horizontal movement so
queue scrolling never changes volume. Do not enable scroll snapping; trackpad
deltas are too small to cross snap thresholds reliably.

## Local folders

`local/mod.rs` is a second source behind the same `Core` channels. While a folder
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
lives on its own thread, opens when something plays (a paused load only
parks the track at a position, `Local::parked`), closes after 30 s paused, and is reopened when its error
callback fires or the default device changes (checked every two seconds
and on each load). Loads do file IO: run them on blocking threads or the
`local-tick` thread, never on the AppKit main thread or an async worker. `loading`
is held across every load and seek; the ticker only `try_lock`s it, so it never
sees the gap between sinks and advances twice. `close` and `toggle_play` take
it too. Advancing tries each track at most once and publishes only the track
that started (or the last failure), never each file it skipped. If the root is
no longer a directory it stops at the first failure. Position is pushed on
`state` once per whole second, like YouTube Music's.

`scan` (`local/library.rs`) lists the folder (cheap) and reads tags only for files whose size or
mtime differs from the index at
`~/Library/Caches/pear-music-widget/library/<hash of root>.json`. Reading
every tag on a cold USB drive takes several seconds. The index records what
a track's own art is (`Art`: none, embedded, or the sidecar's path); it
stores only none/embedded, and the sidecar is found per folder at each scan.
An index written with the old `has_art` flag still parses (the flag meant
embedded). Sidecar decodes are memoised by sidecar path (`side:<path>@px`),
embedded ones by audio path, and at most `FILE_ART_DECODES` (2) file-art
decodes run at once, each re-checking the memo when its turn comes. Local
search matches `Session.haystacks`, the lowercased title, raw artist and
album made once per folder; only a renamed artist is folded per query. Track ids are
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

Covers (`local/art.rs`): embedded or sidecar art first, then `artwork::find` — iTunes album
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
emits `queue-art-ready` when lookups that missed its reply have landed. The
lookup's cache reads and every `.jpg`/`.miss` write go through
`spawn_blocking`; `artwork::cached` stays synchronous for search rows and
the sorter, which call it from blocking contexts.
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
(`Likes::set` returns a `Save`). `Likes::load` never runs under the session
lock: `reload_likes` and the sorter's writes (`Sorter::reloaded`) snapshot
the tracks, read off the lock and swap the result in only if the same folder
is still open, because the tray menu (`scope`) and `sorter_view` take that
lock on the main thread. The player and Sort Songs each write from
their own copy, so `Save::write` re-reads both files and merges them into the
snapshot before writing; a key only the file holds is kept when it is newer
than the orphan cutoff (the other writer's), not when it is an orphan the
snapshot pruned. `at` is milliseconds; files written in seconds read as such. Entries are keyed by relative path,
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

## Sort Songs

Tray → **Sort Songs…** (`sorter::open`) pauses whatever is playing, in either
source, and opens a window that plays each unrated song of a local folder
from its chorus. ← likes, → dislikes, ↓ skips.

- **No cost while closed.** The runtime (session, player, HTTP client) is
  built by `open` and lives in `sorter::RUNTIME`. The window is destroyed on
  close, not hidden (`CloseRequested` is not prevented for `sorter::LABEL`).
  `Destroyed` drops the runtime. The player's ticker holds only a `Weak`, so
  it exits too, and the output stream's thread with it. Spawned work checks
  `runtime()` before playing anything.
- **Activation policy.** It is `Regular` while the window is open, so the
  window gets a Dock icon and keyboard focus, and `Accessory` again after it
  closes.
- **Own player.** It has its own player (`sorter/player.rs`) on the same
  `audio.rs` streams and decoders as `local.rs`: fresh decoder and `Sink` per
  start, never `Sink::clear` or `Sink::try_seek`. A dead or changed output
  device restarts the card where it was. Play starts at the chorus and runs
  to the end of the song. Nothing loops or advances on its own. A restart or
  seek replaces the sink only if the same card is still up.
- **Main thread.** Commands that touch the drive (`sorter_rate`,
  `sorter_undo`) are async and do their work on a blocking thread.
- **One rating write at a time.** `Sorter.ratings` is held from re-reading
  the likes file to writing it back, in rate, undo, reset and trash. The
  re-read inside it is off the session lock (`Sorter::reloaded`).
- **Trash.** Trashed songs are taken out of the player too
  (`Local::forget_tracks`), when it has that folder open.
- **Lyrics one at a time.** Lookups go through `lyrics::fetch_lyrics`, one at
  a time (`Sorter::lookup`). Run side by side, providers time out and the
  failures are cached as misses for a week. A miss is retried once after 3 s only when the lookup just made
  it (`lyrics::Origin::Network`); one read back from memory or disk had its
  retry when it was made.
- **Finding the chorus.** `sorter/chorus.rs` scores repeated blocks of synced
  lines, then refines to the line that sings the title. Without synced lyrics
  it starts at 35%.
- **Progress is one transform.** `--p` on the track: the bar scales and the
  head translates by it (`--track-w` kept by a `ResizeObserver`); nothing on
  the timeline animates `width` or `left`, and the play icon's markup is
  rewritten only when the glyph changes.
- **Ratings.** Ratings are written through `likes.rs` and re-read before each
  write. After every write (rate, undo, reset, trash), `Local::reload_likes`
  re-reads the file if that folder is the one the player has open. Otherwise
  the player's next save, made from what it loaded, would put back what was
  changed here, a reset above all.
- **Queue order.** The queue is folder order, except that songs sharing a
  title (`title_key`: bracketed notes dropped, simplified, folded) follow the
  first of them directly. **Reset ratings** clears every rating in the folder
  as dated tombstones.
- **Events and commands.** Events go only to the sorter window (`emit_to`)
  and are prefixed `sorter-`. Commands are prefixed `sorter_`.
- **Remembered folder.** The last folder is stored apart, in
  `~/Library/Application Support/pear-music-sorter/settings.json`.

## Volume

The renderer's `setVolume` (`shared/volume.js`, `volumeControl`; `input.js`
and the strip only supply `apply`, `volume` and `send`) and Rust's
`VOLUME_CHANGED` handler jointly enforce:

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
`handle_menu` rebuilds it once after every item, so synchronous branches
(`handle_source` and `after_skin_change` included) must not call `refresh`
themselves; only callbacks that run after `handle_menu` has returned do. The
two template icons are decoded once (`icon`), and `show_icon` calls
`set_icon` only when the glyph changes.

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
for palette interpolation. A transition started in a hidden page stays
running forever and keeps the app drawing, so `renderTurning` sets
`offscreen` on the root while a surface is off screen, which turns every
transition off (and cancels any already stuck). `PMW_THEME=dark|light` pins
appearance for testing.

## Settings

Settings live at
`~/Library/Application Support/pear-music-widget/settings.json`, preserving the
legacy Electron path and unknown keys. `Store::write` serialises the latest
settings under its write lock, skips unchanged text, and writes via temp file,
`sync_all` and rename (`fsutil::write_atomic`, which every store uses): a
truncated file would load as defaults and lose the token, sizes and saved
folders. A file that does not parse is renamed to `settings.json.broken-<secs>`
before defaults are used, so a hand edit with a typo can be repaired.
Main-thread callers (the skin and dropdown-skin switches, the strip toggle,
`apply_skin`'s bounds) use `update_soon`. The skin itself is written once,
by `tray::set_skin`/`set_panel_skin` before the main-thread hop;
`window::apply_skin` and `panel::apply_panel_skin` must not write it again.

`corners` is keyed by skin. `corners_from_file` must continue accepting the old
flat shape; a parse failure sets the whole file aside and starts from defaults. The state snapshot includes the whole map because the two
surfaces could use different skins. Menu IDs follow `corner:<skin>:<button>`.

`cornersAutohide` is renderer-driven. Keep the 200 ms `wakeChrome` guard, allow
mousedown to wake faded controls before click, and run it even with a panel
open. The floating close button shares wake timing but is not in `CORNERS`, does
not consume title gutter, and is always absent from the dropdown.

## Build, test, and prototype

```bash
cargo test
node --test 'scripts/*.test.cjs'
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
