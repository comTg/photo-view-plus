import { useTauriEvent } from "@/lib/events";
import {
  analysisProgress,
  analysisRequeueOutdated,
  analysisRetryFailed,
  nightTaskSet,
  nightTaskStatus,
  tagsCleanupRam,
  tagsRamStatus,
  vlmDetectPaths,
  vlmServerStop,
} from "@/lib/tauri";
import type {
  AnalysisProgress,
  AppSettings,
  AppSettingsPatch,
  NightTaskStatus,
  RamTagStatus,
} from "@/lib/tauri-types";
import { ask } from "@tauri-apps/plugin-dialog";
import { Eraser, RotateCcw, Search, Square } from "lucide-react";
import { memo, useCallback, useEffect, useState } from "react";

const HOURS = Array.from({ length: 24 }, (_, hour) => hour);

const SERVER_TEXT: Record<string, string> = {
  stopped: "未加载",
  starting: "加载中",
  ready: "已加载",
  degraded: "启动失败",
};

/** 订阅 VLM 打标进度（docs/10 A6）：先取一次快照，之后跟 `analysis:progress` 事件。 */
export function useAnalysisProgress(): AnalysisProgress | null {
  const [progress, setProgress] = useState<AnalysisProgress | null>(null);
  useEffect(() => {
    let cancelled = false;
    analysisProgress()
      .then((next) => {
        if (!cancelled) setProgress(next);
      })
      .catch((error) => console.warn("读取大模型打标进度失败", error));
    return () => {
      cancelled = true;
    };
  }, []);
  const handleEvent = useCallback((next: AnalysisProgress) => setProgress(next), []);
  useTauriEvent<AnalysisProgress>("analysis:progress", handleEvent);
  return progress;
}

export function analysisStateText(progress: AnalysisProgress | null): string {
  if (!progress) return "未知";
  const reason = progress.reason ? `：${progress.reason}` : "";
  switch (progress.state) {
    case "disabled":
      return "已关闭";
    case "notConfigured":
      return `未配置${reason}`;
    case "waiting":
      return `等待${reason}`;
    case "paused":
      return `暂停${reason}`;
    case "starting":
      return "正在加载模型…";
    case "running":
      return "分析中";
    case "idle":
      return "全部完成";
    default:
      return `出错${reason}`;
  }
}

export function formatEta(seconds: number | null | undefined): string {
  if (seconds == null) return "-";
  if (seconds < 90) return `约 ${Math.max(1, Math.round(seconds))} 秒`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 90) return `约 ${minutes} 分钟`;
  return `约 ${Math.floor(minutes / 60)} 小时 ${minutes % 60} 分钟`;
}

function serverText(progress: AnalysisProgress | null): string {
  if (!progress) return "-";
  const { server } = progress;
  const base = SERVER_TEXT[server.state] ?? server.state;
  if (server.state === "ready") {
    return server.owned
      ? `${base}（端口 ${server.port}）`
      : `${base}（复用端口 ${server.port} 上的服务）`;
  }
  return server.lastError && server.state !== "stopped" ? `${base}：${server.lastError}` : base;
}

/** 路径输入：本地编辑，失焦或回车时才提交，避免每个按键都写一次配置。 */
function PathInput({
  label,
  value,
  placeholder,
  onCommit,
}: {
  label: string;
  value: string;
  placeholder: string;
  onCommit: (value: string) => void;
}) {
  const [draft, setDraft] = useState(value);
  useEffect(() => setDraft(value), [value]);
  const commit = () => {
    if (draft.trim() !== value) onCommit(draft.trim());
  };
  return (
    <label className="settings-field settings-field--wide">
      <span>{label}</span>
      <input
        type="text"
        value={draft}
        placeholder={placeholder}
        spellCheck={false}
        onChange={(event) => setDraft(event.target.value)}
        onBlur={commit}
        onKeyDown={(event) => {
          if (event.key === "Enter") commit();
        }}
      />
    </label>
  );
}

/** 设置页"大模型打标"一节（docs/10 A6）：开关、模型路径、并发与输入尺寸、进度与维护操作。 */
export const AnalysisSettings = memo(function AnalysisSettings({
  settings,
  onPatch,
  onMessage,
}: {
  settings: AppSettings;
  onPatch: (patch: AppSettingsPatch) => Promise<void> | void;
  onMessage: (message: string) => void;
}) {
  const progress = useAnalysisProgress();
  const [busy, setBusy] = useState(false);
  const [ram, setRam] = useState<RamTagStatus | null>(null);
  const [night, setNight] = useState<NightTaskStatus | null>(null);
  useEffect(() => {
    nightTaskStatus()
      .then(setNight)
      .catch((error) => console.warn("读取夜间分析状态失败", error));
  }, []);
  // 覆盖率每分析 100 张刷新一次就够了。
  const analyzedBucket = Math.floor((progress?.analyzed ?? 0) / 100);
  // biome-ignore lint/correctness/useExhaustiveDependencies: analyzedBucket 只是刷新信号
  useEffect(() => {
    tagsRamStatus()
      .then(setRam)
      .catch((error) => console.warn("读取 RAM 旧标签状态失败", error));
  }, [analyzedBucket]);

  const run = useCallback(
    async (action: () => Promise<string>) => {
      setBusy(true);
      try {
        onMessage(await action());
      } catch (error) {
        onMessage(String(error));
      } finally {
        setBusy(false);
      }
    },
    [onMessage],
  );

  const detect = () =>
    run(async () => {
      const found = await vlmDetectPaths();
      if (!found) return "没有找到 llama-server 和 Qwen3.5-9B 模型文件，请手动填写路径";
      await onPatch({
        vlmServerExe: found.exe,
        vlmModelPath: found.model,
        vlmMmprojPath: found.mmproj,
      });
      return `已找到模型：${found.model}`;
    });

  const total = progress ? progress.analyzed + progress.pending : 0;

  const updateNight = (enabled: boolean, startHour: number, endHour: number) =>
    run(async () => {
      const next = await nightTaskSet(enabled, startHour, endHour);
      setNight(next);
      await onPatch({
        nightAnalyzeEnabled: next.enabled,
        nightStartHour: next.startHour,
        nightEndHour: next.endHour,
      });
      if (!enabled) return "已关闭夜间自动分析，计划任务已删除";
      return `已注册计划任务"${next.taskName}"：每天 ${next.startHour}:00 开始，${next.endHour}:00 前结束`;
    });

  const cleanupRam = () =>
    run(async () => {
      if (!ram) return "RAM 旧标签状态未知";
      const confirmed = await ask(
        `将删除 ${ram.ramImages.toLocaleString("zh-CN")} 张图片上的 RAM-plus 旧标签（共 ${ram.ramTags.toLocaleString("zh-CN")} 个不同标签）。大模型已分析 ${(ram.coverage * 100).toFixed(1)}% 的图片，标签以后由大模型提供。删除后不能恢复，要继续吗？`,
        { title: "清理 RAM 旧标签", kind: "warning" },
      );
      if (!confirmed) return "已取消";
      const next = await tagsCleanupRam();
      setRam(next);
      return "RAM 旧标签已清理";
    });
  return (
    <div className="settings-panel settings-panel--wide">
      <div className="settings-panel__header">
        <div>
          <span className="settings-eyebrow">大模型打标（Qwen3.5-9B）</span>
          <strong>{analysisStateText(progress)}</strong>
        </div>
        <div className="settings-panel__actions">
          <button
            type="button"
            className="toolbar-button"
            disabled={busy}
            onClick={() => void detect()}
            title="在本应用的模型目录和 memora 的目录里查找 llama-server 与模型文件"
          >
            <Search aria-hidden="true" />
            自动查找
          </button>
          <button
            type="button"
            className="toolbar-button"
            disabled={busy || progress?.server.state !== "ready" || !progress?.server.owned}
            onClick={() =>
              void run(async () =>
                (await vlmServerStop()) ? "已卸载大模型，显存已释放" : "没有本应用启动的大模型服务",
              )
            }
            title="立即卸载本应用启动的 llama-server（复用的外部服务不受影响）"
          >
            <Square aria-hidden="true" />
            卸载模型
          </button>
          <button
            type="button"
            className="toolbar-button"
            disabled={busy || !progress || progress.failed === 0}
            onClick={() =>
              void run(
                async () =>
                  `已重新排队 ${(await analysisRetryFailed()).toLocaleString("zh-CN")} 张失败的图`,
              )
            }
            title="清掉失败记录，下次空闲时重试（例如 NAS 修好之后）"
          >
            <RotateCcw aria-hidden="true" />
            重试失败
          </button>
          <button
            type="button"
            className="toolbar-button"
            disabled={busy}
            onClick={() =>
              void run(
                async () =>
                  `已把 ${(await analysisRequeueOutdated()).toLocaleString("zh-CN")} 张旧版本结果重新排队`,
              )
            }
            title="提示词或模型升级后，让旧版本的分析结果在空闲时重新分析"
          >
            <RotateCcw aria-hidden="true" />
            重新分析旧版本
          </button>
        </div>
      </div>
      <p className="settings-hint muted">
        用本地视觉大模型给照片写中文描述、标签和画面文字，用于自然语言搜索。全库分析需要几个小时
        GPU， 只在电脑空闲时运行（状态栏可切"全速"）；进入全屏或游戏时立即卸载。和 memora 共用 8081
        端口的服务。
      </p>
      <label className="settings-toggle">
        <span>启用大模型打标</span>
        <input
          type="checkbox"
          checked={settings.vlmEnabled}
          onChange={(event) => void onPatch({ vlmEnabled: event.target.checked })}
        />
      </label>
      <div className="settings-field-grid">
        <PathInput
          label="llama-server 程序"
          value={settings.vlmServerExe}
          placeholder="留空自动查找"
          onCommit={(value) => void onPatch({ vlmServerExe: value })}
        />
        <PathInput
          label="模型文件（.gguf）"
          value={settings.vlmModelPath}
          placeholder="留空自动查找"
          onCommit={(value) => void onPatch({ vlmModelPath: value })}
        />
        <PathInput
          label="视觉投影文件（mmproj）"
          value={settings.vlmMmprojPath}
          placeholder="留空自动查找"
          onCommit={(value) => void onPatch({ vlmMmprojPath: value })}
        />
        <label className="settings-field">
          <span>并发槽位</span>
          <select
            value={settings.vlmParallel}
            onChange={(event) => void onPatch({ vlmParallel: Number(event.target.value) })}
          >
            <option value={2}>2</option>
            <option value={4}>4（推荐）</option>
          </select>
        </label>
        <label className="settings-field">
          <span>输入图长边</span>
          <select
            value={settings.vlmLongEdge}
            onChange={(event) => void onPatch({ vlmLongEdge: Number(event.target.value) })}
          >
            <option value={1024}>1024（推荐）</option>
            <option value={768}>768（更快）</option>
          </select>
        </label>
      </div>
      <div className="ai-kv">
        <span>已分析</span>
        <strong>
          {progress
            ? `${progress.analyzed.toLocaleString("zh-CN")} / ${total.toLocaleString("zh-CN")}`
            : "-"}
        </strong>
        <span>待分析</span>
        <strong>{progress ? progress.pending.toLocaleString("zh-CN") : "-"}</strong>
        <span>失败</span>
        <strong>{progress ? progress.failed.toLocaleString("zh-CN") : "-"}</strong>
        <span>速度</span>
        <strong>
          {progress?.secsPerImage != null ? `${progress.secsPerImage.toFixed(1)} 秒/张` : "-"}
        </strong>
        <span>预计剩余</span>
        <strong>{formatEta(progress?.etaSecs)}</strong>
        <span>模型服务</span>
        <strong>{serverText(progress)}</strong>
        <span>版本</span>
        <strong>{progress?.model ?? "-"}</strong>
      </div>
      {progress?.lastError && progress.state !== "running" && (
        <p className="settings-warning">最近的错误：{progress.lastError}</p>
      )}
      <div className="settings-inline-row">
        <label className="settings-toggle settings-toggle--inline">
          <span>夜间自动分析（Windows 计划任务）</span>
          <input
            type="checkbox"
            checked={settings.nightAnalyzeEnabled}
            disabled={busy || (!settings.vlmEnabled && !settings.nightAnalyzeEnabled)}
            onChange={(event) =>
              void updateNight(event.target.checked, settings.nightStartHour, settings.nightEndHour)
            }
          />
        </label>
        <span>
          <select
            value={settings.nightStartHour}
            disabled={busy}
            aria-label="开始时间"
            onChange={(event) =>
              void updateNight(
                settings.nightAnalyzeEnabled,
                Number(event.target.value),
                settings.nightEndHour,
              )
            }
          >
            {HOURS.map((hour) => (
              <option key={hour} value={hour}>
                {hour}:00
              </option>
            ))}
          </select>
          {" 至 "}
          <select
            value={settings.nightEndHour}
            disabled={busy}
            aria-label="结束时间"
            onChange={(event) =>
              void updateNight(
                settings.nightAnalyzeEnabled,
                settings.nightStartHour,
                Number(event.target.value),
              )
            }
          >
            {HOURS.map((hour) => (
              <option key={hour} value={hour}>
                {hour}:00
              </option>
            ))}
          </select>
        </span>
      </div>
      <p className="settings-hint muted">
        {night?.registered
          ? `已注册计划任务"${night.taskName}"。到点后在后台（无窗口）分析，处理完或到结束时间就退出；主程序开着时交给主程序。`
          : settings.vlmEnabled
            ? "打开后注册一个计划任务，每天在开始时间运行一次后台分析（只在电脑开机、已登录时运行）。默认 3:00–7:00，避开 memora 2:00 的任务。"
            : "需要先启用大模型打标。"}
      </p>
      {ram && ram.ramImages > 0 && (
        <div className="settings-inline-row">
          <span>
            RAM-plus 旧标签：{ram.ramImages.toLocaleString("zh-CN")} 张图、
            {ram.ramTags.toLocaleString("zh-CN")} 个标签；大模型已覆盖{" "}
            {(ram.coverage * 100).toFixed(1)}%
            {ram.canCleanup ? "，可以清理" : "（到 95% 后可清理）"}
          </span>
          <button
            type="button"
            className="toolbar-button"
            disabled={busy || !ram.canCleanup}
            onClick={() => void cleanupRam()}
            title="大模型覆盖率到 95% 后，删除 RAM-plus 打的旧标签（需要确认）"
          >
            <Eraser aria-hidden="true" />
            清理 RAM 旧标签
          </button>
        </div>
      )}
    </div>
  );
});

/** 底部状态栏：大模型正在分析或排队时显示一行进度；其他时候不占位置。 */
export const AnalysisBadge = memo(function AnalysisBadge() {
  const progress = useAnalysisProgress();
  if (!progress) return null;
  let text: string | null = null;
  if (progress.state === "starting") {
    text = "大模型 加载中…";
  } else if (progress.state === "running") {
    const rate =
      progress.secsPerImage != null ? ` · ${progress.secsPerImage.toFixed(1)} 秒/张` : "";
    const eta = progress.etaSecs != null ? ` · ${formatEta(progress.etaSecs)}` : "";
    text = `大模型 待分析 ${progress.pending.toLocaleString("zh-CN")}${rate}${eta}`;
  } else if (
    (progress.state === "waiting" || progress.state === "paused") &&
    progress.pending > 0
  ) {
    text = `大模型 待分析 ${progress.pending.toLocaleString("zh-CN")}（空闲时运行）`;
  }
  if (!text) return null;
  return (
    <span className="analysis-badge" title={analysisStateText(progress)}>
      {text}
    </span>
  );
});
