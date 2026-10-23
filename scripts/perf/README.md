# R0 测量工具（docs/11 §4）

用来回答 docs/11 §3 的 H1–H7：后台处理时整台电脑变卡，主因到底是 GPU、CPU、杀毒、前端还是磁盘/网络。
全部只用系统自带组件（PowerShell + PDH 计数器、nvidia-smi、Python 标准库、Node 24、Chrome），不需要额外下载。

| 文件 | 作用 | 输出 |
|------|------|------|
| `record.ps1` | 每秒采样系统计数器（CPU、各进程 CPU/内存、可用内存、磁盘繁忙度/队列、网络接收、GPU 各引擎利用率、显存），并行跑 `nvidia-smi -lms 1000` | `record_<标签>_<时间>.csv`、`nvidia_<标签>_<时间>.csv`、`record_<标签>_<时间>.meta.json` |
| `probe.py` | 前台响应探针（普通优先级）：sleep(5ms) 计时器超出量、固定计算量耗时、C: 盘 4KB 写+fsync 延迟，每 5 秒一行 | `probe_<标签>_<时间>.csv`、`probe_calibration.json` |
| `frame_probe.html` | 在 Chrome 里打开：自动滚动长图墙 + rAF 帧间隔统计，测"其他应用感受到的卡"（GPU 争用） | 点按钮下载 `frames_<标签>_<时间>.csv` |
| `drive_app.mjs` | 通过 CDP 驱动应用的 WebView2：调 Tauri 命令（加根目录、扫描、开关 AI）、程序化滚动画廊、统计应用内长任务和帧时间 | `ui_<标签>_<时间>.json/.csv`（加 `--out` 时） |
| `summarize.py` | 读取测量目录，按场景标签分组，输出各指标 mean / p95 和派生指标（变慢 %、p99 增量、>33ms 帧、长任务/分钟） | `summary.md` |
| `run_scenarios.ps1` | 无人值守跑完 S0–S5（§6）：串起上面几个工具，模拟"使用中"、检测有没有人在用电脑、重置缩略图负载、测可见区域出图和降档速度 | 以上全部 + `events.log`、`interference.csv`、`visible_latency.json`、`S5_events.json` |
| `frame_probe_run.mjs` | 用独立的浏览器实例跑一次 frame_probe，经 CDP 取结果（不用手动下载） | `frames_<标签>_<时间>.csv` |
| `downgrade_report.py` | S5 降档速度：输入前后 CPU / GPU 的水平和回落用时 | 打印表格 |

> docs/11 §4 表里的"应用内探针（`PVP_PERF_PROBE=1`）"没有做——那需要改 `src/`。`drive_app.mjs ui-stats / scroll-gallery` 通过 CDP 在页面里装同样的 `PerformanceObserver('longtask')` 和帧采样器，效果等价，且不用改应用代码。

---

## 0. 一次性准备

### 0.1 测量环境

- 测量期间电脑会卡，需要人在场。S0–S4 各 2 轮纯测量 30 分钟，加上切换和等缩略图队列跑空，预计 1 小时左右。
- 关掉无关的重负载程序（游戏、下载、视频、Steam 更新等），测量期间也不要用这台电脑做别的事。**冒烟测试时本机"空闲"总 CPU 就有 7–11%（p95 12–25%）**：窗口内最忙的是 System、VS Code、终端、Steam、火绒托盘（`HipsTray`）；按开机以来的累计 CPU 时间，Clash Party / mihomo、TrafficMonitor 也是大户。关不掉的话，至少保证每个场景开着的程序一样。`summary.md` 的"最忙的进程"一节会列出每个场景里 CPU 最高的 10 个进程，用来发现意料之外的干扰（例如 Clash 开了 TUN 模式时，访问 NAS 的流量可能经过 mihomo）。
- 建一个测量目录，所有场景共用（**不要放在仓库里**）：

  ```powershell
  $R0 = 'D:\perf\r0-20261022'
  New-Item -ItemType Directory -Force $R0 | Out-Null
  ```

  `probe.py` 第一次运行时会在这个目录里写 `probe_calibration.json`（固定计算量的单元数），之后所有场景复用它，"变慢 %"才可比。所以**第一轮一定先跑 S0（空闲）**。
- 全程用同一个 Python 解释器跑 `probe.py`（系统的 `python` 3.12 即可；Python < 3.11 在 Windows 上 sleep 精度只有 ~15ms，不能用）。
- PowerShell 7（`pwsh`）和 Windows PowerShell 5.1 都能跑 `record.ps1`，下面统一写 `pwsh -NoProfile -ExecutionPolicy Bypass -File ...`。
- 先在一个空闲时刻检查 AI 环境：`pnpm ai:check`（S3/S4 需要 CUDA 和已下载的模型；不要在测量过程中做）。

### 0.2 构建 test profile 的 release exe（关键）

**数据目录由编译期的 identifier 决定，不由 `PVP_PROFILE` 决定。** 应用用 `app_local_data_dir()` = `%LOCALAPPDATA%\<identifier>`；运行时的 `PVP_PROFILE` 只改日志目录和窗口标题。

- 现有的 `src-tauri\target\release\photo-view-plus.exe`（2026-07-02）是用默认配置构建的，内嵌 identifier 是 `com.vetoer.photoviewplus`（**正式版**）。拿它加 `PVP_PROFILE=test` 启动，打开的是**正式库**。
- 也不要用裸的 `cargo build --release`：它不会开启 Tauri 的 `custom-protocol` 特性（编出来是 dev 模式，会去连 `http://127.0.0.1:5173`，白屏），不会先跑 `pnpm build`，identifier 也还是正式版。

正确做法（在仓库根目录）：

```powershell
$env:PVP_PROFILE = 'test'        # build.rs 把它写进 PVP_BUILD_PROFILE，exe 默认就是 test
pnpm tauri build --no-bundle --config src-tauri/tauri.test.json
Remove-Item Env:PVP_PROFILE
```

- `--config src-tauri/tauri.test.json` 把 identifier 换成 `com.vetoer.photoviewplus.test`，数据落在 `%LOCALAPPDATA%\com.vetoer.photoviewplus.test`。
- `--no-bundle` 跳过 MSI/NSIS 打包。产物是 `src-tauri\target\release\photo-view-plus.exe`（会覆盖旧文件）。**测完后这个 exe 不要当正式版用**，需要正式版时重新 `pnpm tauri:build:prod`。
- **构建之后、测量之前，不要在 `src-tauri` 里跑任何 `--release` 的 cargo 命令。** `cargo test --release`（例如跑 `thumb_codec_eval`）和 `cargo build --release` 会顺带重编 bin 目标，用默认配置覆盖这个 exe。docs/11 R6 就这样出过一次事故：评测测试重编了 exe，测量脚本随后用 `PVP_PROFILE=test` 启动它，打开并迁移了正式库。需要跑 release 评测时，跑完重新按上面构建一次。
- 现在应用自己会拦：`PVP_PROFILE` 指定的 profile 和构建的 identifier 对不上时，记一条 ERROR 日志后直接退出，不打开任何库（`config::profile_mismatch`）。`run_scenarios.ps1` 发现应用启动即退出会直接报错。

### 0.3 test profile 的数据

- 数据目录：`%LOCALAPPDATA%\com.vetoer.photoviewplus.test`（`db\`、`thumbs\`、`vectors\`、`config.json`、`logs\`、`EBWebView\`）。只动这个目录；`com.vetoer.photoviewplus`（正式）和 `.dev` 一律不碰。
- 如果这个目录以前用过：S2 新加的根目录会得到新的 root id，缩略图按 `root_id + 相对路径` 生成文件名，所以旧缓存不影响"从零生成缩略图"。想要完全干净，可以在应用关闭时把整个目录改名备份（例如加 `.bak-20261022` 后缀），应用会重建。
- 全新的 test profile 默认 `aiEnabled = true`。**启动后第一件事是关掉 AI**（见 §1），否则 S2 一出缩略图就会自动拉起 AI worker，污染场景。
- 模型权重在 `%LOCALAPPDATA%\PhotoViewPlus\models`，各 profile 共用，只读。

### 0.4 准备 NAS 子目录

- 每个需要"新缩略图"的轮次用一个**没扫过**的 NAS 子目录，约 2,000 张：S2 两轮、S4 两轮，共 4 个（下文称 A、B、C、D）。
- 避开 `Y:\重要文件夹\唐唐手机备份\PD2324\PD2324相册备份\camera`：它和 `Camera` 只差大小写，SMB 下有 2,512 张打不开（docs/11 §6.1），会白占 NAS 往返、扭曲结果。
- 目录要够大：最好 3 分钟结束时缩略图队列还没跑完（`queue_status` 的 `p0` 仍 > 0）。如果提前跑完了，在记录里注明实际忙碌时长（看 record CSV 里 `proc_app_cpu` 什么时候回落）。
- 不得已复用同一目录时（删根目录后重新添加），原图很可能已经在 Windows 文件缓存里（32GB 内存），NAS 读取量会明显偏低，第二轮的网络和磁盘数据不可比。

---

## 1. 启动应用（test profile + CDP 调试端口）

开两个终端：**终端 A** 跑 `record.ps1`（每次阻塞 3 分钟），**终端 B** 启动应用、发 `drive_app.mjs` 命令。终端 B：

```powershell
cd D:\code\mygit\photo-view-plus
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8   # 让 PowerShell 正确解码 node 输出的中文
Get-Process photo-view-plus -ErrorAction SilentlyContinue  # 必须没有残留实例，否则调试端口参数不生效
$env:PVP_PROFILE = 'test'
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = '--remote-debugging-port=9222'
Start-Process .\src-tauri\target\release\photo-view-plus.exe
```

确认连上的是 test 库（**必须做**）：

```powershell
node scripts\perf\drive_app.mjs invoke ping      # 应输出 "test"（这只说明 PVP_PROFILE 生效，不能证明数据目录）
$line = (Select-String -Path "$env:LOCALAPPDATA\com.vetoer.photoviewplus.test\logs\photo-view-plus.log*" -Pattern 'opening database' |
  Select-Object -Last 1).Line
$line
if ($line -notmatch 'com\.vetoer\.photoviewplus\.test\\+db') { Write-Warning '打开的不是 test 库！立刻关闭应用' }
```

最后这条日志里的数据库路径必须含 `com.vetoer.photoviewplus.test\db\app.sqlite`。如果是 `com.vetoer.photoviewplus\db\app.sqlite`（没有 `.test`），说明 exe 不对，**立刻关掉应用**，回到 §0.2。

然后关掉 AI，并确认 OCR / 人脸是关的：

```powershell
node scripts\perf\drive_app.mjs invoke settings_update patch.aiEnabled=false
node scripts\perf\drive_app.mjs invoke settings_get     # 检查 aiEnabled / ocrEnabled / faceEnabled 都是 false
```

说明：

- `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` 对从这个终端启动的所有 WebView2 程序都生效，这个终端里只启动本应用。测完执行 `Remove-Item Env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS, Env:PVP_PROFILE`。
- 调试端口只监听 127.0.0.1，但本机任何程序都能借它操控应用，测完就关应用。
- `drive_app.mjs` 连不上时先跑 `node scripts\perf\drive_app.mjs targets` 看有没有页面；若 WebSocket 返回 403，把变量改成 `'--remote-debugging-port=9222 --remote-allow-origins=*'` 后重启应用。

### drive_app.mjs 常用命令

参数推荐用 `key=value` 写法（点号表示嵌套，数字/true/false/null 按类型传，其余是字符串），不用处理引号；PowerShell 7.3+ 也可以直接写 JSON（`'{"rootId":3}'`），Windows PowerShell 5.1 会吞掉内层双引号。命令名和参数形状来自 `src-tauri/src/commands/*.rs` 与 `src/lib/tauri.ts`。

| 用途 | 命令 |
|------|------|
| 加根目录（返回 Root，记下 `id`） | `$A = (node scripts\perf\drive_app.mjs invoke roots_add "args.path=Y:\重要文件夹\唐唐手机备份\<子目录A>" args.label=perf-A \| ConvertFrom-Json).id` |
| 列根目录 | `node scripts\perf\drive_app.mjs invoke roots_list` |
| 开始扫描（扫描完自动生成缩略图） | `node scripts\perf\drive_app.mjs invoke scan_start rootId=$A` |
| 扫描进度 | `node scripts\perf\drive_app.mjs invoke scan_status` |
| 队列（`p0` 缩略图，`p1` 扫描，`p5` 标签，`p6` 向量，`running`） | `node scripts\perf\drive_app.mjs invoke queue_status` |
| 开 / 关 AI | `node scripts\perf\drive_app.mjs invoke settings_update patch.aiEnabled=true`（`=false` 关） |
| AI 待办数 | `node scripts\perf\drive_app.mjs invoke ai_pipeline_status` |
| 立刻投递一批 AI（不等 2 秒节拍） | `node scripts\perf\drive_app.mjs invoke ai_process_pending` |
| 停 worker（释放显存） | `node scripts\perf\drive_app.mjs invoke ai_worker_stop` |
| 把就绪图片的 AI 标签重置为待处理（只动 test 库） | `node scripts\perf\drive_app.mjs invoke ai_retag_all` |
| 删除根目录（级联删库里的记录，不碰原图） | `node scripts\perf\drive_app.mjs invoke roots_remove id=$A` |
| 滚动画廊 60 秒并统计长任务/帧 | `node scripts\perf\drive_app.mjs --out $R0 --label S2 scroll-gallery 60` |
| 只统计不滚动 | `node scripts\perf\drive_app.mjs --out $R0 --label S1 ui-stats 60` |

等队列跑空（场景之间用）：

```powershell
do {
  $q = node scripts\perf\drive_app.mjs invoke queue_status | ConvertFrom-Json
  '{0}  p0={1} p1={2} p5={3} p6={4} running={5}' -f (Get-Date -Format HH:mm:ss), $q.p0, $q.p1, $q.p5, $q.p6, $q.running
  Start-Sleep -Seconds 10
} while ($q.p0 + $q.p1 + $q.p5 + $q.p6 + $q.running -gt 0)
```

`scroll-gallery` 找的是 `src/App.tsx` 里 `VirtuosoGrid`（`className="virtual-image-grid"`，即 `gridScrollerRef` 指向的滚动容器），以默认 1200 px/s 来回滚动；到底时最多等 1.5 秒让下一页（`endReached` → `images_query`）加载进来，没有新内容就往回滚。应用必须停在「浏览」视图、图片多于一屏，且窗口可见。

---

## 2. 每个场景的通用步骤（3 分钟）

| 时间 | 终端 A | 终端 B / 手动 |
|------|--------|---------------|
| 0:00 | 启动 record + probe（见下） | — |
| 0:05–0:10 | — | 触发本场景的负载（S2/S3/S4，见 §3） |
| 0:20–1:20 | — | 把 Chrome 放到前台，打开 frame_probe（见下），自动滚动 60 秒，结束自动下载 CSV |
| 1:35–2:35 | — | 把应用放到前台，`scroll-gallery 60`（S0 跳过） |
| 3:00 | record / probe 自动结束 | 收尾：关负载、等队列，见 §3 |

终端 A：

```powershell
cd D:\code\mygit\photo-view-plus
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\perf\record.ps1 -DurationSec 180 -OutDir $R0 -Label S2 -WithProbe
```

- `-WithProbe` 在同一个控制台里同时启动 `probe.py`（同样的时长、标签、目录、文件后缀）；两者的进度行会交替打印。Ctrl+C 会同时停掉两者，已写的数据都保留。
- 换解释器：`-PythonExe D:\code\mygit\photo-view-plus\ai-worker\.venv\Scripts\python.exe`（全程保持同一个）。
- 也可以分开跑：`python scripts\perf\probe.py --duration 180 --out $R0 --label S2`。
- **同一场景的两轮用同一个标签**（如两次都是 `S2`），文件名带时间戳不会覆盖，`summarize.py` 会合并；想逐轮对比时加 `--per-run`。

frame_probe（Chrome 或 Edge 打开本地文件，标签和场景一致）：

```
file:///D:/code/mygit/photo-view-plus/scripts/perf/frame_probe.html?label=S2&duration=60&autodownload=1
```

- 测量的那 60 秒里 Chrome 窗口要在前台且可见（被最小化或完全遮挡时浏览器会把 rAF 降频，这些窗口会记为 `visible=0`，汇总时剔除）。
- 下载的 `frames_S2_<时间>.csv` 在"下载"文件夹里，**每个场景结束后挪进 `$R0`**。
- 也可以不带参数打开，手动点"下载 CSV"；取消"自动滚动"后可以手动滚动。

---

## 3. 场景顺序与操作

按下面的顺序做，S1 放在 S2 第一轮之后，这样画廊里有图可滚。每个场景跑 2 轮。

| 顺序 | 场景 | 前置条件 | 负载怎么触发 | 结束后 |
|------|------|----------|-------------|--------|
| 1 | **S0** 基线 ×2 | 应用和 AI worker 都没在运行 | 无 | 第一轮生成 `probe_calibration.json`；跳过画廊滚动 |
| 2 | 启动应用 | §1 全部做完（含确认 `.test` 和关 AI） | — | — |
| 3 | **S2** 只生成缩略图，第 1 轮 | AI 关；队列空 | `roots_add` A → `scan_start rootId=$A` | 等队列跑空 |
| 4 | **S1** 应用打开、无后台任务 ×2 | 队列空、AI 关；画廊显示 A 的图 | 无 | — |
| 5 | **S2** 第 2 轮 | 同上 | `roots_add` B → `scan_start` | 等队列跑空 |
| 6 | **S3** 只跑 AI ×2 | 队列空；`ai_pipeline_status` 显示待办 ≥ 2,000 | `settings_update patch.aiEnabled=true`（2 秒内自动拉起 worker 开始投递；可再发 `ai_process_pending`） | `patch.aiEnabled=false` → `ai_worker_stop` → 等队列空 |
| 7 | **S4** S2 + S3 同时 ×2 | 同 S3，且有新目录 C / D | `roots_add` C → `scan_start` → `patch.aiEnabled=true` | 同 S3 |

说明：

- `roots_add` 只加记录、不扫描，可以在启动 record 之前做好；`scan_start`（和 S4 的开 AI）在记录开始后 5–10 秒再发，这样曲线里能看到负载上升的过程。
- **S3 第一轮的前 30–60 秒是 worker 冷启动**（导入 torch、CUDA 初始化、加载 RAM-plus 和 CLIP），对应 H5。每轮之前都先 `ai_worker_stop`，让两轮条件一致。
- S3 用的是 S2 生成过缩略图、还没做过 AI 的 A、B 两批图（约 4,000 张），等价于 docs/11 里"把已有缩略图的图标成待处理"，而且只动 test 库。开始前看 `ai_pipeline_status` 的 `embeddingPending` / `taggingPending`；如果第二轮前待办不够 3 分钟的量，可以 `ai_retag_all` 把标签重置为待处理（只重置标签，不重置向量），并在记录里注明"第二轮只有打标"。这一步会清空 test 库里的 AI 标签，执行前再确认一次连的是 test 库。
- S4 开始前同样确认 AI 有待办（A/B 剩余的，或 `ai_retag_all`），C/D 提供新的缩略图负载。
- 各场景之间留 30 秒左右让机器回落。

全部跑完后：

```powershell
python scripts\perf\summarize.py $R0 --all
```

打印并写入 `$R0\summary.md`，把表格整理进 docs/11 附录 A。最后关掉应用，清理环境变量（§1）。

---

## 4. 输出与指标说明

### record_*.csv（每秒一行）

| 列 | 含义 |
|----|------|
| `timestamp`、`elapsed_s`、`label` | 本地时间（带时区）、从开始算的秒数、场景标签 |
| `cpu_total_pct`、`cpu_dpc_pct`、`cpu_intr_pct` | 总 CPU、DPC、中断时间（`\Processor(_Total)`） |
| `cpu_queue_len` | 就绪队列长度（`\System\Processor Queue Length`），持续 > 核数说明 CPU 不够分（H2） |
| `mem_avail_mb` | 可用内存（H7） |
| `proc_<组>_cpu` | 该组进程 CPU 之和，**按单核计**（0–1200%） |
| `proc_<组>_cpu_norm` | 上一列 ÷ 逻辑 CPU 数（本机 12），即占整机百分比 |
| `proc_<组>_n`、`proc_<组>_ws_mb` | 进程数、私有工作集 MB |
| `gpu_3d_pct`、`gpu_compute_pct`、`gpu_copy_pct`、`gpu_videodecode_pct` | 显示用 GPU 上该类型最忙那个引擎的利用率（各进程求和，同任务管理器） |
| `gpu_engine_max_pct` | 所有引擎里最忙的那个 |
| `gpu_<组>_pct`、`vram_<组>_mb` | 该组进程在 GPU 上的利用率（最忙引擎）、专用显存 |
| `gpu_dedicated_mb` | 显卡专用显存总占用（`\GPU Adapter Memory`） |
| `disk_<盘>_idle_pct` / `_busy_pct` / `_queue` | 各物理盘空闲 %、繁忙 %（= 100 − 空闲）、平均队列长度 |
| `net_rx_MBps`、`net_rx_max_if_MBps` | 所有网卡接收合计、最忙网卡（两者差很多时说明有虚拟网卡重复计数） |
| `sampler_ms`、`self_cpu_pct` | 本次采样耗时、record 自身 CPU（单核 %），用来确认测量本身足够轻 |

进程分组：

| 组 | 成员 |
|----|------|
| `app` | `photo-view-plus` |
| `apptree` | `photo-view-plus` 及其全部子孙进程（按 Creating Process ID 建树：WebView2、AI worker 等） |
| `wv2` / `wv2app` | 全机所有 `msedgewebview2`（含其他应用，冒烟时有 19 个） / 只算应用自己的 |
| `python` / `pyapp` | 全机所有 `python*`（含 probe.py 自己和其他工具，冒烟时有 26–27 个） / 只算应用拉起的 AI worker |
| `llama` | `llama-server*`（以后的 VLM） |
| `hipsd` / `hips` | `HipsDaemon` / 全部火绒进程（`Hips*`、`usysdiag`、`wsctrlsvc`） |
| `dwm` | 桌面合成器 |

看应用相关的数据请用 `apptree`、`wv2app`、`pyapp`，不要用 `wv2`、`python`。

`record_*.meta.json`：运行信息、计数器可用性备注、采样开销、`top_processes`（本轮 CPU 最高的 15 个进程名，任何进程）。

`nvidia_*.csv`：nvidia-smi 原样输出，列为 `timestamp, utilization.gpu [%], memory.used [MiB]`（在请求的两列前加了时间戳，表头只打印一次）。

### probe_*.csv（每 5 秒一行）

| 列 | 含义 |
|----|------|
| `timer_p50/p95/p99/max_ms` | `time.sleep(0.005)` 实际耗时超出 5ms 的部分（窗口内约 900 个样本） |
| `cpu_work_ms_mean` / `_max`、`cpu_work_slowdown_pct` | 固定计算量（约 50ms 的 sha256，每 2 秒一次）耗时；相对校准值的变化。**跨场景比较看 summary 里相对 S0 的"变慢 %"** |
| `disk_write_fsync_ms_*`、`disk_read_ms_*` | 每秒一次：打开 `%TEMP%` 下的临时文件 → 写 4KB → fsync → 关闭（写延迟）；再打开 → 读回 → 关闭（读延迟，通常命中缓存）。每轮重新打开/关闭，杀毒软件的过滤开销也算在内 |
| `work_units`、`work_base_ms` | 计算量单元数和校准耗时。单元数不同的轮次不可比，summary 会提示 |

### frames_*.csv（frame_probe，每 5 秒一行）

`frames`、`fps`、`mean/p50/p95/p99/max_ms`、`over20`（间隔 > 20ms）、`over33`、`over50`、`dropped`、`refresh_hz`、`visible`。

- `over33` 的实际阈值是 **34ms**：60Hz 下单次丢帧的间隔正好是 33.3ms，留 0.7ms 余量，避免 `over20` 和 `over33` 完全相同。也就是说 60Hz 下 `over20` ≈ 丢 1 帧以上，`over33` ≈ 连丢 2 帧以上。
- `dropped` = 间隔超过 1.5 个刷新周期（任何丢帧），高刷新率显示器上更有意义。
- `drive_app.mjs` 的 `ui_*.csv` 用同样的定义，另有 `longtask_n`、`longtask_over100_n`、`longtask_max_ms`、`longtask_total_ms`。

### summary.md

1. 每个场景的轮次、记录秒数、文件数。
2. 主表：每个指标一行，每个场景一列，格内是 `mean / p95`（record/nvidia 按秒样本，probe/frames/ui 按 5 秒窗口）。
3. 派生指标（相对 `--baseline`，默认 S0）：计算量耗时和变慢 %、计时器 p99 及增量、frame_probe >33ms 占比和每分钟次数、丢帧占比、画廊滚动时长任务/分钟和 >100ms 长任务/分钟。表头附 docs/11 §8 的验收初值。
4. 最忙的进程 Top 10（来自 meta）。
5. `--all` 时附全部数值列；备注里列出不可用的计数器、被剔除的不可见窗口等。

常用参数：`--baseline S0`、`--per-run`（每轮单独一列）、`--trim-head 15`（丢弃每轮开头 15 秒，只看稳态）、`--no-write`。

---

## 5. 已知限制与注意事项

- **本机（RTX 5070，驱动 610.88）的 `GPU Engine` 计数器里没有 Compute / Cuda 引擎**，只有 3D、Copy、VideoDecode、VideoEncode、JPEG_Decode、OFA、Security、VR，所以 `gpu_compute_pct` 一直是空的。CUDA 推理预计记在 3D 引擎上（和桌面合成同一个引擎，正好是 H1 说的争用），但这一点要在 S3 第一轮里对照验证：如果 `nv_gpu_util_pct` 很高而 `gpu_3d_pct` / `gpu_pyapp_pct` 不动，说明计数器看不到 CUDA，H1 以 nvidia-smi 为准。
- 机器上还有第二个 GPU 适配器（LUID 不同，0 显存，13 个 3D 引擎，可能是虚拟显示适配器），已按"专用显存最大的那个"自动排除。
- 进程 CPU 以 15.6ms 的时钟节拍计账，小进程的单个 1 秒样本会以约 1.56%（单核）跳变，看均值，别看单点。
- 测量本身的开销：`record.ps1` 每次采样约 12–15ms，自身 CPU 稳态约 2–3% 单核（前两秒 JIT 预热更高）；`probe.py` 的计算量探针约占 2.5% 单核。两者在 S0 里同样存在，对比时相互抵消。
- 杀毒（H3）：`HipsDaemon` 是服务进程，实时防护有一部分在内核驱动里完成，那部分 CPU 会记在发起 I/O 的进程（应用自己）或 `System` 上，不一定出现在 `hips` 组。判断 H3 要结合 `hips` 组 CPU、应用 CPU 和 probe 的 fsync 延迟一起看。
- `record.ps1` 用 PDH API 采样（`PdhAddEnglishCounter`，与 `Get-Counter` 同一套英文计数器路径，所以中文系统也能用）。没有直接循环调用 `Get-Counter`：本机实测每次调用要 1.1–3.2 秒（每次都重新展开 `Process(*)` / `GPU Engine(*)` 通配符），撑不住 1 秒一次；而 `-Continuous` 只在开始时展开一次通配符，中途启动的 AI worker、llama-server 会漏掉。现在一个 PDH 查询贯穿全程，每次采集都会重新枚举实例，进程中途出现或退出都能正确处理。
- 帧数据只在页面可见时有效：Chrome 窗口和应用窗口在各自测量的那一分钟里都要在前台。
- 用 PowerShell 变量接 `node` 的输出时，先设 `[Console]::OutputEncoding = [System.Text.Encoding]::UTF8`，否则 JSON 里的中文会乱码（`id` 之类不受影响）。

---

## 6. 无人值守：run_scenarios.ps1

手工流程（§1–§3）要人在场切窗口、滚动页面。`run_scenarios.ps1` 把同一套工具串起来，可以在电脑没人用时自己跑完（docs/11 R6 用它测的）。

```powershell
# 先按 §0.2 构建 test profile 的 release exe；应用不要开着
pwsh -NoProfile -File scripts\perf\run_scenarios.ps1 -OutDir D:\perf\r6-20261023
python scripts\perf\downgrade_report.py D:\perf\r6-20261023      # S5 降档速度
```

| 场景 | 内容 |
|------|------|
| S0 ×2 | 应用不开（基线） |
| S1 | 应用打开，没有后台任务 |
| S2F / S2A | 生成缩略图：F = 治理器"全速"覆盖（≈ 改动前不限速），A = 自动档 + 模拟有人在用（使用中档）。S2F 是第一次扫描 NAS 目录；S2A 重排一批缩略图，结束后测可见区域出图耗时 |
| S3F / S3A | 只跑 AI（图像向量 + 标签）；S3F 含 worker 冷启动 |
| S4F / S4A | 缩略图 + AI 同时 |
| S5 | 降档速度：等电脑连续 3 分钟以上没人碰、治理器升到空闲档全速后，模拟一次输入，看 CPU / GPU 多久降下来 |

和手工流程的差别：

- **"使用中"靠模拟输入**：每 5 秒发一次零位移的鼠标移动（`SendInput`，光标不动），刷新系统的"最近输入时间"。
- **有人在用电脑会被记下来**：不是自己发出的输入记进 `interference.csv`（`user_inputs` 列），这些场景的数据要打折扣。
- **缩略图负载靠重置**：应用关闭时把 test 库里一批已就绪的缩略图标回待生成，重启后由启动重排交给批量队列。只改 test profile 的库，原图只读。NAS 原图第二次读取时可能命中系统文件缓存，网络读取量会偏低，CPU 解码编码量不变。
- **frame_probe 用独立的浏览器实例**（`frame_probe_run.mjs`，独立用户数据目录，不碰日常的浏览器配置）。它和应用窗口在各自测量的那一分钟会被置顶。
- **场景时长可以缩短**：`-DurationSec 30 -Only S0,S1` 用来试运行。`-NasRoot` / `-RootLabel` 可以换成本地测试目录。

注意：

- 远程桌面会话里，浏览器和 WebView 按 RDP 虚拟显示器的刷新率出帧（本机实测 32Hz）。帧相关指标只能在场景之间相对比较，不能和本机显示器上的数字直接对比。CPU、GPU、probe、应用长任务不受影响。
- 跑完约 1 小时，期间电脑会很卡。
