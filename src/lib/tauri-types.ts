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
  indexedAt: number;
  deletedAt: number | null;
  rootPath: string;
  fullPath: string;
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
  locale: string;
  theme: "system" | "light" | "dark" | string;
  thumbCacheGb: number;
  localScanConcurrency: number;
  networkScanConcurrency: number;
}

export interface AppSettingsPatch {
  locale?: string;
  theme?: "system" | "light" | "dark" | string;
  thumbCacheGb?: number;
  localScanConcurrency?: number;
  networkScanConcurrency?: number;
}

// ===== MVP2 去重 =====
//
// 对应 src-tauri/src/commands/dedup.rs / services/dedup_service.rs / trash_service.rs。

export type DedupMethod = "exact" | "phash" | "both";
export type DedupAction = "trash" | "keep_all" | "dismiss";
export type DedupGroupMethod = "exact" | "phash";
export type DedupGroupStatus = "open" | "resolved" | "dismissed";
export type DedupPhase = "idle" | "hashing" | "grouping" | "done" | string;

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

export interface DedupResolveResult {
  groupId: number;
  trashed: number[];
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
