"""Draw Yonder's app icon: a sun setting beyond far hills.

Writes a 1024x1024 PNG; `npx tauri icon` turns it into every size and format.
Usage: python3 scripts/make-icon.py app/src-tauri/icons/source.png
"""
import sys
from PIL import Image, ImageDraw

S = 4  # supersampling factor for smooth edges
N = 1024 * S
img = Image.new("RGBA", (N, N), (0, 0, 0, 0))

# macOS icon grid: an 824px rounded square centred on the canvas.
inset, size, radius = 100 * S, 824 * S, 185 * S
box = (inset, inset, inset + size, inset + size)

# Sky: vertical gradient from deep indigo to dusk violet.
sky = Image.new("RGBA", (N, N))
top, bottom = (24, 28, 64), (92, 64, 140)
d = ImageDraw.Draw(sky)
for y in range(N):
    t = y / N
    d.line([(0, y), (N, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)) + (255,))

scene = ImageDraw.Draw(sky)
cx, horizon = N // 2, inset + int(size * 0.62)
r = int(size * 0.17)
scene.ellipse((cx - r, horizon - r, cx + r, horizon + r), fill=(255, 196, 102, 255))
# Far hills, then near hills, overlapping the lower half of the sun.
scene.ellipse((inset - size * 0.3, horizon - size * 0.06, inset + size * 0.75, horizon + size * 0.5),
              fill=(58, 46, 108, 255))
scene.ellipse((inset + size * 0.35, horizon - size * 0.02, inset + size * 1.4, horizon + size * 0.6),
              fill=(44, 36, 88, 255))
scene.rectangle((0, horizon + size * 0.12, N, N), fill=(30, 26, 66, 255))

mask = Image.new("L", (N, N), 0)
ImageDraw.Draw(mask).rounded_rectangle(box, radius=radius, fill=255)
img.paste(sky, (0, 0), mask)
img.resize((1024, 1024), Image.LANCZOS).save(sys.argv[1])
