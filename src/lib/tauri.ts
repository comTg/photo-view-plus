import { invoke } from "@tauri-apps/api/core";
import type { AddRootArgs, DbStatus, RemoveResult, Root, UpdateRootArgs } from "./tauri-types";

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
