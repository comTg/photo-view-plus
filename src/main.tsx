import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { markBoot, signalFrontendReady } from "./lib/perf";
import "./styles/globals.css";

markBoot("module-eval"); // 入口模块开始执行：相对导航起点的值 ≈ webview 送达并解析 JS 的耗时

const container = document.getElementById("root");
if (!container) throw new Error("missing #root");

createRoot(container).render(
  <StrictMode>
    <App />
  </StrictMode>,
);

markBoot("render-called"); // React 已开始首次渲染
// 连续两帧后再标记，确保浏览器至少合成过一帧（≈ 用户看到内容、白屏结束）。
// 同时通知后端放行被推迟的启动期重活（缩略图重排等）。
requestAnimationFrame(() => {
  requestAnimationFrame(() => {
    markBoot("first-paint");
    signalFrontendReady();
  });
});
