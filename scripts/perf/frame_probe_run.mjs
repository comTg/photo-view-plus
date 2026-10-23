#!/usr/bin/env node
// frame_probe_run.mjs — 无人值守地跑一次 frame_probe（docs/11 §4，run_scenarios.ps1 调用）。
//
// 用独立的用户数据目录启动 Chrome / Edge（不碰日常使用的浏览器配置），打开 frame_probe.html 自动滚动，
// 测满 N 秒后经 CDP 取 window.frameProbe.csv()，写成 frames_<标签>_<时间>.csv（与手动下载的格式相同），
// 然后关掉这个浏览器实例。窗口置顶由调用方负责（页面标题 "Frame Probe"）。
//
// 用法：node scripts/perf/frame_probe_run.mjs --out <目录> --label S2 [--duration 60]
//        [--browser <chrome.exe>] [--port 9333] [--profile <用户数据目录>]

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath, pathToFileURL } from "node:url";

const DEFAULT_BROWSERS = [
  "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
  "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
];

function parseArgs(argv) {
  const opts = {
    out: "",
    label: "run",
    duration: 60,
    browser: "",
    port: 9333,
    profile: path.join(os.tmpdir(), "pvp-frame-probe-profile"),
  };
  for (let i = 0; i < argv.length; i++) {
    const key = argv[i];
    const value = argv[i + 1];
    switch (key) {
      case "--out":
        opts.out = value;
        i++;
        break;
      case "--label":
        opts.label = value;
        i++;
        break;
      case "--duration":
        opts.duration = Number(value);
        i++;
        break;
      case "--browser":
        opts.browser = value;
        i++;
        break;
      case "--port":
        opts.port = Number(value);
        i++;
        break;
      case "--profile":
        opts.profile = value;
        i++;
        break;
      default:
        throw new Error(`未知参数：${key}`);
    }
  }
  if (!opts.out) throw new Error("缺少 --out");
  return opts;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function stamp(date) {
  const pad = (n) => String(n).padStart(2, "0");
  return `${date.getFullYear()}${pad(date.getMonth() + 1)}${pad(date.getDate())}-${pad(date.getHours())}${pad(date.getMinutes())}${pad(date.getSeconds())}`;
}

async function pageTarget(port, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
      const page = targets.find((t) => t.type === "page" && t.url.includes("frame_probe.html"));
      if (page) return page;
    } catch {
      // 浏览器还没起来
    }
    await sleep(300);
  }
  throw new Error("等不到 frame_probe 页面");
}

async function cdp(wsUrl) {
  const ws = new WebSocket(wsUrl);
  await new Promise((resolve, reject) => {
    ws.onopen = resolve;
    ws.onerror = reject;
  });
  let nextId = 1;
  const pending = new Map();
  ws.onmessage = (event) => {
    const message = JSON.parse(event.data);
    if (message.id && pending.has(message.id)) {
      pending.get(message.id)(message);
      pending.delete(message.id);
    }
  };
  const send = (method, params = {}) =>
    new Promise((resolve) => {
      const id = nextId++;
      pending.set(id, resolve);
      ws.send(JSON.stringify({ id, method, params }));
    });
  return { ws, send };
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  const browser = opts.browser || DEFAULT_BROWSERS.find((candidate) => existsSync(candidate));
  if (!browser) throw new Error("找不到 Chrome / Edge");
  const here = path.dirname(fileURLToPath(import.meta.url));
  const url = `${pathToFileURL(path.join(here, "frame_probe.html")).href}?label=${encodeURIComponent(opts.label)}&duration=${opts.duration}`;
  mkdirSync(opts.profile, { recursive: true });
  const child = spawn(
    browser,
    [
      `--user-data-dir=${opts.profile}`,
      `--remote-debugging-port=${opts.port}`,
      "--no-first-run",
      "--no-default-browser-check",
      "--disable-features=Translate",
      "--new-window",
      "--window-position=60,60",
      "--window-size=1280,900",
      url,
    ],
    { detached: false, stdio: "ignore" },
  );
  console.log(`browser pid ${child.pid}`);
  const started = new Date();
  const page = await pageTarget(opts.port, 30000);
  const { ws, send } = await cdp(page.webSocketDebuggerUrl);
  await send("Page.bringToFront");
  // 页面从加载完成开始计时；多等几秒让最后一个窗口收尾。
  await sleep((opts.duration + 4) * 1000);
  const reply = await send("Runtime.evaluate", {
    expression: "window.frameProbe ? window.frameProbe.csv() : ''",
    returnByValue: true,
  });
  const csv = reply.result?.result?.value ?? "";
  if (!csv) throw new Error("frame_probe 没有返回数据");
  mkdirSync(opts.out, { recursive: true });
  const file = path.join(opts.out, `frames_${opts.label}_${stamp(started)}.csv`);
  writeFileSync(file, csv, "utf8");
  console.log(`wrote ${file}`);
  await send("Browser.close").catch(() => undefined);
  ws.close();
  await sleep(500);
  if (child.exitCode === null) child.kill();
}

main().catch((error) => {
  console.error(`[frame_probe_run] ${error.message}`);
  process.exit(1);
});
