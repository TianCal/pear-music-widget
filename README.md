# Pear Music Widget

A floating now-playing widget and menu-bar player for macOS, driven by the
**API Server** plugin in [pear-desktop](https://github.com/pear-devs/pear-desktop)
(and its predecessor [th-ch/youtube-music](https://github.com/th-ch/youtube-music)).

Inspired by [YoutubeMusicCoverWidget](https://github.com/rafailpapastamou/YoutubeMusicCoverWidget),
which polls the same API server from an Übersicht widget. This one is a
standalone [Tauri](https://tauri.app) app that holds the API server's
**WebSocket** open instead, so track, position, volume and shuffle changes
arrive as push events rather than on a timer.

The interface is the system WebView and the rest is Rust, so it installs at
**8.4 MB** rather than the 233 MB the earlier Electron build needed.

![The floating widget on the desktop](docs/widget.png)

## Two surfaces

**A menu-bar dropdown** — click the play icon in the menu bar for a popover with
artwork, transport, a scrubable progress bar and a countdown. Closes on
click-away or Escape. Meant to replace the system Now Playing item.

**A floating widget** — a resizable panel you can park anywhere on the desktop.

Both are the same renderer on the same state, so they never disagree.

| Tray icon | |
| --- | --- |
| **Left click** | Open/close the player dropdown |
| **Right click** | Settings — widget visibility, skin, dropdown skin, opacity, always on top, reset size and position, open at login, reconnect, quit |
| **Solid glyph** | Connected |
| **Hollow glyph** | YouTube Music not running, or its API server is off |

Both the menu-bar menu and the widget's own right-click menu carry **Quit with
YouTube Music**, which closes the player before quitting the widget. The first
use raises the one-time macOS "wants to control" prompt; if it is declined, the
widget still quits on its own.

If YouTube Music is not running, pressing play launches it. **Double-clicking the
card** brings it to the front — anywhere that is not a control, since dragging
only begins once the pointer has actually moved.

## Skins

Two layouts, switchable from the menu bar.

| Skin | Natural size | Layout |
| --- | --- | --- |
| **Classic** | 300×110 | Artwork left, titles and the full transport right |
| **Stack** | 330×284 | Artwork and titles on top, centred transport, full-width progress, and the next four queued tracks below — click any to play it |

**Skin** sets the floating widget; **Dropdown skin** sets the menu-bar popover.
They are independent, so you can run one layout on the desktop and the other in
the dropdown — and each surface's own right-click menu changes only its own
layout. Dragging an edge scales whichever skin is showing — it never
switches between them.

## Lyrics

The list icon in the corner opens a rolling lyrics panel, on either skin. The
active line is highlighted and centred as the track plays, and clicking a line
seeks to it. Scrolling over the panel scrolls the lyrics rather than the volume,
and holds them still for a few seconds before playback takes the roll back. Classic gets tighter type to suit the small card; Stack gets a
larger, airier roll to match its scale.

Lyrics come from [LRCLib](https://lrclib.net) — the same source YouTube Music's
own `synced-lyrics` plugin uses, since the api-server exposes no lyrics route.
**This is the only time the widget talks to anything other than localhost**; it
sends the track title and artist to look them up. Tracks with no synced lyrics
fall back to a plain unsynced block, and tracks with none say so.

## Search

The magnifier in the top-right corner opens a search panel: the widget grows
downwards, you type, and results come back with artwork. Click one — or use
**↑ / ↓** and **Enter** — to play it. **Escape** closes.

Results are queued directly after the current track and jumped to by queue
index, so a song ending mid-click cannot make it play the wrong thing.

## Features

- Real macOS vibrancy, native rounded corners and shadow
- The whole card tinted from the cover art — three hues pulled off the artwork
  and diffused across the glass, with a matching accent on the transport
- Play/pause, next, previous, shuffle, like (right-click the heart to dislike)
- Scrubable progress bar
- Volume three ways: drag the popover on the speaker, scroll anywhere on the
  card, or **↑ / ↓** while focused (**Shift** for 1% steps)
- Drag any edge to resize — aspect locked, and the layout scales rather than
  reflowing
- Light and dark appearance; position and skins remembered across launches, with the size you chose remembered **per skin**
- No Dock icon

There is no repeat button: `GET /api/v1/repeat-mode` returns `null`
unconditionally on the shipped plugin, so repeat state cannot be displayed
truthfully.

## Install

Download the DMG from [the latest release](https://github.com/TianCal/pear-music-widget/releases/latest),
open it, and drag the app onto Applications.

It is ad-hoc signed but not notarised, so a downloaded copy arrives quarantined —
right-click → Open the first time, or:

```bash
xattr -dr com.apple.quarantine "/Applications/Pear Music Widget.app"
```

Then enable the **API Server** plugin in YouTube Music (menu → Plugins). On first
run the widget asks for an access token; if the plugin's auth strategy is
`AUTH_AT_FIRST`, approve the dialog that appears inside YouTube Music.

Requires macOS on Apple Silicon. Built and tested on macOS 26.

## Running from source

Needs a [Rust toolchain](https://rustup.rs) and the Xcode command line tools.
There is no npm step — the interface is plain HTML and JavaScript, served
straight out of `src/`.

```bash
cd ~/projs/pear-music-widget/src-tauri && cargo run
```

If the widget sits on a setup screen, run the connectivity check:

```bash
cargo run -- --doctor
```

To build the DMG yourself: `cargo tauri build` (`cargo install tauri-cli` first).

## Compatibility

The `api-server` plugin is versioned independently of the host app and can change
its protocol. This release targets:

| | |
| --- | --- |
| API surface | `/api/v1` |
| Verified against | YouTube Music **3.12.0**, bundle `com.github.th-ch.youtube-music` |
| Default endpoint | `http://127.0.0.1:26538` |

`--doctor` checks the server's OpenAPI document and fails loudly if the plugin
moves off `/api/v1`, rather than leaving the widget silently blank. The full
endpoint list, the protocol quirks worked around, and where to change things
are in [AGENTS.md](AGENTS.md).

## Configuration

`~/Library/Application Support/pear-music-widget/settings.json` — `host`, `port`,
`clientId`, cached `token`, window `bounds` (position), `sizes` (width per
skin), `skin`, `panelSkin`, `alwaysOnTop`, `opacity`. Quit the app before editing.

## Assets

No third-party assets, nothing to attribute. The transport glyphs are
hand-authored SVG paths in the `<symbol>` sprite in
[src/index.html](src/index.html); the menu-bar icons and the app icon are
computed procedurally at build time by
[src-tauri/build.rs](src-tauri/build.rs). The only binary in the repo is the
screenshot.

## Contributing

[AGENTS.md](AGENTS.md) documents the architecture and the non-obvious behaviour —
read it before changing the volume path, the resize/zoom logic, or the tray menu.
