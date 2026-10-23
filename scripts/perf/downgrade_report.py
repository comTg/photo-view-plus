"""S5 降档速度（docs/11 §8）：空闲档全速运行时模拟一次输入，负载多久降下来。

读 run_scenarios.ps1 写的 S5_events.json（模拟输入的时刻、治理器切到"使用中"的时刻）和同一轮的
record_S5_*.csv / nvidia_S5_*.csv，按 1 秒样本计算：
- 输入前 15 秒的平均水平（全速）和输入 30 秒之后的平均水平（降档后的稳态）；
- 从输入开始，到指标降过两者差值的 80% 所用的秒数。

"输入 30 秒后"要是降档后的稳态才有意义：这段时间里队列做完了（负载归零），降过 80% 的时刻就是
"做完"而不是"降档"，要对照 record CSV 看（docs/11 附录 A 的 S5 主进程就是这样）。

用法：python scripts/perf/downgrade_report.py <测量目录>
"""

import csv
import json
import sys
from datetime import datetime
from pathlib import Path

SERIES = {
    "proc_apptree_cpu_norm": "应用进程树 CPU %（整机）",
    "proc_app_cpu_norm": "主进程 CPU %（缩略图）",
    "proc_pyapp_cpu_norm": "AI worker CPU %",
    "cpu_total_pct": "总 CPU %",
}


def parse_iso(text: str) -> float:
    return datetime.fromisoformat(text.strip().lstrip("﻿")).timestamp()


def parse_nvidia(text: str) -> float:
    return datetime.strptime(text.strip(), "%Y/%m/%d %H:%M:%S.%f").timestamp()


def load_record(path: Path) -> dict[str, list[tuple[float, float]]]:
    series: dict[str, list[tuple[float, float]]] = {key: [] for key in SERIES}
    with path.open(encoding="utf-8-sig", newline="") as fh:
        for row in csv.DictReader(fh):
            ts = parse_iso(row["timestamp"])
            for key in SERIES:
                value = row.get(key, "")
                if value not in ("", None):
                    series[key].append((ts, float(value)))
    return series


def load_nvidia(path: Path) -> list[tuple[float, float]]:
    out = []
    with path.open(encoding="utf-8-sig", newline="") as fh:
        reader = csv.reader(fh)
        next(reader, None)
        for row in reader:
            if len(row) >= 2 and row[1].strip():
                out.append((parse_nvidia(row[0]), float(row[1])))
    return out


def response(samples: list[tuple[float, float]], at: float) -> tuple[float, float, float | None]:
    before = [v for t, v in samples if at - 15 <= t < at]
    after = [v for t, v in samples if t >= at + 30]
    if not before or not after:
        return float("nan"), float("nan"), None
    high = sum(before) / len(before)
    low = sum(after) / len(after)
    if high - low <= 0:
        return high, low, None
    threshold = low + 0.2 * (high - low)
    for t, v in samples:
        if t >= at and v <= threshold:
            return high, low, t - at
    return high, low, None


def main() -> None:
    out_dir = Path(sys.argv[1])
    events = json.loads((out_dir / "S5_events.json").read_text(encoding="utf-8-sig"))
    nudge = parse_iso(events["nudge"])
    record = sorted(out_dir.glob("record_S5_*.csv"))[-1]
    nvidia = sorted(out_dir.glob("nvidia_S5_*.csv"))[-1]
    lines = [
        "| 指标 | 输入前 15 秒 | 输入 30 秒后 | 降过 80% 用时 |",
        "|---|---|---|---|",
    ]
    series = load_record(record)
    series_gpu = {"nv_gpu_util_pct": load_nvidia(nvidia)}
    labels = dict(SERIES, nv_gpu_util_pct="GPU 利用率 %（nvidia-smi）")
    for key, samples in {**series, **series_gpu}.items():
        high, low, seconds = response(samples, nudge)
        took = f"{seconds:.1f} 秒" if seconds is not None else "—"
        lines.append(f"| {labels[key]} | {high:.1f} | {low:.1f} | {took} |")
    switch = events.get("switchMs")
    print(f"治理器切到使用中：{switch} ms" if switch is not None else "治理器没有切到使用中")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
