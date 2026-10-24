#!/usr/bin/env bash
# ebobo installer for Steam Deck (Desktop Mode → Konsole) and other Linux desktops.
# Installs into ~/Applications/ebobo, adds a menu entry and, on SteamOS, a Steam shortcut so
# ebobo starts from Game Mode. No sudo, nothing outside $HOME.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
dest="$HOME/Applications/ebobo"

mkdir -p "$dest"
install -m 755 "$here/ebobo" "$dest/ebobo"
if [ -f "$here/ebobo.png" ]; then
    install -m 644 "$here/ebobo.png" "$dest/ebobo.png"
fi

apps="$HOME/.local/share/applications"
mkdir -p "$apps"
desktop="$apps/ebobo.desktop"
cat > "$desktop" <<EOF
[Desktop Entry]
Type=Application
Name=ebobo
Comment=NES online with friends
Exec=$dest/ebobo
Icon=$dest/ebobo.png
Terminal=false
Categories=Game;Emulator;
EOF
chmod +x "$desktop"

# A ROM folder ebobo always scans (EmuDeck's ~/Emulation/roms/nes is scanned too).
mkdir -p "$HOME/ROMs"

version="$("$dest/ebobo" --version 2>/dev/null || echo ebobo)"
echo "$version installed to $dest"
echo "Put .nes or .zip ROMs into ~/ROMs (or EmuDeck's Emulation/roms/nes)."

# The Steam shortcut points at the stable path above, so updates only replace the binary.
marker="$dest/.added-to-steam"
if [ -f "$marker" ]; then
    echo "Already in Steam (updated in place)."
elif command -v steamos-add-to-steam >/dev/null 2>&1; then
    if steamos-add-to-steam "$desktop"; then
        touch "$marker"
        echo "Added to Steam: find ebobo under Non-Steam in Game Mode."
    fi
else
    echo "To play from Game Mode: Steam → Add a Game → Add a Non-Steam Game → ebobo."
fi
