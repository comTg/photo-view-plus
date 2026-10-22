#!/usr/bin/env python3
"""R0 前台响应探针（docs/11-background-resource-governance.md §4）。只用标准库。

模拟"前台程序感受到的卡"，以普通优先级运行（启动时显式设为 NORMAL）。三个探针在同一个
线程里轮流执行，互不污染——计时器只统计 sleep 调用本身，计算和磁盘探针在两次 sleep 之间做：

  1. 计时器延迟：循环 time.sleep(0.005)，记录实际耗时超出 5ms 的部分（p50/p95/p99/max）。
  2. 固定计算量：每 2 秒做一次约 50ms 的 sha256。计算量（单元数）只在第一次运行时校准，
     写入校准文件；之后的场景复用同一个单元数，耗时变化才可比。默认校准文件放在输出目录，
     所以同一个测量目录里先跑 S0（空闲）就会用空闲机器校准。
  3. C: 盘延迟：每 1 秒对 %TEMP% 下的临时文件写 4KB + os.fsync（FlushFileBuffers）+ 读回，
     每轮重新打开/关闭文件（杀毒软件的打开/关闭过滤开销也计入）。

每 5 秒窗口输出一行 CSV：probe_<label>_<run-id>.csv。

用法：
  python scripts/perf/probe.py --duration 180 --out D:/perf/r0 --label S0
"""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import hashlib
import json
import math
import os
import platform
import re
import statistics
import sys
import tempfile
import time
from pathlib import Path

SLEEP_S = 0.005
DISK_EVERY_S = 1.0
CPU_EVERY_S = 2.0
UNIT_BYTES = 64 * 1024
_BUF = bytes(range(256)) * (UNIT_BYTES // 256)

COLUMNS = [
    "window_start",
    "window_end",
    "elapsed_s",
    "window_s",
    "label",
    "timer_n",
    "timer_p50_ms",
    "timer_p95_ms",
    "timer_p99_ms",
    "timer_max_ms",
    "cpu_work_n",
    "cpu_work_ms_mean",
    "cpu_work_ms_max",
    "cpu_work_slowdown_pct",
    "disk_n",
    "disk_write_fsync_ms_mean",
    "disk_write_fsync_ms_max",
    "disk_read_ms_mean",
    "disk_read_ms_max",
    "work_units",
    "work_base_ms",
]


def work(units: int) -> bytes:
    digest = b""
    for _ in range(units):
        digest = hashlib.sha256(_BUF).digest()
    return digest


def timed_work(units: int) -> float:
    t0 = time.perf_counter()
    work(units)
    return (time.perf_counter() - t0) * 1000.0


def calibrate(target_ms: float) -> tuple[int, float]:
    work(64)  # 预热
    t0 = time.perf_counter()
    work(256)
    per_unit = (time.perf_counter() - t0) / 256
    units = max(1, round(target_ms / 1000.0 / per_unit))
    runs = [timed_work(units) for _ in range(5)]
    units = max(1, round(units * target_ms / statistics.median(runs)))
    base = []
    for _ in range(7):
        base.append(timed_work(units))
        time.sleep(0.25)
    return units, statistics.median(base)


def load_or_calibrate(path: Path, target_ms: float, force: bool) -> tuple[int, float, str]:
    if path.exists() and not force:
        try:
            data = json.loads(path.read_text(encoding="utf-8-sig"))
            units = int(data["units"])
            base = float(data["base_ms"])
            if int(data.get("unit_bytes", UNIT_BYTES)) != UNIT_BYTES:
                raise ValueError("unit_bytes mismatch")
            note = f"使用校准文件 {path}（units={units}, base={base:.1f}ms）"
            if data.get("python") != platform.python_version():
                note += f"；注意：校准时 Python {data.get('python')}，当前 {platform.python_version()}"
            return units, base, note
        except (OSError, ValueError, KeyError, TypeError) as error:
            print(f"[probe] 校准文件不可用（{error}），重新校准", file=sys.stderr)
    units, base = calibrate(target_ms)
    payload = {
        "units": units,
        "unit_bytes": UNIT_BYTES,
        "target_ms": target_ms,
        "base_ms": round(base, 3),
        "python": platform.python_version(),
        "machine": platform.node(),
        "created": dt.datetime.now().astimezone().isoformat(timespec="seconds"),
    }
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")
        note = f"已校准并写入 {path}（units={units}, base={base:.1f}ms）。后续场景请复用此文件"
    except OSError as error:
        note = f"已校准（units={units}, base={base:.1f}ms），但写校准文件失败：{error}"
    return units, base, note


def set_normal_priority() -> str:
    if os.name != "nt":
        return "n/a"
    try:
        import ctypes
        from ctypes import wintypes

        k32 = ctypes.WinDLL("kernel32", use_last_error=True)
        k32.GetCurrentProcess.restype = wintypes.HANDLE
        k32.GetPriorityClass.argtypes = [wintypes.HANDLE]
        k32.GetPriorityClass.restype = wintypes.DWORD
        k32.SetPriorityClass.argtypes = [wintypes.HANDLE, wintypes.DWORD]
        k32.SetPriorityClass.restype = wintypes.BOOL
        handle = k32.GetCurrentProcess()
        before = k32.GetPriorityClass(handle)
        normal = 0x20
        if before != normal:
            k32.SetPriorityClass(handle, normal)
            return f"0x{before:X} -> NORMAL"
        return "NORMAL"
    except (OSError, AttributeError) as error:
        return f"unknown ({error})"


class DiskProbe:
    """每次：打开临时文件 → 写 4KB → os.fsync → 关闭（计为写延迟），再打开 → 读回 → 关闭（读延迟）。

    每轮都重新打开/关闭，杀毒软件在打开/关闭时的过滤开销也算进去，更接近前台程序存小文件的体验。
    读回通常命中系统缓存，主要反映文件系统过滤层开销。
    """

    _BINARY = getattr(os, "O_BINARY", 0)

    def __init__(self, directory: Path):
        directory.mkdir(parents=True, exist_ok=True)
        self.path = str(directory / f"pvp_probe_{os.getpid()}.tmp")

    def probe(self) -> tuple[float, float] | None:
        data = os.urandom(4096)
        try:
            t0 = time.perf_counter()
            fd = os.open(self.path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | self._BINARY, 0o600)
            try:
                os.write(fd, data)
                os.fsync(fd)
            finally:
                os.close(fd)
            t1 = time.perf_counter()
            fd = os.open(self.path, os.O_RDONLY | self._BINARY)
            try:
                os.read(fd, 4096)
            finally:
                os.close(fd)
            t2 = time.perf_counter()
        except OSError as error:  # 被占用/被杀软拦截时记一次缺测，不中断
            print(f"[probe] 磁盘探针失败：{error}", file=sys.stderr)
            return None
        return (t1 - t0) * 1000.0, (t2 - t1) * 1000.0

    def close(self) -> None:
        try:
            os.remove(self.path)
        except OSError:
            pass


def percentile(sorted_values: list[float], p: float) -> float | None:
    if not sorted_values:
        return None
    k = (len(sorted_values) - 1) * p / 100.0
    lo = math.floor(k)
    hi = math.ceil(k)
    if lo == hi:
        return sorted_values[lo]
    return sorted_values[lo] + (sorted_values[hi] - sorted_values[lo]) * (k - lo)


def fmt(value: float | None, digits: int = 3) -> str:
    if value is None or (isinstance(value, float) and not math.isfinite(value)):
        return ""
    return f"{value:.{digits}f}"


class Window:
    def __init__(self, start_wall: dt.datetime, start_pc: float):
        self.start_wall = start_wall
        self.start_pc = start_pc
        self.timer: list[float] = []
        self.cpu: list[float] = []
        self.disk_w: list[float] = []
        self.disk_r: list[float] = []

    def row(self, end_pc: float, t0_pc: float, label: str, units: int, base: float) -> dict:
        timer = sorted(self.timer)
        cpu_mean = statistics.fmean(self.cpu) if self.cpu else None
        end_wall = self.start_wall + dt.timedelta(seconds=end_pc - self.start_pc)
        return {
            "window_start": self.start_wall.isoformat(timespec="milliseconds"),
            "window_end": end_wall.isoformat(timespec="milliseconds"),
            "elapsed_s": fmt(end_pc - t0_pc),
            "window_s": fmt(end_pc - self.start_pc),
            "label": label,
            "timer_n": len(timer),
            "timer_p50_ms": fmt(percentile(timer, 50)),
            "timer_p95_ms": fmt(percentile(timer, 95)),
            "timer_p99_ms": fmt(percentile(timer, 99)),
            "timer_max_ms": fmt(timer[-1] if timer else None),
            "cpu_work_n": len(self.cpu),
            "cpu_work_ms_mean": fmt(cpu_mean),
            "cpu_work_ms_max": fmt(max(self.cpu) if self.cpu else None),
            "cpu_work_slowdown_pct": fmt((cpu_mean / base - 1.0) * 100.0 if cpu_mean else None, 2),
            "disk_n": len(self.disk_w),
            "disk_write_fsync_ms_mean": fmt(statistics.fmean(self.disk_w) if self.disk_w else None),
            "disk_write_fsync_ms_max": fmt(max(self.disk_w) if self.disk_w else None),
            "disk_read_ms_mean": fmt(statistics.fmean(self.disk_r) if self.disk_r else None),
            "disk_read_ms_max": fmt(max(self.disk_r) if self.disk_r else None),
            "work_units": units,
            "work_base_ms": fmt(base),
        }


def safe_label(label: str) -> str:
    cleaned = re.sub(r'[\\/:*?"<>|\s,]+', "_", label).strip("_")
    return cleaned or "run"


def resolve_out(out: str, label: str, run_id: str) -> Path:
    path = Path(out)
    if path.suffix.lower() == ".csv":
        return path
    return path / f"probe_{label}_{run_id}.csv"


def main() -> int:
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="replace")
        sys.stderr.reconfigure(errors="replace")

    parser = argparse.ArgumentParser(description="R0 前台响应探针（计时器 / 固定计算量 / C: 盘 fsync）")
    parser.add_argument("--duration", type=float, default=180.0, help="运行秒数，0 = 直到 Ctrl+C（默认 180）")
    parser.add_argument("--out", default=str(Path(tempfile.gettempdir()) / "pvp-perf"),
                        help="输出目录（或 .csv 文件路径）")
    parser.add_argument("--label", default="run", help="场景标签，如 S0..S4（重复轮次用同一个标签）")
    parser.add_argument("--run-id", default=None, help="文件名后缀；默认当前时间 yyyyMMdd-HHmmss")
    parser.add_argument("--window", type=float, default=5.0, help="统计窗口秒数（默认 5）")
    parser.add_argument("--calibration", default=None,
                        help="校准文件；默认 <输出目录>/probe_calibration.json")
    parser.add_argument("--recalibrate", action="store_true", help="忽略已有校准文件，重新校准")
    parser.add_argument("--work-ms", type=float, default=50.0, help="校准目标耗时 ms（默认 50）")
    parser.add_argument("--disk-dir", default=tempfile.gettempdir(), help="磁盘探针临时文件目录（默认 %%TEMP%%）")
    parser.add_argument("--quiet", action="store_true", help="不打印每个窗口的进度行")
    args = parser.parse_args()

    label = safe_label(args.label)
    run_id = args.run_id or dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    out_path = resolve_out(args.out, label, run_id)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    calib_path = Path(args.calibration) if args.calibration else out_path.parent / "probe_calibration.json"
    disk_dir = Path(args.disk_dir)

    priority = set_normal_priority()
    if sys.version_info < (3, 11) and os.name == "nt":
        print("[probe] 警告：Python < 3.11 在 Windows 上 sleep 精度约 15ms，计时器指标不可比", file=sys.stderr)
    units, base, calib_note = load_or_calibrate(calib_path, args.work_ms, args.recalibrate)
    if disk_dir.drive and disk_dir.drive.upper() != "C:":
        print(f"[probe] 注意：磁盘探针目录在 {disk_dir.drive}，不是 C:", file=sys.stderr)

    print(f"[probe] label={label} duration={args.duration:g}s window={args.window:g}s "
          f"python={platform.python_version()} priority={priority}")
    print(f"[probe] {calib_note}")
    print(f"[probe] csv: {out_path}")

    disk = DiskProbe(disk_dir)
    handle = open(out_path, "w", newline="", encoding="utf-8-sig")
    writer = csv.DictWriter(handle, fieldnames=COLUMNS)
    writer.writeheader()
    handle.flush()

    pc = time.perf_counter
    t0 = pc()
    wall0 = dt.datetime.now().astimezone()
    end = t0 + args.duration if args.duration > 0 else math.inf
    next_disk = t0 + DISK_EVERY_S / 2
    next_cpu = t0 + CPU_EVERY_S / 2
    next_win = t0 + args.window
    win = Window(wall0, t0)
    rows = 0

    def emit(w: Window, end_pc: float) -> None:
        nonlocal rows
        row = w.row(end_pc, t0, label, units, base)
        writer.writerow(row)
        handle.flush()
        rows += 1
        if not args.quiet:
            print(f"[probe {label} {end_pc - t0:5.0f}s] timer p50 {row['timer_p50_ms'] or '-'} "
                  f"p99 {row['timer_p99_ms'] or '-'} max {row['timer_max_ms'] or '-'} ms | "
                  f"work {row['cpu_work_ms_mean'] or '-'} ms ({row['cpu_work_slowdown_pct'] or '-'}%) | "
                  f"fsync {row['disk_write_fsync_ms_mean'] or '-'} ms", flush=True)

    try:
        while True:
            a = pc()
            time.sleep(SLEEP_S)
            b = pc()
            win.timer.append((b - a) * 1000.0 - SLEEP_S * 1000.0)
            if b >= next_disk:
                result = disk.probe()
                if result is not None:
                    win.disk_w.append(result[0])
                    win.disk_r.append(result[1])
                while next_disk <= b:
                    next_disk += DISK_EVERY_S
            if b >= next_cpu:
                win.cpu.append(timed_work(units))
                while next_cpu <= b:
                    next_cpu += CPU_EVERY_S
            now = pc()
            if now >= end:
                if win.timer:
                    emit(win, now)
                break
            if now >= next_win:
                # 正常情况下贴着名义边界切窗口；进程被挂起过（落后 >0.5s）就以当前时刻为界。
                boundary = next_win if now - next_win < 0.5 else now
                emit(win, boundary)
                win = Window(wall0 + dt.timedelta(seconds=boundary - t0), boundary)
                while next_win <= now:
                    next_win += args.window
    except KeyboardInterrupt:
        if win.timer:
            emit(win, pc())
        print("[probe] 已中断", file=sys.stderr)
    finally:
        handle.close()
        disk.close()
    print(f"[probe] done: {rows} windows -> {out_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
