"""搜索评测原型的公共部分：路径、语料读写、中文分词（CJK 单字 + 双字）、BM25、RRF、指标。

只依赖标准库。评测数据（语料、向量、拼图、判定）都放在本机评测目录，不进仓库：
默认 %LOCALAPPDATA%\\PhotoViewPlus\\eval，可用 PVP_EVAL_DIR 覆盖。见 docs/10 §9。
"""

from __future__ import annotations

import json
import math
import os
import re
import unicodedata
from collections import Counter, defaultdict
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
QUERIES_PATH = REPO_ROOT / "tests" / "search_eval" / "queries.json"


def eval_dir() -> Path:
    env = os.environ.get("PVP_EVAL_DIR")
    base = Path(env) if env else Path(os.environ["LOCALAPPDATA"]) / "PhotoViewPlus" / "eval"
    base.mkdir(parents=True, exist_ok=True)
    return base


def source_data_dir() -> Path:
    """被评测的应用数据目录（默认正式版 profile）。只读使用。"""
    env = os.environ.get("PVP_EVAL_SOURCE_DIR")
    return Path(env) if env else Path(os.environ["LOCALAPPDATA"]) / "com.vetoer.photoviewplus"


def memora_db_path() -> Path:
    return Path(os.environ.get("PVP_EVAL_MEMORA_DB", r"D:\code\mygit\memora\data\memora.db"))


def memora_thumbs_dir() -> Path:
    return Path(os.environ.get("PVP_EVAL_MEMORA_THUMBS", r"D:\code\mygit\memora\cache\thumbs"))


def read_jsonl(path: Path) -> list[dict]:
    with path.open(encoding="utf-8") as f:
        return [json.loads(line) for line in f if line.strip()]


def write_jsonl(path: Path, rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as f:
        for row in rows:
            f.write(json.dumps(row, ensure_ascii=False) + "\n")


def load_queries() -> list[dict]:
    return json.loads(QUERIES_PATH.read_text(encoding="utf-8"))["queries"]


def load_corpus() -> list[dict]:
    return read_jsonl(eval_dir() / "corpus.jsonl")


# ---------- 分词：CJK 双字为主、单字低权重兜底，ASCII 按词 ----------

_CJK = r"\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff"
_RUN_RE = re.compile(rf"[{_CJK}]+|[a-z0-9]+")
_CJK_RE = re.compile(rf"[{_CJK}]")
# 单字里过于常见、不带检索意义的字；双字里含这些字的也丢掉（"的小""了一"之类）。
STOP_CHARS = set("的了和与及或是在有着很也都就而又被把从对为这那个一些")
UNIGRAM_WEIGHT = 0.3


def normalize(text: str) -> str:
    return unicodedata.normalize("NFKC", text or "").lower()


def tokenize(text: str) -> list[tuple[str, float]]:
    """返回 (token, 权重)。双字 1.0，单字 UNIGRAM_WEIGHT，ASCII 词 1.0。"""
    out: list[tuple[str, float]] = []
    for run in _RUN_RE.findall(normalize(text)):
        if not _CJK_RE.match(run):
            out.append((run, 1.0))
            continue
        for ch in run:
            if ch not in STOP_CHARS:
                out.append(("u:" + ch, UNIGRAM_WEIGHT))
        for i in range(len(run) - 1):
            pair = run[i : i + 2]
            if not (set(pair) & STOP_CHARS):
                out.append((pair, 1.0))
    return out


class BM25:
    def __init__(self, docs: dict[str, str], k1: float = 1.2, b: float = 0.75):
        self.k1, self.b = k1, b
        self.tf: dict[str, Counter] = {}
        self.weight: dict[str, float] = {}
        df: Counter = Counter()
        lengths = {}
        for key, text in docs.items():
            toks = tokenize(text)
            counts = Counter(t for t, _ in toks)
            for t, w in toks:
                self.weight[t] = w
            self.tf[key] = counts
            lengths[key] = sum(counts.values())
            df.update(counts.keys())
        self.n = len(docs)
        self.avgdl = (sum(lengths.values()) / self.n) if self.n else 0.0
        self.lengths = lengths
        self.idf = {t: math.log(1 + (self.n - d + 0.5) / (d + 0.5)) for t, d in df.items()}
        self.postings: dict[str, list[str]] = defaultdict(list)
        for key, counts in self.tf.items():
            for t in counts:
                self.postings[t].append(key)

    def search(self, query: str, limit: int) -> list[tuple[str, float]]:
        qtoks = {t: w for t, w in tokenize(query)}
        scores: Counter = Counter()
        for t, w in qtoks.items():
            idf = self.idf.get(t)
            if idf is None:
                continue
            for key in self.postings[t]:
                f = self.tf[key][t]
                denom = f + self.k1 * (1 - self.b + self.b * self.lengths[key] / (self.avgdl or 1))
                scores[key] += w * idf * f * (self.k1 + 1) / denom
        return scores.most_common(limit)


def rrf(rankings: list[list[str]], k: int = 60, weights: list[float] | None = None, limit: int = 200) -> list[str]:
    scores: Counter = Counter()
    for idx, ranking in enumerate(rankings):
        w = weights[idx] if weights else 1.0
        for rank, key in enumerate(ranking):
            scores[key] += w / (k + rank + 1)
    return [key for key, _ in scores.most_common(limit)]


# ---------- 指标 ----------


def metrics_for(ranking: list[str], relevant: set[str], judged: set[str], k: int = 20) -> dict:
    top = ranking[:k]
    hits = [1 if key in relevant else 0 for key in top]
    dcg = sum(h / math.log2(i + 2) for i, h in enumerate(hits))
    ideal = sum(1 / math.log2(i + 2) for i in range(min(len(relevant), k)))
    first = next((i for i, h in enumerate(hits) if h), None)
    return {
        "recall": (sum(hits) / len(relevant)) if relevant else None,
        "ndcg": (dcg / ideal) if ideal else None,
        "mrr": (1 / (first + 1)) if first is not None else 0.0,
        "p10": sum(hits[:10]) / 10,
        "unjudged": sum(1 for key in top if key not in judged),
    }
