#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TAURI_DIR="$REPO_ROOT/src-tauri"
APP_NAME="Pear Music Widget.app"
INSTALL_DIR="${PMW_INSTALL_DIR:-/Applications}"
DESTINATION="$INSTALL_DIR/$APP_NAME"

find_cargo() {
  if command -v cargo >/dev/null 2>&1; then
    command -v cargo
  elif [[ -x "$HOME/.cargo/bin/cargo" ]]; then
    printf '%s\n' "$HOME/.cargo/bin/cargo"
  else
    echo "error: cargo was not found; install Rust from https://rustup.rs" >&2
    exit 1
  fi
}

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: local app installation is supported only on macOS" >&2
  exit 1
fi

CARGO="$(find_cargo)"

# Tauri embeds the frontend from generate_context! in main.rs. Updating its
# timestamp guarantees that frontend-only changes are included in this build.
touch "$TAURI_DIR/src/main.rs"
(
  cd "$TAURI_DIR"
  "$CARGO" tauri build --bundles app --ci
)

SOURCE="$TAURI_DIR/target/release/bundle/macos/$APP_NAME"
if [[ ! -d "$SOURCE" ]]; then
  echo "error: build completed without producing $SOURCE" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
STAGED="$INSTALL_DIR/.pear-music-widget-install-$$.app"
BACKUP="$INSTALL_DIR/.pear-music-widget-backup-$$.app"
BACKUP_CREATED=0

cleanup() {
  if [[ "$BACKUP_CREATED" -eq 1 && ! -e "$DESTINATION" && -e "$BACKUP" ]]; then
    mv "$BACKUP" "$DESTINATION"
  fi
  [[ ! -e "$STAGED" ]] || rm -rf "$STAGED"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

ditto "$SOURCE" "$STAGED"

# Stop the installed copy before replacing it. pkill is harmless when it is not
# running, and avoids AppleScript accidentally launching a stopped app.
pkill -x pear-music-widget 2>/dev/null || true
for _ in 1 2 3 4 5; do
  pgrep -x pear-music-widget >/dev/null 2>&1 || break
  sleep 1
done
if pgrep -x pear-music-widget >/dev/null 2>&1; then
  echo "error: Pear Music Widget did not quit" >&2
  exit 1
fi

if [[ -e "$DESTINATION" ]]; then
  mv "$DESTINATION" "$BACKUP"
  BACKUP_CREATED=1
fi

mv "$STAGED" "$DESTINATION"
if [[ "$BACKUP_CREATED" -eq 1 ]]; then
  rm -rf "$BACKUP"
  BACKUP_CREATED=0
fi

trap - EXIT INT TERM
open "$DESTINATION"
echo "Installed and opened $DESTINATION"
