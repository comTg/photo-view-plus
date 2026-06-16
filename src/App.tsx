import { DedupView } from "@/components/dedup/DedupView";
import { RootList } from "@/components/sidebar/RootList";
import { useRoots } from "@/hooks/useRoots";
import { useTauriEvent } from "@/lib/events";
import {
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
  AppSettings,
  ImageQueryParams,
  ImageRecord,
  QueueStatus,
  ScanDone,
  ScanErrorEvent,
  ScanProgress,
  SortDir,
  SortField,
} from "@/lib/tauri-types";
import { open } from "@tauri-apps/plugin-dialog";
import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { VirtuosoGrid } from "react-virtuoso";

const PAGE_SIZE = 200;
const SCAN_REFRESH_MS = 1500;
const FORMATS = ["jpg", "jpeg", "png", "webp", "heic", "heif", "bmp", "tiff", "gif"];

type ViewMode = "grid-lg" | "grid-md" | "grid-sm" | "list";
type GpsFilter = "any" | "yes" | "no";
type ActiveView = "browse" | "dedup" | "settings";
type LoadMode = "reset" | "more" | "refresh";

export default function App() {
  const [profile, setProfile] = useState<string | null>(null);
  const [bootError, setBootError] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [activeView, setActiveView] = useState<ActiveView>("browse");
  const [settings, setSettings] = useState<AppSettings | null>(null);

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
    ping()
      .then(setProfile)
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

  useTauriEvent<ScanProgress>("scan:progress", handleScanProgress);
  useTauriEvent<ScanDone>("scan:done", handleScanDone);
  useTauriEvent<ScanErrorEvent>("scan:error", handleScanError);
  useTauriEvent<QueueStatus>("queue:status", handleQueueStatus);

  const primarySelectedId =
    selectedIds.length > 0 ? (selectedIds[selectedIds.length - 1] ?? null) : null;

  useEffect(() => {
    if (primarySelectedId === null) {
      setDetail(null);
      return;
    }
    imagesGetDetail(primarySelectedId)
      .then(setDetail)
      .catch((error) => setToast(`读取详情失败：${String(error)}`));
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
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [selectionMode]);

  const handleScan = useCallback(async (rootId: number) => {
    try {
      const result = await scanStart(rootId);
      setToast(`已开始扫描任务 #${result.taskId}`);
    } catch (error) {
      setToast(`启动扫描失败：${String(error)}`);
    }
  }, []);

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

  return (
    <div className="app-shell">
      <header className="app-header">
        <div className="brand" title={bootError ?? undefined}>
          <span className="brand__mark">PV</span>
          <span>PhotoView+</span>
        </div>
        <div className="toolbar">
          <button type="button" className="icon-button" onClick={() => history.back()} title="后退">
            ‹
          </button>
          <button
            type="button"
            className="icon-button"
            onClick={() => history.forward()}
            title="前进"
          >
            ›
          </button>
          <button
            type="button"
            className="icon-button"
            disabled={scanTargets.length === 0}
            onClick={() => scanTargets.forEach((id) => void handleScan(id))}
            title={allSelected ? "扫描全部目录" : "扫描当前目录"}
          >
            ↻
          </button>
          <input
            className="search-input"
            value={searchDraft}
            onChange={(event) => setSearchDraft(event.target.value)}
            placeholder="搜索文件名"
          />
          <select
            className="control-select"
            value={viewMode}
            onChange={(event) => setViewMode(event.target.value as ViewMode)}
            title="视图"
          >
            <option value="grid-lg">大网格</option>
            <option value="grid-md">中网格</option>
            <option value="grid-sm">小网格</option>
            <option value="list">列表</option>
          </select>
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
            className="icon-button"
            onClick={() => setActiveView((view) => (view === "settings" ? "browse" : "settings"))}
            title="设置"
          >
            ⚙
          </button>
        </div>
      </header>

      <aside className="app-sidebar">
        <nav className="nav-group">
          <div className="section-label">我的相册</div>
          <button type="button" className="nav-item nav-item--muted">
            最近添加
          </button>
          <button type="button" className="nav-item nav-item--muted">
            收藏
          </button>
          <button type="button" className="nav-item nav-item--muted">
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
          <button type="button" className="nav-item nav-item--muted">
            MVP3 启用
          </button>
        </nav>
        <nav className="nav-group">
          <div className="section-label">智能相册</div>
          <button type="button" className="nav-item nav-item--muted">
            MVP4 启用
          </button>
        </nav>
      </aside>

      <main className={`app-main${activeView === "browse" ? " app-main--browse" : ""}`}>
        {activeView === "settings" ? (
          <SettingsView settings={settings} onPatch={handleSettingsPatch} />
        ) : activeView === "dedup" ? (
          <DedupView onToast={setToast} />
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
                onAction={() => scanTargets.forEach((id) => void handleScan(id))}
              />
            ) : (
              <ImageGrid
                images={images}
                viewMode={viewMode}
                selectedIds={selectedIds}
                selectionMode={selectionMode}
                onClickImage={handleImageClick}
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

      <aside className="app-detail">
        <DetailPane
          image={detail}
          selectedCount={selectedCount}
          selectedRecords={selectedRecords}
          onRename={handleRename}
          onReveal={handleReveal}
          onCopyPaths={handleCopyPaths}
        />
      </aside>

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
        <TaskStatus queue={queue} progress={selectedProgress} />
      </footer>
      {toast && (
        <button type="button" className="toast" onClick={() => setToast(null)}>
          {toast}
        </button>
      )}
      <div className="task-controls">
        <button type="button" onClick={() => void scanPause()} title="暂停队列">
          暂停
        </button>
        <button type="button" onClick={() => void scanResume()} title="恢复队列">
          恢复
        </button>
        <button type="button" onClick={() => void scanCancel()} title="取消运行中任务">
          取消
        </button>
      </div>
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
  onEndReached: () => void;
  scrollerRef: (ref: HTMLElement | null) => void;
}

const ImageGrid = memo(function ImageGrid({
  images,
  viewMode,
  selectedIds,
  selectionMode,
  onClickImage,
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
}

const ImageCard = memo(function ImageCard({
  image,
  selected,
  selectionMode,
  onClickImage,
}: ImageCardProps) {
  return (
    <button
      type="button"
      className={`image-card${selected ? " image-card--selected" : ""}`}
      onClick={(event) => onClickImage(image, event)}
      title={image.fullPath}
      data-image-id={image.id}
    >
      {selectionMode && <span className="image-card__check">{selected ? "✓" : ""}</span>}
      <span className="image-card__thumb">
        {image.thumbStatus === "ready" ? (
          <img src={thumbUrl(image.id)} alt={image.filename} loading="lazy" />
        ) : (
          <span className="image-card__placeholder">{thumbStatusText(image.thumbStatus)}</span>
        )}
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
  onRename: (filename: string) => Promise<void>;
  onReveal: () => Promise<void>;
  onCopyPaths: () => Promise<void>;
}

function DetailPane({
  image,
  selectedCount,
  selectedRecords,
  onRename,
  onReveal,
  onCopyPaths,
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
        <h2>{selectedCount} 张图片</h2>
        <p className="muted">已加载选择大小：{formatBytes(totalSize)}</p>
        <button type="button" className="primary-action" onClick={() => void onCopyPaths()}>
          复制路径
        </button>
      </section>
    );
  }

  if (!image) {
    return (
      <section className="detail-pane detail-pane--empty">
        <div className="section-label">图库状态</div>
        <h2>未选择图片</h2>
        <p className="muted">选择一张图片后，这里会显示路径、尺寸、EXIF 与缩略图状态。</p>
      </section>
    );
  }

  return (
    <section className="detail-pane">
      <div className="detail-preview">
        {image.thumbStatus === "ready" ? (
          <img src={thumbUrl(image.id, 512)} alt={image.filename} />
        ) : (
          <span>{thumbStatusText(image.thumbStatus)}</span>
        )}
      </div>

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
          重命名
        </button>
        <button type="button" onClick={() => void onReveal()}>
          所在文件夹
        </button>
        <button type="button" onClick={() => void onCopyPaths()}>
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
      <p className="muted">MVP3 启用 AI / 用户标签。</p>
      <div className="section-label section-label--spaced">相似</div>
      <p className="muted">MVP3 启用以图搜图。</p>
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
  onPatch,
}: {
  settings: AppSettings | null;
  onPatch: (patch: Partial<AppSettings>) => Promise<void>;
}) {
  if (!settings) {
    return <EmptyState title="正在读取设置" body="配置文件位于应用本地数据目录。" />;
  }

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
}: {
  queue: QueueStatus | null;
  progress?: ScanProgress;
}) {
  const progressText = progress
    ? `扫描 ${progress.processed.toLocaleString("zh-CN")}/${progress.total.toLocaleString("zh-CN")}`
    : "扫描空闲";
  const queueText = queue
    ? `缩略图队列 ${queue.p0} · 运行 ${queue.running}${queue.paused ? " · 已暂停" : ""}`
    : "队列状态未知";
  return (
    <span>
      {progressText} · {queueText}
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

function applyTheme(theme: string) {
  document.documentElement.dataset.theme = theme;
}
