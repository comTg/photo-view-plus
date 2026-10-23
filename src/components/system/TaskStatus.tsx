import type { RootSelection } from "@/hooks/useRoots";
import {
  aiPipelineStore,
  aiWorkerStatusStore,
  queueStatusStore,
  scanProgressStore,
  useEventStore,
} from "@/lib/stores";
import type { ScanProgress } from "@/lib/tauri-types";
import { memo } from "react";

export function aiStatusText(status: string | null | undefined): string {
  if (status === "ready") return "运行中";
  if (status === "starting") return "启动中";
  if (status === "stopping") return "停止中";
  if (status === "degraded") return "降级";
  if (status === "stopped") return "已停止";
  return "未知";
}

/** 全部模式下优先显示仍在进行中的扫描，否则显示最近一条；单选时显示该目录的。 */
function pickProgress(
  progress: Record<number, ScanProgress>,
  selection: RootSelection,
): ScanProgress | undefined {
  if (selection !== "all") return progress[selection];
  const all = Object.values(progress);
  return all.find((item) => item.processed < item.total) ?? all[all.length - 1];
}

/**
 * 底部状态栏的扫描 / 队列 / AI 进度。自己订阅 `scan:progress`、`queue:status`、`ai:worker_status`、
 * `ai:progress`（docs/09 P0-3、docs/11 R5），这些高频事件只重渲染这一小段文字。
 */
export const TaskStatus = memo(function TaskStatus({ selection }: { selection: RootSelection }) {
  const progress = pickProgress(useEventStore(scanProgressStore), selection);
  const queue = useEventStore(queueStatusStore);
  const aiStatus = useEventStore(aiWorkerStatusStore);
  const aiPipeline = useEventStore(aiPipelineStore);

  const progressText = progress
    ? `扫描 ${progress.processed.toLocaleString("zh-CN")}/${progress.total.toLocaleString("zh-CN")}`
    : "扫描空闲";
  const queueText = queue
    ? `缩略图队列 ${queue.p0} · 运行 ${queue.running}${queue.paused ? " · 已暂停" : ""}`
    : "队列状态未知";
  const aiText = `AI ${aiStatusText(aiStatus?.status)}`;
  const aiQueueText = aiPipeline
    ? `CLIP ${aiPipeline.embeddingPending} · 标签 ${aiPipeline.taggingPending}`
    : "AI 队列未知";
  return (
    <span>
      {progressText} · {queueText} · {aiText} · {aiQueueText}
    </span>
  );
});
