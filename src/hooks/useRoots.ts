import { rootsAdd, rootsList, rootsRemove } from "@/lib/tauri";
import type { Root } from "@/lib/tauri-types";
import { useCallback, useEffect, useState } from "react";

// "all" = 浏览全部目录（查询时不带 rootIds，后端返回所有目录的图片）；
// number = 只看某一个目录。
export type RootSelection = "all" | number;

export interface UseRoots {
  roots: Root[];
  loading: boolean;
  error: string | null;
  selection: RootSelection;
  setSelection: (selection: RootSelection) => void;
  reload: () => Promise<void>;
  addRoot: (path: string, label?: string | null) => Promise<Root>;
  removeRoot: (id: number) => Promise<void>;
}

export function useRoots(): UseRoots {
  const [roots, setRoots] = useState<Root[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [selection, setSelection] = useState<RootSelection>("all");

  const reload = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const list = await rootsList();
      setRoots(list);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  // 选中的具体目录被删除后回退到"全部"。
  useEffect(() => {
    setSelection((current) => {
      if (current === "all") return current;
      return roots.some((r) => r.id === current) ? current : "all";
    });
  }, [roots]);

  const addRoot = useCallback(
    async (path: string, label?: string | null) => {
      const created = await rootsAdd({ path, label });
      await reload();
      // 新增目录后回到"全部"，让新目录的图片直接并入合并视图。
      setSelection("all");
      return created;
    },
    [reload],
  );

  const removeRoot = useCallback(
    async (id: number) => {
      await rootsRemove(id);
      setSelection((current) => (current === id ? "all" : current));
      await reload();
    },
    [reload],
  );

  return {
    roots,
    loading,
    error,
    selection,
    setSelection,
    reload,
    addRoot,
    removeRoot,
  };
}
