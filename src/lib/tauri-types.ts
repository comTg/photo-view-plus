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
