import { useTauriEvent } from "@/lib/events";
import { dedupGroups, dedupResolve, dedupStart, dedupStatus } from "@/lib/tauri";
import type {
  DedupGroupMethod,
  DedupGroupStatus,
  DedupMethod,
  DedupResolveArgs,
  DedupResolveResult,
  DedupStatus,
  DuplicateGroup,
} from "@/lib/tauri-types";
import { useCallback, useEffect, useRef, useState } from "react";

const GROUP_PAGE_SIZE = 100;
const GROUP_RELOAD_DEBOUNCE_MS = 250;

export interface UseDedup {
  groups: DuplicateGroup[];
  total: number;
  hasMoreGroups: boolean;
  status: DedupStatus;
  loading: boolean;
  loadingMore: boolean;
  error: string | null;
  filter: { method: DedupGroupMethod | "all"; status: DedupGroupStatus };
  setFilter: (next: { method: DedupGroupMethod | "all"; status: DedupGroupStatus }) => void;
  reloadGroups: (options?: { preserveLoaded?: boolean }) => Promise<void>;
  loadMoreGroups: () => Promise<void>;
  start: (method: DedupMethod, threshold?: number) => Promise<void>;
  resolve: (args: DedupResolveArgs) => Promise<DedupResolveResult>;
}

const INITIAL_STATUS: DedupStatus = {
  running: false,
  hashPending: 0,
  dhashPending: 0,
  groupsFound: 0,
  phase: "idle",
};

export function useDedup(): UseDedup {
  const [groups, setGroups] = useState<DuplicateGroup[]>([]);
  const [total, setTotal] = useState(0);
  const [status, setStatus] = useState<DedupStatus>(INITIAL_STATUS);
  const [loading, setLoading] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilterRaw] = useState<{
    method: DedupGroupMethod | "all";
    status: DedupGroupStatus;
  }>({ method: "all", status: "open" });

  const filterRef = useRef(filter);
  filterRef.current = filter;
  const groupsRef = useRef(groups);
  groupsRef.current = groups;
  const totalRef = useRef(total);
  totalRef.current = total;
  const requestSeqRef = useRef(0);
  const loadingMoreRef = useRef(false);
  const reloadTimerRef = useRef<number | null>(null);

  const setFilter = useCallback((next: typeof filter) => {
    setFilterRaw(next);
  }, []);

  const reloadGroups = useCallback(async (options: { preserveLoaded?: boolean } = {}) => {
    const requestId = requestSeqRef.current + 1;
    requestSeqRef.current = requestId;
    setLoading(true);
    setError(null);
    try {
      const f = filterRef.current;
      const currentCount = groupsRef.current.length;
      const limit = options.preserveLoaded
        ? Math.max(GROUP_PAGE_SIZE, currentCount || GROUP_PAGE_SIZE)
        : GROUP_PAGE_SIZE;
      const page = await dedupGroups({
        method: f.method === "all" ? undefined : f.method,
        status: f.status,
        offset: 0,
        limit,
      });
      if (requestSeqRef.current !== requestId) return;
      setGroups(page.items);
      setTotal(page.total);
    } catch (e) {
      setError(String(e));
    } finally {
      if (requestSeqRef.current === requestId) {
        setLoading(false);
      }
    }
  }, []);

  const loadMoreGroups = useCallback(async () => {
    if (loadingMoreRef.current || loading) return;
    const current = groupsRef.current;
    if (current.length >= totalRef.current) return;

    loadingMoreRef.current = true;
    setLoadingMore(true);
    setError(null);
    const requestId = requestSeqRef.current;
    try {
      const f = filterRef.current;
      const page = await dedupGroups({
        method: f.method === "all" ? undefined : f.method,
        status: f.status,
        offset: current.length,
        limit: GROUP_PAGE_SIZE,
      });
      if (requestSeqRef.current !== requestId) return;
      setGroups((prev) => {
        const seen = new Set(prev.map((group) => group.id));
        const next = [...prev];
        for (const group of page.items) {
          if (!seen.has(group.id)) next.push(group);
        }
        return next;
      });
      setTotal(page.total);
    } catch (e) {
      setError(String(e));
    } finally {
      loadingMoreRef.current = false;
      setLoadingMore(false);
    }
  }, [loading]);

  const scheduleReload = useCallback(() => {
    if (reloadTimerRef.current !== null) {
      window.clearTimeout(reloadTimerRef.current);
    }
    reloadTimerRef.current = window.setTimeout(() => {
      reloadTimerRef.current = null;
      void reloadGroups({ preserveLoaded: true });
    }, GROUP_RELOAD_DEBOUNCE_MS);
  }, [reloadGroups]);

  useEffect(() => {
    return () => {
      if (reloadTimerRef.current !== null) {
        window.clearTimeout(reloadTimerRef.current);
      }
    };
  }, []);

  // biome-ignore lint/correctness/useExhaustiveDependencies: filter 变化时主动重置为第一页；reloadGroups 通过 filterRef 读最新值
  useEffect(() => {
    if (reloadTimerRef.current !== null) {
      window.clearTimeout(reloadTimerRef.current);
      reloadTimerRef.current = null;
    }
    void reloadGroups();
  }, [filter, reloadGroups]);

  useEffect(() => {
    dedupStatus()
      .then(setStatus)
      .catch(() => undefined);
  }, []);

  // 进度事件 → 更新状态；新组事件做防抖刷新，避免分组阶段频繁打查询。
  const onProgress = useCallback((next: DedupStatus) => {
    setStatus(next);
  }, []);
  const onNewGroup = useCallback(() => {
    scheduleReload();
  }, [scheduleReload]);
  const onDone = useCallback(() => {
    void reloadGroups({ preserveLoaded: true });
  }, [reloadGroups]);

  useTauriEvent<DedupStatus>("dedup:progress", onProgress);
  useTauriEvent<{ groupId: number; method: string }>("dedup:new_group", onNewGroup);
  useTauriEvent<{ exactGroups: number; visualGroups: number; error: string | null }>(
    "dedup:done",
    onDone,
  );

  const start = useCallback(async (method: DedupMethod, threshold?: number) => {
    try {
      const result = await dedupStart({ method, threshold });
      setStatus(result.status);
      await reloadGroups();
    } catch (e) {
      setError(String(e));
      throw e;
    }
  }, [reloadGroups]);

  const resolve = useCallback(
    async (args: DedupResolveArgs) => {
      try {
        const result = await dedupResolve(args);
        await reloadGroups({ preserveLoaded: true });
        return result;
      } catch (e) {
        setError(String(e));
        throw e;
      }
    },
    [reloadGroups],
  );

  return {
    groups,
    total,
    hasMoreGroups: groups.length < total,
    status,
    loading,
    loadingMore,
    error,
    filter,
    setFilter,
    reloadGroups,
    loadMoreGroups,
    start,
    resolve,
  };
}
