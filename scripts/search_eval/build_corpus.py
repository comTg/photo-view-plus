"""从正式库的只读快照构建评测语料：按"忽略大小写的完整路径"去重（docs/11 §6.1），
合并每张图的 RAM 标签，并按路径精确匹配 memora 的描述。

用法：python scripts/search_eval/build_corpus.py
输出：<eval_dir>/snapshot/app.sqlite（一致性快照）、<eval_dir>/corpus.jsonl
"""

from __future__ import annotations

import json
import sqlite3
from collections import defaultdict
from pathlib import Path

from common import eval_dir, memora_db_path, memora_thumbs_dir, source_data_dir, write_jsonl


def snapshot_db() -> Path:
    src = source_data_dir() / "db" / "app.sqlite"
    dst = eval_dir() / "snapshot" / "app.sqlite"
    dst.parent.mkdir(parents=True, exist_ok=True)
    if dst.exists():
        dst.unlink()
    with sqlite3.connect(f"file:{src}?mode=ro", uri=True) as source, sqlite3.connect(dst) as target:
        source.backup(target)  # 含 WAL 中已提交的内容，读取期间不阻塞应用
    return dst


def thumb_path(thumbs_dir: Path, thumb_hash: str) -> Path:
    return thumbs_dir / thumb_hash[:2] / f"{thumb_hash}.webp"


def main() -> None:
    db = snapshot_db()
    thumbs_dir = source_data_dir() / "thumbs"
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    rows = conn.execute(
        """SELECT i.id, r.path, i.rel_path, i.extension, i.thumb_status, i.thumb_hash
           FROM images i JOIN roots r ON r.id = i.root_id
           WHERE i.deleted_at IS NULL"""
    ).fetchall()
    tags_by_id: dict[int, dict[str, float]] = defaultdict(dict)
    for image_id, name, score in conn.execute(
        "SELECT it.image_id, t.name, it.score FROM image_tags it JOIN tags t ON t.id = it.tag_id"
    ):
        tags_by_id[image_id][name] = max(score, tags_by_id[image_id].get(name, 0.0))

    mem = sqlite3.connect(f"file:{memora_db_path()}?mode=ro", uri=True)
    memora = {
        path: (mid, desc, cats)
        for mid, path, desc, cats in mem.execute(
            """SELECT id, path, description, categories FROM photos
               WHERE status = 'done' AND description IS NOT NULL AND description <> ''"""
        )
    }

    groups: dict[str, list[tuple]] = defaultdict(list)
    for row in rows:
        image_id, root, rel, ext, thumb_status, thumb_hash = row
        full = root.rstrip("\\") + "\\" + rel.replace("/", "\\")
        groups[full.lower()].append((image_id, full, ext, thumb_status, thumb_hash))

    corpus = []
    for key, members in groups.items():
        # 代表行优先选缩略图已就绪的那条
        members.sort(key=lambda m: (m[3] != "ready", m[0]))
        rep = members[0]
        tags: dict[str, float] = {}
        for m in members:
            for name, score in tags_by_id.get(m[0], {}).items():
                tags[name] = max(score, tags.get(name, 0.0))
        mem_hit = next((memora[m[1]] for m in members if m[1] in memora), None)
        thumb = str(thumb_path(thumbs_dir, rep[4])) if rep[3] == "ready" and rep[4] else None
        memora_thumb = None
        if mem_hit:
            candidate = memora_thumbs_dir() / f"{mem_hit[0]}.jpg"
            memora_thumb = str(candidate) if candidate.exists() else None
        corpus.append(
            {
                "key": key,
                "path": rep[1],
                "ids": [m[0] for m in members],
                "ext": (rep[2] or "").lower(),
                "thumb": thumb,
                "memora_thumb": memora_thumb,
                "tags": sorted(tags.items(), key=lambda kv: -kv[1]),
                "desc": mem_hit[1] if mem_hit else None,
                "memora_cats": json.loads(mem_hit[2]) if mem_hit and mem_hit[2] else [],
            }
        )

    write_jsonl(eval_dir() / "corpus.jsonl", corpus)
    print(
        json.dumps(
            {
                "rows": len(rows),
                "distinct": len(corpus),
                "with_thumb": sum(1 for c in corpus if c["thumb"]),
                "with_tags": sum(1 for c in corpus if c["tags"]),
                "with_desc": sum(1 for c in corpus if c["desc"]),
                "viewable": sum(1 for c in corpus if c["thumb"] or c["memora_thumb"]),
            },
            ensure_ascii=False,
        )
    )


if __name__ == "__main__":
    main()
