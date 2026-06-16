import { invoke } from "@tauri-apps/api/core";

// Tauri command 封装。后续按 docs/02 § 6 的清单逐个补。
// 现在只有一个 ping，用于 T1 验证前后端通路。

export async function ping(): Promise<string> {
  return invoke<string>("ping");
}
