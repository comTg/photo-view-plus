import { ContextMenu, menuPosition } from "@/components/ui/ContextMenu";
import { useDedup } from "@/hooks/useDedup";
import {
  dedupExportCsv,
  dedupGroupDetail,
  imagesRevealInDir,
  originalUrl,
  thumbUrl,
  trashHistory,
  trashUndo,
} from "@/lib/tauri";
import type {
  DedupAction,
  DedupGroupDetail,
  DedupGroupMethod,
  DedupGroupStatus,
  DedupMethod,
  DedupResolveResult,
  DuplicateGroup,
  UndoEntry,
} from "@/lib/tauri-types";
import { save } from "@tauri-apps/plugin-dialog";
import { Archive, CheckCircle2, Circle, Loader2, Trash2, XCircle } from "lucide-react";
import { type CSSProperties, memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Virtuoso } from "react-virtuoso";

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

  const handleStart = useCallback(async () => {
    try {
      await dedup.start(method, threshold);
      onToast(`已开始去重（${method}, 阈值 ${threshold}）`);
    } catch (e) {
      onToast(`启动失败：${String(e)}`);
    }
  }, [dedup, method, threshold, onToast]);

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
          {dedup.status.dhashPending} · 已发现 {dedup.status.groupsFound} 组 · 已加载{" "}
          {dedup.groups.length}/{dedup.total} 组
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

      <DedupVirtualList
        groups={dedup.groups}
        loading={dedup.loading}
        loadingMore={dedup.loadingMore}
        hasMore={dedup.hasMoreGroups}
        running={dedup.status.running}
        onToast={onToast}
        onLoadMore={dedup.loadMoreGroups}
        onResolve={dedup.resolve}
      />

      {dedup.error && <div className="dedup-error">{dedup.error}</div>}
    </section>
  );
}

interface DedupVirtualListProps {
  groups: DuplicateGroup[];
  loading: boolean;
  loadingMore: boolean;
  hasMore: boolean;
  running: boolean;
  onToast: (message: string) => void;
  onLoadMore: () => Promise<void>;
  onResolve: (args: {
    groupId: number;
    keepImageIds: number[];
    action: DedupAction;
  }) => Promise<DedupResolveResult>;
}

const DedupVirtualList = memo(function DedupVirtualList({
  groups,
  loading,
  loadingMore,
  hasMore,
  running,
  onToast,
  onLoadMore,
  onResolve,
}: DedupVirtualListProps) {
  if (groups.length === 0) {
    return (
      <div className="dedup-list-shell dedup-list-shell--empty">
        <p>{loading ? "加载中..." : "当前没有匹配的分组。点击开始查找重复，或调整过滤条件。"}</p>
      </div>
    );
  }

  return (
    <div className="dedup-list-shell">
      <Virtuoso
        className="dedup-virtual-list"
        data={groups}
        computeItemKey={(_, group) => group.id}
        endReached={() => {
          if (hasMore) void onLoadMore();
        }}
        increaseViewportBy={{ top: 400, bottom: 900 }}
        itemContent={(_, group) => (
          <DedupGroupRow group={group} running={running} onToast={onToast} onResolve={onResolve} />
        )}
        components={{
          Footer: () => (
            <div className="dedup-list-footer">
              {loadingMore ? "加载更多..." : hasMore ? "继续向下滚动加载更多" : "已显示全部分组"}
            </div>
          ),
        }}
      />
    </div>
  );
});

interface DedupGroupRowProps {
  group: DuplicateGroup;
  running: boolean;
  onToast: (message: string) => void;
  onResolve: (args: {
    groupId: number;
    keepImageIds: number[];
    action: DedupAction;
  }) => Promise<DedupResolveResult>;
}

const DedupGroupRow = memo(function DedupGroupRow({
  group,
  running,
  onToast,
  onResolve,
}: DedupGroupRowProps) {
  const [detail, setDetail] = useState<DedupGroupDetail | null>(null);
  const [keepIds, setKeepIds] = useState<Set<number>>(new Set());
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [resolving, setResolving] = useState(false);
  const [imageMenu, setImageMenu] = useState<{
    x: number;
    y: number;
    image: DedupGroupDetail["items"][number]["image"];
  } | null>(null);

  const loadDetail = useCallback(
    async (shouldApply: () => boolean = () => true) => {
      setLoading(true);
      setError(null);
      setDetail(null);
      setKeepIds(new Set());

      try {
        const next = await dedupGroupDetail(group.id);
        if (!shouldApply()) return;
        setDetail(next);
        const suggested = next ? suggestKeepId(next) : null;
        setKeepIds(suggested === null ? new Set() : new Set([suggested]));
      } catch (e) {
        if (shouldApply()) setError(String(e));
      } finally {
        if (shouldApply()) setLoading(false);
      }
    },
    [group.id],
  );

  useEffect(() => {
    let alive = true;
    void loadDetail(() => alive);

    return () => {
      alive = false;
    };
  }, [loadDetail]);

  const selectedDeleteCount = useMemo(() => {
    if (!detail) return 0;
    return detail.items.filter((item) => !keepIds.has(item.image.id)).length;
  }, [detail, keepIds]);

  const canResolve = group.status === "open" && !running && !resolving && detail !== null;

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

  const revealImage = useCallback(
    async (imageId: number) => {
      try {
        await imagesRevealInDir(imageId);
      } catch (e) {
        onToast(`打开所在文件夹失败：${String(e)}`);
      }
    },
    [onToast],
  );

  const copyImagePath = useCallback(
    async (path: string) => {
      await navigator.clipboard.writeText(path);
      onToast("已复制图片路径");
    },
    [onToast],
  );

  const handleResolve = useCallback(
    async (action: DedupAction) => {
      if (!detail || resolving) return;
      const keeps = Array.from(keepIds);
      if (action === "trash") {
        if (keeps.length === 0) {
          onToast("请至少保留 1 张图");
          return;
        }
        if (selectedDeleteCount === 0) {
          onToast("当前没有待删除图片");
          return;
        }
      }

      setResolving(true);
      try {
        const result = await onResolve({
          groupId: group.id,
          keepImageIds: keeps,
          action,
        });
        if (action === "trash" && result.trashFailures.length > 0) {
          await loadDetail();
        }
        showResolveToast(action, result, onToast);
      } catch (e) {
        onToast(`处理失败：${String(e)}`);
      } finally {
        setResolving(false);
      }
    },
    [detail, group.id, keepIds, loadDetail, onResolve, onToast, resolving, selectedDeleteCount],
  );

  return (
    <article className="dedup-group-row">
      <header className="dedup-row-header">
        <div className="dedup-row-title">
          <strong>{group.method === "exact" ? "完全重复" : "视觉近似"}</strong>
          <span>
            #{group.id} · {group.itemCount} 张
            {group.threshold !== null ? ` · dist≤${group.threshold}` : ""}
          </span>
        </div>
        <span className={`dedup-group-status dedup-group-status--${group.status}`}>
          {statusLabel(group.status)}
        </span>
        <div className="dedup-row-actions">
          <button
            type="button"
            className="primary-action dedup-action-button"
            disabled={!canResolve || selectedDeleteCount === 0}
            onClick={() => void handleResolve("trash")}
            title={running ? "去重运行中，完成后可删除" : "删除未保留项到回收站"}
          >
            {resolving ? <Loader2 aria-hidden="true" /> : <Trash2 aria-hidden="true" />}
            删除 {selectedDeleteCount}
          </button>
          <button
            type="button"
            className="toolbar-button dedup-action-button"
            disabled={!canResolve}
            onClick={() => void handleResolve("keep_all")}
          >
            <Archive aria-hidden="true" />
            全部保留
          </button>
          <button
            type="button"
            className="toolbar-button dedup-action-button"
            disabled={!canResolve}
            onClick={() => void handleResolve("dismiss")}
          >
            <XCircle aria-hidden="true" />
            忽略
          </button>
        </div>
      </header>

      {loading && <div className="dedup-row-state">加载图片...</div>}
      {error && <div className="dedup-row-state dedup-row-state--error">{error}</div>}
      {!loading && !error && !detail && <div className="dedup-row-state">分组不存在</div>}
      {detail && (
        <div className="dedup-image-list">
          {detail.items.map((item) => {
            const isKeep = keepIds.has(item.image.id);
            return (
              <button
                type="button"
                key={item.image.id}
                className={`dedup-image-row${isKeep ? " dedup-image-row--keep" : ""}`}
                onClick={() => toggleKeep(item.image.id)}
                onContextMenu={(event) => {
                  event.preventDefault();
                  event.stopPropagation();
                  setImageMenu({ ...menuPosition(event), image: item.image });
                }}
                title={item.image.fullPath}
              >
                <span className="dedup-image-keep">
                  {isKeep ? <CheckCircle2 aria-hidden="true" /> : <Circle aria-hidden="true" />}
                </span>
                <ThumbHoverPreview image={item.image} />
                <span className="dedup-image-main">
                  <strong>{item.image.filename}</strong>
                  <span>{item.image.relPath}</span>
                </span>
                <span className="dedup-image-stats">
                  <span>{formatResolution(item.image.width, item.image.height)}</span>
                  <span>{formatBytes(item.image.sizeBytes)}</span>
                  <span>{formatTime(item.image.mtime)}</span>
                  {item.similarity !== null && (
                    <span>相似度 {(item.similarity * 100).toFixed(1)}%</span>
                  )}
                </span>
              </button>
            );
          })}
        </div>
      )}
      <ContextMenu
        menu={imageMenu}
        onClose={() => setImageMenu(null)}
        items={
          imageMenu
            ? [
                {
                  id: "reveal",
                  label: "在文件夹中显示",
                  onSelect: () => void revealImage(imageMenu.image.id),
                },
                {
                  id: "copy-path",
                  label: "复制完整路径",
                  onSelect: () => void copyImagePath(imageMenu.image.fullPath),
                },
              ]
            : []
        }
      />
    </article>
  );
});

type DedupImage = DedupGroupDetail["items"][number]["image"];

// 鼠标在缩略图上停留 350ms 后才弹原图预览，避免快速划过时连发原图请求
// （网络盘原图读取有并发闸门，见 Rust read_gate）。
const HOVER_PREVIEW_DELAY_MS = 350;

/** 去重列表里的缩略图：鼠标悬停一定时间后在浮层里预览原图。 */
function ThumbHoverPreview({ image }: { image: DedupImage }) {
  const [anchor, setAnchor] = useState<DOMRect | null>(null);
  const timerRef = useRef<number | null>(null);

  const clearTimer = () => {
    if (timerRef.current !== null) {
      window.clearTimeout(timerRef.current);
      timerRef.current = null;
    }
  };

  // 卸载（含 Virtuoso 回收行）时清掉待触发的定时器。
  useEffect(
    () => () => {
      if (timerRef.current !== null) {
        window.clearTimeout(timerRef.current);
      }
    },
    [],
  );

  return (
    <span
      className="dedup-image-thumb"
      onMouseEnter={(event) => {
        const rect = event.currentTarget.getBoundingClientRect();
        clearTimer();
        timerRef.current = window.setTimeout(() => setAnchor(rect), HOVER_PREVIEW_DELAY_MS);
      }}
      onMouseLeave={() => {
        clearTimer();
        setAnchor(null);
      }}
    >
      {image.thumbStatus === "ready" ? (
        <img src={thumbUrl(image.id, 256)} alt={image.filename} />
      ) : (
        <span>{image.thumbStatus}</span>
      )}
      {anchor && <HoverPreviewPopup image={image} anchor={anchor} />}
    </span>
  );
}

/** 原图预览浮层。用 portal 挂到 body，避开 Virtuoso 的 transform 容器对 fixed 定位的影响。 */
function HoverPreviewPopup({ image, anchor }: { image: DedupImage; anchor: DOMRect }) {
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");

  const margin = 12;
  const maxHeight = 520;
  // 右侧空间够就放右边，否则放左边；纵向夹在视口内。
  const placeLeft = window.innerWidth - anchor.right < 300;
  const top = Math.max(margin, Math.min(anchor.top, window.innerHeight - maxHeight - margin));
  const style: CSSProperties = placeLeft
    ? { top, right: window.innerWidth - anchor.left + margin }
    : { top, left: anchor.right + margin };

  return createPortal(
    <div className="dedup-hover-preview" style={style}>
      {status !== "ready" && (
        <span className="dedup-hover-preview__hint">
          {status === "error" ? "原图加载失败" : "加载原图…"}
        </span>
      )}
      <img
        className="dedup-hover-preview__img"
        src={originalUrl(image.id)}
        alt={image.filename}
        onLoad={() => setStatus("ready")}
        onError={() => setStatus("error")}
        style={{ display: status === "ready" ? "block" : "none" }}
      />
      {status === "ready" && <span className="dedup-hover-preview__name">{image.filename}</span>}
    </div>,
    document.body,
  );
}

function showResolveToast(
  action: DedupAction,
  result: DedupResolveResult,
  onToast: (message: string) => void,
) {
  if (action === "trash") {
    const failures = result.trashFailures.length;
    const permanent = result.permanentlyDeleted.length;
    const permanentNote =
      permanent > 0 ? `（其中 ${permanent} 张网络盘文件已永久删除，不可恢复）` : "";
    onToast(
      failures === 0
        ? `已删除 ${result.trashed.length} 张，撤销 ID #${result.undoId ?? "-"}${permanentNote}`
        : `删除 ${result.trashed.length} 张，失败 ${failures} 张，分组已保留${permanentNote}`,
    );
    return;
  }
  if (action === "keep_all") {
    onToast("已标记为保留全部");
    return;
  }
  onToast("已忽略此组");
}

function suggestKeepId(detail: DedupGroupDetail): number | null {
  let best = detail.items[0]?.image ?? null;
  for (const item of detail.items.slice(1)) {
    const current = item.image;
    if (!best || keepScore(current) > keepScore(best)) {
      best = current;
    }
  }
  return best?.id ?? null;
}

function keepScore(image: DedupGroupDetail["items"][number]["image"]): number {
  const pixels = (image.width ?? 0) * (image.height ?? 0);
  return pixels * 1_000_000_000 + image.sizeBytes * 1_000 + image.mtime;
}

function formatResolution(width: number | null, height: number | null): string {
  if (width === null || height === null) return "?x?";
  return `${width}x${height}`;
}

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
