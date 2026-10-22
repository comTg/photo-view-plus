import { useTauriEvent } from "@/lib/events";
import { governorSetOverride, governorStatus } from "@/lib/tauri";
import type { GovernorStatus, LoadMode, LoadOverrideChoice } from "@/lib/tauri-types";
import { memo, useCallback, useEffect, useRef, useState } from "react";

const MODE_TEXT: Record<LoadMode, string> = {
  active: "使用中",
  away: "短暂离开",
  idle: "空闲",
  busy: "全屏 / 游戏",
};

const CHOICES: { id: LoadOverrideChoice; label: string; hint: string }[] = [
  { id: "auto", label: "自动", hint: "用电脑时降速，离开后全速" },
  { id: "fullSpeed", label: "全速", hint: "忽略使用中，全屏 / 游戏时仍让出资源" },
  { id: "pause1h", label: "暂停 1 小时", hint: "只保留浏览所需的缩略图" },
  { id: "pauseUntilRestart", label: "暂停到重启", hint: "只保留浏览所需的缩略图" },
];

/** 状态栏文案（docs/11 §5.5）。 */
export function loadModeText(status: GovernorStatus | null): string {
  if (!status) return "后台处理：未知";
  if (status.overrideMode === "paused") {
    return status.pausedUntil
      ? `后台处理：已暂停（至 ${formatClock(status.pausedUntil)}）`
      : "后台处理：已暂停（到重启）";
  }
  if (status.tier === "busy") return "后台处理：全屏 / 游戏中，已让出资源";
  if (status.overrideMode === "fullSpeed") return "后台处理：全速";
  const slowed = status.tier !== "idle";
  return `后台处理：自动（${MODE_TEXT[status.mode]}${slowed ? "，已降速" : "，全速"}）`;
}

function budgetTitle(status: GovernorStatus | null): string {
  if (!status) return "";
  const { budget } = status;
  const gpu =
    budget.gpu === "paused"
      ? "暂停"
      : budget.gpu === "duty"
        ? `占空比 ${Math.round((budget.gpuDutyCycle ?? 0) * 100)}%`
        : "不限";
  return [
    `缩略图并发 ${budget.batchThumbnails}`,
    `网络盘读 ${budget.networkReads}`,
    `哈希 ${budget.hashTasks ?? "不限"}`,
    `AI（GPU）${gpu}`,
    `大模型${budget.vlm ? "可运行" : "不运行"}`,
  ].join(" · ");
}

function formatClock(unixSeconds: number): string {
  return new Date(unixSeconds * 1000).toLocaleTimeString("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
  });
}

function currentChoice(status: GovernorStatus | null): LoadOverrideChoice | null {
  if (!status) return null;
  if (status.overrideMode === "paused") {
    return status.pausedUntil ? "pause1h" : "pauseUntilRestart";
  }
  return status.overrideMode;
}

/** 底部状态栏的"后台处理"开关：显示当前档位，点开切换手动覆盖。自己订阅 `system:load_mode`。 */
export const LoadModeControl = memo(function LoadModeControl({
  onError,
}: {
  onError?: (message: string) => void;
}) {
  const [status, setStatus] = useState<GovernorStatus | null>(null);
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let cancelled = false;
    governorStatus()
      .then((next) => {
        if (!cancelled) setStatus(next);
      })
      .catch((error) => console.warn("读取后台处理状态失败", error));
    return () => {
      cancelled = true;
    };
  }, []);

  const handleEvent = useCallback((next: GovernorStatus) => setStatus(next), []);
  useTauriEvent<GovernorStatus>("system:load_mode", handleEvent);

  useEffect(() => {
    if (!open) return;
    const close = (event: PointerEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) setOpen(false);
    };
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };
    window.addEventListener("pointerdown", close);
    window.addEventListener("keydown", closeOnEscape);
    return () => {
      window.removeEventListener("pointerdown", close);
      window.removeEventListener("keydown", closeOnEscape);
    };
  }, [open]);

  const choose = useCallback(
    (choice: LoadOverrideChoice) => {
      setOpen(false);
      governorSetOverride(choice)
        .then(setStatus)
        .catch((error) => onError?.(`切换后台处理模式失败：${String(error)}`));
    },
    [onError],
  );

  const selected = currentChoice(status);
  const tone =
    status?.overrideMode === "paused" || status?.tier === "busy"
      ? "paused"
      : status?.tier === "idle"
        ? "full"
        : "slowed";

  return (
    <div className="load-mode" ref={rootRef}>
      <button
        type="button"
        className={`load-mode__button load-mode__button--${tone}`}
        title={budgetTitle(status)}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      >
        <span className="load-mode__dot" aria-hidden="true" />
        {loadModeText(status)}
      </button>
      {open && (
        <div className="load-mode__menu" role="menu">
          {CHOICES.map((choice) => (
            <button
              key={choice.id}
              type="button"
              role="menuitemradio"
              aria-checked={selected === choice.id}
              className={selected === choice.id ? "is-selected" : undefined}
              onClick={() => choose(choice.id)}
            >
              <strong>{choice.label}</strong>
              <span>{choice.hint}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
});
