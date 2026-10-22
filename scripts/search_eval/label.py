"""记录一张拼图的标注结果：列出的格子编号判为相关，这张拼图里其余格子判为不相关。

用法：python scripts/search_eval/label.py <拼图名> <编号,编号,...|none>
例：  python scripts/search_eval/label.py q05_1 3,7,12
可一次传多组：python scripts/search_eval/label.py q05_1 3,7 q05_2 none
"""

from __future__ import annotations

import json
import sys

from common import eval_dir


def main() -> None:
    args = sys.argv[1:]
    if not args or len(args) % 2:
        raise SystemExit(__doc__)
    path = eval_dir() / "judgments.json"
    judgments = json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}
    for sheet, cells in zip(args[::2], args[1::2]):
        keys = json.loads((eval_dir() / "sheets" / f"{sheet}.json").read_text(encoding="utf-8"))
        relevant = set() if cells == "none" else {int(c) for c in cells.split(",") if c}
        bad = [c for c in relevant if not 1 <= c <= len(keys)]
        if bad:
            raise SystemExit(f"{sheet}: 编号越界 {bad}（共 {len(keys)} 格）")
        qid = sheet.split("_")[0]
        bucket = judgments.setdefault(qid, {})
        for i, key in enumerate(keys, start=1):
            bucket[key] = 1 if i in relevant else 0
        print(f"{sheet}: {len(relevant)} 相关 / {len(keys)}")
    path.write_text(json.dumps(judgments, ensure_ascii=False, indent=0), encoding="utf-8")


if __name__ == "__main__":
    main()
