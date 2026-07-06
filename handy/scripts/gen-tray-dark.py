#!/usr/bin/env python3
"""Regenerate tray_idle_dark.png from tray_idle.png: same glyph and alpha, ink RGB.

tray_idle.png is the light glyph (for dark taskbars); the _dark variant is the SAME
shape recolored near-black for LIGHT taskbars on Windows/Linux (macOS ignores it —
the template/alpha mask handles theming there). See tray.rs `tray_icon_path`.
"""

from pathlib import Path

from PIL import Image

RES = Path(__file__).resolve().parent.parent / "src-tauri" / "resources"
INK = (10, 10, 10)  # --color-text light theme, App.css

src = Image.open(RES / "tray_idle.png").convert("RGBA")
alpha = src.getchannel("A")
dark = Image.new("RGBA", src.size, INK + (0,))
dark.putalpha(alpha)
dark.save(RES / "tray_idle_dark.png")
print(f"wrote {RES / 'tray_idle_dark.png'} ({src.size[0]}x{src.size[1]})")
