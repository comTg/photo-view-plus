import { rootsAdd, rootsList, rootsRemove } from "@/lib/tauri";
import type { Root } from "@/lib/tauri-types";
import { useCallback, useEffect, useState } from "react";

export interface UseRoots {
  roots: Root[];
  loading: boolean;
  error: string | null;
  selectedId: number | null;
  setSelectedId: (id: number | null) => void;
  reload: () => Promise<void>;
  addRoot: (path: string, label?: string | null) => Promise<Root>;
  removeRoot: (id: number) => Promise<void>;
}

export function useRoots(): UseRoots {
  const [roots, setRoots] = useState<Root[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [selectedId, setSelectedId] = useState<number | null>(null);

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

  const addRoot = useCallback(
    async (path: string, label?: string | null) => {
      const created = await rootsAdd({ path, label });
      await reload();
      setSelectedId(created.id);
      return created;
    },
    [reload],
  );

  const removeRoot = useCallback(
    async (id: number) => {
      await rootsRemove(id);
      if (selectedId === id) setSelectedId(null);
      await reload();
    },
    [reload, selectedId],
  );

  return {
    roots,
    loading,
    error,
    selectedId,
    setSelectedId,
    reload,
    addRoot,
    removeRoot,
  };
}
