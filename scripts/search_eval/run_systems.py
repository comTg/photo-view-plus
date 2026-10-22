"""对每个查询跑各个检索方案（docs/10 §9），把排名写到 <eval_dir>/runs/<system>.json。

缺少对应向量的方案会跳过（例如还没跑 embed.py cnclip）。

方案：
  S0  OpenAI CLIP（线上现状，但向量覆盖全部已有缩略图）
  S1  Chinese-CLIP
  S2  RAM 标签全文（BM25）
  S3  memora 描述全文（BM25）
  S4  memora 描述文本向量（bge-small-zh）
  S5  融合：S1 + S2 + S3 + S4（目标方案）
  S5a 融合：S3 + S4（只靠描述）
  S5b 融合：S1 + S2（不需要大模型描述：Chinese-CLIP + RAM 标签）
  S5c 融合：S0 + S2 + S3（不换 CLIP，只加文字通道）
"""

from __future__ import annotations

import json

import numpy as np

from common import BM25, eval_dir, load_corpus, load_queries, rrf

DEPTH = 200


def load_emb(name: str):
    base = eval_dir() / "emb"
    vec, keys = base / f"{name}.npy", base / f"{name}.keys.json"
    if not vec.exists() or not keys.exists():
        return None, None
    return np.load(vec), json.loads(keys.read_text(encoding="utf-8"))


def vector_runs(img_name: str, query_name: str, queries: list[dict]) -> dict | None:
    img, img_keys = load_emb(img_name)
    q, q_ids = load_emb(query_name)
    if img is None or q is None:
        return None
    by_id = dict(zip(q_ids, q))
    out = {}
    for query in queries:
        scores = img @ by_id[query["id"]]
        order = np.argsort(-scores)[:DEPTH]
        out[query["id"]] = [img_keys[i] for i in order]
    return out


def bm25_runs(docs: dict[str, str], queries: list[dict]) -> dict:
    index = BM25(docs)
    return {q["id"]: [key for key, _ in index.search(q["text"], DEPTH)] for q in queries}


def main() -> None:
    corpus = load_corpus()
    queries = load_queries()
    runs: dict[str, dict] = {}

    for name, img, qry in (("S0", "clip_openai_img", "clip_openai_query"), ("S1", "cnclip_img", "cnclip_query"), ("S4", "bge_desc", "bge_query")):
        result = vector_runs(img, qry, queries)
        if result is not None:
            runs[name] = result

    tag_docs = {c["key"]: " ".join(name for name, _ in c["tags"]) for c in corpus if c["tags"]}
    desc_docs = {c["key"]: c["desc"] for c in corpus if c["desc"]}
    runs["S2"] = bm25_runs(tag_docs, queries)
    runs["S3"] = bm25_runs(desc_docs, queries)

    def fuse(name: str, parts: list[str], weights: list[float] | None = None) -> None:
        if all(p in runs for p in parts):
            runs[name] = {
                q["id"]: rrf([runs[p][q["id"]] for p in parts], weights=weights, limit=DEPTH) for q in queries
            }

    fuse("S5", ["S1", "S2", "S3", "S4"])
    fuse("S5a", ["S3", "S4"])
    fuse("S5b", ["S1", "S2"])
    fuse("S5c", ["S0", "S2", "S3"])
    fuse("S5d", ["S1", "S4"])  # 图像向量 + 描述向量，不要全文和标签
    fuse("S5w", ["S1", "S2", "S3", "S4"], weights=[1.0, 0.3, 0.6, 1.0])  # 加权：压低 RAM 标签和全文

    out_dir = eval_dir() / "runs"
    out_dir.mkdir(parents=True, exist_ok=True)
    for name, data in runs.items():
        (out_dir / f"{name}.json").write_text(json.dumps(data, ensure_ascii=False), encoding="utf-8")
    print("systems:", ", ".join(sorted(runs)))


if __name__ == "__main__":
    main()
