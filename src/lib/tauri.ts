import { invoke } from "@tauri-apps/api/core";
import type {
  AddRootArgs,
  AppSettings,
  AppSettingsPatch,
  DbStatus,
  ImagePage,
  ImageQueryParams,
  ImageRecord,
  QueueStatus,
  RemoveResult,
  RenameImageArgs,
  Root,
  ScanStartResult,
  ScanTaskStatus,
  UpdateRootArgs,
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

// Settings
export async function settingsGet(): Promise<AppSettings> {
  return invoke<AppSettings>("settings_get");
}

export async function settingsUpdate(patch: AppSettingsPatch): Promise<AppSettings> {
  return invoke<AppSettings>("settings_update", { patch });
}
