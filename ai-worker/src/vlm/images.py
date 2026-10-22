"""送模型的输入图（同 memora `images.prepare`）：按 EXIF 方向校正、长边缩到 1024、JPEG 质量 88。

原图字节由 Rust 读好再发过来（读 NAS 要过 Rust 的读闸门，docs/10 §4），这里只做解码和缩放。
HEIC / HEIF 用 pillow-heif 解码。
"""

from __future__ import annotations

import io

from PIL import Image, ImageOps

try:  # pillow-heif 缺失时 HEIC 解码失败，按"格式不支持"处理，其他格式不受影响。
    from pillow_heif import register_heif_opener

    register_heif_opener()
    HEIF_SUPPORTED = True
except ImportError:  # pragma: no cover - 取决于环境
    HEIF_SUPPORTED = False

JPEG_QUALITY = 88
# 防止超大图解码爆内存：1.5 亿像素以上视为异常文件。
Image.MAX_IMAGE_PIXELS = 150_000_000


def load(data: bytes, min_edge: int | None = None) -> Image.Image:
    """解码并按 EXIF 方向校正为 RGB；透明图铺白底。JPEG 按 min_edge 降采样解码，省时省内存。"""
    img = Image.open(io.BytesIO(data))
    if min_edge and img.format == "JPEG":
        img.draft("RGB", (min_edge, min_edge))
    img = ImageOps.exif_transpose(img)
    if img.mode in ("RGBA", "LA", "PA") or (img.mode == "P" and "transparency" in img.info):
        rgba = img.convert("RGBA")
        background = Image.new("RGB", rgba.size, (255, 255, 255))
        background.paste(rgba, mask=rgba.getchannel("A"))
        return background
    return img.convert("RGB")


def shrink(img: Image.Image, edge: int) -> Image.Image:
    img = img.copy()
    img.thumbnail((edge, edge), Image.LANCZOS)
    return img


def to_jpeg(img: Image.Image, quality: int = JPEG_QUALITY) -> bytes:
    buffer = io.BytesIO()
    img.save(buffer, "JPEG", quality=quality, optimize=True)
    return buffer.getvalue()


def prepare(data: bytes, long_edge: int) -> bytes:
    """原图字节 → 送模型的 JPEG（长边不超过 long_edge）。"""
    return to_jpeg(shrink(load(data, long_edge), long_edge))
