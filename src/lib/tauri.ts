import { invoke } from "@tauri-apps/api/core";
import type {
  AddRootArgs,
  AiDiagnostics,
  AiPipelineStatus,
  AiSearchArgs,
  AiSearchResult,
  AiTag,
  AiWorkerStatus,
  AppSettings,
  AppSettingsPatch,
  BackupExportArgs,
  BackupExportResult,
  BackupImportResult,
  DbStatus,
  DedupBatchResolveArgs,
  DedupBatchResolveResult,
  DedupExportArgs,
  DedupGroupDetail,
  DedupGroupsArgs,
  DedupResolveArgs,
  DedupResolveResult,
  DedupStartArgs,
  DedupStartResult,
  DedupStatus,
  DuplicateGroupPage,
  Face,
  FaceCluster,
  FaceStatus,
  ImagePage,
  ImageQueryParams,
  ImageRecord,
  ImageTag,
  MapImagePoint,
  OcrStatus,
  QueueStatus,
  RemoveResult,
  RenameImageArgs,
  Root,
  ScanStartResult,
  ScanTaskStatus,
  SmartAlbum,
  SmartAlbumInput,
  TimelineBucket,
  UndoEntry,
  UndoOutcome,
  UpdateRootArgs,
  WatcherStatus,
} from "./tauri-types";

// 系统
export async function ping(): Promise<string> {
  return invoke<string>("ping");
}

export async function dbStatus(): Promise<DbStatus> {
  return invoke<DbStatus>("db_status");
}

// Roots
export async function rootsAdd(args: AddRootArgs): Promise<Root> {
  return invoke<Root>("roots_add", { args });
}

export async function rootsList(): Promise<Root[]> {
  return invoke<Root[]>("roots_list");
}

export async function rootsRemove(id: number): Promise<RemoveResult> {
  return invoke<RemoveResult>("roots_remove", { id });
}

export async function rootsUpdate(args: UpdateRootArgs): Promise<Root | null> {
  return invoke<Root | null>("roots_update", { args });
}

// Scan
export async function scanStart(rootId: number): Promise<ScanStartResult> {
  return invoke<ScanStartResult>("scan_start", { rootId });
}

export async function scanPause(): Promise<QueueStatus> {
  return invoke<QueueStatus>("scan_pause");
}

export async function scanResume(): Promise<QueueStatus> {
  return invoke<QueueStatus>("scan_resume");
}

export async function scanCancel(): Promise<QueueStatus> {
  return invoke<QueueStatus>("scan_cancel");
}

export async function scanStatus(): Promise<ScanTaskStatus[]> {
  return invoke<ScanTaskStatus[]>("scan_status");
}

export async function queueStatus(): Promise<QueueStatus> {
  return invoke<QueueStatus>("queue_status");
}

// Images
export async function imagesQuery(params: ImageQueryParams): Promise<ImagePage> {
  return invoke<ImagePage>("images_query", { params });
}

export async function imagesGetDetail(id: number): Promise<ImageRecord | null> {
  return invoke<ImageRecord | null>("images_get_detail", { id });
}

export async function imagesSearchText(
  q: string,
  offset?: number,
  limit?: number,
): Promise<ImagePage> {
  return invoke<ImagePage>("images_search_text", { q, offset, limit });
}

export async function imagesTimeline(limit?: number): Promise<TimelineBucket[]> {
  return invoke<TimelineBucket[]>("images_timeline", { limit });
}

export async function imagesMapPoints(limit?: number): Promise<MapImagePoint[]> {
  return invoke<MapImagePoint[]>("images_map_points", { limit });
}

export async function imagesRename(args: RenameImageArgs): Promise<ImageRecord | null> {
  return invoke<ImageRecord | null>("images_rename", { args });
}

export async function imagesRevealInDir(id: number): Promise<void> {
  return invoke<void>("images_reveal_in_dir", { id });
}

export async function thumbsPath(imageId: number, size?: number): Promise<string | null> {
  return invoke<string | null>("thumbs_path", { imageId, size });
}

export function thumbUrl(imageId: number, size = 256): string {
  return `http://thumb.localhost/${imageId}?size=${size}`;
}

export function originalUrl(imageId: number): string {
  return `http://original.localhost/${imageId}`;
}

// Settings
export async function settingsGet(): Promise<AppSettings> {
  return invoke<AppSettings>("settings_get");
}

export async function settingsUpdate(patch: AppSettingsPatch): Promise<AppSettings> {
  return invoke<AppSettings>("settings_update", { patch });
}

// AI Worker
export async function aiWorkerStart(): Promise<AiWorkerStatus> {
  return invoke<AiWorkerStatus>("ai_worker_start");
}

export async function aiWorkerStop(): Promise<AiWorkerStatus> {
  return invoke<AiWorkerStatus>("ai_worker_stop");
}

export async function aiWorkerStatus(): Promise<AiWorkerStatus> {
  return invoke<AiWorkerStatus>("ai_worker_status");
}

export async function aiWorkerDiagnostics(): Promise<AiDiagnostics> {
  return invoke<AiDiagnostics>("ai_worker_diagnostics");
}

export async function aiModelsStatus(): Promise<unknown> {
  return invoke<unknown>("ai_models_status");
}

export async function aiModelDownload(modelKey: string): Promise<unknown> {
  return invoke<unknown>("ai_model_download", { modelKey });
}

export async function aiSearch(args: AiSearchArgs): Promise<AiSearchResult[]> {
  return invoke<AiSearchResult[]>("ai_search", { args });
}

export async function aiSearchByImage(imageId: number, limit?: number): Promise<AiSearchResult[]> {
  return invoke<AiSearchResult[]>("ai_search_by_image", { imageId, limit });
}

export async function aiTagImage(imageId: number): Promise<ImageTag[]> {
  return invoke<ImageTag[]>("ai_tag_image", { imageId });
}

export async function aiPipelineStatus(): Promise<AiPipelineStatus> {
  return invoke<AiPipelineStatus>("ai_pipeline_status");
}

export async function aiProcessPending(): Promise<void> {
  return invoke<void>("ai_process_pending");
}

export async function aiRetagAll(): Promise<number> {
  return invoke<number>("ai_retag_all");
}

export async function aiTagsList(limit?: number): Promise<AiTag[]> {
  return invoke<AiTag[]>("ai_tags_list", { limit });
}

export async function aiImageTags(imageId: number): Promise<ImageTag[]> {
  return invoke<ImageTag[]>("ai_image_tags", { imageId });
}

export async function aiImagesByTag(
  tagId: number,
  offset?: number,
  limit?: number,
): Promise<ImagePage> {
  return invoke<ImagePage>("ai_images_by_tag", { tagId, offset, limit });
}

// MVP4
export async function ocrRun(imageIds?: number[]): Promise<number> {
  return invoke<number>("ocr_run", { args: imageIds ? { imageIds } : null });
}

export async function ocrStatus(): Promise<OcrStatus> {
  return invoke<OcrStatus>("ocr_status");
}

export async function faceRun(imageIds?: number[]): Promise<number> {
  return invoke<number>("face_run", { args: imageIds ? { imageIds } : null });
}

export async function faceStatus(): Promise<FaceStatus> {
  return invoke<FaceStatus>("face_status");
}

export async function facesCluster(): Promise<number> {
  return invoke<number>("faces_cluster");
}

export async function facesListClusters(): Promise<FaceCluster[]> {
  return invoke<FaceCluster[]>("faces_list_clusters");
}

export async function facesForImage(imageId: number): Promise<Face[]> {
  return invoke<Face[]>("faces_for_image", { imageId });
}

export async function facesImagesForCluster(
  clusterId: number,
  offset?: number,
  limit?: number,
): Promise<ImagePage> {
  return invoke<ImagePage>("faces_images_for_cluster", { clusterId, offset, limit });
}

export async function faceClusterRename(clusterId: number, label: string | null): Promise<void> {
  return invoke<void>("face_cluster_rename", { args: { clusterId, label } });
}

export async function smartAlbumSave(input: SmartAlbumInput): Promise<SmartAlbum> {
  return invoke<SmartAlbum>("smart_album_save", { input });
}

export async function smartAlbumList(): Promise<SmartAlbum[]> {
  return invoke<SmartAlbum[]>("smart_album_list");
}

export async function smartAlbumDelete(id: number): Promise<boolean> {
  return invoke<boolean>("smart_album_delete", { id });
}

export async function smartAlbumQuery(
  id: number,
  offset?: number,
  limit?: number,
): Promise<ImagePage> {
  return invoke<ImagePage>("smart_album_query", { id, offset, limit });
}

export async function watcherStart(): Promise<WatcherStatus> {
  return invoke<WatcherStatus>("watcher_start");
}

export async function watcherStop(): Promise<WatcherStatus> {
  return invoke<WatcherStatus>("watcher_stop");
}

export async function watcherStatus(): Promise<WatcherStatus> {
  return invoke<WatcherStatus>("watcher_status");
}

export async function libraryBackupExport(args: BackupExportArgs): Promise<BackupExportResult> {
  return invoke<BackupExportResult>("library_backup_export", { args });
}

export async function libraryBackupImport(source: string): Promise<BackupImportResult> {
  return invoke<BackupImportResult>("library_backup_import", { source });
}

// Dedup
export async function dedupStart(args: DedupStartArgs): Promise<DedupStartResult> {
  return invoke<DedupStartResult>("dedup_start", { args });
}

export async function dedupStatus(): Promise<DedupStatus> {
  return invoke<DedupStatus>("dedup_status");
}

export async function dedupGroups(args: DedupGroupsArgs = {}): Promise<DuplicateGroupPage> {
  return invoke<DuplicateGroupPage>("dedup_groups", { args });
}

export async function dedupGroupDetail(groupId: number): Promise<DedupGroupDetail | null> {
  return invoke<DedupGroupDetail | null>("dedup_group_detail", { groupId });
}

export async function dedupResolve(args: DedupResolveArgs): Promise<DedupResolveResult> {
  return invoke<DedupResolveResult>("dedup_resolve", { args });
}

export async function dedupBatchResolve(
  args: DedupBatchResolveArgs,
): Promise<DedupBatchResolveResult> {
  return invoke<DedupBatchResolveResult>("dedup_batch_resolve", { args });
}

export async function dedupExportCsv(args: DedupExportArgs): Promise<number> {
  return invoke<number>("dedup_export_csv", { args });
}

// Trash
export async function trashHistory(limit?: number): Promise<UndoEntry[]> {
  return invoke<UndoEntry[]>("trash_history", { limit });
}

export async function trashUndo(undoId: number): Promise<UndoOutcome> {
  return invoke<UndoOutcome>("trash_undo", { undoId });
}
