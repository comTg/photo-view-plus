import { useDedup } from "@/hooks/useDedup";
import { dedupExportCsv, thumbUrl, trashHistory, trashUndo } from "@/lib/tauri";
import type {
  DedupAction,
  DedupGroupMethod,
  DedupGroupStatus,
  DedupMethod,
  DuplicateGroup,
  UndoEntry,
} from "@/lib/tauri-types";
import { save } from "@tauri-apps/plugin-dialog";
import { memo, useCallback, useEffect, useState } from "react";

const THRESHOLD_PRESETS = [
  { label: "极严格 (Original)", value: 0 },
  { label: "严格 (VeryHigh)", value: 1 },
  { label: "平衡 (默认)", value: 2 },
  { label: "宽松 (High)", value: 5 },
  { label: "很宽松 (Medium)", value: 7 },
];

export interface DedupViewProps {
  onToast: (message: string) => void;
}

export function DedupView({ onToast }: DedupViewProps) {
  const dedup = useDedup();
  const [method, setMethod] = useState<DedupMethod>("both");
  const [threshold, setThreshold] = useState(2);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [keepIds, setKeepIds] = useState<Set<number>>(new Set());

  // biome-ignore lint/correctness/useExhaustiveDependencies: 故意在选中分组切换时清空 keep 选择
  useEffect(() => {
    setKeepIds(new Set());
  }, [dedup.selectedGroupId]);

  const handleStart = useCallback(async () => {
    try {
      await dedup.start(method, threshold);
      onToast(`已开始去重（${method}, 阈值 ${threshold}）`);
    } catch (e) {
      onToast(`启动失败：${String(e)}`);
    }
  }, [dedup, method, threshold, onToast]);

  const handleResolve = useCallback(
    async (action: DedupAction) => {
      if (dedup.selectedGroupId === null) return;
      try {
        const keeps = Array.from(keepIds);
        if (action === "trash" && keeps.length === 0) {
          onToast("请至少勾选 1 张要保留的图");
          return;
        }
        const result = await dedup.resolve({
          groupId: dedup.selectedGroupId,
          keepImageIds: keeps,
          action,
        });
        if (action === "trash") {
          const failures = result.trashFailures.length;
          onToast(
            failures === 0
              ? `已删除 ${result.trashed.length} 张，撤销 ID #${result.undoId ?? "-"}`
              : `删除 ${result.trashed.length} 张，失败 ${failures} 张`,
          );
        } else if (action === "keep_all") {
          onToast("已标记为保留全部");
        } else {
          onToast("已忽略此组");
        }
      } catch (e) {
        onToast(`处理失败：${String(e)}`);
      }
    },
    [dedup, keepIds, onToast],
  );

  const handleExport = useCallback(async () => {
    try {
      const filePath = await save({
        defaultPath: "duplicates.csv",
        filters: [{ name: "CSV", extensions: ["csv"] }],
      });
      if (!filePath || typeof filePath !== "string") return;
      const count = await dedupExportCsv({
        savePath: filePath,
        method: dedup.filter.method === "all" ? undefined : dedup.filter.method,
        status: dedup.filter.status,
      });
      onToast(`已导出 ${count} 行到 ${filePath}`);
    } catch (e) {
      onToast(`导出失败：${String(e)}`);
    }
  }, [dedup.filter.method, dedup.filter.status, onToast]);

  const toggleKeep = useCallback((id: number) => {
    setKeepIds((prev) => {
      const next = new Set(prev);
      if (next.has(id)) {
        next.delete(id);
      } else {
        next.add(id);
      }
      return next;
    });
  }, []);

  return (
    <section className="dedup-view">
      <header className="dedup-toolbar">
        <select
          className="control-select"
          value={method}
          onChange={(event) => setMethod(event.target.value as DedupMethod)}
        >
          <option value="both">完全重复 + 视觉近似</option>
          <option value="exact">仅完全重复 (BLAKE3)</option>
          <option value="phash">仅视觉近似 (dHash)</option>
        </select>
        <select
          className="control-select"
          value={threshold}
          onChange={(event) => setThreshold(Number(event.target.value))}
          disabled={method === "exact"}
          title="视觉近似的 hamming distance 阈值（仅 phash/both 时生效）"
        >
          {THRESHOLD_PRESETS.map((preset) => (
            <option key={preset.value} value={preset.value}>
              {preset.label}（dist ≤ {preset.value}）
            </option>
          ))}
        </select>
        <button
          type="button"
          className="toolbar-button"
          onClick={() => void handleStart()}
          disabled={dedup.status.running}
        >
          {dedup.status.running ? "运行中…" : "开始查找重复"}
        </button>
        <span className="dedup-status">
          阶段：{phaseLabel(dedup.status.phase)} · 哈希待办 {dedup.status.hashPending} · dHash 待办{" "}
          {dedup.status.dhashPending} · 已发现 {dedup.status.groupsFound} 组
        </span>
        <span className="dedup-spacer" />
        <select
          className="control-select"
          value={dedup.filter.method}
          onChange={(event) =>
            dedup.setFilter({
              ...dedup.filter,
              method: event.target.value as DedupGroupMethod | "all",
            })
          }
        >
          <option value="all">全部分组</option>
          <option value="exact">仅完全重复</option>
          <option value="phash">仅视觉近似</option>
        </select>
        <select
          className="control-select"
          value={dedup.filter.status}
          onChange={(event) =>
            dedup.setFilter({
              ...dedup.filter,
              status: event.target.value as DedupGroupStatus,
            })
          }
        >
          <option value="open">未处理</option>
          <option value="resolved">已处理</option>
          <option value="dismissed">已忽略</option>
        </select>
        <button type="button" className="toolbar-button" onClick={() => void handleExport()}>
          导出 CSV
        </button>
        <button
          type="button"
          className="toolbar-button"
          onClick={() => setHistoryOpen((value) => !value)}
        >
          {historyOpen ? "关闭历史" : "撤销历史"}
        </button>
      </header>

      {historyOpen && <UndoPanel onToast={onToast} onClose={() => setHistoryOpen(false)} />}

      <div className="dedup-split">
        <GroupList
          groups={dedup.groups}
          loading={dedup.loading}
          selectedId={dedup.selectedGroupId}
          onSelect={dedup.setSelectedGroupId}
        />
        <CompareView
          detail={dedup.detail}
          loading={dedup.detailLoading}
          keepIds={keepIds}
          onToggleKeep={toggleKeep}
          onResolve={handleResolve}
        />
      </div>

      {dedup.error && <div className="dedup-error">{dedup.error}</div>}
    </section>
  );
}

interface GroupListProps {
  groups: DuplicateGroup[];
  loading: boolean;
  selectedId: number | null;
  onSelect: (id: number | null) => void;
}

const GroupList = memo(function GroupList({
  groups,
  loading,
  selectedId,
  onSelect,
}: GroupListProps) {
  if (loading) {
    return <aside className="dedup-group-list">加载中…</aside>;
  }
  if (groups.length === 0) {
    return (
      <aside className="dedup-group-list dedup-group-list--empty">
        <p>当前没有匹配的分组。点击"开始查找重复"运行去重，或调整过滤条件。</p>
      </aside>
    );
  }
  return (
    <aside className="dedup-group-list">
      {groups.map((group) => (
        <button
          type="button"
          key={group.id}
          className={`dedup-group-item${selectedId === group.id ? " dedup-group-item--active" : ""}`}
          onClick={() => onSelect(group.id)}
        >
          <span className="dedup-group-method">
            {group.method === "exact" ? "完全重复" : "视觉近似"}
          </span>
          <span className="dedup-group-meta">
            #{group.id} · {group.itemCount} 张
            {group.threshold !== null ? ` · dist≤${group.threshold}` : ""}
          </span>
          <span className={`dedup-group-status dedup-group-status--${group.status}`}>
            {statusLabel(group.status)}
          </span>
        </button>
      ))}
    </aside>
  );
});

interface CompareViewProps {
  detail: ReturnType<typeof useDedup>["detail"];
  loading: boolean;
  keepIds: Set<number>;
  onToggleKeep: (id: number) => void;
  onResolve: (action: DedupAction) => Promise<void>;
}

const CompareView = memo(function CompareView({
  detail,
  loading,
  keepIds,
  onToggleKeep,
  onResolve,
}: CompareViewProps) {
  if (loading) {
    return <main className="dedup-compare">加载中…</main>;
  }
  if (!detail) {
    return (
      <main className="dedup-compare dedup-compare--empty">
        <p>从左侧选择一个分组查看详情。</p>
      </main>
    );
  }
  return (
    <main className="dedup-compare">
      <div className="dedup-compare-grid">
        {detail.items.map((item) => {
          const isKeep = keepIds.has(item.image.id);
          return (
            <button
              type="button"
              key={item.image.id}
              className={`dedup-item${isKeep ? " dedup-item--keep" : ""}`}
              onClick={() => onToggleKeep(item.image.id)}
              title={item.image.fullPath}
            >
              <div className="dedup-item-thumb">
                {item.image.thumbStatus === "ready" ? (
                  <img src={thumbUrl(item.image.id, 512)} alt={item.image.filename} />
                ) : (
                  <span>{item.image.thumbStatus}</span>
                )}
              </div>
              <div className="dedup-item-meta">
                <strong>{item.image.filename}</strong>
                <span>
                  {item.image.width ?? "?"}×{item.image.height ?? "?"} ·{" "}
                  {formatBytes(item.image.sizeBytes)}
                </span>
                <span className="dedup-item-path">{item.image.relPath}</span>
                {item.similarity !== null && (
                  <span>相似度 {(item.similarity * 100).toFixed(1)}%</span>
                )}
                <span
                  className={`dedup-item-keep-flag${isKeep ? " dedup-item-keep-flag--on" : ""}`}
                >
                  {isKeep ? "✓ 保留" : "点击勾选保留"}
                </span>
              </div>
            </button>
          );
        })}
      </div>

      <footer className="dedup-actions">
        <button type="button" className="primary-action" onClick={() => void onResolve("trash")}>
          删除未勾选项（回收站）
        </button>
        <button type="button" className="toolbar-button" onClick={() => void onResolve("keep_all")}>
          全部保留
        </button>
        <button type="button" className="toolbar-button" onClick={() => void onResolve("dismiss")}>
          标记不是重复
        </button>
      </footer>
    </main>
  );
});

interface UndoPanelProps {
  onToast: (message: string) => void;
  onClose: () => void;
}

function UndoPanel({ onToast, onClose }: UndoPanelProps) {
  const [entries, setEntries] = useState<UndoEntry[] | null>(null);
  const [loading, setLoading] = useState(false);

  const reload = useCallback(async () => {
    setLoading(true);
    try {
      const list = await trashHistory(50);
      setEntries(list);
    } catch (e) {
      onToast(`读取撤销历史失败：${String(e)}`);
    } finally {
      setLoading(false);
    }
  }, [onToast]);

  useEffect(() => {
    void reload();
  }, [reload]);

  const handleUndo = useCallback(
    async (id: number) => {
      try {
        const result = await trashUndo(id);
        const note = result.note ? `（${result.note}）` : "";
        onToast(`已撤销 #${id}，恢复 ${result.restored.length} 张${note}`);
        await reload();
      } catch (e) {
        onToast(`撤销失败：${String(e)}`);
      }
    },
    [onToast, reload],
  );

  return (
    <div className="dedup-undo-panel">
      <div className="dedup-undo-header">
        <strong>撤销历史</strong>
        <button type="button" className="toolbar-button" onClick={() => void reload()}>
          刷新
        </button>
        <button type="button" className="toolbar-button" onClick={onClose}>
          关闭
        </button>
      </div>
      {loading && <p>加载中…</p>}
      {entries && entries.length === 0 && <p className="muted">最近没有可撤销的删除。</p>}
      {entries && entries.length > 0 && (
        <ul className="dedup-undo-list">
          {entries.map((entry) => {
            const itemCount = parsePayloadItemCount(entry.payloadJson);
            return (
              <li key={entry.id} className="dedup-undo-item">
                <span>
                  #{entry.id} · {entry.action} · {itemCount ?? "?"} 张 · 创建{" "}
                  {formatTime(entry.createdAt)}
                </span>
                <button
                  type="button"
                  className="toolbar-button"
                  onClick={() => void handleUndo(entry.id)}
                >
                  撤销
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

function parsePayloadItemCount(json: string): number | null {
  try {
    const parsed = JSON.parse(json) as { items?: unknown[] };
    return Array.isArray(parsed.items) ? parsed.items.length : null;
  } catch {
    return null;
  }
}

function phaseLabel(phase: string): string {
  if (phase === "idle") return "空闲";
  if (phase === "hashing") return "哈希计算中";
  if (phase === "grouping") return "分组中";
  if (phase === "done") return "已完成";
  return phase;
}

function statusLabel(status: string): string {
  if (status === "open") return "未处理";
  if (status === "resolved") return "已处理";
  if (status === "dismissed") return "已忽略";
  return status;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value.toFixed(value >= 10 ? 0 : 1)} ${units[index]}`;
}

function formatTime(unix: number): string {
  return new Date(unix * 1000).toLocaleString("zh-CN");
}
