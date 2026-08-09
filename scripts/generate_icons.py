from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageDraw


ROOT = Path(__file__).resolve().parents[1]
ICON_DIR = ROOT / "apps" / "desktop" / "src-tauri" / "icons"
CANVAS_SIZE = 1024


def linear_gradient(size: tuple[int, int], start: str, end: str) -> Image.Image:
    width, height = size
    start_rgb = tuple(bytes.fromhex(start.removeprefix("#")))
    end_rgb = tuple(bytes.fromhex(end.removeprefix("#")))
    image = Image.new("RGBA", size)
    pixels = image.load()
    for y in range(height):
        for x in range(width):
            ratio = (x + y) / max(width + height - 2, 1)
            color = tuple(round(a + (b - a) * ratio) for a, b in zip(start_rgb, end_rgb, strict=True))
            pixels[x, y] = (*color, 255)
    return image


def petal(size: tuple[int, int], colors: tuple[str, str], angle: float, opacity: int) -> Image.Image:
    width, height = size
    mask = Image.new("L", size)
    ImageDraw.Draw(mask).ellipse((0, 0, width - 1, height - 1), fill=opacity)
    layer = linear_gradient(size, *colors)
    layer.putalpha(mask)
    return layer.rotate(angle, resample=Image.Resampling.BICUBIC, expand=True)


def paste_center(canvas: Image.Image, layer: Image.Image, center: tuple[int, int]) -> None:
    left = round(center[0] - layer.width / 2)
    top = round(center[1] - layer.height / 2)
    canvas.alpha_composite(layer, (left, top))


def build_master() -> Image.Image:
    canvas = Image.new("RGBA", (CANVAS_SIZE, CANVAS_SIZE), (0, 0, 0, 0))
    paste_center(canvas, petal((328, 540), ("#67CDF4", "#4D84E5"), 0, 240), (512, 320))
    paste_center(canvas, petal((328, 540), ("#A69BE9", "#6654C4"), 54, 228), (324, 644))
    paste_center(canvas, petal((328, 540), ("#F5AAB7", "#ED7E94"), -54, 228), (700, 644))
    return canvas


def main() -> None:
    ICON_DIR.mkdir(parents=True, exist_ok=True)
    master = build_master()
    for size in (32, 128, 256, 512):
        resized = master.resize((size, size), Image.Resampling.LANCZOS)
        resized.save(ICON_DIR / f"{size}x{size}.png", optimize=True)
    master.save(
        ICON_DIR / "icon.ico",
        format="ICO",
        sizes=[(16, 16), (20, 20), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )


if __name__ == "__main__":
    main()
