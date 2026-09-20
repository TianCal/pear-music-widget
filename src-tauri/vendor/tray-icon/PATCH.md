# macOS 27 tray click patch

This is `tray-icon` 0.24.2 with upstream commit
`42eb44ea1507d51b68a8b2fbb0d96a9c85f5b4cd` applied.

macOS 27 no longer forwards status-item mouse events to the overlay view while
an `NSMenu` is attached. The patch keeps the menu detached at rest, then
attaches it only while AppKit presents it. This restores Pear Music Widget's
left-click dropdown while preserving its right-click settings menu.

Upstream shipped the fix in `tray-icon` 0.25.1. Tauri 2.11.5 requires the
incompatible `tray-icon ^0.24`, so the patch can be removed once Tauri updates
that dependency.
