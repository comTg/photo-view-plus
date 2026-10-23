"""VLM 样本的人工抽查拼图（docs/10 G3）：每格一张缩略图 + 类别、前 5 个标签、描述开头。

用法（ai-worker 的虚拟环境或任何装了 Pillow 的 Python）：
    python scripts/search_eval/vlm_sheets.py --sample sample_1024.jsonl

输入输出都在评测目录 `%LOCALAPPDATA%\\PhotoViewPlus\\eval\\vlm\\`（含私人照片，不进仓库）：
- 拼图：`sheets/<样本名>_NN.jpg`，每张 8 格（4 × 2），格子左上角是全局序号；
- 索引：`sheets/<样本名>_index.json`，序号 → 图片 id、标签，供记录判定和计分（score_g3）。
"""

from __future__ import annotations

import argparse
import json
import os
import textwrap
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

FONT_PATH = r"C:\Windows\Fonts\msyh.ttc"
COLS, ROWS = 4, 2
CELL_W, THUMB_H, TEXT_H = 380, 300, 170


def font(size: int):
    try:
        return ImageFont.truetype(FONT_PATH, size)
    except OSError:
        return ImageFont.load_default()


def eval_dir() -> Path:
    base = os.environ.get("PVP_EVAL_DIR") or str(Path(os.environ["LOCALAPPDATA"]) / "PhotoViewPlus" / "eval")
    return Path(base) / "vlm"


def thumbs_dir() -> Path:
    base = os.environ.get("PVP_EVAL_THUMBS") or str(
        Path(os.environ["LOCALAPPDATA"]) / "com.vetoer.photoviewplus" / "thumbs"
    )
    return Path(base)


def load_records(path: Path) -> list[dict]:
    records = []
    for line in path.read_text(encoding="utf-8").splitlines():
        record = json.loads(line)
        result = ((record.get("response") or {}).get("result")) if record.get("ok") else None
        if result and record.get("thumb_hash"):
            records.append({"record": record, "result": result})
    records.sort(key=lambda item: item["record"]["image_id"])
    return records


def cell(item: dict, number: int, fonts) -> Image.Image:
    title_font, text_font = fonts
    img = Image.new("RGB", (CELL_W, THUMB_H + TEXT_H), (24, 24, 28))
    thumb_hash = item["record"]["thumb_hash"]
    # docs/11 R4 起缩略图是 JPEG；还没转换的旧缩略图是 WebP。
    path = thumbs_dir() / thumb_hash[:2] / f"{thumb_hash}.jpg"
    if not path.exists():
        path = path.with_suffix(".webp")
    try:
        thumb = Image.open(path).convert("RGB")
        thumb.thumbnail((CELL_W, THUMB_H))
        img.paste(thumb, ((CELL_W - thumb.width) // 2, (THUMB_H - thumb.height) // 2))
    except OSError:
        pass
    draw = ImageDraw.Draw(img)
    draw.rectangle((0, 0, 44, 30), fill=(0, 0, 0))
    draw.text((6, 2), str(number), fill=(255, 230, 120), font=title_font)
    result = item["result"]
    tags = [tag["name"] for tag in result.get("tags", [])][:5]
    lines = [
        f"类别：{'、'.join(result.get('categories', []))}  人数：{result.get('people_count')}",
        "标签：" + "  ".join(f"{index + 1}.{name}" for index, name in enumerate(tags)),
    ]
    lines += textwrap.wrap(result.get("description", ""), 24)[:4]
    y = THUMB_H + 6
    for line in lines:
        draw.text((8, y), line, fill=(235, 235, 235), font=text_font)
        y += 26
    return img


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--sample", default="sample_1024.jsonl")
    args = parser.parse_args()
    base = eval_dir()
    items = load_records(base / args.sample)
    out_dir = base / "sheets"
    out_dir.mkdir(parents=True, exist_ok=True)
    stem = Path(args.sample).stem
    fonts = (font(22), font(17))
    per_sheet = COLS * ROWS
    index = {}
    for sheet_no in range(0, (len(items) + per_sheet - 1) // per_sheet):
        sheet = Image.new("RGB", (COLS * CELL_W, ROWS * (THUMB_H + TEXT_H)), (10, 10, 12))
        for slot in range(per_sheet):
            position = sheet_no * per_sheet + slot
            if position >= len(items):
                break
            item = items[position]
            number = position + 1
            sheet.paste(cell(item, number, fonts), ((slot % COLS) * CELL_W, (slot // COLS) * (THUMB_H + TEXT_H)))
            index[number] = {
                "image_id": item["record"]["image_id"],
                "key": item["record"]["key"],
                "categories": item["result"].get("categories", []),
                "tags": [tag["name"] for tag in item["result"].get("tags", [])][:5],
            }
        sheet.save(out_dir / f"{stem}_{sheet_no + 1:02d}.jpg", quality=88)
    (out_dir / f"{stem}_index.json").write_text(json.dumps(index, ensure_ascii=False, indent=1), encoding="utf-8")
    print(f"{len(items)} images -> {out_dir} ({(len(items) + per_sheet - 1) // per_sheet} sheets)")


if __name__ == "__main__":
    main()
