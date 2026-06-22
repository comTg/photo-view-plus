# 08 · 路线图

> 项目从 0 → 1.0 的整体规划。本文档随开发推进**持续更新**：里程碑勾选、风险关闭、ADR 追加。

## 1. 里程碑

| 里程碑 | 范围 | 发布判据 | 状态 | 计划周 |
|--------|------|---------|------|--------|
| **M0** | 文档与规则（本批 10 份） | 全部 docs 写完并审过 | 进行中 | 2026-W24 |
| **M1** | MVP1 基础浏览 | `docs/03` § 0 全部验收过 | 待验收 | W24-W26（约 2-3 周） |
| **M2** | MVP2 去重 | `docs/04` § 0 全部验收过 | 待开始 | W27-W28（约 2 周） |
| **M3** | MVP3 AI 与搜索 | `docs/05` § 0 全部验收过 | 待开始 | W29-W33（约 4-5 周） |
| **M4** | MVP4 高级管理 | `docs/06` § 0 全部验收过 | 待验收 | W34-W37（约 3-4 周） |
| **M5** | 1.0 RC | 性能基准达标、安装包签名、用户文档 | 待开始 | W38 |

> 周数为 ISO 周；时间预估为单人全职粗估，按"全职 1 人 + 没大坑"算。AI 阶段坑多，留缓冲。

## 2. 任务进度（速查）

每个 MVP 的具体任务清单见对应文档，本节只勾里程碑级状态。

### M0 文档
- [ ] CLAUDE.md
- [ ] README.md
- [ ] docs/00-architecture.md
- [ ] docs/01-data-model.md
- [ ] docs/02-ui-design.md
- [ ] docs/03-mvp1-browse.md
- [ ] docs/04-mvp2-dedup.md
- [ ] docs/05-mvp3-ai-search.md
- [ ] docs/06-mvp4-advanced.md
- [ ] docs/07-windows-platform.md
- [ ] docs/08-roadmap.md（本文档）

> 完成后由用户复查，全部签字才进入 M1。

### M1 MVP1（详见 `docs/03` § 2）
- [x] T1-T16 实现闭环已完成（队列 / 扫描 / EXIF / 缩略图 / 查询 / 三栏浏览 / 筛选 / 多选 / 详情 / 设置）
- [ ] T16 大样本性能验收待用户用真实图库确认（5 万图扫描、滚动 FPS、内存峰值）

### M2 MVP2（详见 `docs/04` § 2）
- [x] T1 migration 0002（blake3 / phash / dhash / hash_status + duplicate_groups / duplicate_items）
- [x] T2 BLAKE3 哈希服务（P2，流式 4MB 块，失败标 `hash_status='failed'`）
- [x] T3 dHash 视觉哈希服务（P3，从缩略图缓存读，czkawka 默认参数）
- [x] T4 BK-tree 索引（启动加载常驻 state，增量插入）
- [x] T5 exact 分组（blake3 GROUP BY）
- [x] T6 视觉近似分组（BK-tree + union-find，阈值 czkawka High=2）
- [x] T7 dedup 命令层（start/status/groups/group_detail/resolve/export_csv）
- [x] T8 回收站服务（trash crate；恢复 Windows/Linux 走 os_limited，macOS 退回 DB 标记）
- [x] T9 去重对比 UI（分组列表 + A/B 对比 + keep 多选 + 行动按钮）
- [x] T10 批量保留：通过 keep_image_ids 任意选择保留集合（最大分辨率等具体策略由前端预填，本次未做策略下拉，留 1.0 前补）
- [x] T11 撤销 UI（撤销历史抽屉 + 单条撤销）
- [x] T12 CSV 导出（UTF-8 BOM）
- [x] T13 性能基础：哈希任务低优先级（P2/P3），不阻塞 P0 缩略图与 P1 扫描
- [ ] T14 端到端验收：macOS 上跑完单测 + 集成测试，Windows 实机回收站撤销路径需用户验收

### M3 MVP3（详见 `docs/05` § 2）
- [x] T1 migration 0003（tag_status / embedding_status + tags / image_tags）
- [x] T2 AI Worker 脚手架（FastAPI `/health`、`/ready`、`/diagnostics`、`/shutdown`；`uv.lock` 已生成）
- [x] T3 进程托管底座（Rust supervisor 按需启动 / 停止 / 状态事件 / 健康检查 / 3 次每分钟重启上限）
- [x] T4 CUDA / 模型可用性自检（RTX 5070 + CUDA 12.8 + torch 2.11.0+cu128 已确认）
- [x] T5 模型下载管理（worker `/models/status` / `/models/download` / `/models/cancel` + 设置页下载入口）
- [x] T6 CLIP 图片向量化（open_clip ViT-B-32，CUDA 可用时 GPU；无模型依赖时 fallback）
- [x] T7 LanceDB 写入与查询（`lancedb` Rust crate，集成测试覆盖 upsert / get / top_k）
- [x] T8 主进程拉取流水线（P5 标签 / P6 embedding 后台队列，尊重 AI 开关）
- [x] T9 自然语言搜图（CLIP text embedding + LanceDB top-k + SQLite join）
- [x] T10 以图搜图（复用图片 embedding，缺失时临时 embed）
- [x] T11 自动标签（CLIP zero-shot + 视觉 fallback；RAM++ 权重下载入口保留）
- [x] T12 标签筛选与标签云（左侧标签、AI 面板标签云）
- [x] T13 搜索 DSL 子集（`#tag`、`format:jpg`、`size:>5MB` / `size:<20MB`）
- [x] T14 AI 状态面板（Worker、CUDA、队列、模型状态）
- [x] T15 AI Worker 容错（超时、健康检查、重启上限、degraded 状态）
- [ ] T16 大样本端到端验收待用户用真实图库手动验证（当前 smoke 见 `tests/ai_bench.md`）

### M4 MVP4（详见 `docs/06` § 2）
- [x] T1 migration 0004 / 0005（OCR / face / FTS5 / smart_albums）
- [x] T2 OCR 服务（worker `/ocr/run`，RapidOCR 可选依赖，P4 队列，默认隐私关闭）
- [x] T3 全文索引（SQLite FTS5，filename + ocr_text，同步触发器）
- [x] T4 人脸检测（worker `/face/detect`，InsightFace 可选依赖，P7 队列，默认隐私关闭）
- [x] T5 人脸 embedding + 聚类（LanceDB `face_embeddings` + Rust cosine connected components）
- [x] T6 人脸 UI（人物聚类列表、命名、按 cluster 查看图片）
- [x] T7 时间轴视图（按年月分桶，样本图墙）
- [x] T8 地图视图（Leaflet + OpenStreetMap，GPS 图片点）
- [x] T9 智能相册（保存 / 列表 / 删除 / 应用当前筛选 JSON）
- [x] T10 文件监听（notify，本地 root 递归监听；网络盘跳过）
- [x] T11 隐私设置面板（OCR / 人脸默认 OFF，GPS / watcher 可控）
- [x] T12 稳定性基础（备份导出 zip；导入到运行中安全的暂存目录）
- [ ] T13 端到端验收待用户用真实 Windows + 模型环境验证

### M5 1.0 RC
- [ ] 性能基准（见 § 4）全部达标
- [ ] 代码签名证书购买并签名
- [ ] 用户手册（替换 README placeholder）
- [ ] 已知问题清单
- [ ] 安装包 MSI 在干净 Windows 11 上完整跑通

## 3. 风险表

按概率 × 影响排序。每条 owner = "项目负责人"（即用户）。

| 风险 | 概率 | 影响 | 缓解 | 状态 |
|------|------|------|------|------|
| RTX 5070 sm_120 在 PyTorch stable 不支持 | 中 | 高 | 用 nightly / 临时回退 sm_89；监控 PyTorch 2.6 发布 | 待观察 |
| HEIC 解码集成（libheif）卡壳 | 中 | 中 | MVP1 内可标 `unsupported` 暂留；libheif PoC 提前在 W23 跑 | 待 PoC |
| 100 万图扫描性能不达标 | 中 | 高 | MVP1 完成后用 fixture 集做基准；不达标时拆分扫描器 | 未测 |
| 网络盘哈希耗带宽 | 高 | 中 | 默认跳过 NAS 哈希；设置中允许打开 | 已规划 |
| 人脸聚类精度不稳 | 高 | 低 | 可手动合并/拆分；不影响主流程 | 已规划 |
| AI Worker 内存泄漏（长跑后 OOM） | 中 | 中 | 闲置自动重启；运行 24h+ 看 RSS | 未测 |
| Windows 杀软误报 | 中 | 中 | 申请代码签名证书；放白名单文档 | 待 1.0 前 |
| LanceDB 在 Windows 上行为差异 | 低 | 中 | 早期 PoC 验证；备选 sqlite-vec | 未测 |
| SQLite WAL 文件膨胀 | 中 | 低 | 定期 wal_checkpoint | 已规划 |
| 用户误删图未察觉 | 中 | 高 | 全走回收站；30 天 undo；导出报告 | 已规划 |

每条风险关闭时改为 ✅ + 关闭日期。

## 4. 性能基准

测试机假设：Intel i7-13700K + RTX 5070 + 32GB DDR5 + NVMe SSD。  
基准数据集：5 万张混合（JPG/PNG/HEIC），平均 4MB / 张。

| 指标 | MVP1 目标 | MVP2 目标 | MVP3 目标 | 实际 |
|------|-----------|-----------|-----------|------|
| 首次扫描 5 万图 | < 60s | - | - | - |
| 二次扫描（无变化） | < 5s | - | - | - |
| 缩略图 1 万张 | < 90s | - | - | - |
| 主页瀑布流首屏 | < 1.5s | - | - | - |
| 瀑布流滚动 FPS | ≥ 55 | ≥ 55 | ≥ 55 | - |
| BLAKE3 1000 张 | - | < 60s | - | - |
| pHash 1 万张 | - | < 120s | - | - |
| pHash top-100 检索 | - | < 200ms | - | - |
| 完全重复检出率 | - | 100% | - | - |
| 视觉近似 F1（fixture） | - | ≥ 0.92 | - | - |
| CLIP 1000 张 embed | - | - | < 30s | - |
| 文本搜 top-20 命中率 | - | - | ≥ 0.85 | - |
| LanceDB 10 万向量查询 | - | - | < 100ms | - |
| 内存峰值（5 万图 + AI 关） | < 800MB | < 1GB | - | - |
| 内存峰值（10 万图 + AI 开） | - | - | < 4GB | - |

完成对应 MVP 时在"实际"列填数据。

## 5. ADR（架构决策记录）

> Architecture Decision Records，做一条改动就追加。格式：编号 / 标题 / 状态 / 时间 / 上下文 / 决定 / 后果。

### ADR-001 · 选 Tauri 2 而非 Electron
- 状态：已决（M0）
- 时间：2026-06-15
- 上下文：用户对前两者均不陌生，需在内存/体积与生态成熟度间权衡
- 决定：Tauri 2 + Rust + Vite + React 19（最初定 Next.js 15 SSG，MVP1 T1 实施时改为 Vite——桌面 SPA 不需要 SSR/SSG，Vite 启动快、依赖轻）
- 后果：开发节奏略慢于 Electron；运行时优势明显；与同栈的 smart-search 共享经验

### ADR-002 · 选 LanceDB 而非 sqlite-vec
- 状态：已决（M0）
- 时间：2026-06-15
- 上下文：50 万向量后查询性能差异显著
- 决定：LanceDB
- 后果：多一个本地数据存储；备份/恢复需要包含两个目录

### ADR-003 · Python AI Worker 独立子进程
- 状态：已决（M0）
- 时间：2026-06-15
- 上下文：Rust 直接调 ONNX Runtime 也能跑 CLIP，但模型生态在 Python 侧成熟
- 决定：Python FastAPI worker；通过 127.0.0.1 HTTP 通信
- 后果：多一个进程；多了进程托管复杂度；获得 Python 生态的全部 AI 模型

### ADR-004 · 删除一律走回收站
- 状态：已决（M0）
- 时间：2026-06-15
- 上下文：本地图库删错代价高
- 决定：禁用永久删除；保留 30 天 undo 窗口
- 后果：回收站可能满；增加 undo_log 表与撤销 UI

### ADR-005 · 用 `image_hasher` crate，借鉴 czkawka 但不依赖 czkawka_core
- 状态：已决（M0）
- 时间：2026-06-16
- 上下文：MVP2 需要 pHash / dHash 实现。候选：(a) 用 `rustdct` 自实现；(b) 直接 depend on `czkawka_core`（多媒体去重工具，11.0.1, MIT, 2026-02 仍维护）；(c) 单独用 czkawka 依赖的 `image_hasher` crate
- 决定：选 (c)。`image_hasher` 是 czkawka 的算法核心、独立发布在 crates.io，自带 pHash/dHash/aHash/Wavelet/Blockhash，仅依赖 `image` + `transpose`，无额外负担。在 `docs/04-mvp2-dedup.md` T3 直接调用
- 不选 (a) 的理由：DCT 是数学题不是工程题，自实现没有任何业务价值；`image_hasher` 在 czkawka 上有百万级图库实战验证
- 不选 (b) 的理由：
  - czkawka_core 还带音频（symphonia/lofty）、PDF（lopdf）、视频（similario_core）、压缩档（zip/7z/tar）等我们一律用不到的模块，二进制 +10-30MB，编译时间显著拉长
  - 它的 API 是"自带扫描器 + 输出报告"流程，假设你给它一组目录，并发模型自己说了算，无法插入我们的优先级队列（P0 缩略图 / P2 BLAKE3 / P3 pHash）
  - `FileEntry` 与我们的 `images.id` 表错位，要写一层 adapter，得不偿失
- 借鉴方式：把 czkawka 当**参考实现**，从 `czkawka_core/src/tools/similar_images.rs` 学习阈值默认值、多 hash size 取舍、特殊格式（RAW/JXL/动图）跳过策略。MIT 允许直接抄代码（保留 license 头），但具体逻辑短，我们重写更清爽——抄思路不抄实现
- 后果：MVP2 T3 工作量从"写 DCT + 测对齐 czkawka"减到"配 `HasherConfig` + 喂缩略图"；默认参数与 czkawka 一致（**dHash/Gradient, hash_size=8, Lanczos3, 不启用 DCT**），可以直接对照它的用户反馈调优。具体阈值表见 `docs/04` T3 速查（czkawka 的 `SIMILAR_VALUES` 完整照搬）

### ADR-006 · MVP1 完成总结与遗留事项
- 状态：待验收（M1）
- 时间：2026-06-16
- 上下文：MVP1 T3 已完成 roots 管理，本次继续实现 T4-T16 的基础浏览闭环。
- 决定：
  - Rust 端新增纯业务任务队列，扫描任务为 P1，缩略图任务为 P0；队列状态由启动层转发为 Tauri event，队列模块本身不依赖 Tauri runtime。
  - 扫描采用 `walkdir` 递归发现 MVP1 图片格式，写 `images` 表，二次扫描按 `mtime + size` 增量更新，未再出现的文件标记 `deleted_at`。
  - EXIF 读取使用 `kamadak-exif`，失败不阻塞扫描；缩略图使用 `image` crate 生成 256px WebP，并通过 `thumb` 自定义协议给前端读取。
  - 前端在 pnpm 依赖策略临时阻止新增 UI 包的情况下，用 React + CSS 完成三栏、网格、筛选、多选、详情和设置；暂未引入 `react-virtuoso`。
- 验证：
  - `cargo test` / `cargo clippy --all-targets -- -D warnings` / `cargo fmt --check` 通过。
  - `pnpm lint` / `pnpm typecheck` / `pnpm test` / `pnpm build` 通过。
  - Browser 烟测：1280px 三栏布局正常；900px 断点右侧详情隐藏，主区不溢出。
- 遗留：
  - 真实 5 万图性能、FPS、内存峰值待用户验收环境补测。
  - `react-virtuoso`、Radix、Lucide 依赖因 pnpm supply-chain/build approval 策略未在本次引入；MVP1.1 可替换当前分页网格为虚拟瀑布流。
  - HEIC 解码仍按文档允许的 MVP1 降级策略处理，不支持时缩略图标记 `unsupported`。

### ADR-007 · MVP2 完成总结与遗留事项
- 状态：待 Windows 实机验收（M2）
- 时间：2026-06-16
- 上下文：MVP2 的全部 T1-T13 在 macOS 开发环境实施完成；T14 端到端验收需要 Windows 真实图库。
- 决定：
  - **schema**：migration 0002 加 `blake3 / phash / dhash / hash_status` 四列 + `duplicate_groups / duplicate_items` 两张表，索引按 docs/01 § 2.2/2.5。
  - **哈希流水线**：`hash_service`（BLAKE3, P2）从原文件流式读 4MB 块；`phash_service`（dHash/Gradient, P3）从 thumbs 缓存读 256px WebP（不重解原图）；两者写完结果后更新 `images.blake3 / images.dhash`，失败统一标 `hash_status='failed'`。
  - **协调器**：自维护 `DedupCoordinator { hash_inflight, dhash_inflight, groups_found, phase }`，不依赖 `Scheduler::status()`（后者会被并行的扫描任务干扰）。`dedup_start` 把哈希任务包成 `TrackedBlakeTask / TrackedDhashTask` 跟踪进度，watcher 在 inflight 归零后 spawn_blocking 跑分组。
  - **BK-tree**：用 `bk-tree` crate 包装 hamming 度量，启动时全表加载，phash 完成后增量插入。
  - **分组**：exact = `blake3 GROUP BY HAVING count>1`；visual = BK-tree 找 hamming ≤ threshold + union-find 合并。重跑前 drop 同 method 下的 open group 避免累积。
  - **删除**：通过 `trash` crate 走系统回收站；恢复在 Windows/Linux 用 `trash::os_limited::list+restore_all`，macOS 没有可编程恢复 API，退回"DB 标记 + 提示用户从访达回收站手动还原"。所有删除写 `undo_log`（30 天 TTL）。
  - **阈值默认值**：czkawka High = hamming 2（hash_size=8），用户可在前端切 0/1/2/5/7 档；与 docs/04 § T3 速查表对齐。
  - **前端**：`activeView` 加 `"dedup"` 视图；`useDedup` hook 照 `useRoots` 模式；`DedupView` 单文件包含 toolbar / GroupList / CompareView / UndoPanel；多选保留通过点击图卡切换 `keepIds`，三个动作按钮（trash / keep_all / dismiss）走 `dedup_resolve`。
- 验证：
  - `cargo test`：单元 + 集成共 30+ 测试通过（含 `test_004_visual_grouping_f1_meets_threshold` 在 30 组合成 fixture 上 F1 ≥ 0.92）。
  - `cargo clippy --all-targets -- -D warnings` 0 警告。
  - `pnpm typecheck` / `pnpm lint` 0 错误。
  - 唯一 ignored：`trash_real_file_writes_undo_log` 走真实系统回收站，在 macOS 沙盒/受限环境不稳，标 `#[ignore]`，Windows 实机由用户跑 `cargo test trash_real_file -- --ignored`。
- 遗留：
  - **Windows 真实回收站撤销**：`trash::os_limited::list+restore_all` 在 macOS 没有 API，已用 `cfg` 隔离。Windows 实机需用户验证：(a) 删 N 张确实进资源管理器回收站；(b) 撤销恢复到原位置；(c) `undo_log.undone_at` 被设置。
  - **T10 批量保留策略下拉**（largest / newest / biggest_file / shortest_name / prefer_root）：本次后端 `dedup_resolve` 接 `keep_image_ids` 已支持任意保留集合，前端只暴露"点击勾选"手动方式；自动策略下拉留 1.0 前补。
  - **几何不变性**（镜像/旋转）：czkawka 默认 Off，本次也未启用；作为高级选项保留。
  - **5 万图扫描+哈希性能**：与 MVP1 T16 一起在 Windows 实机测。
- 后果：M2 主体功能完整可用；Windows 实机验收（T14）后可关闭 M2，开始 M3。

### ADR-009 · MVP4 人脸聚类与地图/备份实现
- 状态：待实机验收（M4）
- 时间：2026-06-18
- 上下文：MVP4 需要 OCR、人脸、时间轴、地图、智能相册、文件监听和备份恢复闭环，但真实 OCR/InsightFace 模型依赖体积大，且运行中覆盖 SQLite/LanceDB 有损坏风险。
- 决定：
  - OCR 使用 worker `/ocr/run`，优先 RapidOCR；缺依赖时 endpoint 返回空 fallback，不拖死主程序。
  - 人脸使用 worker `/face/detect`，优先 InsightFace buffalo_l；embedding 写 LanceDB `face_embeddings`，聚类先用 Rust 侧余弦阈值连通分量，保留用户命名并支持重建。
  - 地图视图引入 Leaflet + OSM 瓦片，先用 circle marker；大量点聚合留 1.0 前做 markercluster。
  - 文件监听用 notify，只监听本地 root；网络盘仍按 docs/07 走手动/定时 rescan。
  - 备份导出 SQLite/WAL/LanceDB/config 到 zip；导入先解压到 `restore-staging`，不在应用运行中覆盖活动库。
- 后果：
  - 功能面已闭合，前端可操作；OCR/人脸准确率、文件监听 5s 入库、地图瓦片网络可用性仍需 Windows 实机与真实模型验收。
  - 聚类算法可用但不是最终高精度方案；后续可替换为 HDBSCAN/Chinese Whispers，不改变 DB/UI 契约。

后续每个 MVP 完成时追加新的 ADR：
- ADR-008 · MVP3 CLIP vs SigLIP 选型（待填）

## 6. 未决议题（讨论后再写 ADR）

- 是否支持 RAW（.cr3 / .arw / .nef）？解码慢 + 库依赖（libraw）→ 1.0 之后看用户反馈
- 视频缩略图（.mp4 / .mov）？需引入 ffmpeg-rs → 单列一个 MVP5
- 云备份（家庭场景）？默认本地优先，但有人会要 → 1.0 之后看
- iCloud / OneDrive 在线占位文件如何处理？读取会触发下载，扫描时如何识别"占位"→ MVP1 调研

## 7. 发布渠道

| 渠道 | 准备 | 状态 |
|------|------|------|
| GitHub Releases | MVP1 完成后开始放 nightly | 未开 |
| 官网下载（如建站） | 1.0 时再说 | 未开 |
| Microsoft Store | 1.0 之后可选 | 未开 |
| WinGet | 1.0 之后 | 未开 |

## 8. 不做的事

- 多语言文字界面（仅中英两套）：在 MVP3 之后；MVP1/2 默认中文
- 移动端：永不
- Web 在线版：永不（本地优先）
- 多用户/多账户：永不
- 内嵌 LLM 对话："和图库聊天"暂不在范围（如做单列 MVP6）

## 9. 关键日期

| 日期 | 事件 |
|------|------|
| 2026-06-15 | 项目启动（M0 开始） |
| TBD | M0 文档审完 |
| TBD | MVP1 验收 |
| TBD | MVP2 验收 |
| TBD | MVP3 验收 |
| TBD | MVP4 验收 |
| TBD | 1.0 发布 |

完成时填实际日期，便于复盘节奏。
