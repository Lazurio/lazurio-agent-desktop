#!/usr/bin/env python3
"""Draws the placeholder icons of Lazurio Agent Desktop (needs Pillow).

    python3 scripts/make-icons.py
    npx --yes @tauri-apps/cli@2.12.4 icon src-tauri/icons/source.png -o src-tauri/icons

source.png is the app icon; tray-*.png are macOS menu bar template icons
(black on transparent); tray-win-*.png are the Windows notification area icons.
The icons are neutral placeholders until Lazurio has a brand icon for the app.
"""
from pathlib import Path
from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parent.parent / "src-tauri" / "icons"
OUT.mkdir(parents=True, exist_ok=True)

GRAPHITE = (47, 50, 55, 255)
WHITE = (255, 255, 255, 255)
GREEN = (52, 168, 83, 255)
AMBER = (232, 160, 32, 255)


def screen_and_cursor(d, s, fg, bg, cursor=True):
    """A screen outline with a pointer arrow in its lower right, on an s×s canvas."""
    w = max(1, round(s * 0.07))
    d.rounded_rectangle([s * 0.12, s * 0.2, s * 0.88, s * 0.72], radius=s * 0.06, outline=fg, width=w)
    d.line([s * 0.38, s * 0.84, s * 0.62, s * 0.84], fill=fg, width=w)
    d.line([s * 0.5, s * 0.72, s * 0.5, s * 0.84], fill=fg, width=w)
    if cursor:
        x, y = s * 0.55, s * 0.42
        arrow = [(x, y), (x, y + s * 0.4), (x + s * 0.1, y + s * 0.3), (x + s * 0.29, y + s * 0.29)]
        d.polygon(arrow, fill=fg, outline=bg, width=max(1, round(s * 0.03)))


def app_icon():
    s = 1024
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle([64, 64, s - 64, s - 64], radius=200, fill=GRAPHITE)
    inner = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    screen_and_cursor(ImageDraw.Draw(inner), s, WHITE, GRAPHITE)
    inner = inner.resize((int(s * 0.7), int(s * 0.7)), Image.LANCZOS)
    img.alpha_composite(inner, (int(s * 0.15), int(s * 0.15)))
    img.save(OUT / "source.png")


def tray(name, dot=None, cursor=False, windows=False):
    s = 256
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    if windows:
        d.rounded_rectangle([8, 8, s - 8, s - 8], radius=48, fill=GRAPHITE)
        fg, bg = WHITE, GRAPHITE
    else:
        fg, bg = (0, 0, 0, 255), (0, 0, 0, 0)
    screen_and_cursor(d, s, fg, bg, cursor)
    if dot is not None:
        r = s * 0.15
        cx, cy = s * 0.8, s * 0.22
        color = dot if windows else (0, 0, 0, 255)
        d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=color, outline=bg, width=round(s * 0.04))
    size = 32 if windows else 36
    img.resize((size, size), Image.LANCZOS).save(OUT / f"{name}.png")


app_icon()
tray("tray-off")
tray("tray-on", dot=GREEN)
tray("tray-busy", dot=AMBER, cursor=True)
tray("tray-win-off", windows=True)
tray("tray-win-on", dot=GREEN, windows=True)
tray("tray-win-busy", dot=AMBER, cursor=True, windows=True)
