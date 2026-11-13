#!/usr/bin/env python3
"""Draws the Steam library artwork for the ebobo non-Steam shortcut into packaging/steam/.

Everything is derived from packaging/icon/ebobo.png (a 32x32 pixel-art grid drawn at 16x) and
the Press Start 2P font the frontend embeds, scaled by whole factors so pixels stay crisp.
Run: python3 scripts/make-steam-art.py
"""
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
ICON = ROOT / "packaging/icon/ebobo.png"
FONT = ROOT / "crates/ebobo/assets/PressStart2P-Regular.ttf"
OUT = ROOT / "packaging/steam"

RED_TOP = (233, 66, 50)
RED_BOTTOM = (176, 38, 30)
SHADOW = (110, 20, 14)
WHITE = (250, 246, 236)
# Player colours: brand red, blue, green, yellow (the tile ground of each small pad).
PLAYERS = [(227, 61, 46), (52, 120, 230), (60, 178, 92), (240, 186, 40)]


def native_icon() -> Image.Image:
    """The icon at its native 32x32 pixel grid (sampled at cell centres)."""
    big = Image.open(ICON).convert("RGBA")
    cell = big.width // 32
    out = Image.new("RGBA", (32, 32))
    for y in range(32):
        for x in range(32):
            out.putpixel((x, y), big.getpixel((x * cell + cell // 2, y * cell + cell // 2)))
    return out


def scale(img: Image.Image, k: int) -> Image.Image:
    return img.resize((img.width * k, img.height * k), Image.NEAREST)


def tint(icon: Image.Image, colour) -> Image.Image:
    """Recolours the red tile (and its darker shadow) of the icon to a player colour."""
    out = icon.copy()
    dark = tuple(int(c * 0.72) for c in colour)
    px = out.load()
    for y in range(out.height):
        for x in range(out.width):
            r, g, b, a = px[x, y]
            if a and r > 150 and g < 90 and b < 80:
                # The tile ground; the pad's red buttons are inside the dark pad and stay red.
                inside_pad = 6 <= x <= 25 and 12 <= y <= 20
                if not inside_pad:
                    px[x, y] = (*colour, a)
            elif a and 120 < r <= 200 and g < 60 and b < 50:
                px[x, y] = (*dark, a)
    return out


def ground(w: int, h: int, px: int) -> Image.Image:
    """Red 2-stop gradient in px-tall bands, dithered at band edges, with faint scanlines."""
    img = Image.new("RGB", (w, h))
    d = ImageDraw.Draw(img)
    bands = 12
    for y in range(0, h, px):
        t = y / max(1, h - 1)
        band = min(bands - 1, int(t * bands))
        frac = t * bands - band
        lo = [RED_TOP[i] + (RED_BOTTOM[i] - RED_TOP[i]) * band / (bands - 1) for i in range(3)]
        hi = [RED_TOP[i] + (RED_BOTTOM[i] - RED_TOP[i]) * min(band + 1, bands - 1) / (bands - 1) for i in range(3)]
        row_y = y // px
        for x in range(0, w, px):
            # Ordered 2x2 dither between this band and the next in the last half of the band.
            thr = ((row_y & 1) * 2 + ((x // px) & 1)) / 4 * 0.5 + 0.5
            c = hi if frac > thr else lo
            d.rectangle([x, y, x + px - 1, y + px - 1], fill=tuple(int(v) for v in c))
    # Scanlines: darken one pixel-row in every four.
    lines = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    ld = ImageDraw.Draw(lines)
    for y in range(px * 3, h, px * 4):
        ld.rectangle([0, y, w, y + px - 1], fill=(0, 0, 0, 18))
    return Image.alpha_composite(img.convert("RGBA"), lines)


def text_layer(text: str, unit: int, fill=WHITE, shadow=SHADOW) -> Image.Image:
    """Pixel-font text: rendered at 8 px (one font pixel per image pixel), scaled by unit."""
    font = ImageFont.truetype(str(FONT), 8)
    left, top, right, bottom = font.getbbox(text)
    w, h = right - left + 1, bottom - top + 1
    small = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    d = ImageDraw.Draw(small)
    d.fontmode = "1"
    if shadow is not None:
        d.text((-left + 1, -top + 1), text, font=font, fill=(*shadow, 255))
    d.text((-left, -top), text, font=font, fill=(*fill, 255))
    return scale(small.crop(small.getbbox()), unit)


def paste(base: Image.Image, img: Image.Image, x: int, y: int):
    base.alpha_composite(img, (x, y))


def arc(base: Image.Image, cx: int, cy: int, radius: int, px: int, colour, start, end):
    """A blocky quarter-ish arc of px-sized squares between angles (degrees)."""
    import math

    d = ImageDraw.Draw(base)
    steps = max(8, int(radius * math.radians(end - start) / px))
    seen = set()
    for i in range(steps + 1):
        a = math.radians(start + (end - start) * i / steps)
        gx = round((cx + radius * math.cos(a)) / px)
        gy = round((cy - radius * math.sin(a)) / px)
        if (gx, gy) in seen:
            continue
        seen.add((gx, gy))
        d.rectangle([gx * px, gy * px, gx * px + px - 1, gy * px + px - 1], fill=colour)


def dotted_link(base: Image.Image, a, b, px: int, colour):
    """Dashed straight 'signal' line of px squares between points a and b."""
    d = ImageDraw.Draw(base)
    (x0, y0), (x1, y1) = a, b
    n = int(max(abs(x1 - x0), abs(y1 - y0)) / px)
    for i in range(0, n + 1):
        if (i // 2) % 2:
            continue
        x = round((x0 + (x1 - x0) * i / max(1, n)) / px) * px
        y = round((y0 + (y1 - y0) * i / max(1, n)) / px) * px
        d.rectangle([x, y, x + px - 1, y + px - 1], fill=colour)


def four_pads(base: Image.Image, icon, cx: int, cy: int, k: int, gap: int, px: int):
    """Four player-tinted tiles in a 2x2 around (cx, cy), linked by dashed signal lines."""
    size = 32 * k
    centres = []
    for i, colour in enumerate(PLAYERS):
        x = cx + (gap // 2 if i % 2 else -gap // 2 - size)
        y = cy + (gap // 2 if i // 2 else -gap // 2 - size)
        centres.append((x + size // 2, y + size // 2))
    link = (*WHITE, 200)
    overlay = Image.new("RGBA", base.size, (0, 0, 0, 0))
    for a, b in [(0, 1), (2, 3), (0, 2), (1, 3)]:
        dotted_link(overlay, centres[a], centres[b], px, link)
    base.alpha_composite(overlay)
    for i, colour in enumerate(PLAYERS):
        x, y = centres[i][0] - size // 2, centres[i][1] - size // 2
        paste(base, scale(tint(icon, colour), k), x, y)
    return centres


def capsule(icon) -> Image.Image:
    w, h = 600, 900
    img = ground(w, h, 6)
    # Big pad sprite: the icon at 10x (320 px), centred in the upper part.
    big = scale(icon, 10)
    paste(img, big, (w - big.width) // 2, 40)
    word = text_layer("ebobo", 12)
    paste(img, word, (w - word.width) // 2, 390)
    tag = text_layer("NES ONLINE WITH FRIENDS", 3)
    paste(img, tag, (w - tag.width) // 2, 390 + word.height + 30)
    # Four players, kept above the bottom 15 % (765 px).
    four_pads(img, icon, w // 2, 664, 2, 36, 6)
    return img.convert("RGB")


def wide(icon) -> Image.Image:
    w, h = 920, 430
    img = ground(w, h, 5)
    big = scale(icon, 10)
    paste(img, big, 50, (h - big.height) // 2)
    word = text_layer("ebobo", 10)
    tag = text_layer("NES ONLINE WITH FRIENDS", 2)
    x = 50 + big.width + (w - 50 - big.width - word.width) // 2
    y = (h - word.height - 40 - tag.height) // 2
    paste(img, word, x, y)
    paste(img, tag, x + (word.width - tag.width) // 2, y + word.height + 40)
    return img.convert("RGB")


def hero(icon) -> Image.Image:
    w, h = 3840, 1240
    img = ground(w, h, 10)
    # Faint scattered tiles across the whole hero as texture.
    ghost = scale(icon, 4)
    alpha = ghost.getchannel("A").point(lambda a: a * 22 // 100)
    ghost.putalpha(alpha)
    for row, y in enumerate(range(20, h, 200)):
        for x in range(20 + (row % 2) * 160, w, 320):
            paste(img, ghost, x, y)
    # The scene: four player pads linked by signal lines on the right half, away from the
    # centre-left where Steam draws the logo.
    four_pads(img, icon, 2900, h // 2, 8, 200, 10)
    return img.convert("RGB")


def logo() -> Image.Image:
    word = text_layer("ebobo", 32)
    pad = 16
    out = Image.new("RGBA", (word.width + 2 * pad, word.height + 2 * pad), (0, 0, 0, 0))
    out.alpha_composite(word, (pad, pad))
    return out


def icon256() -> Image.Image:
    return Image.open(ICON).convert("RGBA").resize((256, 256), Image.NEAREST)


def board(paths) -> Image.Image:
    imgs = [Image.open(p).convert("RGBA") for p in paths]
    checker = lambda im: Image.alpha_composite(Image.new("RGBA", im.size, (40, 40, 48, 255)), im)
    # Row 1: hero full width; row 2: capsule, wide, logo, icon.
    W = 1500
    hero_img = imgs[2].resize((W - 40, int((W - 40) * imgs[2].height / imgs[2].width)), Image.LANCZOS)
    cap = imgs[0].resize((400, 600), Image.LANCZOS)
    wd = imgs[1].resize((600, 280), Image.LANCZOS)
    lg = checker(imgs[3]).resize((600, int(600 * imgs[3].height / imgs[3].width)), Image.LANCZOS)
    ic = checker(imgs[4])
    H = 20 + hero_img.height + 20 + 600 + 20
    out = Image.new("RGBA", (W, H), (24, 24, 28, 255))
    out.alpha_composite(hero_img, (20, 20))
    y = 40 + hero_img.height
    out.alpha_composite(cap, (20, y))
    out.alpha_composite(wd, (440, y))
    out.alpha_composite(lg, (440, y + 300))
    out.alpha_composite(ic, (1060, y))
    return out.convert("RGB")


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    icon = native_icon()
    outputs = {
        "capsule.png": capsule(icon),
        "wide.png": wide(icon),
        "hero.png": hero(icon),
        "logo.png": logo(),
        "icon.png": icon256(),
    }
    for name, img in outputs.items():
        img.save(OUT / name, optimize=True)
        print(name, img.size, img.mode)
    board([OUT / n for n in outputs]).save("/tmp/steam-art-board.png")


if __name__ == "__main__":
    main()
