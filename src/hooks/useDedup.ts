import { useTauriEvent } from "@/lib/events";
import { dedupGroupDetail, dedupGroups, dedupResolve, dedupStart, dedupStatus } from "@/lib/tauri";
import type {
  DedupGroupDetail,
  DedupGroupMethod,
  DedupGroupStatus,
  DedupMethod,
  DedupResolveArgs,
  DedupResolveResult,
  DedupStatus,
  DuplicateGroup,
} from "@/lib/tauri-types";
import { useCallback, useEffect, useRef, useState } from "react";

export interface UseDedup {
  groups: DuplicateGroup[];
  status: DedupStatus;
  loading: boolean;
  error: string | null;
  filter: { method: DedupGroupMethod | "all"; status: DedupGroupStatus };
  setFilter: (next: { method: DedupGroupMethod | "all"; status: DedupGroupStatus }) => void;
  selectedGroupId: number | null;
  setSelectedGroupId: (id: number | null) => void;
  detail: DedupGroupDetail | null;
  detailLoading: boolean;
  reloadGroups: () => Promise<void>;
  reloadDetail: () => Promise<void>;
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
  const [status, setStatus] = useState<DedupStatus>(INITIAL_STATUS);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilterRaw] = useState<{
    method: DedupGroupMethod | "all";
    status: DedupGroupStatus;
  }>({ method: "all", status: "open" });
  const [selectedGroupId, setSelectedGroupIdRaw] = useState<number | null>(null);
  const [detail, setDetail] = useState<DedupGroupDetail | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);

  const filterRef = useRef(filter);
  filterRef.current = filter;

  const setFilter = useCallback((next: typeof filter) => {
    setFilterRaw(next);
  }, []);

  const setSelectedGroupId = useCallback((id: number | null) => {
    setSelectedGroupIdRaw(id);
  }, []);

  const reloadGroups = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const f = filterRef.current;
      const page = await dedupGroups({
        method: f.method === "all" ? undefined : f.method,
        status: f.status,
        offset: 0,
        limit: 500,
      });
      setGroups(page.items);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  const reloadDetail = useCallback(async () => {
    if (selectedGroupId === null) {
      setDetail(null);
      return;
    }
    setDetailLoading(true);
    try {
      const d = await dedupGroupDetail(selectedGroupId);
      setDetail(d);
    } catch (e) {
      setError(String(e));
    } finally {
      setDetailLoading(false);
    }
  }, [selectedGroupId]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: 故意以 filter 触发列表刷新；reloadGroups 通过 filterRef 读最新值
  useEffect(() => {
    void reloadGroups();
  }, [filter, reloadGroups]);

  useEffect(() => {
    void reloadDetail();
  }, [reloadDetail]);

  useEffect(() => {
    dedupStatus()
      .then(setStatus)
      .catch(() => undefined);
  }, []);

  // 进度事件 → 更新状态 + 在 grouping 阶段触发列表刷新
  const onProgress = useCallback((next: DedupStatus) => {
    setStatus(next);
  }, []);
  const onNewGroup = useCallback(() => {
    void reloadGroups();
  }, [reloadGroups]);
  const onDone = useCallback(() => {
    void reloadGroups();
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
    } catch (e) {
      setError(String(e));
      throw e;
    }
  }, []);

  const resolve = useCallback(
    async (args: DedupResolveArgs) => {
      try {
        const result = await dedupResolve(args);
        await reloadGroups();
        if (selectedGroupId === args.groupId) {
          setSelectedGroupIdRaw(null);
          setDetail(null);
        }
        return result;
      } catch (e) {
        setError(String(e));
        throw e;
      }
    },
    [reloadGroups, selectedGroupId],
  );

  return {
    groups,
    status,
    loading,
    error,
    filter,
    setFilter,
    selectedGroupId,
    setSelectedGroupId,
    detail,
    detailLoading,
    reloadGroups,
    reloadDetail,
    start,
    resolve,
  };
}
