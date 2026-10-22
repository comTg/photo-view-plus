# 11 · 后台任务资源治理（整机卡顿）

> 状态：方案（2026-10-22），待评审。实施按 §7 的阶段推进，一项一提交，提交信息引用条目 ID（如 `perf(R1-1): AI 只在空闲时运行`）。
> 分工：[docs/09](09-performance-optimization.md) 跟踪查询和渲染类优化；本文件解决"后台处理时整台电脑变卡"。VLM 批处理（[docs/10](10-vlm-tagging-and-search.md) A6）的启停也由这里的治理器控制。

## 0. 结论

- 卡顿不太可能来自某一个慢查询，而是几种后台重活同时满载，而现有的"降优先级"手段只覆盖了其中一部分：
  - 缩略图线程用了 Windows 后台模式；哈希和扫描线程没有用；杀毒软件的实时扫描以普通优先级运行，也不受我们控制。
  - AI 推理会占满显卡，而这块显卡同时负责桌面显示。CPU 优先级管不到显卡，桌面合成和浏览器渲染都会跟着掉帧。
  - 扫描期间前端每 1.5 秒把已加载的全部图片重拉一遍（docs/09 P1-5 还没做），应用自己也会卡。
- 方案分三步：
  1. **先测量**（R0），确认主因。
  2. **低风险止血**（R1）：AI 只在空闲时跑、加 GPU 冷却、所有后台重活都进后台模式、扫描期刷新减负。
  3. **资源治理器**（R2）：按"用户是否正在用电脑"动态调节后台并发和 GPU 占用。用户一动鼠标或键盘，2 秒内降档；离开几分钟后恢复全速；全屏或游戏时让出全部资源。
- **顺带发现一个数据问题**（§6.1）：NAS 上的 `camera` 和 `Camera` 两个目录名只差大小写。这导致约 3,786 对同名文件被重复入库，其中 2,512 张缩略图因此报"找不到文件"。建议先在 NAS 上合并这两个目录。

## 1. 本机环境与图库（2026-10-22 实测）

| 项 | 值 |
|----|----|
| CPU | AMD Ryzen 5 5600，6 核 12 线程 |
| 内存 | 32GB |
| GPU | RTX 5070 12GB，同时驱动显示器；桌面常驻显存约 1.5GB |
| 磁盘 | C:（SATA SSD，应用数据和缩略图）、D:（SATA SSD）、E:（NVMe） |
| 图库 | 只有一个根目录 `Y:\重要文件夹\唐唐手机备份`，是映射到 NAS 的网络盘（SMB） |
| 规模 | 13,960 条在库记录，87.7GB，平均每张 4.1MB、770 万像素（最大 8,900 万像素）；jpg 10,558 / heic 2,260 / png 1,040 |
| 缩略图 | 256px **无损** WebP：16,564 个文件，共 979MB，平均每个 59KB（[thumbnail_service.rs:336](../src-tauri/src/services/thumbnail_service.rs#L336)） |
| 实时防护 | 火绒（`HipsDaemon` 在运行）；Windows Defender 服务已停止 |
| 配置 | 正式版 `aiEnabled=false`；开发版 AI、OCR、人脸全开 |

## 2. 后台负载清单（现状）

| 负载 | 触发方式 | 并发 | 优先级处理 | 主要消耗 |
|------|---------|------|-----------|---------|
| 扫描（P1） | 手动；文件监听只管本地盘，NAS 不监听 | 单线程遍历 + 逐个读图头/EXIF | 普通 | NAS 目录列举、读图头 |
| 缩略图（P0） | 扫描入库后；启动时重排未完成的 | CPU 闸门 min(核数/2, 6) = **6**，读闸门网络盘 4（[thumbnail_service.rs:225](../src-tauri/src/services/thumbnail_service.rs#L225)、[read_gate.rs:16](../src-tauri/src/utils/read_gate.rs#L16)） | 线程进入后台模式（同时压低 CPU、I/O、内存优先级） | 经 SMB 读原图 + 解码 + 无损 WebP 编码 + 写小文件 |
| BLAKE3（P2） | 手动去重 | 读闸门 8 / 4 | **普通** | 读取整个文件 |
| dHash（P3） | 手动去重 | 只受全局 16 并发限制 | **普通** | 解码缩略图 |
| OCR（P4）/ 人脸（P7） | 设置开启后 | 各 1 | worker 进程"低于正常" | CPU / GPU |
| 标签（P5）/ 向量（P6） | `aiEnabled`，每 2 秒投递一次 | 各 1 批（16 / 32 张） | worker 进程"低于正常"，数学库线程 ≤ 4 | **GPU**（RAM-plus Swin-L 和 CLIP 都是 FP32、逐张推理）、显存；worker 启动时导入 torch、加载约 1.8GB 权重 |
| 前端 | 扫描期间 | 每 1.5 秒 refresh，`limit = 已加载数量`（[App.tsx:328](../src/App.tsx#L328)）；每张缩略图发一个事件（已按动画帧合批） | — | WebView 主线程、IPC 序列化 |
| 全局 | — | 调度器全局并发 16；P0 上限 6，P4–P7 各 1（[lib.rs:63](../src-tauri/src/lib.rs#L63)） | — | — |

另外，已有设置项 `aiIdleStopMinutes`、`thumbCacheGb`、`localScanConcurrency`、`networkScanConcurrency` 只写进了配置，代码里并没有用到（[config.rs:87](../src-tauri/src/config.rs#L87)）。worker 一旦启动就会一直常驻，占着显存和内存。

## 3. 卡顿成因假设

| # | 假设 | 依据 | 怎么证实（R0） |
|---|------|------|---------------|
| H1 | GPU 与桌面合成争用：AI 推理时整个桌面掉帧 | 显卡同时驱动显示器；RAM-plus 和 CLIP 用 FP32 连续推理；正式版 AI 是关闭的 | 开关 AI 对比 GPU 3D/Compute 利用率，以及浏览器帧时间探针 |
| H2 | CPU 争用：后台线程优先级低，但 6 个线程正好等于物理核数 | 同一物理核上的两个超线程互相抢执行单元；全核负载时睿频降低；L3 缓存和内存带宽争用。这些都不受优先级控制 | 固定计算量的前台基准变慢了多少 |
| H3 | 杀毒软件实时扫描 | 每张原图从 NAS 读入、每个新缩略图写出，都可能触发火绒扫描；火绒以普通优先级运行 | 场景对比时采样 `HipsDaemon` 的 CPU |
| H4 | 前端自身卡顿 | 扫描期全量 refresh 会拉上千条 30 列记录，经 IPC 序列化后整体替换数组 | 应用内长任务次数、滚动帧时间 |
| H5 | worker 启动尖峰 | 导入 torch 和 CUDA 初始化、加载 RAM-plus（1.2GB）和 CLIP | 启动阶段的 CPU、磁盘曲线 |
| H6 | 磁盘 / SMB | 写缩略图约 3MB/s，对 SSD 来说微不足道；NAS 读取走网络 | 磁盘繁忙度、网络接收速率（预计不是主因） |
| H7 | 内存 | 大 PNG 会全量解码（没有降采样），乘以并发数，瞬时可能占数百 MB | 可用内存曲线（32GB，预计不是主因） |

## 4. R0：先测量

**工具**（都放在 `scripts/perf/`，不需要额外下载）：

| 文件 | 作用 |
|------|------|
| `record.ps1` | 每秒采样一次。实现上直接调用 PDH API，原因见下方"工具实现时的发现"。<br>内容：总 CPU；各进程 CPU（`photo-view-plus`、`msedgewebview2*`、`python*`、`llama-server`、`HipsDaemon`、`dwm`）；可用内存；各物理盘繁忙度和队列长度；网络接收速率；`GPU Engine(*)` 按 3D/Compute 分类的利用率；`GPU Adapter Memory` 专用显存。同时后台运行 `nvidia-smi --query-gpu=utilization.gpu,memory.used -lms 1000`。<br>输出 CSV。进程 CPU 计数器是按单核计的，要除以 12。 |
| `probe.py` | 只用标准库，以普通优先级运行，模拟前台：<br>① 计时器延迟：循环 `sleep(5ms)`，记录超出量的 p50/p99；<br>② 固定计算量基准：每 2 秒做一次约 50ms 的哈希计算，记录耗时；<br>③ C: 盘 4KB 写入并 fsync 的延迟。 |
| `frame_probe.html` | 在 Chrome 里打开。用 `requestAnimationFrame` 统计帧间隔，自动滚动一个长页面，按 5 秒窗口记录超过 20ms / 33ms 的帧数。测的是"其他应用感受到的卡"，能反映 GPU 争用。 |
| 应用内探针 | WebView 里用 `PerformanceObserver('longtask')` 统计长任务，再加一个帧时间采样器。实现上由 `drive_app.mjs` 通过调试端口注入（`ui-stats` / `scroll-gallery`），不改 `src/`。 |

**场景**：每个场景 3 分钟，重复 2 次；`record` 和 `probe` 全程开着；每个场景里手动滚动 `frame_probe` 页面 60 秒、滚动应用画廊 60 秒。

| 场景 | 内容 |
|------|------|
| S0 | 基线：应用不开 |
| S1 | 应用打开，没有后台任务 |
| S2 | 只生成缩略图：dev profile 新增一个根目录，指向 NAS 上约 2,000 张照片的子目录，缩略图缓存为空 |
| S3 | 只跑 AI：dev 库副本里把 2,000 张已有缩略图的图标成待打标签、待向量化 |
| S4 | S2 + S3 同时跑 |

**产出**：附录 A 填一张对比表（各场景的 CPU、各进程 CPU、GPU 利用率、显存、磁盘、网络、探针 p99、基准变慢百分比、超过 33ms 的帧数每分钟、应用长任务每分钟），据此给 H1–H7 排序，决定 R1 做哪些、按什么顺序做。

**注意**：
- 测量期间电脑会卡，需要你在场配合，大约 30–40 分钟。
- S3 要改 dev 库的状态字段，只动 dev profile，执行前再确认一次。

**工具实现时的发现**（2026-10-22，详见 [scripts/perf/README.md](../scripts/perf/README.md)）：
- **数据目录在编译时就定死了。** 单设 `PVP_PROFILE=test` 隔离不了数据：现有 release exe 用的仍是正式版的 identifier，只有日志目录和窗口标题会变。测量用的 exe 必须这样构建：`pnpm tauri build --no-bundle --config src-tauri/tauri.test.json`。启动后先在日志里确认 `opening database` 的路径含 `.test`。
- 全新的 test profile 默认开着 AI，跑 S2 之前要先关掉。
- `Get-Counter` 在本机每次调用要 1–3 秒，撑不住每秒采样。`record.ps1` 改为直接调用 PDH API，计数器路径仍是同一套英文路径，单次采样约 15ms。
- 本机 RTX 5070 的 `GPU Engine` 计数器里没有 Compute 类引擎，CUDA 负载预计记在 3D 引擎上，S3 要用 nvidia-smi 对照确认。
- 机器上有很多别的程序的 `msedgewebview2` 和 `python` 进程，所以按父子进程关系另加了只统计本应用的进程组：`apptree`、`wv2app`、`pyapp`。
- 应用内探针改由 `drive_app.mjs` 通过 WebView2 调试端口注入，统计长任务和帧时间，不改 `src/`。
- 需要 4 个没扫描过的 NAS 子目录，各约 2,000 张，避开 `camera` 目录。复用已经读过的目录，网络数据不可比。

## 5. 治理方案（R2 的核心）

### 5.1 负载模式

| 模式 | 判定 | 含义 |
|------|------|------|
| 使用中（Active） | 最近 20 秒内有键盘或鼠标输入（`GetLastInputInfo`） | 用户在用电脑，不一定在用本应用 |
| 短暂离开（Away） | 20 秒到 3 分钟没有输入 | — |
| 空闲（Idle） | 3 分钟以上没有输入，或者锁屏 | 可以全速 |
| 全屏 / 游戏（Busy） | `SHQueryUserNotificationState` 返回 `QUNS_BUSY`、`QUNS_RUNNING_D3D_FULL_SCREEN` 或 `QUNS_PRESENTATION_MODE` | 优先级最高，压过其他所有模式（docs/07 §9.3 的"游戏模式"） |

- **切换规则**：降档立即生效，每 500ms 检查一次输入时间（这是读本机 API，不是前端轮询）；升档要求条件持续满足 10 秒，避免抖动。
- **手动覆盖**：
  - "全速"：忽略使用中和短暂离开两档，但仍然服从全屏/游戏档。
  - "暂停"：1 小时 / 直到重启。暂停时只保留可见区域的缩略图生成。
- 模式变化时发 `system:load_mode` 事件，前端订阅即可，不轮询。

### 5.2 各档预算（初值，按 R0 结果调整）

| 资源 | 使用中 | 短暂离开 | 空闲 | 全屏 / 游戏 |
|------|--------|---------|------|------------|
| 可见区域缩略图（§5.4） | 2 | 2 | 2 | 1 |
| 批量缩略图 | 1 | 3 | 6 | 0 |
| 网络盘读原图（红线上限 4） | ≤ 3 | ≤ 4 | ≤ 4 | ≤ 1 |
| BLAKE3 / dHash | 1 | 2 | 不限（受读闸门约束） | 0 |
| torch worker 的 GPU 任务（向量、标签、OCR、人脸） | 暂停（只响应搜索编码） | 占空比 ≤ 30% | 不限 | 暂停 |
| VLM（`llama-server`） | 停止投递；已加载的 5 分钟后卸载 | 同左 | `-np 4` | 立即卸载 |
| worker 数学库线程 | 1 | 2 | 4 | 1 |

### 5.3 执行手段

- **可调的并发许可**：新增 `utils/limiter.rs`（`AdjustableLimiter`），把缩略图 CPU 闸门、读闸门和调度器的单优先级上限都换成它。
  - 增加名额：直接补许可。
  - 减少名额：先占住多余的许可再丢弃。
  - 正在运行的任务不打断，只影响新任务。
- **GPU 占空比**：Rust 侧每跑完一批 AI，按 `冷却时间 = 本批耗时 × (1/占空比 − 1)` 等待后再投下一批。worker 推理改成 FP16，缩短单批时长。worker 新增 `/runtime/throttle` 接口，运行时调用 `torch.set_num_threads`。
- **优先级**：
  - 把 `BackgroundPriorityGuard` 抽到 `utils/priority.rs`，覆盖 BLAKE3、dHash 和扫描时读图头的部分。
  - 扫描写数据库的事务不要放在后台模式里：后台模式会压低 I/O 优先级，拉长持有写锁的时间，反而拖住缩略图线程（它们的 `busy_timeout` 是 5 秒）。
  - worker 启动时用 `PROCESS_MODE_BACKGROUND_BEGIN` 把自己设成后台模式（这个只能由进程自己调用）。
  - `llama-server` 用 `BELOW_NORMAL_PRIORITY_CLASS` 加隐藏窗口启动。
  - 可选实验：对子进程开启 EcoQoS（`SetProcessInformation` 的 `ProcessPowerThrottling`）。Ryzen 5600 没有能效核，预计收益有限。
- **实现位置**：
  - `services/resource_governor.rs`：模式状态机、预算表、下发许可。
  - `utils/win_activity.rs`：`GetLastInputInfo` 和 `SHQueryUserNotificationState`，需要给 `windows-sys` 增加 `Win32_UI_Input_KeyboardAndMouse` 和 `Win32_UI_Shell` 两个 feature。非 Windows 平台用桩实现。

### 5.4 可见区域优先的缩略图

批量生成被限速之后，用户正在看的那一屏不能等。
- 前端在 `VirtuosoGrid` 的 `rangeChanged` 回调里，把可见范围内还没有缩略图的 id 收集起来，防抖 200ms 后调用新命令 `thumbs_prioritize(ids)`。
- 后端把这些图放进独立的"可见队列"，有固定并发，不占批量名额；用正在处理的 id 集合去重，避免同一张图生成两次。

### 5.5 界面

- **底部状态栏**：显示"后台处理：自动（使用中，已降速）/ 全速 / 已暂停"，点开可以切换手动覆盖。
- **设置页**：空闲判定阈值；"使用电脑时也运行 AI"（默认关）；"大模型只在空闲时运行"（默认开）。

## 6. 单项优化（不依赖治理器，可以并行做）

### 6.1 NAS 目录大小写冲突（数据问题，建议最先处理）

- **现象**：`Y:\重要文件夹\唐唐手机备份\PD2324\PD2324相册备份\` 下同时存在 `camera`（2026-06-18）和 `Camera`（2026-10-21）两个目录。NAS 的文件系统区分大小写，Windows 通过 SMB 访问时不区分，所以这两个目录下的同名路径只能落到其中一个目录。
- **影响**（正式库只读统计）：
  - 13,960 条在库记录，忽略大小写后只有 10,174 个不同路径，约 3,786 对是 `camera`/`Camera` 下的同名文件，很可能是同一批照片被备份了两次。
  - 共 2,907 张缩略图报"系统找不到指定的文件"。其中 2,512 张在这两个目录下（`camera` 1,962 张、`Camera` 550 张）；抽查了 3 个 `camera\…HEIC` 路径，当前都打不开。另外 395 张在微信目录，多半是文件已经被清理，重新扫描后会被标记删除。
  - 每次读取失败都会白占一次 NAS 往返，而这些图也进不了 AI 流水线。
- **建议**：
  1. 由你在 NAS 上（网页管理界面或 SSH）把两个目录合并成一个，然后在本应用里重新扫描。打不开的记录会被标记为已删除。
  2. 代码侧（R1-6）：扫描时检测同一层里只差大小写的目录名，在扫描结果里告警，提示用户去 NAS 上处理。

### 6.2 缩略图改有损编码（R4）

- 无损 WebP 对照片很不划算。改成 JPEG 质量 80–85：可以用 `image` crate 自带的 `JpegEncoder`，不需要新的原生依赖；如果编码速度不够，再换 `jpeg-encoder` crate。
- 预计体积缩小 4–5 倍（979MB 降到约 200–250MB），写盘量、WebView 解码和缩略图协议的读取量都会随之减少。R4 先抽 200 张实测。
- **迁移**：
  - migration 给 `images` 加 `thumb_ext` 列，默认 `'webp'`。新生成的缩略图写 `'jpg'`。
  - 缩略图协议按 `thumb_ext` 设置 Content-Type；`thumb_path()` 增加扩展名参数。
  - 旧缩略图继续可用；空闲模式下可以后台把它们转成 JPEG（只读本地缓存，不读 NAS），转完删掉旧的 `.webp` 文件。缓存是应用自己生成的文件，不是用户照片。
  - 带透明通道的 PNG 截图，铺到白底上再编码。
- **回归**：
  - dHash 从新缩略图计算后数值会有微小变化，要重跑视觉去重的阈值测试（`dedup_tests` 里的 F1 ≥ 0.92 用例）。
  - AI worker 用 PIL 读 JPEG 没有问题。

### 6.3 后台模式覆盖所有重活（R1-3）

见 §5.3。哈希、dHash、扫描读图头目前都以普通优先级运行。

### 6.4 AI worker（R1-2、R1-4）

- **FP16 推理**（R1-2）：CLIP 和 RAM-plus 都改成 `half()` 或 autocast。用 50 张图对比，前 10 个标签的重合率要 ≥ 90%，同时记录吞吐变化。
- **空闲自动停止**（R1-4）：实现 `aiIdleStopMinutes`，没有 AI 任务也没有搜索满 N 分钟后停掉 worker，释放显存和内存。
- **作业对象**（R1-4）：把 worker（以及以后的 `llama-server`）放进带"句柄关闭即结束全部子进程"标志的 Windows 作业对象，参考 memora 的 `winproc.py`。现在只靠 `kill_on_drop`，主进程一旦崩溃，子进程会残留，继续占着显存。
- VLM 接管打标以后，RAM-plus 不再加载（docs/10 A7）。

### 6.5 前端（R1-5、R5）

- **R1-5**（即 docs/09 P1-5）：扫描期间的自动刷新只在用户停留在第一屏时进行，而且只拉第一页再合并。用户已经往下翻了，就改为在顶部提示"有 N 张新照片，点击刷新"，不再整体重拉。
- **R5**：后端把缩略图完成事件每 250ms 合并成一个 `thumbnails:updated`，只带窄字段（id、状态、hash、宽高、错误信息）；完成 docs/09 P0-3 剩余的部分，把进度类事件的订阅下沉到小组件里。

### 6.6 杀毒软件（仅当 R0 证实 H3 时）

如果火绒在缩略图生成期间 CPU 占用明显，你可以考虑把 `%LOCALAPPDATA%\com.vetoer.photoviewplus\thumbs` 加入火绒的信任区。这是安全设置，由你自己决定；应用只在文档和设置页里给出说明，不会自动修改。

### 6.7 其他（记录在案，暂不排期）

- 缩略图缓存里有 16,564 个文件，但在库且状态为 ready 的只有 9,456 张。约 7,000 个孤儿文件来自已删除的图，而 `thumbCacheGb` 的清理逻辑还没实现。
- HEIC：1,503 张因为不支持而没有缩略图。docs/07 §2 选定的 `libheif-rs` 方案还没落地。VLM 打标在 worker 里用 pillow-heif 读原图，能覆盖这部分（docs/10 §4），但画廊里的缩略图仍然缺失。

## 7. 实施阶段与进度

**状态图例**：⬜ 待办 · 🔄 进行中 · ✅ 已完成 · ⏭️ 已跳过（附原因）

| ID | 内容 | 状态 |
|----|------|------|
| R0 | 测量工具 + S0–S4 基线 + 附录 A。工具已就绪，冒烟测试通过（`scripts/perf/`）；S0–S4 实测要约时间进行 | 🔄 |
| R1-1 | AI 只在空闲时运行（先用 `GetLastInputInfo` ≥ 3 分钟做简单判断；设置"使用电脑时也运行 AI"默认关）+ 批次间 GPU 冷却 | ✅ 由 R2 覆盖 |
| R1-2 | worker FP16 推理 | 🔄 Chinese-CLIP、bge 已是 FP16（docs/10 A5、A4）；RAM-plus 随 docs/10 A7 下线，不再单独改 |
| R1-3 | `BackgroundPriorityGuard` 抽到 utils，覆盖哈希、dHash 和扫描读图头 | ✅ |
| R1-4 | 实现 `aiIdleStopMinutes`；worker 放进作业对象 | ✅ |
| R1-5 | 扫描期刷新减负（docs/09 P1-5） | ⬜ |
| R1-6 | 扫描时检测只差大小写的同级目录并告警 | ⬜ |
| R1-7 | （条件项）若 R0 显示 CPU 是主因，先把缩略图默认并发从 6 降到 4 | ⬜ |
| R2 | 资源治理器：负载模式、可调许可、预算表、GPU 占空比、`system:load_mode` 事件、状态栏与手动覆盖 | ✅ 前后对比待 R0 / R6 实测 |
| R3 | 可见区域优先的缩略图 | ✅ |
| R4 | 缩略图改有损编码 + 迁移 + dHash 回归 | ⬜ |
| R5 | 事件合批 + docs/09 P0-3 剩余部分 | ⬜ |
| R6 | 回归测量（与 R0 同场景）+ 写结论 | ⬜ |

**每项的验证**：
- 改 Rust：`cargo clippy --all-targets -- -D warnings && cargo test`。
- 改前端：`pnpm lint && pnpm typecheck && pnpm test`。
- R1 和 R2 的每一项都要重跑对应的 R0 场景（S2/S3/S4），把前后对比写进附录 A。
- R2 的单元测试：模式状态机用注入的假时钟和假输入源，覆盖降档立即生效、升档迟滞、全屏优先、手动覆盖；许可容量调整后，新任务遵守新上限。

### 7.1 实施记录

**R1-3**：`utils/priority.rs` 的 `BackgroundPriorityGuard`（`THREAD_MODE_BACKGROUND_BEGIN/END`）覆盖缩略图生成、BLAKE3、dHash 和扫描读图头（`probe_image`）。扫描写库的事务不在后台模式里（§5.3）。

**R1-4**：
- `aiIdleStopMinutes`（默认 10）：supervisor 记录最后一次使用 worker 的时间（任何 worker 请求都算，包括搜索编码），健康检查循环里发现空闲超时就停掉 worker；设置修改立即生效。下次搜索或有待处理任务时按需重启。
- 作业对象：`utils/job_object.rs`，带 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，worker 启动后立即绑定；以后的 `llama-server` 也绑这个作业。

**R2**：
- `services/resource_governor.rs`：`classify`（一次采样 → 模式）、`ModeMachine`（降档立即、升档持续 10 秒）、`budget_for`（§5.2 预算表）、`effective_tier`（模式 + 手动覆盖）、`GpuGate`（暂停 / 一次一批 / 占空比冷却）、`ResourceGovernor`（每 500ms 采样，预算变化时调用下发回调，发 `system:load_mode`）。纯逻辑部分时间由调用方传入。
- `utils/win_activity.rs`：`GetLastInputInfo` + `SHQueryUserNotificationState`（`QUNS_BUSY` / `QUNS_RUNNING_D3D_FULL_SCREEN` / `QUNS_PRESENTATION_MODE` → 全屏档，`QUNS_NOT_PRESENT` → 空闲）。非 Windows 用桩实现。
- `utils/limiter.rs`：`AdjustableLimiter` 替换缩略图 CPU 闸门和读闸门；调度器的单优先级上限改成原子量，可运行时调整。
- 下发点（`lib.rs` 的 `budget_applier`）：批量缩略图（CPU 闸门 + P0 上限）、网络盘读闸门（夹在 1–4，红线）、P2/P3（BLAKE3 / dHash）上限、worker 数学库线程数（新接口 `POST /runtime/throttle`，worker 运行时 `torch.set_num_threads`；启动时也会应用当前档位）。
- AI 后台任务（图像向量、描述向量、RAM 标签、OCR、人脸）统一先过 supervisor 的 `GpuGate`；GPU 暂停时 AI 流水线不投递新批次、也不唤醒 worker。搜索编码不经过闸门。
- 命令 `governor_status`、`governor_set_override`（`auto` / `fullSpeed` / `pause1h` / `pauseUntilRestart`）；底部状态栏组件 `LoadModeControl` 自己订阅 `system:load_mode`。
- 设置项：`idleThresholdMinutes`（默认 3）、`aiWhileActive`（默认关）、`vlmIdleOnly`（默认开），修改后立即生效。
- 测试：`tests/governor_tests.rs` 20 个，覆盖假时钟 / 假输入源下的降档、升档迟滞、全屏优先、手动覆盖与到期、预算表、GPU 闸门、许可和调度器上限调整、网络盘红线，以及真实系统上的输入探测和作业对象绑定。
- **与方案的差异**：
  - worker 没有用 `PROCESS_MODE_BACKGROUND_BEGIN`：同一个 worker 也负责搜索时的查询编码，后台模式会把它的 I/O 和内存优先级压到很低，冷启动搜索会明显变慢；保留现有的 `BELOW_NORMAL_PRIORITY_CLASS`，再加上 GPU 闸门和线程数调节。
  - 用户手动触发的批量任务（"处理待办"、OCR、人脸）同样受 GPU 闸门约束：使用中档会等到离开或空闲才跑。状态栏能看到当前档位，需要立刻处理时可以切"全速"。
- **待做**：R0 的 S2–S4 场景前后对比（需要约时间实测），结果写进附录 A。

**R3**：
- 前端 `ImageGrid` 用 `VirtuosoGrid` 的 `rangeChanged`，范围或列表内容变化后防抖 200ms，把可见范围里 `thumbStatus = pending` 的 id 发给 `thumbs_prioritize`（列表没变不重复发）。
- 后端 `thumbnail_service::prioritize_visible`：独立的可见队列，固定并发 2（全屏档 1），不经过调度器、不占批量名额，以正常优先级生成（批量队列仍是后台模式）；每次调用替换"可见区域"，排队期间滚出可见区域的图不再生成，留给批量队列。
- 去重：两条队列共用"正在生成"集合；批量任务拿到 CPU 名额后先确认图仍是 pending，已被可见队列生成的直接跳过，不重复读原图。
- 手动"重新生成缩略图"也走可见队列，不排在批量任务后面，暂停时也能用。
- 预算表回到方案原值：全屏 / 暂停档批量缩略图 0；暂停档可见队列 2、网络盘读 2（只够可见区域用）。
- 测试：`thumbnail_tests::visible_queue_generates_ahead_of_batch_and_dedupes`；`governor_tests::budget_table_matches_docs` 覆盖新的预算。

## 8. 验收指标（初值，R0 之后修订）

**S4 负载、"使用中"模式下**，与 S0 基线相比：
- 计时器延迟 p99 增加不超过 5ms。
- 固定计算量基准变慢不超过 15%。
- `frame_probe` 里超过 33ms 的帧不超过 1%。
- 滚动应用画廊时，超过 100ms 的长任务每分钟不超过 1 次。
- 可见区域的缩略图 2 秒内出图。

**降档速度**：从第一次输入算起，CPU 类任务 ≤ 2 秒内降档，torch worker ≤ 3 秒内停止占用 GPU，VLM ≤ 10 秒内停止（在途请求自然完成）。

**空闲模式吞吐**：缩略图和 AI 的吞吐不低于改动前的 90%。

## 9. 风险

| 风险 | 对策 |
|------|------|
| 窗口化看视频、不碰鼠标键盘时会被判为空闲，后台全速可能让视频卡顿 | 全屏播放能被全屏/游戏档识别；窗口化播放时用户可以点"暂停 1 小时" |
| 手柄输入可能不更新 `GetLastInputInfo` | 游戏通常是全屏，会走全屏/游戏档 |
| 限速后，首次导入大库会变慢 | 可见区域优先保证浏览体验；空闲时全速补回进度；提供"全速"按钮 |
| 动态调许可时状态不一致 | 只影响新任务，不打断在途任务；状态机单元测试覆盖 |

## 附录 A · 测量记录

_（R0 完成后填写：场景 × 指标表、主因排序、R1 取舍）_

**冒烟测试**（2026-10-22，S0，30 秒，没有额外负载；只用来确认工具可用，不作为正式基线）：

| 指标 | 均值 / p95 |
|------|-----------|
| 总 CPU | 7.3% / 12.4% |
| GPU 利用率（nvidia-smi） | 0.4% / 1.6% |
| 显存占用 | 约 1.2GB |
| 计时器超出量 p99 | 0.73 / 0.80ms |
| 固定计算量 | 50.7 / 51.6ms |
| C: 盘 4KB 写入并 fsync | 1.16 / 1.31ms |
| 火绒（所有进程合计） | 0.21% / 0.46% |
