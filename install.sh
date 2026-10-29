#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

echo "Building palicode (release)…"
cargo build --release

TARGET_DIR="$(cargo metadata --format-version 1 --no-deps \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
BIN="$TARGET_DIR/release/palicode"
if [[ ! -x "$BIN" ]]; then
  echo "error: release binary not found at $BIN" >&2
  exit 1
fi

install -d "$HOME/.local/bin"
install -m755 "$BIN" "$HOME/.local/bin/palicode"
echo "Installed: $HOME/.local/bin/palicode"
echo "Run: palicode   (from any project directory)"
