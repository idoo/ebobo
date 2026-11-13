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

# Desktop icon for Desktop Mode (executable, so Plasma launches it without asking).
desk="$(xdg-user-dir DESKTOP 2>/dev/null || true)"
if [ -z "$desk" ] || [ "$desk" = "$HOME" ]; then
    desk="$HOME/Desktop"
fi
if [ -d "$desk" ]; then
    install -m 755 "$desktop" "$desk/ebobo.desktop"
fi

# A ROM folder ebobo always scans (EmuDeck's ~/Emulation/roms/nes is scanned too).
mkdir -p "$HOME/ROMs"

version="$("$dest/ebobo" --version 2>/dev/null || echo ebobo)"
echo "$version installed to $dest"
echo "Put .nes or .zip ROMs into ~/ROMs (or EmuDeck's Emulation/roms/nes)."

# Steam Game Mode artwork (capsule, hero, logo, icon) for the shortcut. Steam names grid
# files by the shortcut's appid, which only shortcuts.vdf knows; never fails the install.
steam_art_appid() {
    python3 - "$1" "$dest/ebobo" <<'PY'
import struct, sys
data = open(sys.argv[1], "rb").read()
want = sys.argv[2]
def parse(i):
    d = {}
    while i < len(data):
        t = data[i]; i += 1
        if t == 8:
            return d, i
        e = data.index(b"\0", i); k = data[i:e].decode("utf-8", "replace").lower(); i = e + 1
        if t == 0:
            d[k], i = parse(i)
        elif t == 1:
            e = data.index(b"\0", i); d[k] = data[i:e].decode("utf-8", "replace"); i = e + 1
        elif t == 2:
            d[k] = struct.unpack_from("<I", data, i)[0]; i += 4
        elif t == 7:
            i += 8
        else:
            raise ValueError(t)
    return d, i
root, _ = parse(0)
for e in root.get("shortcuts", {}).values():
    if isinstance(e, dict) and str(e.get("appname", "")).lower() == "ebobo" \
            and want in str(e.get("exe", "")) and "appid" in e:
        print(e["appid"] & 0xFFFFFFFF)
        break
PY
}

install_steam_art() {
    [ -d "$here/steam" ] && command -v python3 >/dev/null 2>&1 || return 0
    local tries=$1 n=0 ok=0 vdf appid grid
    while :; do
        for vdf in "$HOME"/.steam/steam/userdata/*/config/shortcuts.vdf; do
            [ -f "$vdf" ] || continue
            appid="$(steam_art_appid "$vdf" 2>/dev/null || true)"
            [ -n "$appid" ] || continue
            grid="$(dirname "$vdf")/grid"
            mkdir -p "$grid"
            cp -f "$here/steam/capsule.png" "$grid/${appid}p.png"
            cp -f "$here/steam/wide.png" "$grid/${appid}.png"
            cp -f "$here/steam/hero.png" "$grid/${appid}_hero.png"
            cp -f "$here/steam/logo.png" "$grid/${appid}_logo.png"
            cp -f "$here/steam/icon.png" "$grid/${appid}_icon.png"
            ok=1
        done
        n=$((n + 1))
        if [ "$ok" = 1 ] || [ "$n" -ge "$tries" ]; then break; fi
        sleep 1
    done
    if [ "$ok" = 1 ]; then
        echo "Steam artwork installed (shows after Steam restarts / in Game Mode)."
    fi
}

# The Steam shortcut points at the stable path above, so updates only replace the binary.
marker="$dest/.added-to-steam"
if [ -f "$marker" ]; then
    echo "Already in Steam (updated in place)."
    install_steam_art 1 || true
elif command -v steamos-add-to-steam >/dev/null 2>&1; then
    if steamos-add-to-steam "$desktop"; then
        touch "$marker"
        echo "Added to Steam: find ebobo under Non-Steam in Game Mode."
        install_steam_art 15 || true
    fi
else
    echo "To play from Game Mode: Steam → Add a Game → Add a Non-Steam Game → ebobo."
fi
