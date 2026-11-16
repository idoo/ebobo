#!/usr/bin/env python3
"""Draws the picture at the top of the README into docs/readme.png.

Two pictures of the running app go in, both written by ebobo's own --screenshot:
  GAME  a game through the CRT effect, 1280x800
        (ebobo ROM --effect crt --enhance depth --screen fit --screenshot-size 1280x800)
  ROOM  a host's room with its guests in it, 256x240
        (ebobo --host ROM --screenshot-size 256x240, guests started with --join CODE)
The ground, the icon and the lettering are those of the Steam artwork (make-steam-art.py).
Run: python3 scripts/make-readme-art.py GAME.png ROOM.png
"""
import importlib.util
import sys
from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("steam_art", HERE / "make-steam-art.py")
art = importlib.util.module_from_spec(spec)
spec.loader.exec_module(art)

W, H, PX = 1600, 800, 5
MARGIN = 75


def screen(base: Image.Image, img: Image.Image, x: int, y: int):
    """`img` at (x, y) in a white pixel frame with a blocky shadow."""
    d = ImageDraw.Draw(base)
    w, h = img.size
    d.rectangle([x + PX, y + PX, x + w + 3 * PX - 1, y + h + 3 * PX - 1], fill=art.SHADOW)
    d.rectangle([x - PX, y - PX, x + w + PX - 1, y + h + PX - 1], fill=art.WHITE)
    base.paste(img, (x, y))


def main():
    game = Image.open(sys.argv[1]).convert("RGB").resize((800, 500), Image.LANCZOS)
    room = art.scale(Image.open(sys.argv[2]).convert("RGB"), 2)
    img = art.ground(W, H, PX)

    art.paste(img, art.scale(art.native_icon(), 4), MARGIN, 45)
    art.paste(img, art.text_layer("ebobo", 10), MARGIN + 165, 70)
    for i, line in enumerate(["NES AND MEGA DRIVE", "ONLINE WITH FRIENDS"]):
        text = art.text_layer(line, 3)
        art.paste(img, text, W - MARGIN - text.width, 75 + i * 45)

    gx, gy = MARGIN, 230
    rx = (W - MARGIN - room.width) // PX * PX
    ry = gy + (game.height - room.height) // 2
    mid = gy + game.height // 2
    art.dotted_link(img, (gx + game.width + 4 * PX, mid), (rx - 3 * PX, mid), PX, (*art.WHITE, 255))
    screen(img, game, gx, gy)
    screen(img, room, rx, ry)

    out = HERE.parent / "docs/readme.png"
    img.convert("RGB").save(out, optimize=True)
    print(out.relative_to(HERE.parent), img.size)


if __name__ == "__main__":
    main()
