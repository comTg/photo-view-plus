"""计分：按标注结果计算各方案的 Recall@20 / nDCG@20 / MRR / P@10（docs/10 §9）。

用法：python scripts/search_eval/score.py
输出：
  <eval_dir>/report_full.md       含逐查询明细（本机）
  tests/search_eval/report.md     只含汇总指标，不含任何路径，可提交
召回率的分母是"池化后已标注为相关的图"，即相对召回。
"""

from __future__ import annotations

import json
from collections import defaultdict
from datetime import date

from common import REPO_ROOT, eval_dir, load_queries, metrics_for

ORDER = ["S0", "S1", "S2", "S3", "S4", "S5", "S5a", "S5b", "S5c", "S5d", "S5w", "S6", "R1", "R2", "R2s", "R3", "R3s", "R4", "R5b", "R5", "R5s", "R6"]
NAMES = {
    "S0": "OpenAI CLIP（现状）",
    "S1": "Chinese-CLIP",
    "S2": "RAM 标签全文",
    "S3": "描述全文",
    "S4": "描述文本向量",
    "S5": "融合 S1+S2+S3+S4",
    "S5a": "融合 S3+S4（只靠描述）",
    "S5b": "融合 S1+S2（不用大模型描述）",
    "S5c": "融合 S0+S2+S3（不换 CLIP）",
    "S5d": "融合 S1+S4（两路向量）",
    "S5w": "加权融合 S1×1 + S2×0.3 + S3×0.6 + S4×1",
    "S6": "融合（VLM 描述）",
    # R*：线上 Rust search_service 在正式库副本上的复核结果（src-tauri/tests/search_eval.rs）
    "R1": "线上·图像向量（Chinese-CLIP）",
    "R2": "线上·标签 + 全文（RAM 标签、OCR）",
    "R3": "线上·描述全文（memora 描述）",
    "R4": "线上·描述文本向量",
    "R5b": "线上·融合（图像向量 + 标签 + 全文，无描述）",
    "R5": "线上·融合（默认权重）",
    "R4@bge-base-zh-v1.5": "线上·描述文本向量（bge-base 对比）",
    "R5@bge-base-zh-v1.5": "线上·融合（描述向量用 bge-base）",
    "R2s": "线上·标签 + 全文 + 同义词扩展（A2b）",
    "R3s": "线上·描述全文 + 同义词扩展（A2b）",
    "R5s": "线上·融合 + 同义词扩展（A2b）",
    "R6": "线上·融合（VLM 描述）",
}


def avg(values):
    values = [v for v in values if v is not None]
    return sum(values) / len(values) if values else None


def fmt(v):
    return "—" if v is None else f"{v:.3f}"


def main() -> None:
    queries = load_queries()
    judgments = json.loads((eval_dir() / "judgments.json").read_text(encoding="utf-8"))
    runs = {
        p.stem: json.loads(p.read_text(encoding="utf-8")) for p in (eval_dir() / "runs").glob("*.json")
    }
    systems = [s for s in ORDER if s in runs] + sorted(s for s in runs if s not in ORDER)
    per = defaultdict(dict)
    for s in systems:
        for q in queries:
            j = judgments.get(q["id"], {})
            relevant = {k for k, v in j.items() if v}
            per[s][q["id"]] = metrics_for(runs[s].get(q["id"], []), relevant, set(j))

    labeled = [q for q in queries if q["id"] in judgments]
    with_rel = [q for q in labeled if any(judgments[q["id"]].values())]
    n_judged = sum(len(judgments.get(q["id"], {})) for q in queries)
    n_rel = sum(sum(judgments.get(q["id"], {}).values()) for q in queries)

    # 相关图多于 20 张的查询，Recall@20 本来就到不了 1，给出上限便于对照
    recall_cap = avg(min(20, sum(judgments[q["id"]].values())) / sum(judgments[q["id"]].values()) for q in with_rel)
    lines = [
        "# 搜索评测汇总",
        "",
        f"- 日期：{date.today().isoformat()}；查询 {len(queries)} 个，已标注 {len(labeled)} 个，其中有相关结果的 {len(with_rel)} 个",
        f"- 标注：共 {n_judged} 条判定，{n_rel} 条相关（池化深度 20；召回率为相对召回）",
        f"- Recall@20 的理论上限为 {fmt(recall_cap)}（多数查询的相关图超过 20 张），比较时以 nDCG@20 和 P@10 为主",
        "- 语料：正式库按忽略大小写的路径去重后的图；图像向量只覆盖已有缩略图的图，描述来自 memora 导入",
        "",
        "| 方案 | Recall@20 | nDCG@20 | MRR | P@10 | 前 20 未标注数（均值） |",
        "|------|-----------|---------|-----|------|----------------------|",
    ]
    for s in systems:
        rows = [per[s][q["id"]] for q in with_rel]
        lines.append(
            f"| {s} {NAMES.get(s, '')} | {fmt(avg(r['recall'] for r in rows))} | {fmt(avg(r['ndcg'] for r in rows))} "
            f"| {fmt(avg(r['mrr'] for r in rows))} | {fmt(avg(r['p10'] for r in rows))} | {fmt(avg(r['unjudged'] for r in rows))} |"
        )
    types = sorted({q["type"] for q in with_rel})
    lines += ["", "**按查询类型的 Recall@20**", "", "| 方案 | " + " | ".join(types) + " |", "|---" * (len(types) + 1) + "|"]
    for s in systems:
        cells = [fmt(avg(per[s][q["id"]]["recall"] for q in with_rel if q["type"] == t)) for t in types]
        lines.append(f"| {s} | " + " | ".join(cells) + " |")
    summary = "\n".join(lines) + "\n"
    (REPO_ROOT / "tests" / "search_eval" / "report.md").write_text(summary, encoding="utf-8")

    detail = [summary, "", "## 逐查询 Recall@20", "", "| 查询 | 相关数 | " + " | ".join(systems) + " |", "|---" * (len(systems) + 2) + "|"]
    for q in queries:
        j = judgments.get(q["id"], {})
        detail.append(
            f"| {q['id']} {q['text']} | {sum(j.values())} | "
            + " | ".join(fmt(per[s][q["id"]]["recall"]) for s in systems)
            + " |"
        )
    (eval_dir() / "report_full.md").write_text("\n".join(detail) + "\n", encoding="utf-8")
    print(summary)


if __name__ == "__main__":
    main()
