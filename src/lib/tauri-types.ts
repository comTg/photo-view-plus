// Tauri 命令的请求/响应类型。手维护，后续 Rust 端字段稳定后可换 ts-rs 自动生成。
// 对应 src-tauri/src/repo/roots_repo.rs::Root（serde rename_all = "camelCase"）

export type RootType = "local" | "network";

export interface Root {
  id: number;
  path: string;
  label: string | null;
  rootType: RootType;
  enabled: boolean;
  lastScanAt: number | null;
  createdAt: number;
  settingsJson: string | null;
}

export interface AddRootArgs {
  path: string;
  label?: string | null;
}

export interface RootPatch {
  label?: string | null;
  enabled?: boolean;
  settingsJson?: string | null;
}

export interface UpdateRootArgs {
  id: number;
  // 与后端 #[serde(flatten)] 对齐
  label?: string | null;
  enabled?: boolean;
  settingsJson?: string | null;
}

export interface RemoveResult {
  removed: boolean;
}

export interface DbStatus {
  schemaVersion: number;
  poolSize: number;
}

export type ThumbStatus = "pending" | "ready" | "failed" | "unsupported";

/** `thumbnails:updated` 事件里的一项：缩略图状态的窄字段（docs/11 R5，每 250ms 合成一批）。 */
export interface ThumbnailUpdate {
  id: number;
  thumbStatus: ThumbStatus;
  thumbHash: string | null;
  thumbError: string | null;
  width: number | null;
  height: number | null;
}

export interface ImageRecord {
  id: number;
  rootId: number;
  relPath: string;
  filename: string;
  extension: string;
  sizeBytes: number;
  mtime: number;
  width: number | null;
  height: number | null;
  orientation: number | null;
  takenAt: number | null;
  gpsLat: number | null;
  gpsLng: number | null;
  cameraMake: string | null;
  cameraModel: string | null;
  thumbStatus: ThumbStatus;
  thumbHash: string | null;
  thumbError: string | null;
  /** 缩略图文件扩展名：`jpg`；R4 之前生成、还没转换的是 `webp`（docs/11 R4）。 */
  thumbExt: string;
  indexedAt: number;
  deletedAt: number | null;
  rootPath: string;
  fullPath: string;
  blake3: string | null;
  phash: number | null;
  dhash: number | null;
  hashStatus: string;
  tagStatus: string;
  embeddingStatus: string;
  ocrStatus: string;
  ocrText: string | null;
  faceStatus: string;
}

export interface ImagePage {
  items: ImageRecord[];
  total: number;
  offset: number;
  limit: number;
}

export type SortField = "taken_at" | "mtime" | "filename" | "size";
export type SortDir = "asc" | "desc";

export interface ImageSort {
  field: SortField;
  dir: SortDir;
}

export interface ImageQueryParams {
  rootIds?: number[];
  formats?: string[];
  q?: string;
  sizeMin?: number;
  sizeMax?: number;
  takenFrom?: number;
  takenTo?: number;
  hasGps?: boolean;
  sort?: ImageSort;
  offset?: number;
  limit?: number;
  includeDeleted?: boolean;
}

export interface RenameImageArgs {
  id: number;
  newFilename: string;
}

export interface ScanStartResult {
  taskId: number;
  rootId: number;
  status: string;
}

export interface ScanTaskStatus {
  id: number;
  rootId: number;
  status: string;
  totalFiles: number | null;
  processed: number;
  startedAt: number | null;
  finishedAt: number | null;
  error: string | null;
}

export interface ScanProgress {
  rootId: number;
  taskId: number;
  processed: number;
  total: number;
  currentFile: string | null;
  etaSec: number | null;
}

export interface ScanDone {
  rootId: number;
  taskId: number;
  added: number;
  updated: number;
  removed: number;
  /** 只差大小写的同级目录（docs/11 R1-6）。 */
  caseConflicts: CaseConflict[];
}

/** 同一父目录下只差大小写的几个子目录：Windows 经 SMB 访问区分大小写的 NAS 时分不清它们。 */
export interface CaseConflict {
  /** 父目录，相对根目录；就在根目录下时为空串。 */
  parent: string;
  names: string[];
}

export interface ScanErrorEvent {
  rootId: number;
  taskId: number;
  error: string;
  file: string | null;
}

export interface QueueStatus {
  p0: number;
  p1: number;
  p2: number;
  p3: number;
  p4: number;
  p5: number;
  p6: number;
  p7: number;
  running: number;
  completed: number;
  failed: number;
  paused: boolean;
}

export interface AppSettings {
  settingsVersion: number;
  locale: string;
  theme: "system" | "light" | "dark" | string;
  thumbCacheGb: number;
  localScanConcurrency: number;
  networkScanConcurrency: number;
  aiEnabled: boolean;
  aiIdleStopMinutes: number;
  aiClipModel: string;
  /** 描述文本向量模型（"描述语义"检索通道）。 */
  aiTextModel: string;
  aiTaggerModel: string;
  ocrEnabled: boolean;
  faceEnabled: boolean;
  gpsEnabled: boolean;
  fileWatcherEnabled: boolean;
  /** 多少分钟没有键盘鼠标输入算"空闲"，后台全速（docs/11 §5.1）。 */
  idleThresholdMinutes: number;
  /** 使用电脑时也运行 AI（默认关：使用中暂停 GPU 任务）。 */
  aiWhileActive: boolean;
  /** 大模型只在空闲时运行（默认开）。 */
  vlmIdleOnly: boolean;
  /** 大模型打标（docs/10 A6），默认关。 */
  vlmEnabled: boolean;
  /** llama-server 与模型文件；三项都空时自动查找。 */
  vlmServerExe: string;
  vlmModelPath: string;
  vlmMmprojPath: string;
  /** llama-server 并发槽位（-np）。 */
  vlmParallel: number;
  /** 送模型的图片长边。 */
  vlmLongEdge: number;
  /** 夜间自动分析（Windows 计划任务）。 */
  nightAnalyzeEnabled: boolean;
  nightStartHour: number;
  nightEndHour: number;
}

export interface AppSettingsPatch {
  locale?: string;
  theme?: "system" | "light" | "dark" | string;
  thumbCacheGb?: number;
  localScanConcurrency?: number;
  networkScanConcurrency?: number;
  aiEnabled?: boolean;
  aiIdleStopMinutes?: number;
  aiClipModel?: string;
  aiTextModel?: string;
  aiTaggerModel?: string;
  ocrEnabled?: boolean;
  faceEnabled?: boolean;
  gpsEnabled?: boolean;
  fileWatcherEnabled?: boolean;
  idleThresholdMinutes?: number;
  aiWhileActive?: boolean;
  vlmIdleOnly?: boolean;
  vlmEnabled?: boolean;
  vlmServerExe?: string;
  vlmModelPath?: string;
  vlmMmprojPath?: string;
  vlmParallel?: number;
  vlmLongEdge?: number;
  nightAnalyzeEnabled?: boolean;
  nightStartHour?: number;
  nightEndHour?: number;
}

/** 夜间自动分析：设置与计划任务注册状态（docs/10 A6b）。 */
export interface NightTaskStatus {
  enabled: boolean;
  registered: boolean;
  taskName: string;
  startHour: number;
  endHour: number;
  exe: string;
}

// ===== 大模型打标（docs/10 A6） =====

export interface VlmServerStatus {
  /** stopped | starting | ready | degraded */
  state: string;
  /** 是否由本应用启动（复用外部服务时为 false）。 */
  owned: boolean;
  pid: number | null;
  port: number;
  lastError: string | null;
  startsLastMinute: number;
  loadSecs: number | null;
}

export interface AnalysisProgress {
  /** disabled | notConfigured | waiting | paused | starting | running | idle | error */
  state: string;
  reason: string | null;
  pending: number;
  analyzed: number;
  failed: number;
  inflight: number;
  sessionDone: number;
  sessionFailed: number;
  secsPerImage: number | null;
  etaSecs: number | null;
  lastError: string | null;
  model: string;
  server: VlmServerStatus;
}

/** RAM-plus 旧标签与大模型覆盖率（覆盖率 ≥ 95% 才允许清理旧标签）。 */
export interface RamTagStatus {
  ramImages: number;
  ramTags: number;
  vlmAnalyzed: number;
  totalImages: number;
  coverage: number;
  canCleanup: boolean;
}

export interface VlmPaths {
  exe: string;
  model: string;
  mmproj: string;
}

// ===== 后台资源治理（docs/11 R2） =====

/** 观测到的负载模式。 */
export type LoadMode = "active" | "away" | "idle" | "busy";
/** 叠加手动覆盖之后实际生效的档位。 */
export type LoadTier = LoadMode | "paused";
export type LoadOverrideMode = "auto" | "fullSpeed" | "paused";
/** 状态栏菜单的选项（`governor_set_override` 的参数）。 */
export type LoadOverrideChoice = "auto" | "fullSpeed" | "pause1h" | "pauseUntilRestart";

export interface LoadBudget {
  visibleThumbnails: number;
  batchThumbnails: number;
  networkReads: number;
  /** null = 不限。 */
  hashTasks: number | null;
  gpu: "paused" | "duty" | "unlimited";
  gpuDutyCycle: number | null;
  vlm: boolean;
  workerThreads: number;
}

export interface GovernorStatus {
  mode: LoadMode;
  tier: LoadTier;
  overrideMode: LoadOverrideMode;
  /** 暂停截止时间（Unix 秒）；暂停且为 null = 暂停到重启。 */
  pausedUntil: number | null;
  idleSecs: number;
  budget: LoadBudget;
}

// ===== MVP4 高级管理 =====

export interface TimelineBucket {
  year: number;
  month: number;
  count: number;
  startTs: number;
  endTs: number;
  samples: ImageRecord[];
}

export interface MapImagePoint {
  image: ImageRecord;
  lat: number;
  lng: number;
}

export interface OcrStatus {
  pending: number;
  ready: number;
  failed: number;
  disabled: number;
}

export interface FaceStatus {
  pending: number;
  ready: number;
  failed: number;
  disabled: number;
  clusters: number;
  faces: number;
}

export interface FaceBox {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface Face {
  id: number;
  imageId: number;
  clusterId: number | null;
  bbox: FaceBox;
  confidence: number;
  embeddingRef: string | null;
}

export interface FaceCluster {
  id: number;
  label: string | null;
  sampleImageId: number | null;
  faceCount: number;
  createdAt: number;
}

export interface SmartAlbum {
  id: number;
  name: string;
  filterJson: string;
  icon: string | null;
  sortOrder: number;
  createdAt: number;
}

export interface SmartAlbumInput {
  name: string;
  filterJson: string;
  icon?: string | null;
  sortOrder?: number | null;
}

export interface WatcherStatus {
  running: boolean;
  watchedRoots: number[];
  skippedNetworkRoots: number[];
  lastError: string | null;
}

export interface BackupExportArgs {
  destination: string;
  includeThumbs?: boolean;
  includeModels?: boolean;
}

export interface BackupExportResult {
  path: string;
  files: number;
  bytes: number;
}

export interface BackupImportResult {
  restoredDir: string;
  files: number;
  bytes: number;
}

// ===== MVP3 AI worker =====

export type AiWorkerLifecycle = "stopped" | "starting" | "ready" | "stopping" | "degraded" | string;

export interface AiWorkerStatus {
  status: AiWorkerLifecycle;
  device: string | null;
  compute: string | null;
  pid: number | null;
  port: number | null;
  lastError: string | null;
  restartsLastMinute: number;
  workerDir: string;
  modelsDir: string;
}

export interface AiModelDiagnostics {
  downloaded?: boolean;
  sizeMb?: number;
  loaded?: boolean;
  [key: string]: unknown;
}

export interface AiDiagnostics {
  torchVersion: string | null;
  cudaAvailable: boolean;
  cudaVersion: string | null;
  deviceName: string | null;
  computeCapability: string | null;
  vramTotalGb: number | null;
  vramFreeGb: number | null;
  device: string;
  models: Record<string, AiModelDiagnostics>;
  warnings?: string[];
  packages?: Record<string, string | null>;
  [key: string]: unknown;
}

export interface AiPipelineStatus {
  embeddingPending: number;
  taggingPending: number;
  embeddingInflight: number;
  taggingInflight: number;
  /** 当前图像向量模型（设置项 aiClipModel）。 */
  embeddingModel: string;
  /** 描述文本向量（docs/10 A4）。 */
  textEmbeddingPending: number;
  textEmbeddingInflight: number;
  textEmbeddingModel: string;
}

export interface AiSearchArgs {
  text: string;
  rootIds?: number[];
  limit?: number;
}

/** 高亮片段的一段：hit 为 true 的是命中的查询词。 */
export interface SnippetPart {
  text: string;
  hit: boolean;
}

/**
 * 命中来源：description（描述）、text（画面文字 / OCR）、tag（标签）、filename（文件名）、
 * semantic（描述语义，文本向量）、image（画面，图像向量）。
 */
export type SearchMatch = "description" | "text" | "tag" | "filename" | "semantic" | "image";

export interface AiSearchResult {
  image: ImageRecord;
  score: number;
  source: string;
  matchedBy?: SearchMatch[];
  snippet?: SnippetPart[] | null;
}

export type SearchChannel = "tag" | "fullText" | "imageVector" | "textVector";

export interface SearchChannelStat {
  channel: SearchChannel;
  hits: number;
  elapsedMs: number;
}

export interface AiSearchResponse {
  results: AiSearchResult[];
  /** 语义通道没跑（worker 启动中 / 兜底），只返回了文字通道的结果。 */
  degraded: boolean;
  degradedReason: string | null;
  channels: SearchChannelStat[];
  elapsedMs: number;
}

/** 画面分析结果（memora 导入或 VLM 打标，docs/10 §7）。 */
export interface ImageAnalysis {
  imageId: number;
  source: "memora" | "vlm" | string;
  model: string;
  promptVersion: string;
  description: string;
  categories: string[];
  keywords: string[];
  peopleCount: number | null;
  visibleText: string | null;
  sensitive: boolean;
  durationMs: number | null;
  analyzedAt: number;
}

export interface MemoraImportOutcome {
  memoraRows: number;
  matchedPhotos: number;
  imagesWritten: number;
  skippedVlm: number;
  elapsedMs: number;
}

export interface AnalysisStatus {
  /** [来源, 条数] */
  bySource: [string, number][];
  defaultMemoraDb: string;
}

export interface SearchIndexStatus {
  indexed: number;
  pending: number;
}

export interface SearchIndexRebuildOutcome {
  indexed: number;
  elapsedMs: number;
}

export interface AiTag {
  id: number;
  name: string;
  source: string;
  category: string | null;
  createdAt: number;
  imageCount: number;
  /** 标签云分组（docs/10 A7）：带这个标签的图里最常见的大模型主类别；没有大模型结果时为 null。 */
  group: string | null;
}

export interface ImageTag {
  imageId: number;
  tagId: number;
  name: string;
  score: number;
  source: string;
  category: string | null;
}

// ===== MVP2 去重 =====
//
// 对应 src-tauri/src/commands/dedup.rs / services/dedup_service.rs / trash_service.rs。

export type DedupMethod = "exact" | "phash" | "both";
export type DedupAction = "trash" | "keep_all" | "dismiss";
export type DedupGroupMethod = "exact" | "phash";
export type DedupGroupStatus = "open" | "resolved" | "dismissed";
export type DedupPhase = "idle" | "hashing" | "grouping" | "done" | string;
export type DedupBatchKeepRule = "largest" | "newest" | "biggest_file" | "shortest_name";

export interface DedupStatus {
  running: boolean;
  hashPending: number;
  dhashPending: number;
  groupsFound: number;
  phase: DedupPhase;
}

export interface DedupStartArgs {
  method: DedupMethod;
  threshold?: number;
}

export interface DedupStartResult {
  started: boolean;
  status: DedupStatus;
}

export interface DuplicateGroup {
  id: number;
  method: DedupGroupMethod;
  threshold: number | null;
  keepImageId: number | null;
  status: DedupGroupStatus;
  createdAt: number;
  resolvedAt: number | null;
  itemCount: number;
}

export interface DuplicateGroupPage {
  items: DuplicateGroup[];
  total: number;
  offset: number;
  limit: number;
}

export interface DedupItemDetail {
  image: ImageRecord;
  similarity: number | null;
}

export interface DedupGroupDetail {
  group: DuplicateGroup;
  items: DedupItemDetail[];
}

export interface DedupGroupsArgs {
  method?: DedupGroupMethod;
  status?: DedupGroupStatus;
  offset?: number;
  limit?: number;
}

export interface DedupResolveArgs {
  groupId: number;
  keepImageIds: number[];
  action: DedupAction;
}

export interface TrashFailure {
  imageId: number;
  error: string;
}

export interface TrashMoveResult {
  succeeded: number[];
  /** succeeded 的子集：网络位置不支持回收站时，已永久删除且无法撤销。 */
  permanentlyDeleted: number[];
  failed: TrashFailure[];
  undoId: number | null;
}

export interface DedupResolveResult {
  groupId: number;
  trashed: number[];
  /** trashed 的子集：网络盘无回收站，已永久删除（不可撤销恢复）。 */
  permanentlyDeleted: number[];
  trashFailures: TrashFailure[];
  undoId: number | null;
}

export interface DedupBatchResolveArgs {
  method?: DedupGroupMethod | "all";
  action: DedupAction;
  keepRule?: DedupBatchKeepRule;
}

export interface DedupBatchGroupFailure {
  groupId: number;
  error: string;
}

export interface DedupBatchResolveResult {
  matchedGroups: number;
  resolvedGroups: number[];
  failedGroups: DedupBatchGroupFailure[];
  trashed: number[];
  /** trashed 的子集：网络盘无回收站，已永久删除（不可撤销恢复）。 */
  permanentlyDeleted: number[];
  trashFailures: TrashFailure[];
  undoId: number | null;
}

export interface DedupExportArgs {
  savePath: string;
  method?: DedupGroupMethod;
  status?: DedupGroupStatus;
}

export interface UndoEntry {
  id: number;
  action: string;
  payloadJson: string;
  canUndoUntil: number;
  undoneAt: number | null;
  createdAt: number;
}

export interface UndoOutcome {
  restored: number[];
  manualRecoveryRequired: boolean;
  note: string | null;
}

export interface DedupProgressEvent {
  running: boolean;
  hashPending: number;
  dhashPending: number;
  groupsFound: number;
  phase: DedupPhase;
}

export interface DedupNewGroupEvent {
  groupId: number;
  method: DedupGroupMethod;
}

export interface DedupDoneEvent {
  exactGroups: number;
  visualGroups: number;
  error: string | null;
}
