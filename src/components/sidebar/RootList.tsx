import type { Root } from "@/lib/tauri-types";
import { useCallback } from "react";

interface Props {
  roots: Root[];
  loading: boolean;
  error: string | null;
  selectedId: number | null;
  onSelect: (id: number | null) => void;
  onRemove: (id: number) => void;
  onScan: (id: number) => void;
  onAdd: () => void;
}

export function RootList({
  roots,
  loading,
  error,
  selectedId,
  onSelect,
  onRemove,
  onScan,
  onAdd,
}: Props) {
  const handleRemove = useCallback(
    (e: React.MouseEvent, id: number, label: string) => {
      e.stopPropagation();
      if (window.confirm(`移除目录"${label}"？\n（仅从应用中移除，不删除磁盘文件）`)) {
        onRemove(id);
      }
    },
    [onRemove],
  );

  return (
    <div className="rootlist">
      <div className="rootlist__header">
        <span className="section-label">文件夹</span>
        <button type="button" className="rootlist__add" onClick={onAdd}>
          + 添加
        </button>
      </div>

      {loading && <div className="rootlist__hint">加载中…</div>}
      {error && <div className="rootlist__hint rootlist__hint--error">{error}</div>}
      {!loading && !error && roots.length === 0 && (
        <div className="rootlist__hint">尚未添加目录</div>
      )}

      <ul className="rootlist__items">
        {roots.map((r) => {
          const display = r.label ?? shortenPath(r.path);
          const isSelected = r.id === selectedId;
          return (
            <li
              key={r.id}
              className={`rootlist__item${isSelected ? " rootlist__item--selected" : ""}`}
            >
              <button
                type="button"
                className="rootlist__main"
                onClick={() => onSelect(isSelected ? null : r.id)}
                title={r.path}
              >
                <span className="rootlist__icon">{r.rootType === "network" ? "NET" : "DIR"}</span>
                <span className="rootlist__label">{display}</span>
              </button>
              <button
                type="button"
                className="rootlist__action"
                onClick={(e) => {
                  e.stopPropagation();
                  onScan(r.id);
                }}
                aria-label={`扫描 ${display}`}
                title="扫描"
              >
                ↻
              </button>
              <button
                type="button"
                className="rootlist__remove"
                onClick={(e) => handleRemove(e, r.id, display)}
                aria-label={`移除 ${display}`}
              >
                ×
              </button>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

function shortenPath(path: string): string {
  // 显示最后两段：".../parent/leaf" 风格
  const sep = path.includes("\\") ? "\\" : "/";
  const parts = path.split(sep).filter(Boolean);
  if (parts.length <= 2) return path;
  return `…${sep}${parts.slice(-2).join(sep)}`;
}
