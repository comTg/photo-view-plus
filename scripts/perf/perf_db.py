"""run_scenarios.ps1 用的 test 库操作（docs/11 R6）：给测量场景造负载。只用于 test profile 的库。

  root-id    <db> <标签> <路径>             按标签或路径找根目录 id（找不到打印空行）
  reset-thumbs <db> <root_id> newest|oldest <张数>
                                           把这个根目录里一批已就绪的缩略图标回待生成（应用关闭时执行，重启后由启动重排生成）
                                           newest = 按修改时间最新（画廊顶部），oldest = 按 id 最小
  reset-ai   <db> <张数>                    把一批已算过图像向量的图标回待处理（不分根目录，按 id 倒序挑；
                                           删 ai_jobs 的向量记录、标签改回 pending）

reset-ai 可以在应用运行时执行（WAL 模式，等写锁最多 30 秒），流水线下一个节拍就会拾取。
"""

import sqlite3
import sys


def connect(path: str) -> sqlite3.Connection:
    if ".photoviewplus.test" not in path.replace("/", "\\").lower():
        raise SystemExit(f"只允许操作 test profile 的库：{path}")
    return sqlite3.connect(path, timeout=30)


def root_id(db: str, label: str, path: str) -> None:
    with connect(db) as conn:
        row = conn.execute(
            "SELECT id FROM roots WHERE label = ? OR lower(path) = lower(?)", (label, path)
        ).fetchone()
    print(row[0] if row else "")


def reset_thumbs(db: str, root: int, selection: str, limit: int) -> None:
    if selection == "newest":
        pick = """SELECT id FROM images
                  WHERE root_id = ? AND deleted_at IS NULL AND thumb_status = 'ready'
                  ORDER BY mtime DESC LIMIT ?"""
    elif selection == "oldest":
        pick = """SELECT id FROM images
                  WHERE root_id = ? AND deleted_at IS NULL AND thumb_status = 'ready'
                  ORDER BY id LIMIT ?"""
    else:
        raise SystemExit(f"未知选择：{selection}")
    with connect(db) as conn:
        ids = [row[0] for row in conn.execute(pick, (root, limit))]
        conn.executemany(
            "UPDATE images SET thumb_status = 'pending', thumb_hash = NULL, thumb_error = NULL WHERE id = ?",
            [(image_id,) for image_id in ids],
        )
    print(len(ids))


def reset_ai(db: str, limit: int) -> None:
    # 不分根目录、按 id 倒序挑（AI 按 id 正序处理）：缩略图重新生成的同时，AI 也有图可做。
    with connect(db) as conn:
        ids = [
            row[0]
            for row in conn.execute(
                """SELECT i.id FROM images i
                   JOIN ai_jobs a ON a.image_id = i.id AND a.kind = 'image_embedding' AND a.status = 'ready'
                   WHERE i.deleted_at IS NULL AND i.thumb_status = 'ready'
                   ORDER BY i.id DESC LIMIT ?""",
                (limit,),
            )
        ]
        conn.executemany(
            "DELETE FROM ai_jobs WHERE image_id = ? AND kind = 'image_embedding'", [(i,) for i in ids]
        )
        conn.executemany("UPDATE images SET tag_status = 'pending' WHERE id = ?", [(i,) for i in ids])
    print(len(ids))


def main() -> None:
    command, db, *rest = sys.argv[1:]
    if command == "root-id":
        root_id(db, rest[0], rest[1])
    elif command == "reset-thumbs":
        reset_thumbs(db, int(rest[0]), rest[1], int(rest[2]))
    elif command == "reset-ai":
        reset_ai(db, int(rest[0]))
    else:
        raise SystemExit(f"未知命令：{command}")


if __name__ == "__main__":
    main()
