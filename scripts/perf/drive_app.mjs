#!/usr/bin/env node
// drive_app.mjs — 通过 CDP 驱动 photo-view-plus 的 WebView2（Node 24，无第三方依赖）。
//
// 应用必须带下面的环境变量、从同一个终端启动（启动前不能已有同一 profile 的实例在运行）：
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=9222"
//
// 用法：node scripts/perf/drive_app.mjs [选项] <子命令> [参数...]
//
// 子命令：
//   targets                      列出 CDP 页面（排查连不上时用）
//   invoke <command> [args]      在页面里调 window.__TAURI_INTERNALS__.invoke(command, args)，打印 JSON 结果
//                                args 三种写法：
//                                  key=value 列表（推荐，不用处理引号）：rootId=3  patch.aiEnabled=false
//                                    值为数字 / true / false / null 时按类型传，其余按字符串；点号表示嵌套
//                                  JSON 字符串：'{"rootId":3}'（PowerShell 7.3+ 可用，5.1 会吞掉引号）
//                                  @文件.json 或 -（从 stdin 读 JSON）
//   eval <js表达式>              在页面里求值并打印结果（调试用，支持 Promise）
//   scroll-gallery <秒>          程序化来回滚动主画廊（VirtuosoGrid 滚动容器），同时统计帧间隔和长任务
//   ui-stats <秒>                不滚动，只统计长任务（PerformanceObserver 'longtask'）和帧间隔
//
// 选项：
//   --port 9222  --host 127.0.0.1
//   --target <片段>              按 url / 标题片段选页面（默认自动选 tauri.localhost / 127.0.0.1:5173）
//   --out <目录> --label <标签>  scroll-gallery / ui-stats 的结果另存为 ui_<标签>_<时间>.json 和 .csv（供 summarize.py）
//   --speed <px/s>               scroll-gallery 滚动速度，默认 1200
//   --windows                    stdout 里也打印逐窗口数据
//
// 例：
//   node scripts/perf/drive_app.mjs invoke ping
//   node scripts/perf/drive_app.mjs invoke roots_add "args.path=Y:\重要文件夹\唐唐手机备份\某目录" args.label=perf-A
//   node scripts/perf/drive_app.mjs invoke scan_start rootId=3
//   node scripts/perf/drive_app.mjs --out D:\perf\r0 --label S2 scroll-gallery 60

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

const WINDOW_MS = 5000;
// 与 frame_probe.html 一致：60Hz 下单次丢帧正好 33.3ms，over33 用 34ms 留余量（= 至少连丢 2 帧）。
const OVER33_MS = 34;

function usage(code) {
  // 打印文件开头的注释块（第 2 行起，到第一行非注释为止）
  const header = [];
  for (const line of readFileSync(new URL(import.meta.url), "utf8").split(/\r?\n/).slice(1)) {
    if (!line.startsWith("//")) break;
    header.push(line.replace(/^\/\/ ?/, ""));
  }
  (code === 0 ? console.log : console.error)(header.join("\n"));
  process.exit(code);
}

function fail(message, code = 1) {
  console.error(`[drive_app] ${message}`);
  process.exit(code);
}

function parseCli(argv) {
  const opts = { host: "127.0.0.1", port: 9222, target: "", out: "", label: "run", speed: 1200, windows: false };
  const rest = [];
  for (let i = 0; i < argv.length; i++) {
    let arg = argv[i];
    let inline = null;
    if (arg.startsWith("--") && arg.includes("=")) {
      inline = arg.slice(arg.indexOf("=") + 1);
      arg = arg.slice(0, arg.indexOf("="));
    }
    const take = () => {
      if (inline !== null) return inline;
      if (i + 1 >= argv.length) fail(`${arg} 缺少取值`, 2);
      i += 1;
      return argv[i];
    };
    switch (arg) {
      case "--host":
        opts.host = take();
        break;
      case "--port":
        opts.port = Number(take());
        break;
      case "--target":
        opts.target = take();
        break;
      case "--out":
        opts.out = take();
        break;
      case "--label":
        opts.label = take();
        break;
      case "--speed":
        opts.speed = Number(take());
        break;
      case "--windows":
        opts.windows = true;
        break;
      case "-h":
      case "--help":
        usage(0);
        break;
      default:
        rest.push(argv[i]);
    }
  }
  if (!Number.isFinite(opts.port) || opts.port <= 0) fail("--port 无效", 2);
  if (!Number.isFinite(opts.speed) || opts.speed <= 0) fail("--speed 无效", 2);
  return { opts, rest };
}

// ---------------------------------------------------------------- CDP

class CdpClient {
  constructor(url) {
    this.url = url;
    this.nextId = 1;
    this.pending = new Map();
    this.ws = null;
  }

  async connect(timeoutMs = 5000) {
    if (typeof WebSocket !== "function") throw new Error("需要 Node 22+（全局 WebSocket）");
    this.ws = new WebSocket(this.url);
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`连接 ${this.url} 超时`)), timeoutMs);
      this.ws.addEventListener(
        "open",
        () => {
          clearTimeout(timer);
          resolve();
        },
        { once: true },
      );
      this.ws.addEventListener(
        "error",
        (event) => {
          clearTimeout(timer);
          reject(
            new Error(
              `WebSocket 连接失败（${event?.message || "error"}）。若调试端口返回 403，` +
                "在 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS 里追加 --remote-allow-origins=* 后重启应用",
            ),
          );
        },
        { once: true },
      );
    });
    this.ws.addEventListener("message", (event) => this.onMessage(event));
    this.ws.addEventListener("close", () => {
      for (const pending of this.pending.values()) {
        clearTimeout(pending.timer);
        pending.reject(new Error("CDP 连接已关闭（页面被关闭或应用退出？）"));
      }
      this.pending.clear();
    });
  }

  onMessage(event) {
    let msg;
    try {
      msg = JSON.parse(typeof event.data === "string" ? event.data : Buffer.from(event.data).toString("utf8"));
    } catch {
      return;
    }
    if (!msg.id || !this.pending.has(msg.id)) return; // 事件通知，忽略
    const pending = this.pending.get(msg.id);
    this.pending.delete(msg.id);
    clearTimeout(pending.timer);
    if (msg.error) pending.reject(new Error(`${msg.error.message}${msg.error.data ? `: ${msg.error.data}` : ""}`));
    else pending.resolve(msg.result);
  }

  send(method, params = {}, timeoutMs = 30000) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`${method} 超时（${timeoutMs}ms）`));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }

  close() {
    try {
      this.ws?.close();
    } catch {
      // ignore
    }
  }
}

async function listTargets(opts) {
  const url = `http://${opts.host}:${opts.port}/json/list`;
  let res;
  try {
    res = await fetch(url, { signal: AbortSignal.timeout(3000) });
  } catch (error) {
    throw new Error(
      `连不上 ${url}（${error.cause?.code || error.message}）。检查：\n` +
        `  1. 应用是否在设置了 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=${opts.port} 的终端里启动；\n` +
        "  2. 启动前同一 profile 的应用实例是否已全部退出（已有 WebView2 浏览器进程时参数不生效）；\n" +
        "  3. 端口是否被别的程序占用（netstat -ano | findstr :" +
        opts.port +
        "）。",
    );
  }
  if (!res.ok) throw new Error(`${url} 返回 HTTP ${res.status}`);
  return res.json();
}

function pickTarget(targets, opts) {
  const pages = targets.filter((t) => t.type === "page" && t.webSocketDebuggerUrl);
  if (pages.length === 0) throw new Error("调试端口上没有可用的页面（type=page）。用 targets 子命令查看。");
  if (opts.target) {
    const hit = pages.find((t) => t.url.includes(opts.target) || (t.title || "").includes(opts.target));
    if (!hit) throw new Error(`没有 url/标题包含 "${opts.target}" 的页面。用 targets 子命令查看。`);
    return hit;
  }
  const app =
    pages.find((t) => /^https?:\/\/(tauri\.localhost|127\.0\.0\.1:5173|localhost:5173)/.test(t.url)) ||
    pages.find((t) => /PhotoView/i.test(t.title || ""));
  if (app) return app;
  if (pages.length > 1) console.error(`[drive_app] 有 ${pages.length} 个页面，默认用第一个：${pages[0].url}`);
  return pages[0];
}

async function connectPage(opts) {
  const target = pickTarget(await listTargets(opts), opts);
  const client = new CdpClient(target.webSocketDebuggerUrl);
  await client.connect();
  return { client, target };
}

async function evaluate(client, expression, timeoutMs = 30000) {
  const result = await client.send(
    "Runtime.evaluate",
    { expression, awaitPromise: true, returnByValue: true, userGesture: true },
    timeoutMs,
  );
  if (result.exceptionDetails) {
    const ex = result.exceptionDetails.exception;
    throw new Error(ex?.description || ex?.value || result.exceptionDetails.text || "页面脚本异常");
  }
  return result.result?.value;
}

// ---------------------------------------------------------------- invoke

function stripBom(text) {
  return text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;
}

function coerce(raw) {
  if (raw === "true") return true;
  if (raw === "false") return false;
  if (raw === "null") return null;
  if (/^-?\d+(\.\d+)?$/.test(raw)) return Number(raw);
  if (/^\s*[[{"]/.test(raw)) {
    try {
      return JSON.parse(raw);
    } catch {
      // 不是合法 JSON 就当普通字符串
    }
  }
  return raw;
}

function setPath(obj, keys, value) {
  let cur = obj;
  for (let i = 0; i < keys.length - 1; i++) {
    if (typeof cur[keys[i]] !== "object" || cur[keys[i]] === null) cur[keys[i]] = {};
    cur = cur[keys[i]];
  }
  cur[keys[keys.length - 1]] = value;
}

function parseInvokeArgs(tokens) {
  if (tokens.length === 0) return {};
  if (tokens.length === 1) {
    const token = tokens[0];
    try {
      if (token === "-") return JSON.parse(stripBom(readFileSync(0, "utf8")));
      if (token.startsWith("@")) return JSON.parse(stripBom(readFileSync(token.slice(1), "utf8")));
      if (/^\s*[[{]/.test(token)) return JSON.parse(token);
    } catch (error) {
      throw new Error(
        `参数不是合法 JSON：${error.message}\n  （Windows PowerShell 5.1 会吞掉内层双引号，改用 key=value 写法或 @文件.json）`,
      );
    }
  }
  const out = {};
  for (const token of tokens) {
    const eq = token.indexOf("=");
    if (eq <= 0) throw new Error(`无法解析参数 "${token}"：请用 key=value、JSON 或 @文件.json`);
    setPath(out, token.slice(0, eq).split("."), coerce(token.slice(eq + 1)));
  }
  return out;
}

async function cmdInvoke(opts, rest) {
  const [command, ...argTokens] = rest;
  if (!command) fail("用法：invoke <command> [args]", 2);
  const args = parseInvokeArgs(argTokens);
  const expression = `(async () => {
    const internals = window.__TAURI_INTERNALS__;
    if (!internals || typeof internals.invoke !== "function") {
      return { ok: false, error: "window.__TAURI_INTERNALS__.invoke 不存在：连到的不是应用页面？（用 --target 指定）" };
    }
    try {
      const value = await internals.invoke(${JSON.stringify(command)}, ${JSON.stringify(args)});
      return { ok: true, value: value === undefined ? null : value };
    } catch (error) {
      return { ok: false, error: typeof error === "string" ? error : (error && error.message) || JSON.stringify(error) };
    }
  })()`;
  const { client } = await connectPage(opts);
  try {
    const reply = await evaluate(client, expression, 10 * 60 * 1000);
    if (!reply?.ok) fail(`invoke ${command} 失败：${reply?.error ?? "未知错误"}`);
    console.log(JSON.stringify(reply.value, null, 2));
  } finally {
    client.close();
  }
}

async function cmdEval(opts, rest) {
  if (rest.length === 0) fail("用法：eval <js表达式>", 2);
  const { client } = await connectPage(opts);
  try {
    const value = await evaluate(client, rest.join(" "), 10 * 60 * 1000);
    console.log(JSON.stringify(value === undefined ? null : value, null, 2));
  } finally {
    client.close();
  }
}

async function cmdTargets(opts) {
  const targets = await listTargets(opts);
  for (const t of targets) console.log(`${t.type.padEnd(14)} ${t.title || "(无标题)"}\n${"".padEnd(15)}${t.url}`);
  if (targets.length === 0) console.log("(没有 target)");
}

// ---------------------------------------------------------------- 页面内测量

// 这个函数会被 toString() 后注入页面执行，只能用浏览器 API，不能引用外部变量。
async function pageMeasure(p) {
  const pct = (sorted, q) => {
    if (!sorted.length) return null;
    const k = ((sorted.length - 1) * q) / 100;
    const lo = Math.floor(k);
    const hi = Math.ceil(k);
    return lo === hi ? sorted[lo] : sorted[lo] + (sorted[hi] - sorted[lo]) * (k - lo);
  };
  const findScroller = () => {
    const candidates = [
      ...document.querySelectorAll(".virtual-image-grid"),
      ...document.querySelectorAll("[data-virtuoso-scroller]"),
    ];
    for (const el of candidates) {
      if (el.scrollHeight > el.clientHeight + 10 && el.querySelector(".image-card, .image-grid")) return el;
    }
    const grid = document.querySelector(".image-grid");
    for (let el = grid; el; el = el.parentElement) {
      const style = getComputedStyle(el);
      if (/(auto|scroll)/.test(style.overflowY) && el.scrollHeight > el.clientHeight + 10) return el;
    }
    return null;
  };

  let scroller = null;
  if (p.scroll) {
    scroller = findScroller();
    if (!scroller) {
      return {
        ok: false,
        error:
          "找不到可滚动的画廊容器（.virtual-image-grid / [data-virtuoso-scroller]）。请切到「浏览」视图，并确认图片多于一屏。",
      };
    }
  }

  const t0 = performance.now();
  const wall0 = Date.now();
  const frames = [];
  const longtasks = [];
  const loafs = [];
  let hidden = document.visibilityState !== "visible";
  const onVisibility = () => {
    if (document.visibilityState !== "visible") hidden = true;
  };
  document.addEventListener("visibilitychange", onVisibility);

  const supported = (typeof PerformanceObserver !== "undefined" && PerformanceObserver.supportedEntryTypes) || [];
  const observers = [];
  const watch = (type, sink) => {
    if (!supported.includes(type)) return false;
    const handle = (entries) => {
      for (const e of entries) sink(e);
    };
    const po = new PerformanceObserver((list) => handle(list.getEntries()));
    po.observe({ type });
    observers.push({ po, handle });
    return true;
  };
  const hasLongtask = watch("longtask", (e) => longtasks.push({ t: e.startTime - t0, d: e.duration }));
  const hasLoaf = watch("long-animation-frame", (e) =>
    loafs.push({ t: e.startTime - t0, d: e.duration, b: e.blockingDuration || 0 }),
  );

  const scroll = scroller
    ? {
        speedPxPerS: p.speed,
        distancePx: 0,
        bounces: 0,
        bottomWaits: 0,
        heightGrowths: 0,
        startScrollTop: Math.round(scroller.scrollTop),
        startScrollHeight: scroller.scrollHeight,
        endScrollHeight: null,
        clientHeight: scroller.clientHeight,
      }
    : null;

  await new Promise((resolve) => {
    let last = 0;
    let dir = 1;
    let pos = scroller ? scroller.scrollTop : 0;
    let waitUntil = 0;
    let waitHeight = 0;
    const step = (ts) => {
      if (last) frames.push({ t: ts - t0, d: ts - last });
      const dt = last ? Math.min(100, ts - last) : 0;
      last = ts;
      if (scroller && dt > 0) {
        const max = scroller.scrollHeight - scroller.clientHeight;
        if (Math.abs(scroller.scrollTop - pos) > 50) pos = scroller.scrollTop; // 被重排/刷新改过
        if (waitUntil) {
          // 到底后等下一页加载（VirtuosoGrid endReached → images_query），最多 1.5s，否则往回滚
          if (scroller.scrollHeight > waitHeight + 1) {
            waitUntil = 0;
            scroll.heightGrowths += 1;
          } else if (ts >= waitUntil) {
            waitUntil = 0;
            dir = -1;
            scroll.bounces += 1;
          }
        } else {
          let next = pos + (dir * p.speed * dt) / 1000;
          if (dir > 0 && next >= max) {
            next = max;
            waitUntil = ts + 1500;
            waitHeight = scroller.scrollHeight;
            scroll.bottomWaits += 1;
          } else if (dir < 0 && next <= 0) {
            next = 0;
            dir = 1;
            scroll.bounces += 1;
          }
          scroll.distancePx += Math.abs(next - pos);
          pos = next;
          scroller.scrollTop = pos;
        }
      }
      if (ts - t0 >= p.durationMs) resolve();
      else requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  });
  await new Promise((r) => setTimeout(r, 150)); // 让最后的 longtask 条目送达
  for (const { po, handle } of observers) {
    try {
      handle(po.takeRecords());
      po.disconnect();
    } catch {
      // ignore
    }
  }
  document.removeEventListener("visibilitychange", onVisibility);
  const durationMs = performance.now() - t0;
  if (scroll) {
    scroll.endScrollHeight = scroller.scrollHeight;
    scroll.distancePx = Math.round(scroll.distancePx);
  }

  const frameStats = (list) => {
    const sorted = list.map((f) => f.d).sort((a, b) => a - b);
    const med = pct(sorted, 50);
    return {
      frames: sorted.length,
      mean_ms: sorted.length ? sorted.reduce((a, b) => a + b, 0) / sorted.length : null,
      p50_ms: med,
      p95_ms: pct(sorted, 95),
      p99_ms: pct(sorted, 99),
      max_ms: sorted.length ? sorted[sorted.length - 1] : null,
      over20: sorted.filter((d) => d > 20).length,
      over33: sorted.filter((d) => d > p.over33Ms).length,
      over50: sorted.filter((d) => d > 50).length,
      median_ms: med,
    };
  };
  const taskStats = (list) => {
    const d = list.map((x) => x.d);
    return {
      n: d.length,
      over100: d.filter((x) => x > 100).length,
      max_ms: d.length ? Math.max(...d) : 0,
      total_ms: d.reduce((a, b) => a + b, 0),
      tbt_ms: d.reduce((a, b) => a + Math.max(0, b - 50), 0),
    };
  };

  const all = frameStats(frames);
  const period = all.median_ms || 16.7;
  all.dropped = frames.filter((f) => f.d > period * 1.5).length;
  all.fps = durationMs > 0 ? all.frames / (durationMs / 1000) : 0;
  delete all.median_ms;

  const windows = [];
  for (let start = 0; start < durationMs - 1; start += p.windowMs) {
    const end = Math.min(durationMs, start + p.windowMs);
    const fw = frames.filter((f) => f.t > start && f.t <= end);
    const lw = longtasks.filter((x) => x.t >= start && x.t < end);
    const fs = frameStats(fw);
    const ls = taskStats(lw);
    windows.push({
      start_ms: start,
      end_ms: end,
      frames: fs.frames,
      fps: end > start ? fs.frames / ((end - start) / 1000) : 0,
      p50_ms: fs.p50_ms,
      p95_ms: fs.p95_ms,
      p99_ms: fs.p99_ms,
      max_ms: fs.max_ms,
      over20: fs.over20,
      over33: fs.over33,
      dropped: fw.filter((f) => f.d > period * 1.5).length,
      longtask_n: ls.n,
      longtask_over100: ls.over100,
      longtask_max_ms: ls.max_ms,
      longtask_total_ms: ls.total_ms,
    });
  }

  const minutes = durationMs / 60000;
  const lt = taskStats(longtasks);
  const warnings = [];
  if (hidden) warnings.push("测量期间页面不可见（最小化/被遮挡/切到后台），帧数据无效");
  if (all.fps < 20) warnings.push(`平均 FPS 只有 ${all.fps.toFixed(1)}：窗口可能被遮挡或 WebView 被节流`);
  if (!hasLongtask) warnings.push("此 WebView 不支持 longtask 观察");
  return {
    ok: true,
    mode: p.scroll ? "scroll" : "idle",
    url: location.href,
    wall_start_ms: wall0,
    duration_ms: durationMs,
    visible: !hidden,
    refresh_hz_est: 1000 / period,
    frames: all,
    longtask: {
      supported: hasLongtask,
      ...lt,
      per_min: minutes > 0 ? lt.n / minutes : null,
      over100_per_min: minutes > 0 ? lt.over100 / minutes : null,
    },
    long_animation_frame: hasLoaf
      ? {
          n: loafs.length,
          over100: loafs.filter((x) => x.d > 100).length,
          max_ms: loafs.length ? Math.max(...loafs.map((x) => x.d)) : 0,
          blocking_ms: loafs.reduce((a, x) => a + x.b, 0),
        }
      : null,
    scroll,
    warnings,
    windows,
  };
}

function pad(n, w = 2) {
  return String(n).padStart(w, "0");
}

function localIso(ms) {
  const d = new Date(ms);
  const off = -d.getTimezoneOffset();
  const sign = off >= 0 ? "+" : "-";
  const a = Math.abs(off);
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}:` +
    `${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}${sign}${pad(Math.floor(a / 60))}:${pad(a % 60)}`
  );
}

function stamp(ms) {
  const d = new Date(ms);
  return `${d.getFullYear()}${pad(d.getMonth() + 1)}${pad(d.getDate())}-${pad(d.getHours())}${pad(d.getMinutes())}${pad(d.getSeconds())}`;
}

function safeLabel(label) {
  return String(label).replace(/[\\/:*?"<>|\s,]+/g, "_").replace(/^_+|_+$/g, "") || "run";
}

const UI_COLUMNS = [
  "window_start",
  "window_end",
  "elapsed_s",
  "window_s",
  "label",
  "mode",
  "frames",
  "fps",
  "frame_p50_ms",
  "frame_p95_ms",
  "frame_p99_ms",
  "frame_max_ms",
  "frames_over20",
  "frames_over33",
  "dropped",
  "longtask_n",
  "longtask_over100_n",
  "longtask_max_ms",
  "longtask_total_ms",
  "visible",
];

function csvValue(v) {
  if (v === null || v === undefined) return "";
  if (typeof v === "number") return Number.isInteger(v) ? String(v) : v.toFixed(3);
  const s = String(v);
  return /[",\r\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
}

function writeOutputs(opts, result) {
  const label = safeLabel(opts.label);
  const dir = path.resolve(opts.out);
  mkdirSync(dir, { recursive: true });
  const base = path.join(dir, `ui_${label}_${stamp(result.wall_start_ms)}`);
  writeFileSync(`${base}.json`, `${JSON.stringify({ label, ...result }, null, 2)}\n`, "utf8");
  const lines = [UI_COLUMNS.join(",")];
  for (const w of result.windows) {
    const row = {
      window_start: localIso(result.wall_start_ms + w.start_ms),
      window_end: localIso(result.wall_start_ms + w.end_ms),
      elapsed_s: w.end_ms / 1000,
      window_s: (w.end_ms - w.start_ms) / 1000,
      label,
      mode: result.mode,
      frames: w.frames,
      fps: w.fps,
      frame_p50_ms: w.p50_ms,
      frame_p95_ms: w.p95_ms,
      frame_p99_ms: w.p99_ms,
      frame_max_ms: w.max_ms,
      frames_over20: w.over20,
      frames_over33: w.over33,
      dropped: w.dropped,
      longtask_n: w.longtask_n,
      longtask_over100_n: w.longtask_over100,
      longtask_max_ms: w.longtask_max_ms,
      longtask_total_ms: w.longtask_total_ms,
      visible: result.visible ? 1 : 0,
    };
    lines.push(UI_COLUMNS.map((c) => csvValue(row[c])).join(","));
  }
  writeFileSync(`${base}.csv`, `\ufeff${lines.join("\r\n")}\r\n`, "utf8");
  return [`${base}.json`, `${base}.csv`];
}

async function cmdMeasure(opts, rest, scroll) {
  const seconds = Number(rest[0]);
  if (!Number.isFinite(seconds) || seconds <= 0) fail(`用法：${scroll ? "scroll-gallery" : "ui-stats"} <秒>`, 2);
  const params = { durationMs: seconds * 1000, windowMs: WINDOW_MS, scroll, speed: opts.speed, over33Ms: OVER33_MS };
  const expression = `(${pageMeasure.toString()})(${JSON.stringify(params)})`;
  const { client, target } = await connectPage(opts);
  console.error(`[drive_app] ${scroll ? "滚动画廊" : "统计"} ${seconds}s：${target.url}`);
  let result;
  try {
    result = await evaluate(client, expression, seconds * 1000 + 60000);
  } finally {
    client.close();
  }
  if (!result?.ok) fail(result?.error ?? "页面内测量失败");
  for (const w of result.warnings) console.error(`[drive_app] 警告：${w}`);
  if (opts.out) {
    const files = writeOutputs(opts, result);
    console.error(`[drive_app] 已写入 ${files.join(" , ")}`);
  }
  const printable = { ...result };
  if (!opts.windows) delete printable.windows;
  console.log(JSON.stringify(printable, null, 2));
}

// ---------------------------------------------------------------- main

async function main() {
  const { opts, rest } = parseCli(process.argv.slice(2));
  const [command, ...args] = rest;
  switch (command) {
    case "targets":
      return cmdTargets(opts);
    case "invoke":
      return cmdInvoke(opts, args);
    case "eval":
      return cmdEval(opts, args);
    case "scroll-gallery":
      return cmdMeasure(opts, args, true);
    case "ui-stats":
      return cmdMeasure(opts, args, false);
    case undefined:
      return usage(2);
    default:
      fail(`未知子命令：${command}（targets / invoke / eval / scroll-gallery / ui-stats）`, 2);
  }
}

main().catch((error) => fail(error?.message || String(error)));
