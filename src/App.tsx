import { RootList } from "@/components/sidebar/RootList";
import { ContextMenu, menuPosition } from "@/components/ui/ContextMenu";
import { useRoots } from "@/hooks/useRoots";
import { useTauriEvent } from "@/lib/events";
import { markBoot } from "@/lib/perf";
import {
  aiImageTags,
  aiImagesByTag,
  aiModelDownload,
  aiPipelineStatus,
  aiProcessPending,
  aiRetagAll,
  aiSearch,
  aiSearchByImage,
  aiTagImage,
  aiTagsList,
  aiWorkerDiagnostics,
  aiWorkerStart,
  aiWorkerStatus,
  aiWorkerStop,
  imagesGetDetail,
  imagesQuery,
  imagesRename,
  imagesRevealInDir,
  ping,
  queueStatus,
  scanCancel,
  scanPause,
  scanResume,
  scanStart,
  settingsGet,
  settingsUpdate,
  thumbUrl,
} from "@/lib/tauri";
import type {
  AiDiagnostics,
  AiPipelineStatus,
  AiSearchResult,
  AiTag,
  AiWorkerStatus,
  AppSettings,
  ImageQueryParams,
  ImageRecord,
  ImageTag,
  QueueStatus,
  ScanDone,
  ScanErrorEvent,
  ScanProgress,
  SortDir,
  SortField,
} from "@/lib/tauri-types";
import { open } from "@tauri-apps/plugin-dialog";
import {
  ArrowLeft,
  ArrowRight,
  Camera,
  Check,
  CirclePause,
  CirclePlay,
  CircleX,
  Clock3,
  Copy,
  FolderOpen,
  Image as ImageIcon,
  Images,
  LayoutGrid,
  Maximize2,
  Pencil,
  RefreshCw,
  Search,
  Settings,
  SlidersHorizontal,
  Sparkles,
  Star,
  Tags,
} from "lucide-react";
import { Suspense, lazy, memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { VirtuosoGrid } from "react-virtuoso";

const PAGE_SIZE = 200;
const SCAN_REFRESH_MS = 1500;
const FORMATS = ["jpg", "jpeg", "png", "webp", "heic", "heif", "bmp", "tiff", "gif"];
const DedupView = lazy(() =>
  import("@/components/dedup/DedupView").then((module) => ({ default: module.DedupView })),
);
const ImagePreviewLightbox = lazy(() =>
  import("@/components/browse/ImagePreviewLightbox").then((module) => ({
    default: module.ImagePreviewLightbox,
  })),
);

type ViewMode = "grid-lg" | "grid-md" | "grid-sm" | "list";
type GpsFilter = "any" | "yes" | "no";
type ActiveView = "browse" | "dedup" | "ai" | "settings";
type LoadMode = "reset" | "more" | "refresh";

export default function App() {
  const [profile, setProfile] = useState<string | null>(null);
  const [bootError, setBootError] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [activeView, setActiveView] = useState<ActiveView>("browse");
  const [settings, setSettings] = useState<AppSettings | null>(null);
  const [aiStatus, setAiStatus] = useState<AiWorkerStatus | null>(null);
  const [aiDiagnostics, setAiDiagnostics] = useState<AiDiagnostics | null>(null);
  const [aiPipeline, setAiPipeline] = useState<AiPipelineStatus | null>(null);
  const [aiTags, setAiTags] = useState<AiTag[]>([]);
  const [imageTags, setImageTags] = useState<ImageTag[]>([]);
  const [aiSearchDraft, setAiSearchDraft] = useState("");
  const [aiResults, setAiResults] = useState<AiSearchResult[]>([]);
  const [aiResultsTitle, setAiResultsTitle] = useState("语义搜索");
  const [aiBusy, setAiBusy] = useState(false);

  const roots = useRoots();
  const selection = roots.selection;
  const allSelected = selection === "all";
  const hasRoots = roots.roots.length > 0;
  const selectedRoot =
    typeof selection === "number" ? (roots.roots.find((r) => r.id === selection) ?? null) : null;
  // 工具栏/空态"扫描"按钮要扫的目录：全部 → 所有目录；单选 → 当前目录。
  const scanTargets = allSelected
    ? roots.roots.map((r) => r.id)
    : selectedRoot
      ? [selectedRoot.id]
      : [];

  const [searchDraft, setSearchDraft] = useState("");
  const [searchQuery, setSearchQuery] = useState("");
  const [formats, setFormats] = useState<string[]>([]);
  const [sizeMinMb, setSizeMinMb] = useState("");
  const [sizeMaxMb, setSizeMaxMb] = useState("");
  const [takenFrom, setTakenFrom] = useState("");
  const [takenTo, setTakenTo] = useState("");
  const [gpsFilter, setGpsFilter] = useState<GpsFilter>("any");
  const [sortField, setSortField] = useState<SortField>("mtime");
  const [sortDir, setSortDir] = useState<SortDir>("desc");
  const [viewMode, setViewMode] = useState<ViewMode>("grid-md");

  const [images, setImages] = useState<ImageRecord[]>([]);
  const imagesRef = useRef<ImageRecord[]>([]);
  const [total, setTotal] = useState(0);
  const [imageLoading, setImageLoading] = useState(false);
  const [imageError, setImageError] = useState<string | null>(null);
  const [selectedIds, setSelectedIds] = useState<number[]>([]);
  const [selectionMode, setSelectionMode] = useState(false);
  const lastSelectedId = useRef<number | null>(null);
  const [detail, setDetail] = useState<ImageRecord | null>(null);
  const [previewIndex, setPreviewIndex] = useState<number | null>(null);
  const [imageMenu, setImageMenu] = useState<{
    x: number;
    y: number;
    image: ImageRecord;
  } | null>(null);

  const [scanProgress, setScanProgress] = useState<Record<number, ScanProgress>>({});
  const [queue, setQueue] = useState<QueueStatus | null>(null);
  const gridScrollerRef = useRef<HTMLElement | null>(null);
  const refreshTimer = useRef<number | null>(null);
  const imageLoadingRef = useRef(false);
  const lastQueryKeyRef = useRef("");
  const requestSeqRef = useRef(0);
  const rootsReloadRef = useRef(roots.reload);

  useEffect(() => {
    imagesRef.current = images;
  }, [images]);

  useEffect(() => {
    rootsReloadRef.current = roots.reload;
  }, [roots.reload]);

  useEffect(() => {
    markBoot("app-committed"); // App 首次挂载提交（React 已构建并提交 DOM）
    ping()
      .then((value) => {
        markBoot("ping-resolved"); // 首个 invoke 往返完成：判断后端响应是否拖慢首屏
        setProfile(value);
      })
      .catch((error) => setBootError(String(error)));
    settingsGet()
      .then((loaded) => {
        setSettings(loaded);
        applyTheme(loaded.theme);
      })
      .catch((error) => setToast(`读取设置失败：${String(error)}`));
    queueStatus()
      .then(setQueue)
      .catch(() => undefined);
    aiWorkerStatus()
      .then(setAiStatus)
      .catch(() => undefined);
    aiPipelineStatus()
      .then(setAiPipeline)
      .catch(() => undefined);
    aiTagsList(80)
      .then(setAiTags)
      .catch(() => undefined);
  }, []);

  useEffect(() => {
    const timer = window.setTimeout(() => setSearchQuery(searchDraft.trim()), 250);
    return () => window.clearTimeout(timer);
  }, [searchDraft]);

  const buildQuery = useCallback(
    (offset: number, limit = PAGE_SIZE): ImageQueryParams => {
      const sizeMin = mbToBytes(sizeMinMb);
      const sizeMax = mbToBytes(sizeMaxMb);
      return {
        // "全部" → 不带 rootIds（后端返回所有目录）；单选 → 只查该目录。
        rootIds: typeof selection === "number" ? [selection] : undefined,
        formats: formats.length > 0 ? formats : undefined,
        q: searchQuery || undefined,
        sizeMin,
        sizeMax,
        takenFrom: dateToUnixStart(takenFrom),
        takenTo: dateToUnixEnd(takenTo),
        hasGps: gpsFilter === "any" ? undefined : gpsFilter === "yes",
        sort: { field: sortField, dir: sortDir },
        offset,
        limit,
        includeDeleted: false,
      };
    },
    [
      formats,
      gpsFilter,
      searchQuery,
      selection,
      sizeMaxMb,
      sizeMinMb,
      sortDir,
      sortField,
      takenFrom,
      takenTo,
    ],
  );

  const restoreGridScroll = useCallback((scrollTop: number) => {
    window.requestAnimationFrame(() => {
      window.requestAnimationFrame(() => {
        const el = gridScrollerRef.current;
        if (!el) return;
        const maxScrollTop = Math.max(0, el.scrollHeight - el.clientHeight);
        el.scrollTop = Math.min(scrollTop, maxScrollTop);
      });
    });
  }, []);

  const loadImages = useCallback(
    async (mode: LoadMode = "reset") => {
      if (!hasRoots) {
        requestSeqRef.current += 1;
        imageLoadingRef.current = false;
        setImageLoading(false);
        setImages([]);
        setTotal(0);
        return;
      }
      if ((mode === "more" || mode === "refresh") && imageLoadingRef.current) return;
      const showLoading = mode !== "refresh";
      const scrollTop = mode === "refresh" ? (gridScrollerRef.current?.scrollTop ?? 0) : 0;
      imageLoadingRef.current = true;
      if (showLoading) {
        setImageLoading(true);
      }
      setImageError(null);
      const seq = requestSeqRef.current + 1;
      requestSeqRef.current = seq;
      try {
        const offset = mode === "more" ? imagesRef.current.length : 0;
        const limit =
          mode === "refresh" ? Math.max(PAGE_SIZE, imagesRef.current.length) : PAGE_SIZE;
        const page = await imagesQuery(buildQuery(offset, limit));
        if (seq !== requestSeqRef.current) return;
        setTotal(page.total);
        setImages((prev) => (mode === "more" ? mergeImages(prev, page.items) : page.items));
        if (mode === "refresh") {
          restoreGridScroll(scrollTop);
        } else if (mode === "reset") {
          gridScrollerRef.current?.scrollTo({ top: 0 });
        }
      } catch (error) {
        if (seq !== requestSeqRef.current) return;
        setImageError(String(error));
      } finally {
        if (seq === requestSeqRef.current) {
          imageLoadingRef.current = false;
          if (showLoading) {
            setImageLoading(false);
          }
        }
      }
    },
    [buildQuery, hasRoots, restoreGridScroll],
  );

  useEffect(() => {
    setSelectedIds([]);
    setDetail(null);
    // 把"是否已有目录"并入 key：选"全部"时 buildQuery 在 0→N 个目录的过程中
    // 不会变化，靠 hasRoots 翻转来触发首次加载。
    const queryKey = JSON.stringify({ ready: hasRoots, query: buildQuery(0) });
    if (queryKey === lastQueryKeyRef.current) return;
    lastQueryKeyRef.current = queryKey;
    void loadImages("reset");
  }, [buildQuery, hasRoots, loadImages]);

  const scheduleRefresh = useCallback(() => {
    if (refreshTimer.current !== null) return;
    refreshTimer.current = window.setTimeout(() => {
      refreshTimer.current = null;
      void loadImages("refresh");
    }, SCAN_REFRESH_MS);
  }, [loadImages]);

  const handleScanProgress = useCallback(
    (payload: ScanProgress) => {
      setScanProgress((prev) => ({ ...prev, [payload.rootId]: payload }));
      if ((allSelected || payload.rootId === selection) && activeView === "browse") {
        scheduleRefresh();
      }
    },
    [activeView, allSelected, scheduleRefresh, selection],
  );

  const handleScanDone = useCallback(
    (payload: ScanDone) => {
      setToast(`扫描完成：新增 ${payload.added}，更新 ${payload.updated}，移除 ${payload.removed}`);
      void rootsReloadRef.current();
      if (allSelected || payload.rootId === selection) {
        void loadImages("refresh");
      }
    },
    [allSelected, loadImages, selection],
  );

  const handleScanError = useCallback((payload: ScanErrorEvent) => {
    setToast(`扫描错误：${payload.error}`);
  }, []);

  const handleQueueStatus = useCallback((payload: QueueStatus) => setQueue(payload), []);
  const handleAiStatus = useCallback((payload: AiWorkerStatus) => setAiStatus(payload), []);
  const handleAiProgress = useCallback((payload: AiPipelineStatus) => setAiPipeline(payload), []);

  useTauriEvent<ScanProgress>("scan:progress", handleScanProgress);
  useTauriEvent<ScanDone>("scan:done", handleScanDone);
  useTauriEvent<ScanErrorEvent>("scan:error", handleScanError);
  useTauriEvent<QueueStatus>("queue:status", handleQueueStatus);
  useTauriEvent<AiWorkerStatus>("ai:worker_status", handleAiStatus);
  useTauriEvent<AiPipelineStatus>("ai:progress", handleAiProgress);

  const primarySelectedId =
    selectedIds.length > 0 ? (selectedIds[selectedIds.length - 1] ?? null) : null;

  const openPreviewById = useCallback((imageId: number) => {
    const index = imagesRef.current.findIndex((image) => image.id === imageId);
    if (index < 0) {
      setToast("当前图片还没有加载到预览列表");
      return;
    }
    setPreviewIndex(index);
  }, []);

  useEffect(() => {
    if (primarySelectedId === null) {
      setDetail(null);
      setImageTags([]);
      return;
    }
    imagesGetDetail(primarySelectedId)
      .then(setDetail)
      .catch((error) => setToast(`读取详情失败：${String(error)}`));
    aiImageTags(primarySelectedId)
      .then(setImageTags)
      .catch(() => setImageTags([]));
  }, [primarySelectedId]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "a" && selectionMode) {
        event.preventDefault();
        setSelectedIds(imagesRef.current.map((image) => image.id));
      }
      if (event.key === "Escape") {
        setSelectionMode(false);
        setSelectedIds([]);
      }
      if (event.key === " " && primarySelectedId !== null && !isTypingTarget(event.target)) {
        event.preventDefault();
        openPreviewById(primarySelectedId);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [openPreviewById, primarySelectedId, selectionMode]);

  useEffect(() => {
    if (previewIndex !== null && previewIndex >= images.length) {
      setPreviewIndex(null);
    }
  }, [images.length, previewIndex]);

  const handleScan = useCallback(async (rootId: number) => {
    try {
      const result = await scanStart(rootId);
      setToast(`已开始扫描任务 #${result.taskId}`);
    } catch (error) {
      setToast(`启动扫描失败：${String(error)}`);
    }
  }, []);

  const handleScanTargets = useCallback(() => {
    for (const id of scanTargets) {
      void handleScan(id);
    }
  }, [handleScan, scanTargets]);

  const handleAdd = useCallback(async () => {
    try {
      const picked = await open({ directory: true, multiple: false });
      if (!picked || typeof picked !== "string") return;
      const created = await roots.addRoot(picked);
      setActiveView("browse");
      setToast("目录已添加，已开始扫描");
      await handleScan(created.id);
    } catch (error) {
      setToast(`添加目录失败：${String(error)}`);
    }
  }, [handleScan, roots]);

  const handleImageClick = useCallback(
    (image: ImageRecord, event: React.MouseEvent) => {
      if (selectionMode || event.ctrlKey || event.metaKey || event.shiftKey) {
        setSelectionMode(true);
        setSelectedIds((prev) => {
          if (event.shiftKey && lastSelectedId.current !== null) {
            return selectRange(imagesRef.current, prev, lastSelectedId.current, image.id);
          }
          if (event.ctrlKey || event.metaKey) {
            return prev.includes(image.id)
              ? prev.filter((id) => id !== image.id)
              : [...prev, image.id];
          }
          return prev.includes(image.id)
            ? prev.filter((id) => id !== image.id)
            : [...prev, image.id];
        });
      } else {
        setSelectedIds([image.id]);
      }
      lastSelectedId.current = image.id;
    },
    [selectionMode],
  );

  const handleCopyPaths = useCallback(async () => {
    const selected = imagesRef.current.filter((image) => selectedIds.includes(image.id));
    await navigator.clipboard.writeText(selected.map((image) => image.fullPath).join("\n"));
    setToast(`已复制 ${selected.length} 个路径`);
  }, [selectedIds]);

  const handleReveal = useCallback(async () => {
    if (primarySelectedId === null) return;
    try {
      await imagesRevealInDir(primarySelectedId);
    } catch (error) {
      setToast(`打开所在文件夹失败：${String(error)}`);
    }
  }, [primarySelectedId]);

  const handleRevealImage = useCallback(async (image: ImageRecord) => {
    try {
      await imagesRevealInDir(image.id);
    } catch (error) {
      setToast(`打开所在文件夹失败：${String(error)}`);
    }
  }, []);

  const handleOpenImageMenu = useCallback((image: ImageRecord, event: React.MouseEvent) => {
    event.preventDefault();
    event.stopPropagation();
    setSelectedIds((prev) => (prev.includes(image.id) ? prev : [image.id]));
    lastSelectedId.current = image.id;
    setImageMenu({ ...menuPosition(event), image });
  }, []);

  const handleCopyImagePath = useCallback(async (image: ImageRecord) => {
    await navigator.clipboard.writeText(image.fullPath);
    setToast("已复制图片路径");
  }, []);

  const handleRename = useCallback(
    async (newFilename: string) => {
      if (primarySelectedId === null) return;
      try {
        const renamed = await imagesRename({ id: primarySelectedId, newFilename });
        if (!renamed) return;
        setDetail(renamed);
        setImages((prev) => prev.map((image) => (image.id === renamed.id ? renamed : image)));
        setToast("文件名已更新");
      } catch (error) {
        setToast(`重命名失败：${String(error)}`);
      }
    },
    [primarySelectedId],
  );

  const handleSettingsPatch = useCallback(async (patch: Partial<AppSettings>) => {
    try {
      const updated = await settingsUpdate(patch);
      setSettings(updated);
      applyTheme(updated.theme);
      setToast("设置已保存");
    } catch (error) {
      setToast(`保存设置失败：${String(error)}`);
    }
  }, []);

  const handleAiStart = useCallback(async () => {
    setAiBusy(true);
    try {
      const status = await aiWorkerStart();
      setAiStatus(status);
      setToast("AI Worker 已启动");
    } catch (error) {
      setToast(`启动 AI Worker 失败：${String(error)}`);
      void aiWorkerStatus()
        .then(setAiStatus)
        .catch(() => undefined);
    } finally {
      setAiBusy(false);
    }
  }, []);

  const handleAiStop = useCallback(async () => {
    setAiBusy(true);
    try {
      const status = await aiWorkerStop();
      setAiStatus(status);
      setToast("AI Worker 已停止");
    } catch (error) {
      setToast(`停止 AI Worker 失败：${String(error)}`);
    } finally {
      setAiBusy(false);
    }
  }, []);

  const handleAiDiagnostics = useCallback(async () => {
    setAiBusy(true);
    try {
      const diagnostics = await aiWorkerDiagnostics();
      setAiDiagnostics(diagnostics);
      void aiWorkerStatus()
        .then(setAiStatus)
        .catch(() => undefined);
      setToast("AI 诊断已刷新");
    } catch (error) {
      setToast(`AI 诊断失败：${String(error)}`);
      void aiWorkerStatus()
        .then(setAiStatus)
        .catch(() => undefined);
    } finally {
      setAiBusy(false);
    }
  }, []);

  const refreshAiState = useCallback(async () => {
    const [pipeline, tags] = await Promise.all([aiPipelineStatus(), aiTagsList(80)]);
    setAiPipeline(pipeline);
    setAiTags(tags);
  }, []);

  const handleAiSearch = useCallback(async () => {
    const text = aiSearchDraft.trim();
    if (!text) {
      setToast("请输入要搜索的自然语言");
      return;
    }
    setAiBusy(true);
    try {
      const results = await aiSearch({
        text,
        rootIds: typeof selection === "number" ? [selection] : undefined,
        limit: 80,
      });
      setAiResults(results);
      setAiResultsTitle(`语义搜索：${text}`);
      setActiveView("ai");
    } catch (error) {
      setToast(`AI 搜索失败：${String(error)}`);
    } finally {
      setAiBusy(false);
    }
  }, [aiSearchDraft, selection]);

  const handleAiSimilar = useCallback(
    async (imageId?: number) => {
      const id = imageId ?? primarySelectedId;
      if (id === null || id === undefined) {
        setToast("先选择一张图片");
        return;
      }
      setAiBusy(true);
      try {
        const results = await aiSearchByImage(id, 80);
        setAiResults(results);
        setAiResultsTitle(`以图搜图：#${id}`);
        setActiveView("ai");
      } catch (error) {
        setToast(`以图搜图失败：${String(error)}`);
      } finally {
        setAiBusy(false);
      }
    },
    [primarySelectedId],
  );

  const handleAiTagSelected = useCallback(async () => {
    if (primarySelectedId === null) {
      setToast("先选择一张图片");
      return;
    }
    setAiBusy(true);
    try {
      const tags = await aiTagImage(primarySelectedId);
      setImageTags(tags);
      await refreshAiState();
      setToast(`已生成 ${tags.length} 个标签`);
    } catch (error) {
      setToast(`生成标签失败：${String(error)}`);
    } finally {
      setAiBusy(false);
    }
  }, [primarySelectedId, refreshAiState]);

  const handleAiProcessPending = useCallback(async () => {
    setAiBusy(true);
    try {
      await aiProcessPending();
      await refreshAiState();
      setToast("已触发 AI 后台处理");
    } catch (error) {
      setToast(`触发 AI 处理失败：${String(error)}`);
    } finally {
      setAiBusy(false);
    }
  }, [refreshAiState]);

  const handleAiRetagAll = useCallback(async () => {
    setAiBusy(true);
    try {
      const count = await aiRetagAll();
      await refreshAiState();
      setImageTags([]);
      setToast(`已清空旧标签，将重新打 ${count} 张图片的标签`);
    } catch (error) {
      setToast(`重打标签失败：${String(error)}`);
    } finally {
      setAiBusy(false);
    }
  }, [refreshAiState]);

  const handleAiDownloadModel = useCallback(
    async (modelKey: string) => {
      setAiBusy(true);
      try {
        await aiModelDownload(modelKey);
        setToast(`已开始下载模型：${modelKey}`);
        void handleAiDiagnostics();
      } catch (error) {
        setToast(`模型下载失败：${String(error)}`);
      } finally {
        setAiBusy(false);
      }
    },
    [handleAiDiagnostics],
  );

  const handleTagFilter = useCallback(async (tag: AiTag) => {
    setAiBusy(true);
    try {
      const page = await aiImagesByTag(tag.id, 0, 200);
      setAiResults(
        page.items.map((image) => ({
          image,
          score: 1,
          source: "tag",
        })),
      );
      setAiResultsTitle(`#${tag.name}`);
      setActiveView("ai");
    } catch (error) {
      setToast(`读取标签图片失败：${String(error)}`);
    } finally {
      setAiBusy(false);
    }
  }, []);

  const handleGridEndReached = useCallback(() => {
    if (imagesRef.current.length < total && !imageLoadingRef.current) {
      void loadImages("more");
    }
  }, [loadImages, total]);

  const handleGridScrollerRef = useCallback((ref: HTMLElement | null) => {
    gridScrollerRef.current = ref;
  }, []);

  // 全部模式下：优先显示仍在进行中的扫描，否则显示最近一条进度。
  const allProgress = Object.values(scanProgress);
  const selectedProgress = allSelected
    ? (allProgress.find((p) => p.processed < p.total) ?? allProgress[allProgress.length - 1])
    : typeof selection === "number"
      ? scanProgress[selection]
      : undefined;
  const selectedCount = selectedIds.length;
  const selectedRecords = useMemo(
    () => images.filter((image) => selectedIds.includes(image.id)),
    [images, selectedIds],
  );
  const showDetailPane = activeView === "browse";

  return (
    <div className={`app-shell${showDetailPane ? "" : " app-shell--no-detail"}`}>
      <header className="app-header">
        <div className="brand" title={bootError ?? undefined}>
          <span className="brand__mark">PV</span>
          <span>PhotoView+</span>
        </div>
        <div className="toolbar">
          <button type="button" className="icon-button" onClick={() => history.back()} title="后退">
            <ArrowLeft aria-hidden="true" />
          </button>
          <button
            type="button"
            className="icon-button"
            onClick={() => history.forward()}
            title="前进"
          >
            <ArrowRight aria-hidden="true" />
          </button>
          <button
            type="button"
            className="icon-button"
            disabled={scanTargets.length === 0}
            onClick={handleScanTargets}
            title={allSelected ? "扫描全部目录" : "扫描当前目录"}
          >
            <RefreshCw aria-hidden="true" />
          </button>
          <label className="search-box">
            <Search aria-hidden="true" />
            <input
              className="search-input"
              value={searchDraft}
              onChange={(event) => setSearchDraft(event.target.value)}
              placeholder="搜索文件名、标签、地点..."
            />
          </label>
          <span className="toolbar-divider" />
          <label className="select-with-icon" title="视图">
            <LayoutGrid aria-hidden="true" />
            <select
              className="control-select"
              value={viewMode}
              onChange={(event) => setViewMode(event.target.value as ViewMode)}
              aria-label="视图"
            >
              <option value="grid-lg">大网格</option>
              <option value="grid-md">中网格</option>
              <option value="grid-sm">小网格</option>
              <option value="list">列表</option>
            </select>
          </label>
          <select
            className="control-select"
            value={`${sortField}:${sortDir}`}
            onChange={(event) => {
              const [field, dir] = event.target.value.split(":") as [SortField, SortDir];
              setSortField(field);
              setSortDir(dir);
            }}
            title="排序"
          >
            <option value="mtime:desc">修改时间 新到旧</option>
            <option value="mtime:asc">修改时间 旧到新</option>
            <option value="taken_at:desc">拍摄时间 新到旧</option>
            <option value="filename:asc">文件名 A-Z</option>
            <option value="size:desc">文件大小 大到小</option>
          </select>
          <button
            type="button"
            className={`toolbar-button${selectionMode ? " toolbar-button--active" : ""}`}
            onClick={() => setSelectionMode((value) => !value)}
          >
            选择
          </button>
          <button
            type="button"
            className={`toolbar-button${activeView === "dedup" ? " toolbar-button--active" : ""}`}
            onClick={() => setActiveView((view) => (view === "dedup" ? "browse" : "dedup"))}
            title="去重"
          >
            去重
          </button>
          <button
            type="button"
            className={`toolbar-button${activeView === "ai" ? " toolbar-button--active" : ""}`}
            onClick={() => setActiveView((view) => (view === "ai" ? "browse" : "ai"))}
            title="AI 搜索"
          >
            <Sparkles aria-hidden="true" />
            AI
          </button>
          <button
            type="button"
            className="icon-button"
            onClick={() => setActiveView((view) => (view === "settings" ? "browse" : "settings"))}
            title="设置"
          >
            <Settings aria-hidden="true" />
          </button>
        </div>
      </header>

      <aside className="app-sidebar">
        <nav className="nav-group">
          <div className="section-label">我的相册</div>
          <button type="button" className="nav-item nav-item--muted">
            <Clock3 aria-hidden="true" />
            最近添加
          </button>
          <button type="button" className="nav-item nav-item--muted">
            <Star aria-hidden="true" />
            收藏
          </button>
          <button type="button" className="nav-item nav-item--muted">
            <Camera aria-hidden="true" />
            截图
          </button>
        </nav>
        <RootList
          roots={roots.roots}
          loading={roots.loading}
          error={roots.error}
          selection={roots.selection}
          onSelect={(sel) => {
            roots.setSelection(sel);
            setActiveView("browse");
          }}
          onRemove={(id) => {
            void roots.removeRoot(id);
          }}
          onScan={(id) => {
            void handleScan(id);
          }}
          onAdd={() => {
            void handleAdd();
          }}
        />
        <nav className="nav-group">
          <div className="section-label">标签</div>
          {aiTags.length === 0 ? (
            <button type="button" className="nav-item nav-item--muted">
              <Tags aria-hidden="true" />
              等待 AI 标签
            </button>
          ) : (
            aiTags.slice(0, 12).map((tag) => (
              <button
                key={tag.id}
                type="button"
                className="nav-item"
                onClick={() => void handleTagFilter(tag)}
                title={`${tag.name} · ${tag.imageCount}`}
              >
                <Tags aria-hidden="true" />
                {tag.name} · {tag.imageCount}
              </button>
            ))
          )}
        </nav>
        <nav className="nav-group">
          <div className="section-label">智能相册</div>
          <button type="button" className="nav-item nav-item--muted">
            <Sparkles aria-hidden="true" />
            MVP4 启用
          </button>
        </nav>
      </aside>

      <main className={`app-main${activeView === "browse" ? " app-main--browse" : ""}`}>
        {activeView === "settings" ? (
          <SettingsView
            settings={settings}
            aiStatus={aiStatus}
            aiDiagnostics={aiDiagnostics}
            aiPipeline={aiPipeline}
            aiBusy={aiBusy}
            onPatch={handleSettingsPatch}
            onAiStart={handleAiStart}
            onAiStop={handleAiStop}
            onAiDiagnostics={handleAiDiagnostics}
            onAiProcessPending={handleAiProcessPending}
            onAiRetagAll={handleAiRetagAll}
            onAiDownloadModel={handleAiDownloadModel}
          />
        ) : activeView === "dedup" ? (
          <Suspense fallback={<div className="loading-row">正在加载去重视图…</div>}>
            <DedupView onToast={setToast} />
          </Suspense>
        ) : activeView === "ai" ? (
          <AiSearchView
            query={aiSearchDraft}
            onQueryChange={setAiSearchDraft}
            onSearch={handleAiSearch}
            onProcessPending={handleAiProcessPending}
            onRefresh={refreshAiState}
            onSelectTag={handleTagFilter}
            onSelectImage={(image) => {
              setSelectedIds([image.id]);
              lastSelectedId.current = image.id;
            }}
            onPreviewImage={(image) => openPreviewById(image.id)}
            tags={aiTags}
            results={aiResults}
            title={aiResultsTitle}
            pipeline={aiPipeline}
            workerStatus={aiStatus}
            busy={aiBusy}
          />
        ) : (
          <div className="browse-view">
            <FilterBar
              formats={formats}
              setFormats={setFormats}
              sizeMinMb={sizeMinMb}
              setSizeMinMb={setSizeMinMb}
              sizeMaxMb={sizeMaxMb}
              setSizeMaxMb={setSizeMaxMb}
              takenFrom={takenFrom}
              setTakenFrom={setTakenFrom}
              takenTo={takenTo}
              setTakenTo={setTakenTo}
              gpsFilter={gpsFilter}
              setGpsFilter={setGpsFilter}
            />
            {!hasRoots ? (
              <EmptyState
                title="添加图片目录开始"
                body="MVP1 会扫描目录、生成缩略图，并在这里显示可筛选的图片网格。"
                actionLabel="添加目录"
                onAction={handleAdd}
              />
            ) : imageError ? (
              <EmptyState
                title="读取图片失败"
                body={imageError}
                actionLabel="重试"
                onAction={() => void loadImages("reset")}
              />
            ) : images.length === 0 && !imageLoading ? (
              <EmptyState
                title={allSelected ? "还没有图片" : "当前目录还没有图片"}
                body="点击扫描后，图片会随着后台任务逐步出现在这里。"
                actionLabel={allSelected ? "扫描全部目录" : "扫描目录"}
                onAction={handleScanTargets}
              />
            ) : (
              <ImageGrid
                images={images}
                viewMode={viewMode}
                selectedIds={selectedIds}
                selectionMode={selectionMode}
                onClickImage={handleImageClick}
                onPreviewImage={(image) => openPreviewById(image.id)}
                onOpenContextMenu={handleOpenImageMenu}
                onEndReached={handleGridEndReached}
                scrollerRef={handleGridScrollerRef}
              />
            )}
            {hasRoots && (
              <div className="browse-footer" aria-live="polite">
                {imageLoading ? (
                  <div className="loading-row">正在加载图片…</div>
                ) : images.length < total ? (
                  <button
                    type="button"
                    className="load-more"
                    onClick={() => void loadImages("more")}
                  >
                    加载更多（{images.length}/{total}）
                  </button>
                ) : null}
              </div>
            )}
          </div>
        )}
      </main>

      {showDetailPane && (
        <aside className="app-detail">
          <DetailPane
            image={detail}
            selectedCount={selectedCount}
            selectedRecords={selectedRecords}
            imageTags={imageTags}
            aiBusy={aiBusy}
            onRename={handleRename}
            onReveal={handleReveal}
            onCopyPaths={handleCopyPaths}
            onPreview={() => {
              if (primarySelectedId !== null) openPreviewById(primarySelectedId);
            }}
            onTagImage={handleAiTagSelected}
            onFindSimilar={() => void handleAiSimilar()}
          />
        </aside>
      )}

      <footer className="app-footer">
        <span>
          {profile ? `profile: ${profile}` : "连接后端中"}
          {bootError ? ` · ${bootError}` : ""}
        </span>
        <span>
          {!hasRoots
            ? `${roots.roots.length} 个目录`
            : allSelected
              ? `全部 ${roots.roots.length} 个目录 · ${total.toLocaleString("zh-CN")} 张`
              : selectedRoot
                ? `${selectedRoot.path} · ${total.toLocaleString("zh-CN")} 张`
                : `${roots.roots.length} 个目录`}
        </span>
        <TaskStatus
          queue={queue}
          progress={selectedProgress}
          aiStatus={aiStatus}
          aiPipeline={aiPipeline}
        />
      </footer>
      {toast && (
        <button type="button" className="toast" onClick={() => setToast(null)}>
          {toast}
        </button>
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
                  onSelect: () => void handleRevealImage(imageMenu.image),
                },
                {
                  id: "copy-path",
                  label: "复制完整路径",
                  onSelect: () => void handleCopyImagePath(imageMenu.image),
                },
              ]
            : []
        }
      />
      <div className="task-controls">
        <button type="button" onClick={() => void scanPause()} title="暂停队列">
          <CirclePause aria-hidden="true" />
          暂停
        </button>
        <button type="button" onClick={() => void scanResume()} title="恢复队列">
          <CirclePlay aria-hidden="true" />
          恢复
        </button>
        <button type="button" onClick={() => void scanCancel()} title="取消运行中任务">
          <CircleX aria-hidden="true" />
          取消
        </button>
      </div>
      {previewIndex !== null && (
        <Suspense fallback={null}>
          <ImagePreviewLightbox
            images={images}
            index={previewIndex}
            open={true}
            onClose={() => setPreviewIndex(null)}
            onIndexChange={setPreviewIndex}
          />
        </Suspense>
      )}
    </div>
  );
}

interface FilterBarProps {
  formats: string[];
  setFormats: (formats: string[]) => void;
  sizeMinMb: string;
  setSizeMinMb: (value: string) => void;
  sizeMaxMb: string;
  setSizeMaxMb: (value: string) => void;
  takenFrom: string;
  setTakenFrom: (value: string) => void;
  takenTo: string;
  setTakenTo: (value: string) => void;
  gpsFilter: GpsFilter;
  setGpsFilter: (value: GpsFilter) => void;
}

interface AiSearchViewProps {
  query: string;
  onQueryChange: (value: string) => void;
  onSearch: () => Promise<void>;
  onProcessPending: () => Promise<void>;
  onRefresh: () => Promise<void>;
  onSelectTag: (tag: AiTag) => Promise<void>;
  onSelectImage: (image: ImageRecord) => void;
  onPreviewImage: (image: ImageRecord) => void;
  tags: AiTag[];
  results: AiSearchResult[];
  title: string;
  pipeline: AiPipelineStatus | null;
  workerStatus: AiWorkerStatus | null;
  busy: boolean;
}

function AiSearchView({
  query,
  onQueryChange,
  onSearch,
  onProcessPending,
  onRefresh,
  onSelectTag,
  onSelectImage,
  onPreviewImage,
  tags,
  results,
  title,
  pipeline,
  workerStatus,
  busy,
}: AiSearchViewProps) {
  return (
    <section className="ai-view">
      <div className="ai-toolbar">
        <label className="ai-search-box">
          <Search aria-hidden="true" />
          <input
            value={query}
            onChange={(event) => onQueryChange(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                void onSearch();
              }
            }}
            placeholder="输入自然语言，例如：海边日落、红色跑车、报错截图"
          />
        </label>
        <button
          type="button"
          className="primary-action"
          disabled={busy}
          onClick={() => void onSearch()}
        >
          <Search aria-hidden="true" />
          搜索
        </button>
        <button
          type="button"
          className="toolbar-button"
          disabled={busy}
          onClick={() => void onProcessPending()}
        >
          <Sparkles aria-hidden="true" />
          处理待办
        </button>
        <button
          type="button"
          className="toolbar-button"
          disabled={busy}
          onClick={() => void onRefresh()}
        >
          <RefreshCw aria-hidden="true" />
          刷新
        </button>
      </div>

      <div className="ai-status-strip">
        <span>Worker {aiStatusText(workerStatus?.status)}</span>
        <span>
          Embedding {pipeline?.embeddingPending ?? 0} 待处理 / {pipeline?.embeddingInflight ?? 0}{" "}
          运行
        </span>
        <span>
          标签 {pipeline?.taggingPending ?? 0} 待处理 / {pipeline?.taggingInflight ?? 0} 运行
        </span>
      </div>

      <div className="ai-tag-cloud">
        {tags.length === 0 ? (
          <span className="muted">还没有标签</span>
        ) : (
          tags.map((tag) => (
            <button
              key={tag.id}
              type="button"
              className="tag-chip"
              onClick={() => void onSelectTag(tag)}
            >
              #{tag.name}
              <small>{tag.imageCount}</small>
            </button>
          ))
        )}
      </div>

      <div className="ai-results-header">
        <h1>{title}</h1>
        <span>{results.length.toLocaleString("zh-CN")} 张</span>
      </div>
      {results.length === 0 ? (
        <EmptyState
          title="等待 AI 搜索"
          body="先处理 embedding，再输入自然语言或点击标签查看结果。"
        />
      ) : (
        <div className="ai-result-grid">
          {results.map((result) => (
            <div className="ai-result-item" key={`${result.source}:${result.image.id}`}>
              <ImageCard
                image={result.image}
                selected={false}
                selectionMode={false}
                onClickImage={(image) => onSelectImage(image)}
                onPreviewImage={onPreviewImage}
                onOpenContextMenu={() => undefined}
              />
              <span className="ai-result-score">
                {result.source} · {(result.score * 100).toFixed(0)}
              </span>
            </div>
          ))}
        </div>
      )}
    </section>
  );
}

function FilterBar(props: FilterBarProps) {
  const toggleFormat = (format: string) => {
    props.setFormats(
      props.formats.includes(format)
        ? props.formats.filter((item) => item !== format)
        : [...props.formats, format],
    );
  };

  return (
    <section className="filter-bar">
      <div className="filter-bar__title">
        <SlidersHorizontal aria-hidden="true" />
        <span>筛选</span>
      </div>
      <div className="format-pills">
        {FORMATS.map((format) => (
          <button
            key={format}
            type="button"
            className={`pill${props.formats.includes(format) ? " pill--active" : ""}`}
            onClick={() => toggleFormat(format)}
          >
            {format.toUpperCase()}
          </button>
        ))}
      </div>
      <label className="field-compact">
        <span>最小 MB</span>
        <input
          value={props.sizeMinMb}
          onChange={(event) => props.setSizeMinMb(event.target.value)}
          inputMode="decimal"
        />
      </label>
      <label className="field-compact">
        <span>最大 MB</span>
        <input
          value={props.sizeMaxMb}
          onChange={(event) => props.setSizeMaxMb(event.target.value)}
          inputMode="decimal"
        />
      </label>
      <label className="field-compact">
        <span>开始日期</span>
        <input
          type="date"
          value={props.takenFrom}
          onChange={(event) => props.setTakenFrom(event.target.value)}
        />
      </label>
      <label className="field-compact">
        <span>结束日期</span>
        <input
          type="date"
          value={props.takenTo}
          onChange={(event) => props.setTakenTo(event.target.value)}
        />
      </label>
      <select
        className="control-select"
        value={props.gpsFilter}
        onChange={(event) => props.setGpsFilter(event.target.value as GpsFilter)}
      >
        <option value="any">GPS 不限</option>
        <option value="yes">有 GPS</option>
        <option value="no">无 GPS</option>
      </select>
    </section>
  );
}

interface ImageGridProps {
  images: ImageRecord[];
  viewMode: ViewMode;
  selectedIds: number[];
  selectionMode: boolean;
  onClickImage: (image: ImageRecord, event: React.MouseEvent) => void;
  onPreviewImage: (image: ImageRecord) => void;
  onOpenContextMenu: (image: ImageRecord, event: React.MouseEvent) => void;
  onEndReached: () => void;
  scrollerRef: (ref: HTMLElement | null) => void;
}

const ImageGrid = memo(function ImageGrid({
  images,
  viewMode,
  selectedIds,
  selectionMode,
  onClickImage,
  onPreviewImage,
  onOpenContextMenu,
  onEndReached,
  scrollerRef,
}: ImageGridProps) {
  const selectedIdSet = useMemo(() => new Set(selectedIds), [selectedIds]);
  return (
    <VirtuosoGrid
      className="virtual-image-grid"
      computeItemKey={(_index, image) => image.id}
      data={images}
      endReached={onEndReached}
      increaseViewportBy={{ top: 640, bottom: 1280 }}
      itemClassName="image-grid__item"
      itemContent={(_index, image) => (
        <ImageCard
          image={image}
          selected={selectedIdSet.has(image.id)}
          selectionMode={selectionMode}
          onClickImage={onClickImage}
          onPreviewImage={onPreviewImage}
          onOpenContextMenu={onOpenContextMenu}
        />
      )}
      listClassName={`image-grid image-grid--${viewMode}`}
      scrollerRef={scrollerRef}
      style={{ height: "100%" }}
    />
  );
});

interface ImageCardProps {
  image: ImageRecord;
  selected: boolean;
  selectionMode: boolean;
  onClickImage: (image: ImageRecord, event: React.MouseEvent) => void;
  onPreviewImage: (image: ImageRecord) => void;
  onOpenContextMenu: (image: ImageRecord, event: React.MouseEvent) => void;
}

const ImageCard = memo(function ImageCard({
  image,
  selected,
  selectionMode,
  onClickImage,
  onPreviewImage,
  onOpenContextMenu,
}: ImageCardProps) {
  return (
    <button
      type="button"
      className={`image-card${selected ? " image-card--selected" : ""}`}
      onClick={(event) => onClickImage(image, event)}
      onContextMenu={(event) => onOpenContextMenu(image, event)}
      onDoubleClick={() => onPreviewImage(image)}
      title={image.fullPath}
      data-image-id={image.id}
    >
      {selectionMode && (
        <span className="image-card__check">{selected ? <Check aria-hidden="true" /> : ""}</span>
      )}
      <span className="image-card__thumb">
        {image.thumbStatus === "ready" ? (
          <img src={thumbUrl(image.id)} alt={image.filename} loading="lazy" />
        ) : (
          <span className="image-card__placeholder">
            <ImageIcon aria-hidden="true" />
            {thumbStatusText(image.thumbStatus)}
          </span>
        )}
      </span>
      <span className="image-card__quick" aria-hidden="true">
        <Maximize2 />
      </span>
      <span className="image-card__name">{image.filename}</span>
      <span className="image-card__meta">
        {image.extension.toUpperCase()} · {formatBytes(image.sizeBytes)}
      </span>
    </button>
  );
});

interface DetailPaneProps {
  image: ImageRecord | null;
  selectedCount: number;
  selectedRecords: ImageRecord[];
  imageTags: ImageTag[];
  aiBusy: boolean;
  onRename: (filename: string) => Promise<void>;
  onReveal: () => Promise<void>;
  onCopyPaths: () => Promise<void>;
  onPreview: () => void;
  onTagImage: () => Promise<void>;
  onFindSimilar: () => void;
}

function DetailPane({
  image,
  selectedCount,
  selectedRecords,
  imageTags,
  aiBusy,
  onRename,
  onReveal,
  onCopyPaths,
  onPreview,
  onTagImage,
  onFindSimilar,
}: DetailPaneProps) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");

  useEffect(() => {
    setDraft(image?.filename ?? "");
    setEditing(false);
  }, [image?.filename]);

  if (selectedCount > 1) {
    const totalSize = selectedRecords.reduce((sum, item) => sum + item.sizeBytes, 0);
    return (
      <section className="detail-pane">
        <div className="section-label">批量选择</div>
        <h2>
          <Images aria-hidden="true" />
          {selectedCount} 张图片
        </h2>
        <p className="muted">已加载选择大小：{formatBytes(totalSize)}</p>
        <button type="button" className="primary-action" onClick={() => void onCopyPaths()}>
          <Copy aria-hidden="true" />
          复制路径
        </button>
      </section>
    );
  }

  if (!image) {
    return (
      <section className="detail-pane detail-pane--empty">
        <div className="section-label">图库状态</div>
        <h2>
          <Images aria-hidden="true" />
          未选择图片
        </h2>
        <p className="muted">选择一张图片后，这里会显示路径、尺寸、EXIF 与缩略图状态。</p>
      </section>
    );
  }

  return (
    <section className="detail-pane">
      <button
        type="button"
        className="detail-preview"
        onClick={onPreview}
        onDoubleClick={onPreview}
        title="打开原图预览"
      >
        {image.thumbStatus === "ready" ? (
          <img src={thumbUrl(image.id, 512)} alt={image.filename} />
        ) : (
          <span>{thumbStatusText(image.thumbStatus)}</span>
        )}
        <span className="detail-preview__hint">
          <Maximize2 aria-hidden="true" />
        </span>
      </button>

      {editing ? (
        <form
          className="rename-form"
          onSubmit={(event) => {
            event.preventDefault();
            void onRename(draft);
          }}
        >
          <input value={draft} onChange={(event) => setDraft(event.target.value)} />
          <button type="submit">保存</button>
          <button type="button" onClick={() => setEditing(false)}>
            取消
          </button>
        </form>
      ) : (
        <button type="button" className="detail-title" onDoubleClick={() => setEditing(true)}>
          {image.filename}
        </button>
      )}

      <div className="detail-actions">
        <button type="button" onClick={() => setEditing(true)}>
          <Pencil aria-hidden="true" />
          重命名
        </button>
        <button type="button" onClick={() => void onReveal()}>
          <FolderOpen aria-hidden="true" />
          所在文件夹
        </button>
        <button type="button" onClick={() => void onCopyPaths()}>
          <Copy aria-hidden="true" />
          复制路径
        </button>
      </div>

      <DetailRow label="路径" value={image.fullPath} />
      <DetailRow
        label="大小"
        value={`${formatBytes(image.sizeBytes)} · ${image.extension.toUpperCase()}`}
      />
      <DetailRow
        label="尺寸"
        value={image.width && image.height ? `${image.width} × ${image.height}` : null}
      />
      <DetailRow label="修改时间" value={formatTime(image.mtime)} />
      <DetailRow label="拍摄时间" value={image.takenAt ? formatTime(image.takenAt) : null} />
      <DetailRow
        label="相机"
        value={[image.cameraMake, image.cameraModel].filter(Boolean).join(" ") || null}
      />
      <DetailRow
        label="GPS"
        value={
          image.gpsLat !== null && image.gpsLng !== null
            ? `${image.gpsLat.toFixed(6)}, ${image.gpsLng.toFixed(6)}`
            : null
        }
      />
      <DetailRow label="缩略图" value={thumbStatusText(image.thumbStatus)} />

      <div className="section-label section-label--spaced">标签</div>
      <div className="tag-row">
        {imageTags.length > 0 ? (
          imageTags.map((tag) => (
            <span
              key={`${tag.tagId}:${tag.source}`}
              title={`${tag.score.toFixed(2)} · ${tag.source}`}
            >
              {tag.name}
            </span>
          ))
        ) : (
          <span className="tag-row__empty">暂无标签</span>
        )}
      </div>
      <button
        type="button"
        className="primary-action"
        disabled={aiBusy}
        onClick={() => void onTagImage()}
      >
        <Tags aria-hidden="true" />
        生成标签
      </button>
      <div className="section-label section-label--spaced">相似</div>
      <button type="button" className="primary-action" disabled={aiBusy} onClick={onFindSimilar}>
        <Sparkles aria-hidden="true" />
        查找相似
      </button>
    </section>
  );
}

function DetailRow({ label, value }: { label: string; value: string | null }) {
  if (!value) return null;
  return (
    <div className="detail-row">
      <span>{label}</span>
      <strong title={value}>{value}</strong>
    </div>
  );
}

function SettingsView({
  settings,
  aiStatus,
  aiDiagnostics,
  aiPipeline,
  aiBusy,
  onPatch,
  onAiStart,
  onAiStop,
  onAiDiagnostics,
  onAiProcessPending,
  onAiRetagAll,
  onAiDownloadModel,
}: {
  settings: AppSettings | null;
  aiStatus: AiWorkerStatus | null;
  aiDiagnostics: AiDiagnostics | null;
  aiPipeline: AiPipelineStatus | null;
  aiBusy: boolean;
  onPatch: (patch: Partial<AppSettings>) => Promise<void>;
  onAiStart: () => Promise<void>;
  onAiStop: () => Promise<void>;
  onAiDiagnostics: () => Promise<void>;
  onAiProcessPending: () => Promise<void>;
  onAiRetagAll: () => Promise<void>;
  onAiDownloadModel: (modelKey: string) => Promise<void>;
}) {
  if (!settings) {
    return <EmptyState title="正在读取设置" body="配置文件位于应用本地数据目录。" />;
  }

  const aiReady = aiStatus?.status === "ready";
  const modelRows = aiDiagnostics ? Object.entries(aiDiagnostics.models) : [];

  return (
    <section className="settings-view">
      <h1>设置</h1>
      <div className="settings-grid">
        <label>
          <span>语言</span>
          <select
            value={settings.locale}
            onChange={(event) => void onPatch({ locale: event.target.value })}
          >
            <option value="zh-CN">中文</option>
            <option value="en-US">English</option>
          </select>
        </label>
        <label>
          <span>主题</span>
          <select
            value={settings.theme}
            onChange={(event) => void onPatch({ theme: event.target.value })}
          >
            <option value="system">跟随系统</option>
            <option value="light">浅色</option>
            <option value="dark">深色</option>
          </select>
        </label>
        <label>
          <span>缩略图缓存上限 GB</span>
          <input
            type="number"
            min={1}
            max={100}
            value={settings.thumbCacheGb}
            onChange={(event) => void onPatch({ thumbCacheGb: Number(event.target.value) })}
          />
        </label>
        <label>
          <span>本地盘扫描并发</span>
          <input
            type="number"
            min={1}
            max={16}
            value={settings.localScanConcurrency}
            onChange={(event) => void onPatch({ localScanConcurrency: Number(event.target.value) })}
          />
        </label>
        <label>
          <span>网络盘扫描并发</span>
          <input
            type="number"
            min={1}
            max={4}
            value={settings.networkScanConcurrency}
            onChange={(event) =>
              void onPatch({ networkScanConcurrency: Number(event.target.value) })
            }
          />
        </label>
        <label>
          <span>启用 AI 后台任务</span>
          <input
            type="checkbox"
            checked={settings.aiEnabled}
            onChange={(event) => void onPatch({ aiEnabled: event.target.checked })}
          />
        </label>
        <label>
          <span>AI 闲置停止分钟</span>
          <input
            type="number"
            min={1}
            max={120}
            value={settings.aiIdleStopMinutes}
            onChange={(event) => void onPatch({ aiIdleStopMinutes: Number(event.target.value) })}
          />
        </label>
        <label>
          <span>CLIP 模型</span>
          <select
            value={settings.aiClipModel}
            onChange={(event) => void onPatch({ aiClipModel: event.target.value })}
          >
            <option value="clip-vit-b-32">clip-vit-b-32</option>
            <option value="siglip-so400m">siglip-so400m</option>
          </select>
        </label>
        <label>
          <span>标签模型</span>
          <select
            value={settings.aiTaggerModel}
            onChange={(event) => void onPatch({ aiTaggerModel: event.target.value })}
          >
            <option value="ram-plus">ram-plus</option>
          </select>
        </label>
        <div className="settings-panel settings-panel--wide">
          <div className="settings-panel__header">
            <div>
              <span className="section-label">AI Worker</span>
              <strong>{aiStatusText(aiStatus?.status)}</strong>
            </div>
            <div className="settings-panel__actions">
              <button
                type="button"
                className="toolbar-button"
                disabled={aiBusy || aiReady}
                onClick={() => void onAiStart()}
              >
                <CirclePlay aria-hidden="true" />
                启动
              </button>
              <button
                type="button"
                className="toolbar-button"
                disabled={aiBusy || !aiStatus || aiStatus.status === "stopped"}
                onClick={() => void onAiStop()}
              >
                <CircleX aria-hidden="true" />
                停止
              </button>
              <button
                type="button"
                className="toolbar-button"
                disabled={aiBusy}
                onClick={() => void onAiDiagnostics()}
              >
                <RefreshCw aria-hidden="true" />
                诊断
              </button>
              <button
                type="button"
                className="toolbar-button"
                disabled={aiBusy || !settings.aiEnabled}
                onClick={() => void onAiProcessPending()}
              >
                <Sparkles aria-hidden="true" />
                处理待办
              </button>
              <button
                type="button"
                className="toolbar-button"
                disabled={aiBusy}
                onClick={() => void onAiRetagAll()}
                title="清空已有 AI 标签（含旧的英文标签），并用当前模型重新打一遍"
              >
                <Tags aria-hidden="true" />
                清空并重打标签
              </button>
            </div>
          </div>
          <div className="ai-kv">
            <span>设备</span>
            <strong>{aiStatus?.device ?? aiDiagnostics?.device ?? "未知"}</strong>
            <span>Compute</span>
            <strong>{aiStatus?.compute ?? aiDiagnostics?.computeCapability ?? "未知"}</strong>
            <span>PID / Port</span>
            <strong>
              {aiStatus?.pid ?? "-"} / {aiStatus?.port ?? "-"}
            </strong>
            <span>重启</span>
            <strong>{aiStatus?.restartsLastMinute ?? 0}/min</strong>
            <span>Embedding</span>
            <strong>
              {aiPipeline
                ? `${aiPipeline.embeddingPending} 待处理 · ${aiPipeline.embeddingInflight} 运行`
                : "未知"}
            </strong>
            <span>标签</span>
            <strong>
              {aiPipeline
                ? `${aiPipeline.taggingPending} 待处理 · ${aiPipeline.taggingInflight} 运行`
                : "未知"}
            </strong>
            <span>Worker</span>
            <strong title={aiStatus?.workerDir}>{aiStatus?.workerDir ?? "未初始化"}</strong>
            <span>Models</span>
            <strong title={aiStatus?.modelsDir}>{aiStatus?.modelsDir ?? "未初始化"}</strong>
            {aiDiagnostics && (
              <>
                <span>CUDA</span>
                <strong>
                  {aiDiagnostics.cudaAvailable
                    ? `${aiDiagnostics.cudaVersion ?? "CUDA"} · ${aiDiagnostics.deviceName ?? "GPU"}`
                    : "不可用"}
                </strong>
                <span>VRAM</span>
                <strong>
                  {aiDiagnostics.vramFreeGb !== null && aiDiagnostics.vramTotalGb !== null
                    ? `${aiDiagnostics.vramFreeGb} / ${aiDiagnostics.vramTotalGb} GB`
                    : "未知"}
                </strong>
              </>
            )}
          </div>
          {aiStatus?.lastError && <p className="settings-error">{aiStatus.lastError}</p>}
          {aiDiagnostics?.warnings?.map((warning) => (
            <p className="settings-warning" key={warning}>
              {warning}
            </p>
          ))}
          {modelRows.length > 0 && (
            <div className="model-list">
              {modelRows.map(([key, model]) => (
                <button
                  key={key}
                  type="button"
                  className="model-chip"
                  disabled={aiBusy || Boolean(model.downloaded)}
                  onClick={() => void onAiDownloadModel(key)}
                >
                  {key} · {model.downloaded ? "已下载" : "下载"} · {model.sizeMb ?? "-"} MB
                </button>
              ))}
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

function EmptyState({
  title,
  body,
  actionLabel,
  onAction,
}: {
  title: string;
  body: string;
  actionLabel?: string;
  onAction?: () => void;
}) {
  return (
    <section className="empty-state">
      <h1>{title}</h1>
      <p>{body}</p>
      {actionLabel && onAction && (
        <button type="button" className="primary-action" onClick={onAction}>
          {actionLabel}
        </button>
      )}
    </section>
  );
}

const TaskStatus = memo(function TaskStatus({
  queue,
  progress,
  aiStatus,
  aiPipeline,
}: {
  queue: QueueStatus | null;
  progress?: ScanProgress;
  aiStatus: AiWorkerStatus | null;
  aiPipeline: AiPipelineStatus | null;
}) {
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

function mergeImages(prev: ImageRecord[], next: ImageRecord[]) {
  const seen = new Set(prev.map((image) => image.id));
  return [...prev, ...next.filter((image) => !seen.has(image.id))];
}

function selectRange(images: ImageRecord[], prev: number[], fromId: number, toId: number) {
  const from = images.findIndex((image) => image.id === fromId);
  const to = images.findIndex((image) => image.id === toId);
  if (from < 0 || to < 0) return prev;
  const [start, end] = from < to ? [from, to] : [to, from];
  const range = images.slice(start, end + 1).map((image) => image.id);
  return Array.from(new Set([...prev, ...range]));
}

function mbToBytes(value: string): number | undefined {
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed <= 0) return undefined;
  return Math.round(parsed * 1024 * 1024);
}

function dateToUnixStart(value: string): number | undefined {
  if (!value) return undefined;
  const date = new Date(`${value}T00:00:00`);
  return Number.isFinite(date.getTime()) ? Math.floor(date.getTime() / 1000) : undefined;
}

function dateToUnixEnd(value: string): number | undefined {
  if (!value) return undefined;
  const date = new Date(`${value}T23:59:59`);
  return Number.isFinite(date.getTime()) ? Math.floor(date.getTime() / 1000) : undefined;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value.toFixed(value >= 10 ? 0 : 1)} ${units[index]}`;
}

function formatTime(seconds: number): string {
  return new Date(seconds * 1000).toLocaleString("zh-CN");
}

function thumbStatusText(status: string): string {
  if (status === "ready") return "已就绪";
  if (status === "failed") return "缩略图失败";
  if (status === "unsupported") return "格式暂不支持";
  return "生成中";
}

function aiStatusText(status: string | null | undefined): string {
  if (status === "ready") return "运行中";
  if (status === "starting") return "启动中";
  if (status === "stopping") return "停止中";
  if (status === "degraded") return "降级";
  if (status === "stopped") return "已停止";
  return "未知";
}

function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  return (
    target.isContentEditable ||
    target.tagName === "INPUT" ||
    target.tagName === "TEXTAREA" ||
    target.tagName === "SELECT"
  );
}

function applyTheme(theme: string) {
  document.documentElement.dataset.theme = theme;
}
