# Pear Music Widget

A floating now-playing widget and menu-bar player for macOS, driven by the
**API Server** plugin in [pear-desktop](https://github.com/pear-devs/pear-desktop)
(and its predecessor [th-ch/youtube-music](https://github.com/th-ch/youtube-music)).

It holds the API server's **WebSocket** open, so track, position, volume and
shuffle changes arrive as push events rather than on a timer.

Inspired by [YoutubeMusicCoverWidget](https://github.com/rafailpapastamou/YoutubeMusicCoverWidget),
which polls the same API server from an Übersicht widget.

![The floating widget on the desktop](docs/widget.png)

## Two surfaces

**A menu-bar dropdown** — click the play icon in the menu bar for a popover with
artwork, transport, a scrubable progress bar and a countdown. Closes on
click-away or Escape.

**A floating widget** — a resizable panel you can park anywhere on the desktop.

Both are the same renderer on the same state, so they never disagree.

| Tray icon | |
| --- | --- |
| **Left click** | Open/close the player dropdown |
| **Right click** | Settings — widget visibility, skin, dropdown skin, cover tint, opacity, always on top, reset size and position, open at login, reconnect, quit |
| **Solid glyph** | Connected |
| **Hollow glyph** | YouTube Music not running, or its API server is off |

If YouTube Music is not running, pressing play launches it. **Double-clicking the
card** brings it to the front. Every menu carries **Quit with App**, which closes
YouTube Music before quitting the widget.

## Skins

| Skin | Natural size | Layout |
| --- | --- | --- |
| **Classic** | 300×110 | Artwork left, titles and the full transport right |
| **Stack** | 330×284 | Artwork and titles on top, centred transport, full-width progress, and the next four queued tracks below — click any to play it |

**Skin** sets the floating widget, **Dropdown skin** the menu-bar popover, and
they are independent. Dragging an edge scales whichever skin is showing; it never
switches between them.

## Lyrics

The list icon opens a rolling lyrics panel on either skin. The line being sung is
highlighted and held just above centre, clicking a line seeks to it, and
scrolling over the panel scrolls the lyrics rather than the volume. Leave the
panel open and it reopens next launch.

Lyrics come from [LRCLib](https://lrclib.net) first, and from YouTube Music's own
timed lyrics when LRCLib has never heard of the track. **This is the only time
the widget talks to anything other than localhost**: it sends the title and
artist to LRCLib, and the video id to YouTube. Tracks with no synced lyrics fall
back to a plain block, and tracks with none say so.

## Search

The magnifier opens a search panel: the widget grows downwards, you type, and
results come back with artwork. Click one — or use **↑ / ↓** and **Enter** — to
play it. Results are queued directly after the current track and jumped to by
queue index, so a song ending mid-click cannot make it play the wrong thing.

## Features

- Real macOS vibrancy, native rounded corners and shadow
- The whole card tinted from the cover art — three hues pulled off the artwork
  and diffused across the glass, with a matching accent on the transport.
  **Cover tint** sets how far it goes, from Off to Vivid
- Play/pause, next, previous, shuffle, like (right-click the heart to dislike)
- Scrubable progress bar
- Volume three ways: drag the popover on the speaker, scroll anywhere on the
  card, or **↑ / ↓** while focused (**Shift** for 1% steps)
- Drag any edge to resize — aspect locked, and the layout scales rather than
  reflowing
- Light and dark appearance; position, skins and per-skin size remembered
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
run the widget asks for an access token; approve the dialog that appears inside
YouTube Music.

Requires macOS on Apple Silicon. Built and tested on macOS 26.

## Running from source

Needs a [Rust toolchain](https://rustup.rs) and the Xcode command line tools.
There is no npm step — the interface is plain HTML and JavaScript served straight
out of `src/`.

```bash
cd src-tauri && cargo run
```

If the widget sits on a setup screen, run the connectivity check with
`cargo run -- --doctor`. To build the DMG: `cargo tauri build` (`cargo install
tauri-cli` first).

## Compatibility

The `api-server` plugin is versioned independently of the host app and can change
its protocol. This release targets `/api/v1`, verified against YouTube Music
**3.12.0** on the default endpoint `http://127.0.0.1:26538`. `--doctor` checks the
server's OpenAPI document and fails loudly if the plugin moves off `/api/v1`,
rather than leaving the widget silently blank.

## Configuration

`~/Library/Application Support/pear-music-widget/settings.json` — `host`, `port`,
`clientId`, cached `token`, window `bounds`, `sizes` (width per skin), `skin`,
`panelSkin`, `alwaysOnTop`, `opacity`, `tint` and `panel`. Quit the app before
editing.

## Assets

No third-party assets. The transport glyphs are hand-authored SVG paths in
[src/index.html](src/index.html); the menu-bar and app icons are computed
procedurally at build time by [src-tauri/build.rs](src-tauri/build.rs). The only
binary in the repo is the screenshot.

## Contributing

[AGENTS.md](AGENTS.md) documents the architecture and the non-obvious behaviour —
read it before changing the volume path, the resize/zoom logic, the event
payloads, or the tray menu.
