#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TAURI_DIR="$REPO_ROOT/src-tauri"
RELEASE_KIND="${1:-}"

usage() {
  cat <<'EOF'
Usage: ./scripts/publish-release.sh small|patch|minor|major

  small, patch  1.6.1 -> 1.6.2
  minor         1.6.1 -> 1.7.0
  major         1.6.1 -> 2.0.0
EOF
}

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

case "$RELEASE_KIND" in
  small|patch|minor|major) ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: releases must be built on macOS" >&2
  exit 1
fi

for tool in git gh perl; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: required command '$tool' was not found" >&2
    exit 1
  fi
done
CARGO="$(find_cargo)"

cd "$REPO_ROOT"

if [[ -n "$(git status --porcelain)" ]]; then
  echo "error: the working tree must be clean before publishing" >&2
  exit 1
fi
if [[ "$(git branch --show-current)" != "main" ]]; then
  echo "error: releases must be published from the main branch" >&2
  exit 1
fi

gh auth status >/dev/null
git fetch origin main --tags
if ! git merge-base --is-ancestor origin/main HEAD; then
  echo "error: local main is behind or has diverged from origin/main" >&2
  exit 1
fi

CURRENT="$(sed -n 's/^version = "\([0-9][0-9.]*\)"/\1/p' "$TAURI_DIR/Cargo.toml" | head -1)"
if [[ ! "$CURRENT" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
  echo "error: Cargo.toml does not contain a three-part semantic version" >&2
  exit 1
fi

MAJOR="${BASH_REMATCH[1]}"
MINOR="${BASH_REMATCH[2]}"
PATCH="${BASH_REMATCH[3]}"
case "$RELEASE_KIND" in
  small|patch) NEXT="$MAJOR.$MINOR.$((PATCH + 1))" ;;
  minor)       NEXT="$MAJOR.$((MINOR + 1)).0" ;;
  major)       NEXT="$((MAJOR + 1)).0.0" ;;
esac
TAG="v$NEXT"

if git rev-parse --verify --quiet "refs/tags/$TAG" >/dev/null; then
  echo "error: tag $TAG already exists" >&2
  exit 1
fi
if gh release view "$TAG" >/dev/null 2>&1; then
  echo "error: GitHub release $TAG already exists" >&2
  exit 1
fi

echo "Preparing $TAG from $CURRENT"

CURRENT_VERSION="$CURRENT" NEXT_VERSION="$NEXT" perl -0pi -e '
  s/^version = "\Q$ENV{CURRENT_VERSION}\E"/version = "$ENV{NEXT_VERSION}"/m
' "$TAURI_DIR/Cargo.toml"
CURRENT_VERSION="$CURRENT" NEXT_VERSION="$NEXT" perl -0pi -e '
  s/(\[\[package\]\]\nname = "pear-music-widget"\nversion = ")\Q$ENV{CURRENT_VERSION}\E("\n)/$1$ENV{NEXT_VERSION}$2/
' "$TAURI_DIR/Cargo.lock"
CURRENT_VERSION="$CURRENT" NEXT_VERSION="$NEXT" perl -0pi -e '
  s/("version": ")\Q$ENV{CURRENT_VERSION}\E(",)/$1$ENV{NEXT_VERSION}$2/
' "$TAURI_DIR/tauri.conf.json"

if ! grep -q "^version = \"$NEXT\"$" "$TAURI_DIR/Cargo.toml" ||
   ! grep -A2 '^name = "pear-music-widget"$' "$TAURI_DIR/Cargo.lock" | grep -q "version = \"$NEXT\"" ||
   ! grep -q "\"version\": \"$NEXT\"" "$TAURI_DIR/tauri.conf.json"; then
  echo "error: version files were not updated consistently" >&2
  exit 1
fi

(
  cd "$TAURI_DIR"
  "$CARGO" test
  touch src/main.rs
  "$CARGO" tauri build --bundles dmg --ci
)

shopt -s nullglob
DMG_FILES=("$TAURI_DIR"/target/release/bundle/dmg/*_"$NEXT"_*.dmg)
if [[ "${#DMG_FILES[@]}" -ne 1 ]]; then
  echo "error: expected exactly one DMG for $NEXT, found ${#DMG_FILES[@]}" >&2
  exit 1
fi

DMG_BASENAME="$(basename "${DMG_FILES[0]}")"
ARCH="${DMG_BASENAME%.dmg}"
ARCH="${ARCH##*_${NEXT}_}"
ASSET_DIR="$TAURI_DIR/target/release/upload"
ASSET="$ASSET_DIR/Pear.Music.Widget_${NEXT}_${ARCH}.dmg"
mkdir -p "$ASSET_DIR"
ditto "${DMG_FILES[0]}" "$ASSET"

git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/tauri.conf.json
git commit -m "Cut $NEXT"
git tag -a "$TAG" -m "$TAG"
git push --atomic origin main "$TAG"

gh release create "$TAG" "$ASSET" \
  --verify-tag \
  --title "$TAG" \
  --generate-notes \
  --fail-on-no-commits

echo "Published $TAG"
