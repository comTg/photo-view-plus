import { invoke } from "@tauri-apps/api/core";

// 诊断用：把启动各阶段的相对耗时（performance.now()，相对 webview 导航起点）
// 同时打到浏览器 console 和主进程终端日志，方便定位白屏发生在哪一段。
// 按 label 去重，避免 StrictMode 下 effect 双跑导致重复刷屏。白屏排查完成后可删除本文件。
const seen = new Set<string>();

export function markBoot(label: string): void {
  if (seen.has(label)) return;
  seen.add(label);
  const t = performance.now();
  console.log(`[boot] ${label} @ ${t.toFixed(1)}ms`);
  void invoke("ui_perf", { label, tMs: t }).catch(() => undefined);
}

// 通知后端首屏已绘制，放行被推迟的启动期重活（缩略图重排等）。
// 这一项不是临时诊断，是 StartupGate 机制的一部分，不要随 markBoot 一起删。
export function signalFrontendReady(): void {
  void invoke("frontend_ready").catch(() => undefined);
}
