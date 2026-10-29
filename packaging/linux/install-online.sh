#!/usr/bin/env bash
# One-line ebobo installer for Steam Deck (Desktop Mode, Konsole) and other x86_64 Linux:
#   curl -sL https://github.com/idoo/ebobo/releases/latest/download/install-steamdeck.sh | bash
# Published as the release asset install-steamdeck.sh. It installs ebobo into
# ~/Applications/ebobo and adds a menu entry and a Steam shortcut (once). No sudo.
set -euo pipefail

url="${EBOBO_URL:-https://github.com/idoo/ebobo/releases/latest/download/ebobo-steamdeck.tar.gz}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading ebobo…"
curl -fL --progress-bar "$url" -o "$tmp/ebobo.tar.gz"
tar -xzf "$tmp/ebobo.tar.gz" -C "$tmp"
bash "$tmp/ebobo-steamdeck/install.sh"
