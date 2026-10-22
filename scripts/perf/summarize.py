#!/usr/bin/env python3
"""汇总 R0 测量结果（docs/11-background-resource-governance.md §4），只用标准库。

读取一个或多个目录（含一层子目录）里的：
  record_<label>_<run>.csv   record.ps1 每秒采样
  nvidia_<label>_<run>.csv   record.ps1 并行跑的 nvidia-smi
  probe_<label>_<run>.csv    probe.py 每 5 秒窗口
  frames_<label>_<run>.csv   frame_probe.html 下载的 CSV（需手动放进目录）
  ui_<label>_<run>.csv       drive_app.mjs scroll-gallery / ui-stats --out 的输出
按场景标签分组（同一标签的多轮合并），输出每个指标的 mean / p95 Markdown 表，以及相对基线场景的派生指标。

用法：
  python scripts/perf/summarize.py D:/perf/r0                  # 打印并写入 D:/perf/r0/summary.md
  python scripts/perf/summarize.py D:/perf/r0 --baseline S0 --all
  python scripts/perf/summarize.py D:/perf/r0 --per-run        # 每轮单独一列，检查重复轮次是否一致
"""

from __future__ import annotations

import argparse
import csv
import json
import math
import re
import sys
from collections import defaultdict
from pathlib import Path

NAME_RX = re.compile(r"^(record|nvidia|probe|frames|ui)_(.+?)_(\d{8}-\d{6})(?:\D.*)?\.csv$", re.IGNORECASE)

# (键, 说明)。"re:" 开头的键是正则，{0} 换成第一个分组（磁盘编号随机器而变）。
CURATED: list[tuple[str, str]] = [
    ("cpu_total_pct", "总 CPU %"),
    ("cpu_queue_len", "CPU 就绪队列长度"),
    ("cpu_dpc_pct", "DPC 时间 %"),
    ("proc_apptree_cpu_norm", "应用进程树 CPU %（整机；含 WebView2、worker）"),
    ("proc_app_cpu_norm", "photo-view-plus 主进程 CPU %（整机）"),
    ("proc_wv2app_cpu_norm", "应用 WebView2 CPU %（整机）"),
    ("proc_pyapp_cpu_norm", "AI worker CPU %（整机）"),
    ("proc_llama_cpu_norm", "llama-server CPU %（整机）"),
    ("proc_hips_cpu_norm", "火绒全部进程 CPU %（整机）"),
    ("proc_hipsd_cpu_norm", "HipsDaemon CPU %（整机）"),
    ("proc_dwm_cpu_norm", "dwm CPU %（整机）"),
    ("proc_wv2_cpu_norm", "全部 WebView2 CPU %（整机，含其他应用）"),
    ("mem_avail_mb", "可用内存 MB"),
    ("proc_apptree_ws_mb", "应用进程树私有工作集 MB"),
    ("proc_pyapp_ws_mb", "AI worker 私有工作集 MB"),
    ("gpu_3d_pct", "GPU 3D 引擎 %"),
    ("gpu_compute_pct", "GPU Compute 引擎 %"),
    ("gpu_copy_pct", "GPU Copy 引擎 %"),
    ("gpu_videodecode_pct", "GPU 视频解码引擎 %"),
    ("gpu_engine_max_pct", "GPU 最忙引擎 %"),
    ("gpu_dwm_pct", "dwm 的 GPU %"),
    ("gpu_wv2app_pct", "应用 WebView2 的 GPU %"),
    ("gpu_pyapp_pct", "AI worker 的 GPU %"),
    ("gpu_llama_pct", "llama-server 的 GPU %"),
    ("nv_gpu_util_pct", "nvidia-smi GPU 利用率 %"),
    ("gpu_dedicated_mb", "专用显存 MB（计数器）"),
    ("nv_mem_used_mib", "显存占用 MiB（nvidia-smi）"),
    ("vram_pyapp_mb", "AI worker 显存 MB"),
    ("vram_llama_mb", "llama-server 显存 MB"),
    ("re:disk_(.+)_busy_pct", "磁盘 {0} 繁忙 %"),
    ("re:disk_(.+)_queue", "磁盘 {0} 平均队列长度"),
    ("net_rx_MBps", "网络接收 MB/s"),
    ("probe_timer_p50_ms", "计时器超出 p50 ms（每 5s 窗口）"),
    ("probe_timer_p99_ms", "计时器超出 p99 ms（每 5s 窗口）"),
    ("probe_timer_max_ms", "计时器超出 max ms（每 5s 窗口）"),
    ("probe_cpu_work_ms_mean", "固定计算量耗时 ms"),
    ("probe_disk_write_fsync_ms_mean", "C: 4KB 写+fsync ms"),
    ("probe_disk_write_fsync_ms_max", "C: 4KB 写+fsync 窗口内最大 ms"),
    ("fp_fps", "frame_probe FPS"),
    ("fp_p95_ms", "frame_probe 帧间隔 p95 ms"),
    ("fp_max_ms", "frame_probe 帧间隔 max ms"),
    ("fp_over33", "frame_probe 每窗口 >33ms 帧数"),
    ("ui_scroll_fps", "应用画廊滚动 FPS"),
    ("ui_scroll_frame_p95_ms", "应用画廊滚动帧间隔 p95 ms"),
    ("ui_scroll_longtask_n", "应用画廊滚动每窗口长任务数"),
    ("ui_scroll_longtask_max_ms", "应用画廊滚动窗口内最长任务 ms"),
    ("ui_idle_longtask_n", "应用静止每窗口长任务数"),
    ("sampler_ms", "record 单次采样耗时 ms"),
    ("self_cpu_pct", "record 自身 CPU %（单核）"),
]

SKIP_RECORD = {"timestamp", "elapsed_s", "label"}
SKIP_PROBE = {"window_start", "window_end", "elapsed_s", "window_s", "label", "work_units", "work_base_ms"}
SKIP_FRAMES = {"window_start", "window_end", "elapsed_s", "window_s", "label", "visible", "autoscroll", "speed_px_s"}
SKIP_UI = {"window_start", "window_end", "elapsed_s", "window_s", "label", "mode", "visible"}


def to_float(text: str | None) -> float | None:
    if text is None:
        return None
    text = text.strip()
    if not text:
        return None
    try:
        value = float(text)
    except ValueError:
        return None
    return value if math.isfinite(value) else None


def percentile(values: list[float], p: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    k = (len(ordered) - 1) * p / 100.0
    lo, hi = math.floor(k), math.ceil(k)
    if lo == hi:
        return ordered[lo]
    return ordered[lo] + (ordered[hi] - ordered[lo]) * (k - lo)


def fmt(value: float | None) -> str:
    if value is None:
        return "—"
    a = abs(value)
    if a >= 100:
        return f"{value:.0f}"
    if a >= 10:
        return f"{value:.1f}"
    return f"{value:.2f}"


def natural_key(label: str):
    return [int(part) if part.isdigit() else part.lower() for part in re.split(r"(\d+)", label)]


class Group:
    def __init__(self) -> None:
        self.series: dict[str, list[float]] = defaultdict(list)
        self.runs: set[str] = set()
        self.files: dict[str, int] = defaultdict(int)
        self.record_samples = 0
        self.work_units: set[str] = set()
        # 派生指标用的累加量
        self.sums: dict[str, float] = defaultdict(float)
        # record meta 里的 top_processes：名字 -> (整机 CPU% × 样本数) 累加，及总样本数
        self.top_weighted: dict[str, float] = defaultdict(float)
        self.top_samples = 0


def read_rows(path: Path, skipinitialspace: bool = False) -> list[dict[str, str]]:
    with open(path, encoding="utf-8-sig", newline="") as handle:
        return list(csv.DictReader(handle, skipinitialspace=skipinitialspace))


def discover(paths: list[Path]) -> list[Path]:
    found: list[Path] = []
    for p in paths:
        if p.is_file():
            found.append(p)
        elif p.is_dir():
            found.extend(sorted(p.glob("*.csv")))
            found.extend(sorted(p.glob("*/*.csv")))
    return [f for f in found if NAME_RX.match(f.name)]


def main() -> int:
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="replace")
    parser = argparse.ArgumentParser(description="汇总 R0 测量 CSV，输出 Markdown 表")
    parser.add_argument("paths", nargs="+", type=Path, help="运行目录或 CSV 文件")
    parser.add_argument("--out", type=Path, default=None, help="Markdown 输出文件；默认 <第一个目录>/summary.md")
    parser.add_argument("--no-write", action="store_true", help="只打印，不写文件")
    parser.add_argument("--baseline", default=None, help="基线场景标签（默认 S0，没有则取第一个）")
    parser.add_argument("--per-run", action="store_true", help="按 标签/轮次 分列，而不是按标签合并")
    parser.add_argument("--all", action="store_true", help="附加全部数值列的完整表")
    parser.add_argument("--trim-head", type=float, default=0.0, help="丢弃 record 每轮开头 N 秒（默认 0）")
    args = parser.parse_args()

    files = discover(args.paths)
    if not files:
        print("没有找到 record_/nvidia_/probe_/frames_/ui_*.csv 文件", file=sys.stderr)
        return 1

    groups: dict[str, Group] = defaultdict(Group)
    notes: list[str] = []

    for path in files:
        m = NAME_RX.match(path.name)
        kind, label, run = m.group(1).lower(), m.group(2), m.group(3)
        key = f"{label}/{run}" if args.per_run else label
        g = groups[key]
        g.files[kind] += 1
        try:
            if kind == "nvidia":
                rows = read_rows(path, skipinitialspace=True)
            else:
                rows = read_rows(path)
        except (OSError, csv.Error, UnicodeDecodeError) as error:
            notes.append(f"读取失败 {path.name}：{error}")
            continue
        if kind in ("record", "probe"):
            g.runs.add(run)

        if kind == "record":
            for row in rows:
                if args.trim_head and (to_float(row.get("elapsed_s")) or 0.0) < args.trim_head:
                    continue
                g.record_samples += 1
                for col, text in row.items():
                    if col in SKIP_RECORD or col is None:
                        continue
                    value = to_float(text)
                    if value is not None:
                        g.series[col].append(value)
            meta = path.with_name(path.stem + ".meta.json")
            if meta.exists():
                try:
                    info = json.loads(meta.read_text(encoding="utf-8-sig"))
                    for note in info.get("notes") or []:
                        notes.append(f"{label}/{run}：{note}")
                    for k in ("nvidia_note", "probe_note"):
                        if info.get(k):
                            notes.append(f"{label}/{run}：{info[k]}")
                    samples = int(info.get("samples") or 0)
                    if samples and info.get("top_processes"):
                        g.top_samples += samples
                        for item in info["top_processes"]:
                            value = item.get("cpu_avg_norm")
                            if isinstance(value, (int, float)):
                                g.top_weighted[str(item.get("name"))] += value * samples
                except (OSError, ValueError):
                    pass
        elif kind == "nvidia":
            for row in rows:
                for col, text in row.items():
                    if col is None:
                        continue
                    name = col.strip().lower()
                    value = to_float(text)
                    if value is None:
                        continue
                    if name.startswith("utilization.gpu"):
                        g.series["nv_gpu_util_pct"].append(value)
                    elif name.startswith("memory.used"):
                        g.series["nv_mem_used_mib"].append(value)
        elif kind == "probe":
            for row in rows:
                if row.get("work_units"):
                    g.work_units.add(row["work_units"])
                n = to_float(row.get("cpu_work_n")) or 0.0
                mean = to_float(row.get("cpu_work_ms_mean"))
                if mean is not None and n > 0:
                    g.sums["work_ms_x_n"] += mean * n
                    g.sums["work_n"] += n
                for col, text in row.items():
                    if col in SKIP_PROBE or col is None:
                        continue
                    value = to_float(text)
                    if value is not None:
                        g.series["probe_" + col].append(value)
        elif kind == "frames":
            for row in rows:
                if row.get("visible", "1").strip() == "0":
                    g.sums["fp_hidden_windows"] += 1
                    continue
                g.sums["fp_frames"] += to_float(row.get("frames")) or 0.0
                g.sums["fp_over33"] += to_float(row.get("over33")) or 0.0
                g.sums["fp_over20"] += to_float(row.get("over20")) or 0.0
                g.sums["fp_dropped"] += to_float(row.get("dropped")) or 0.0
                g.sums["fp_seconds"] += to_float(row.get("window_s")) or 0.0
                for col, text in row.items():
                    if col in SKIP_FRAMES or col is None:
                        continue
                    value = to_float(text)
                    if value is not None:
                        g.series["fp_" + col].append(value)
        elif kind == "ui":
            for row in rows:
                if row.get("visible", "1").strip() == "0":
                    g.sums["ui_hidden_windows"] += 1
                    continue
                mode = (row.get("mode") or "scroll").strip() or "scroll"
                prefix = f"ui_{mode}_"
                g.sums[prefix + "frames"] += to_float(row.get("frames")) or 0.0
                g.sums[prefix + "over33"] += to_float(row.get("frames_over33")) or 0.0
                g.sums[prefix + "longtasks"] += to_float(row.get("longtask_n")) or 0.0
                g.sums[prefix + "longtasks100"] += to_float(row.get("longtask_over100_n")) or 0.0
                g.sums[prefix + "seconds"] += to_float(row.get("window_s")) or 0.0
                for col, text in row.items():
                    if col in SKIP_UI or col is None:
                        continue
                    value = to_float(text)
                    if value is not None:
                        g.series[prefix + col].append(value)

    labels = sorted(groups, key=natural_key)
    baseline = args.baseline
    if baseline is None:
        baseline = next((k for k in labels if k == "S0" or k.startswith("S0/")), labels[0])
    if baseline not in groups:
        notes.append(f"基线 {baseline} 不存在，派生指标的对比列留空")

    out: list[str] = []
    out.append("# R0 测量汇总")
    out.append("")
    out.append("输入：" + "，".join(str(p) for p in args.paths))
    out.append("")
    out.append("| 场景 | 轮次 | record 秒数 | 文件 |")
    out.append("|---|---|---|---|")
    for k in labels:
        g = groups[k]
        file_desc = " ".join(f"{kind}×{n}" for kind, n in sorted(g.files.items()))
        out.append(f"| {k} | {len(g.runs) or '—'} | {g.record_samples or '—'} | {file_desc} |")
    out.append("")

    # ---- 主表
    out.append("## 指标（每格：mean / p95）")
    out.append("")
    out.append("record / nvidia 按 1 秒样本统计；probe / frame_probe / 应用 UI 按 5 秒窗口统计。")
    out.append("`*_cpu_norm` 已除以逻辑 CPU 数（整机百分比）。空白（—）= 该场景没有该数据或计数器不可用。")
    out.append("")
    out.append("| 指标 | 说明 | " + " | ".join(labels) + " |")
    out.append("|---|---|" + "---|" * len(labels))
    all_keys = set()
    for g in groups.values():
        all_keys.update(g.series)
    emitted = set()
    for key, desc in CURATED:
        if key.startswith("re:"):
            rx = re.compile("^" + key[3:] + "$")
            matched = sorted(k for k in all_keys if rx.match(k))
            items = [(k, desc.format(rx.match(k).group(1))) for k in matched]
        else:
            items = [(key, desc)]
        for metric, text in items:
            if not any(groups[k].series.get(metric) for k in labels):
                continue
            emitted.add(metric)
            cells = []
            for k in labels:
                values = groups[k].series.get(metric) or []
                if values:
                    cells.append(f"{fmt(sum(values) / len(values))} / {fmt(percentile(values, 95))}")
                else:
                    cells.append("—")
            out.append(f"| `{metric}` | {text} | " + " | ".join(cells) + " |")
    out.append("")

    # ---- 派生指标
    base = groups.get(baseline)
    base_work = base.sums["work_ms_x_n"] / base.sums["work_n"] if base and base.sums["work_n"] else None
    base_p99 = None
    if base and base.series.get("probe_timer_p99_ms"):
        base_p99 = sum(base.series["probe_timer_p99_ms"]) / len(base.series["probe_timer_p99_ms"])
    out.append(f"## 派生指标（基线：{baseline}）")
    out.append("")
    out.append("docs/11 §8 验收初值（S4、使用中模式，对比 S0）：计时器 p99 增加 ≤ 5ms；计算量变慢 ≤ 15%；"
               "frame_probe >33ms 帧 ≤ 1%；滚动画廊时 >100ms 长任务 ≤ 1 次/分钟。")
    out.append("")
    out.append("| 场景 | 计算量耗时 ms | 变慢 % | 计时器 p99 均值 ms | p99 增量 ms | frame_probe >33ms 占比 % "
               "| >33ms 帧/分钟 | 丢帧占比 % | 画廊长任务/分钟 | >100ms 长任务/分钟 | 画廊 >33ms 帧占比 % |")
    out.append("|---|---|---|---|---|---|---|---|---|---|---|")
    for k in labels:
        g = groups[k]
        work = g.sums["work_ms_x_n"] / g.sums["work_n"] if g.sums["work_n"] else None
        slowdown = None
        if work is not None and base_work:
            if base and g.work_units and base.work_units and g.work_units != base.work_units:
                notes.append(f"{k} 的计算量单元数 {sorted(g.work_units)} 与基线 {sorted(base.work_units)} 不同，"
                             "变慢 % 不可比（请复用同一个 probe_calibration.json）")
            else:
                slowdown = (work / base_work - 1.0) * 100.0
        p99_values = g.series.get("probe_timer_p99_ms") or []
        p99 = sum(p99_values) / len(p99_values) if p99_values else None
        p99_delta = p99 - base_p99 if p99 is not None and base_p99 is not None else None
        fp_frames = g.sums["fp_frames"]
        fp_minutes = g.sums["fp_seconds"] / 60.0
        fp_over33_pct = g.sums["fp_over33"] / fp_frames * 100.0 if fp_frames else None
        fp_over33_min = g.sums["fp_over33"] / fp_minutes if fp_minutes else None
        fp_drop_pct = g.sums["fp_dropped"] / fp_frames * 100.0 if fp_frames else None
        ui_minutes = g.sums["ui_scroll_seconds"] / 60.0
        lt_min = g.sums["ui_scroll_longtasks"] / ui_minutes if ui_minutes else None
        lt100_min = g.sums["ui_scroll_longtasks100"] / ui_minutes if ui_minutes else None
        ui_frames = g.sums["ui_scroll_frames"]
        ui_over33 = g.sums["ui_scroll_over33"] / ui_frames * 100.0 if ui_frames else None
        cells = [fmt(work), fmt(slowdown), fmt(p99), fmt(p99_delta), fmt(fp_over33_pct), fmt(fp_over33_min),
                 fmt(fp_drop_pct), fmt(lt_min), fmt(lt100_min), fmt(ui_over33)]
        out.append(f"| {k} | " + " | ".join(cells) + " |")
        if g.sums["fp_hidden_windows"]:
            notes.append(f"{k}：frame_probe 有 {int(g.sums['fp_hidden_windows'])} 个窗口页面不可见，已剔除")
        if g.sums["ui_hidden_windows"]:
            notes.append(f"{k}：应用 UI 统计有 {int(g.sums['ui_hidden_windows'])} 个窗口页面不可见，已剔除")
        idle_minutes = g.sums["ui_idle_seconds"] / 60.0
        if idle_minutes:
            notes.append(f"{k}：应用静止时长任务 {g.sums['ui_idle_longtasks'] / idle_minutes:.2f} 次/分钟"
                         f"（>100ms {g.sums['ui_idle_longtasks100'] / idle_minutes:.2f} 次/分钟）")
    out.append("")

    # ---- 全机最忙的进程（不限于预设分组，防止漏掉意料之外的元凶）
    if any(groups[k].top_samples for k in labels):
        out.append("## 最忙的进程（整机 CPU % 均值，按进程名合并，来自 record meta）")
        out.append("")
        out.append("| 场景 | Top 10 |")
        out.append("|---|---|")
        for k in labels:
            g = groups[k]
            if not g.top_samples:
                continue
            ranked = sorted(g.top_weighted.items(), key=lambda kv: kv[1], reverse=True)[:10]
            cells = "，".join(f"{name} {total / g.top_samples:.2f}" for name, total in ranked)
            out.append(f"| {k} | {cells} |")
        out.append("")

    # ---- 完整表
    if args.all:
        out.append("## 全部数值列（mean / p95 / 样本数）")
        out.append("")
        out.append("| 指标 | " + " | ".join(labels) + " |")
        out.append("|---|" + "---|" * len(labels))
        for metric in sorted(all_keys):
            cells = []
            for k in labels:
                values = groups[k].series.get(metric) or []
                if values:
                    cells.append(f"{fmt(sum(values) / len(values))} / {fmt(percentile(values, 95))} / {len(values)}")
                else:
                    cells.append("—")
            out.append(f"| `{metric}` | " + " | ".join(cells) + " |")
        out.append("")

    if notes:
        out.append("## 备注")
        out.append("")
        seen = set()
        for note in notes:
            if note not in seen:
                seen.add(note)
                out.append(f"- {note}")
        out.append("")

    text = "\n".join(out)
    print(text)
    if not args.no_write:
        target = args.out
        if target is None:
            first = args.paths[0]
            target = (first if first.is_dir() else first.parent) / "summary.md"
        target.write_text(text, encoding="utf-8")
        print(f"\n[summarize] 已写入 {target}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
