"""池化：取各方案前 POOL_DEPTH 名的并集，把还没标注过的图拼成带编号的拼图，供标注。

用法：python scripts/search_eval/make_sheets.py [方案,...]   （默认全部已跑的方案）
输出：<eval_dir>/sheets/<qid>_<n>.jpg 与同名 .json（格子编号 → 语料 key）。只生成需要标注的新拼图。
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

from common import eval_dir, load_corpus, load_queries

POOL_DEPTH = 20
COLS, ROWS, CELL, TITLE_H = 6, 5, 190, 44
FONT_PATH = r"C:\Windows\Fonts\msyh.ttc"


def load_judgments() -> dict[str, dict[str, int]]:
    path = eval_dir() / "judgments.json"
    return json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}


def font(size: int):
    try:
        return ImageFont.truetype(FONT_PATH, size)
    except OSError:
        return ImageFont.load_default()


def tile(item: dict) -> Image.Image:
    canvas = Image.new("RGB", (CELL, CELL), (40, 40, 40))
    src = item.get("thumb") if item.get("thumb") and Path(item["thumb"]).exists() else item.get("memora_thumb")
    if src:
        try:
            with Image.open(src) as img:
                img = img.convert("RGB")
                img.thumbnail((CELL - 6, CELL - 6))
                canvas.paste(img, ((CELL - img.width) // 2, (CELL - img.height) // 2))
        except Exception:
            pass
    return canvas


def main() -> None:
    corpus = {c["key"]: c for c in load_corpus()}
    queries = load_queries()
    runs_dir = eval_dir() / "runs"
    wanted = sys.argv[1].split(",") if len(sys.argv) > 1 else None
    runs = {
        p.stem: json.loads(p.read_text(encoding="utf-8"))
        for p in sorted(runs_dir.glob("*.json"))
        if wanted is None or p.stem in wanted
    }
    judgments = load_judgments()
    sheets_dir = eval_dir() / "sheets"
    sheets_dir.mkdir(parents=True, exist_ok=True)
    title_font, num_font = font(24), font(22)
    made = []
    for q in queries:
        pool: list[str] = []
        for data in runs.values():
            for key in data.get(q["id"], [])[:POOL_DEPTH]:
                if key not in pool:
                    pool.append(key)
        existing = sorted(sheets_dir.glob(f"{q['id']}_*.json"))
        # 已经出过拼图的图（不管标没标）都不再重复出图
        on_sheets = {k for p in existing for k in json.loads(p.read_text(encoding="utf-8"))}
        todo = [k for k in pool if k not in judgments.get(q["id"], {}) and k not in on_sheets]
        next_no = len(existing) + 1
        per_sheet = COLS * ROWS
        for start in range(0, len(todo), per_sheet):
            chunk = todo[start : start + per_sheet]
            sheet = Image.new("RGB", (COLS * CELL, ROWS * CELL + TITLE_H), (20, 20, 20))
            draw = ImageDraw.Draw(sheet)
            name = f"{q['id']}_{next_no}"
            draw.text((10, 8), f"{name}  查询：{q['text']}", fill=(255, 230, 120), font=title_font)
            for i, key in enumerate(chunk):
                x, y = (i % COLS) * CELL, (i // COLS) * CELL + TITLE_H
                sheet.paste(tile(corpus[key]), (x, y))
                draw.rectangle([x + 2, y + 2, x + 38, y + 30], fill=(0, 0, 0))
                draw.text((x + 6, y + 3), str(i + 1), fill=(255, 255, 255), font=num_font)
            sheet.save(sheets_dir / f"{name}.jpg", quality=85)
            (sheets_dir / f"{name}.json").write_text(json.dumps(chunk, ensure_ascii=False), encoding="utf-8")
            made.append(f"{name}({len(chunk)})")
            next_no += 1
    print(f"new sheets: {len(made)}")
    print(" ".join(made))


if __name__ == "__main__":
    main()
