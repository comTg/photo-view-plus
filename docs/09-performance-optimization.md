# 09 · 性能优化跟踪

本文件是性能优化的**唯一事实来源**与进度台账。来源：2026-07-02 对前端（React）、Rust 后端、SQLite 数据库三层的一次全面性能审查。

> 后台任务导致的整机卡顿（CPU / GPU / 杀毒软件争用、负载模式）单独在 [docs/11](11-background-resource-governance.md) 跟踪；本文件继续跟踪查询和渲染类优化。docs/11 的 R1-5、R5 分别对应本文件的 P1-5 和 P2-4 / P0-3，完成时两边同步更新状态。

## 如何使用本文档

1. 从「进度总表」里挑一项（优先按 P0 → P1 → P2 顺序）。
2. 把该项状态改为 `🔄 进行中`。
3. 按该项的「方案」实施，按「验证」跑验证。
4. 完成后：把状态改为 `✅ 已完成`，在该项的「实测 / 备注」里记录**实际改动的文件、验证结果、可量化的前后对比**（如导入耗时、渲染次数）。
5. 一项一提交（commit），提交信息引用条目 ID（如 `perf(P0-1): 扫描导入批量事务`）。

**状态图例**：⬜ 待办 · 🔄 进行中 · ✅ 已完成 · ⏭️ 已跳过（附原因）

**通用验证要求**（继承 `CLAUDE.md`）：
- 改 Rust：`cargo clippy --all-targets -- -D warnings && cargo test`
- 改前端：`pnpm lint && pnpm typecheck && pnpm test`
- 改 UI/交互：`pnpm tauri dev` 手工跑一遍受影响页面
- 改 schema/索引：必须走 migration（禁止裸 ALTER），用 `tests/fixtures/` 小数据集端到端验证
- 改扫描/去重/AI 流水线：用 `tests/fixtures/` 数据集跑端到端，并尽量记录前后耗时

---

## 进度总表

| ID | 层 | 问题 | 优先级 | 状态 |
|----|----|------|--------|------|
| P0-1 | DB/后端 | 扫描逐文件各自 autocommit，全程无批量事务 | P0 | ✅ |
| P0-2 | 后端 | 人脸聚类每批 8 张就全量 O(n²) 重建 | P0 | ✅ |
| P0-3 | 前端 | 全 App 在每个高频事件上整树重渲染 | P0 | 🔄 部分 |
| P0-4 | 前端 | 内联 `onPreviewImage` 箭头破坏 `ImageGrid` 的 memo | P0 | ✅ |
| P0-5 | DB | Timeline N+1：每桶一次 `strftime` 全表扫描 | P0 | ✅ |
| P0-6 | DB | 主画廊默认排序无索引 + `LIKE '%%'` + COUNT 二次全扫 | P0 | 🔄 部分 |
| P1-1 | DB | hash 流水线 pending 查询索引错配（phash/dhash 错位） | P1 | ✅ |
| P1-2 | DB | 日期/时间线/地图共用的 `COALESCE(taken_at,mtime)` 绕过索引 | P1 | ✅ |
| P1-3 | 后端 | 缩略图全分辨率解码（无降采样 / 不读 EXIF 预览） | P1 | ✅ |
| P1-4 | 后端 | `pending_*` 查询拉全 30 列 `ImageRecord` | P1 | ✅ |
| P1-5 | 前端 | refresh 模式全量重拉已加载图片（扫描期每 1.5s） | P1 | ⬜ |
| P1-6 | 前端 | `replaceImageRecord` 每事件触碰 6 个 state 数组 | P1 | ⬜ |
| P1-7 | 前端 | 二级视图（人脸/OCR/相册/AI 搜索）未虚拟化 | P1 | ⬜ |
| P2-1 | 后端 | 去重分组 insert 无事务（第二处写放大） | P2 | ✅ |
| P2-2 | 后端 | `batch_resolve` N+1（逐 item `get_detail`） | P2 | ⬜ |
| P2-3 | 后端 | 热路径缺 `prepare_cached`，每次重编译 SQL | P2 | ✅ |
| P2-4 | 后端 | `get_detail` 仅为发 `thumbnail:updated` 事件跑 30 列 JOIN | P2 | ⬜ |
| P2-5 | 后端 | `probe_image` 双开文件（尺寸 + EXIF 各一次） | P2 | ✅ |
| P2-6 | DB | `IMAGE_SELECT` 每行都带无界 `ocr_text` | P2 | ✅ |
| P2-7 | 前端 | MapView 一次建 3000 个 Leaflet marker，无聚类 | P2 | ⬜ |
| P3-1 | 前端 | 缩略图缺 `decoding="async"` | P3 | ✅ |
| P3-2 | 前端 | `selectedRecords` 用 `includes` 是 O(n·m) | P3 | ✅ |
| P3-3 | 前端 | Lightbox `slides` 未 memo | P3 | ✅ |
| P3-4 | 前端 | `globals.css` 中 `.image-card`/`.image-grid` 等重复定义 | P3 | ⏭️ 暂缓 |
| P3-5 | 后端 | 缩略图协议每请求 `std::thread::spawn`，无线程池 | P3 | ⬜ |
| P3-6 | 后端 | LanceDB 从不建 ANN 索引，`top_k` 暴力扫描 | P3 | ⬜ |
| P3-7 | 后端 | BLAKE3 双 4MB 缓冲冗余 | P3 | ✅ |

> 已确认合规、无需改动：主画廊 `VirtuosoGrid` 虚拟化 + 分页；缩略图走 `thumb.localhost` 协议（带 `Cache-Control`，非 base64）；SQLite WAL + 连接池；扫描/哈希/缩略图均 `spawn_blocking` 且遵守并发红线（网络 4 / 本地 8）；视觉去重用 BK-tree；路径按 `roots.id + rel_path` 存、缩略图不入主表、外键索引齐全。

---

## P0 — 最高杠杆（改动小、收益最大）

### P0-1 · 扫描导入批量事务
- **层**：DB / 后端
- **问题**：walk 主循环无任何包裹事务，每条 `upsert_scanned_image`（SELECT + INSERT/UPDATE）独立 autocommit，FTS5 触发器每行再放大一次。10 万张图 ≈ 10 万+ 次 WAL 提交，是大库导入慢的头号原因。
- **位置**：[scan_service.rs:258-347](../src-tauri/src/services/scan_service.rs#L258-L347)（walk 主循环）、[:424](../src-tauri/src/services/scan_service.rs#L424)（upsert 调用）；[images_repo.rs:268-353](../src-tauri/src/repo/images_repo.rs#L268-L353)（`upsert_scanned_image`）；`mark_missing_deleted` 逐行删 [scan_service.rs:468-484](../src-tauri/src/services/scan_service.rs#L468-L484)
- **方案**：每 500~1000 个文件包一个 `conn.transaction()` 批量提交；`mark_missing_deleted` 的删除也批量。注意进度事件（每 50 文件）仍需在事务外或用独立连接发，避免长事务持锁。
- **验证**：`cargo test`；用 `tests/fixtures/` 大目录跑扫描，记录导入耗时前后对比。
- **预期收益**：大库导入提速数倍。
- **状态**：✅ 已完成（2026-07-02）
- **实测 / 备注**：
  - 改动文件：[scan_service.rs](../src-tauri/src/services/scan_service.rs)。将 `scan_one_file` 拆为 `prepare_scan_file`（探测/构造记录，**无 DB 访问**，事务外调用）+ `flush_scan_batch`（单事务批量 upsert，提交后再入队缩略图）。`SCAN_BATCH_SIZE=500`。`mark_missing_deleted` 也改为单事务批量。
  - **关键设计**：探测（读图头 + 解 EXIF，磁盘 I/O）刻意留在事务外累积，事务只包内存→WAL 写入，避免长时间持有 WAL 写锁拖住并发的缩略图 worker（撞 `busy_timeout=5s`）。取消扫描时先 flush 已探测批次再暂停，不丢工作。
  - 缩略图入队时机从「每文件即时」变为「每批提交后」，功能等价（后台任务，微小延迟无感）；`is_network` 判定移到循环外只算一次。
  - 验证：`cargo check` ✓、`cargo clippy --all-targets -- -D warnings` ✓、`cargo test` 除 `thumbnail_tests` 外全绿（`images_tests::upsert_and_query_roundtrip`、`dedup_tests` 等覆盖 upsert/DB 路径）。
  - ⚠️ 收益修正：本项目 WAL + `synchronous=NORMAL` 下 commit **不** per-commit fsync（仅 checkpoint 时同步），故「提速数倍」偏乐观；批处理的实际收益来自减少 WAL 帧churn、事务机制开销与 FTS5 触发器提交次数。真实大库导入耗时前后对比待 `pnpm tauri dev` 实机补测。
  - ⚠️ 遗留：`cargo test` 的 `thumbnail_tests` 报 `STATUS_ENTRYPOINT_NOT_FOUND (0xc0000139)`，经 `git stash` 回退到改动前代码复现同样错误 → 确认为**改动前就存在的原生 DLL 加载环境问题**，与本次无关（建议单独排查，疑似 webp/heif 编解码 DLL 或测试残留 DLL 版本不匹配）。

### P0-2 · 人脸聚类去抖，避免每批 O(n²) 重建
- **层**：后端
- **问题**：每处理 8 张人脸图（一个 `FaceBatchTask`）就调一次 `rebuild_clusters`，后者拉全部人脸向量做全量两两余弦比较 + 全量 DELETE/UPDATE。B 批次 × O(N²) ≈ 全库 O(N³)，是人脸功能最差扩展点。
- **位置**：[face_service.rs:152-155](../src-tauri/src/services/face_service.rs#L152-L155)（每批调用）；[face_repo.rs:181-217](../src-tauri/src/repo/face_repo.rs#L181-L217)（rebuild）、[:285-291](../src-tauri/src/repo/face_repo.rs#L285-L291)（O(n²) 循环）
- **方案**：所有人脸批次排空后聚类一次（去抖 / 合并触发），或改增量分配（新向量只与已有簇心比较）。绝不每批全量重建。
- **验证**：`cargo test`；fixtures 人脸集跑 face pipeline，确认聚类结果一致且耗时随批次数不再线性叠加。
- **状态**：✅ 已完成（2026-07-02）
- **实测 / 备注**：
  - 改动文件：[face_service.rs](../src-tauri/src/services/face_service.rs)。把 `rebuild_clusters` 移出 `if !vectors.is_empty()`，改为**仅当 `count_image_status(conn,"pending")==0`（本轮人脸检测全部处理完）时才重建一次**。
  - **效果**：一次 `enqueue_face_detection`（最多 64 图 = 8 批）原先触发 **8 次** O(n²) 全量重建，现在合并为 **1 次**（本轮最后排空的批次触发）。全库累计从 ≈ O(N³) 降到每轮 O(N²)。
  - 并发安全：处理中批次的图仍为 `pending`（批次结束才置 `ready`），故非排空批次判定 pending>0 会跳过；偶发两批同时判定为 0 → 重复重建，但 `rebuild_clusters` 幂等，无正确性问题。
  - 验证：`cargo clippy --all-targets -- -D warnings` ✓（无 face 单测覆盖；聚类结果一致性 + 耗时对比需 AI worker 实机跑，属 MVP3+/CUDA，留待验收）。

### P0-3 · 拆分单体组件，隔离高频事件重渲染
- **层**：前端
- **问题**：3233 行的 `App` 集中约 40 个 state，`scan:progress`/`queue:status`/`ai:progress` 等高频事件每 tick 都 `setState` → 重渲整个外壳（工具栏、侧栏、页脚、详情）。
- **位置**：[App.tsx:137-215](../src/App.tsx#L137-L215)（state 声明）、[:371-379](../src/App.tsx#L371-L379)（scan 进度 handler）、[:396-398](../src/App.tsx#L396-L398)、[:446-452](../src/App.tsx#L446-L452)（监听器）
- **方案**：把进度相关 state 与画廊/工具栏/页脚拆成独立组件或 context slice，让进度更新只重渲订阅它的小组件。可分阶段做：先抽 `TaskStatus`/footer 已 memo 的部分的数据源，再抽工具栏。
- **验证**：`pnpm lint && pnpm typecheck && pnpm test`；`pnpm tauri dev` 开 React DevTools Profiler，扫描时确认外壳不再随每 tick 重渲。
- **状态**：🔄 部分完成（2026-07-02）
- **实测 / 备注**：
  - **已完成（安全、可静态验证的部分）**：
    1. 昂贵的 `ImageGrid` 已由 [[P0-4]] 隔离（memo + 全部 props 稳定）——这是 P0-3 问题里最贵的子树。
    2. `FilterBar` 包进 `memo`（[App.tsx](../src/App.tsx)）：其 props 全是 state 值 + 稳定 setter，进度 tick 时直接跳过。
    3. `DetailPane` 包进 `memo`，并把它的 3 个内联箭头（`onRegenerateThumbnail`/`onPreview`/`onFindSimilar`）用 `useCallback` 稳定，使详情面板在不改变选中项的高频事件时跳过重渲。
    - 现状核实：`TaskStatus`（页脚，消费 `scanProgress`/`queue`/`aiPipeline`）本就是 memo，其在进度 tick 重渲染是**正确且必要**的（要显示实时进度）；`scanProgress` 只在两处读取，消费面窄。
    - 验证：`pnpm typecheck` ✓、`pnpm lint` ✓（加 memo 是零风险改动：props 变化时照常重渲，不改变行为）。
  - **待办（deferred，需实机 profiling）**：header/toolbar/sidebar 仍是 App 内联 JSX，进度 tick 仍会重渲这些轻量 DOM。要彻底隔离需把工具栏/侧栏抽成 memo 组件 + 稳定其回调 props，或把纯 setState 的 `queue:status`/`ai:progress`/`ai:worker_status` 订阅下沉到独立自订阅组件（`scan:progress`/`scan:done` 带副作用须留 App）。此项属结构性重构，回归风险较高且**必须** `pnpm tauri dev` + React DevTools Profiler 实测才能安全推进——留待统一验收时与实机 profiling 一起做。

### P0-4 · `onPreviewImage` 用 useCallback 稳定引用
- **层**：前端
- **问题**：传给 `ImageGrid` 的 `onPreviewImage={(image) => openPreviewById(image.id)}` 每次渲染换新引用，绕过 `ImageGrid`/`ImageCard` 的 `memo`，导致所有可见缩略图重渲染。其余 4 个回调都已正确 `useCallback`，唯此漏了。
- **位置**：[App.tsx:1537](../src/App.tsx#L1537)
- **方案**：`const handlePreviewById = useCallback((image) => openPreviewById(image.id), [openPreviewById])`，传该引用。
- **验证**：`pnpm lint && pnpm typecheck`；Profiler 确认非选中卡片不再重渲。
- **状态**：✅ 已完成（2026-07-02）
- **实测 / 备注**：
  - 改动文件：[App.tsx](../src/App.tsx)。新增 `handlePreviewById = useCallback((image) => openPreviewById(image.id), [openPreviewById])`（`openPreviewById` 本身是 `[]` 依赖的稳定 callback），替换 `ImageGrid` 上的内联箭头。
  - **效果**：`ImageGrid`（memo）的全部 props 现在引用稳定，进度类高频事件（`scan:progress`/`queue:status`/`ai:progress`）触发 App 重渲染时，虚拟网格及其可见卡片被 memo 跳过——这是 P0-3 问题里最贵的那块，已被本项隔离。
  - 验证：`pnpm typecheck` ✓、`pnpm lint` ✓。运行时 Profiler 确认留待实机验收。

### P0-5 · 重写 Timeline，消除 N+1 + strftime 全表扫描
- **层**：DB
- **问题**：`timeline_buckets` 先全表 `strftime` 分组，再**对每个桶**调 `timeline_samples`，后者 `WHERE CAST(strftime(...))=?` 是列上函数表达式，`idx_images_taken_at` 用不上 → 每桶一次全表扫描，最坏 240 桶 × 全扫。
- **位置**：[images_repo.rs:771-811](../src-tauri/src/repo/images_repo.rs#L771-L811)（buckets）、[:838-859](../src-tauri/src/repo/images_repo.rs#L838-L859)（samples）
- **方案**：一趟查询用 `GROUP BY` / 窗口函数取回年月 + 每桶样本，在内存分配 samples，消除 N+1；或加生成列 `bucket` + 索引。与 P1-2 的 `COALESCE` 表达式索引可协同。
- **验证**：`cargo test`；fixtures 跑时间线，对比结果一致 + 查询耗时。
- **状态**：✅ 已完成（2026-07-02）
- **实测 / 备注**：
  - 改动文件：[images_repo.rs](../src-tauri/src/repo/images_repo.rs)。`timeline_buckets` 改为 3 步：① 聚合（单次扫描，年月分桶 count/min/max）；② 新增 `timeline_samples_by_bucket`——单趟 `ROW_NUMBER() OVER (PARTITION BY 年,月 ORDER BY ts DESC, id DESC)` 取每桶前 50 的 id；③ 一次 `get_details_by_ids` 批量取完整记录后按 (年,月) 分组挂回桶。删除原 N+1 的 `timeline_samples`。
  - **效果**：扫描次数从「1 聚合 + 240 桶 × 1 次带 strftime 全表扫描」降到「1 聚合 + 1 窗口扫描 + 1 批量 IN」；`strftime` 每行只算一次（原来每桶重算一遍）。
  - 新增测试：[images_tests.rs](../src-tauri/tests/images_tests.rs) `timeline_buckets_groups_by_month_with_samples`（月中正午 UTC 时间戳，与本机时区无关），断言两月两桶、count、桶内时间倒序。
  - 验证：`cargo test --test images_tests` 2 通过 ✓、`cargo clippy --all-targets -- -D warnings` ✓。
  - 备注：窗口查询仍全表扫描（无法完全避免，除非落 `bucket` 生成列）；[[P1-2]] 的 `COALESCE(taken_at,mtime)` 表达式索引可进一步加速其排序。ids 批量 IN 在现实图库（≤~12000 样本）远低于 SQLite 变量上限。

### P0-6 · 主画廊查询：加 mtime 索引 / FTS 取代 LIKE / COUNT 去 JOIN
- **层**：DB
- **问题**：三合一。① 全库视图默认 `ORDER BY i.mtime DESC` 无独立索引（只有 `idx_images_root_mtime` 复合索引，不带 root_id 过滤时用不上）→ 全表 sort。② 文件名过滤 `LIKE '%q%'` 前导通配全表扫描，而已有 `images_fts(filename)` 未复用。③ 每次分页 COUNT 带 `JOIN roots` 再全扫一遍算 total（JOIN 对计数无用）。
- **位置**：[images_repo.rs:153-184](../src-tauri/src/repo/images_repo.rs#L153-L184)（query + count）、[:1061-1101](../src-tauri/src/repo/images_repo.rs#L1061-L1101)（order_by）、[:1016-1024](../src-tauri/src/repo/images_repo.rs#L1016-L1024)（LIKE）
- **方案**：迁移加 `CREATE INDEX idx_images_mtime ON images(mtime DESC)`（或默认排序退化到 id）；文件名搜索改走 FTS；COUNT 去掉 `JOIN roots`；评估 keyset 分页替代大 OFFSET。
- **验证**：`cargo test`；`EXPLAIN QUERY PLAN` 确认走索引；fixtures 大库翻页 + 搜索耗时对比。
- **状态**：🔄 部分完成（2026-07-02）
- **实测 / 备注**：
  - **已完成**：
    1. 迁移 [0006_perf_indexes.sql](../src-tauri/migrations/0006_perf_indexes.sql) 加 `idx_images_mtime ON images(mtime)`，注册为版本 6（[migrations/mod.rs](../src-tauri/src/migrations/mod.rs)）。
    2. COUNT 去掉 `JOIN roots`（[images_repo.rs](../src-tauri/src/repo/images_repo.rs) `query()`）——`build_where` 只引用 `i.*` 列，去 JOIN 安全。
    - 验证：`EXPLAIN QUERY PLAN` 确认全库 mtime 排序查询走 `SCAN i USING INDEX idx_images_mtime`；`cargo test`（db/images/dedup）全绿、`clippy` ✓、migration 幂等测试通过。
  - **待办（deferred）**：文件名搜索仍是 `LIKE '%q%'` 全表扫描。改走 `images_fts` 会把语义从"子串匹配"变为"分词/前缀匹配"（如 `img` 能否命中 `IMG_001.JPG` 取决于 tokenizer），属**用户可感知的 UX 变更**，需 `pnpm tauri dev` 实测搜索体验后再定，留待验收。keyset 分页替代大 OFFSET 同样留待评估。

---

## P1 — 高收益

### P1-1 · 修补 hash 流水线 pending 查询索引
- **层**：DB
- **问题**：已建 `idx_images_blake3/phash` 是 `WHERE ... IS NOT NULL`，但热点查询找 `IS NULL` 的行 → 全表扫描。且视觉去重实际用 `dhash`，索引却建在没被查的 `phash` 上。
- **位置**：[images_repo.rs:614-615](../src-tauri/src/repo/images_repo.rs#L614-L615)、[:640-652](../src-tauri/src/repo/images_repo.rs#L640-L652)；索引定义 `migrations/0002_hash_columns.sql:12-13`
- **方案**：迁移加部分索引 `ON images(id) WHERE blake3 IS NULL AND deleted_at IS NULL`、`... WHERE dhash IS NULL AND thumb_status='ready'`；删闲置 `idx_images_phash` 或改建 `idx_images_dhash`。
- **验证**：`cargo test`；`EXPLAIN QUERY PLAN` 确认 pending 扫描走新索引。
- **状态**：✅ 已完成（2026-07-02）
- **实测 / 备注**：
  - 迁移 [0006_perf_indexes.sql](../src-tauri/migrations/0006_perf_indexes.sql)：加 `idx_images_blake3_pending ON images(id) WHERE blake3 IS NULL AND deleted_at IS NULL`、`idx_images_dhash_pending ON images(id) WHERE dhash IS NULL AND thumb_status='ready'`；`DROP INDEX idx_images_phash`（确认 `phash` 列从不按值查询，仅 `set_phash` 按 id 写 + row 读，索引闲置）并改建 `idx_images_dhash ON images(dhash) WHERE dhash IS NOT NULL`。
  - 验证：`EXPLAIN QUERY PLAN` 确认 `pending_blake3` 走 `idx_images_blake3_pending`、`pending_dhash` 走 `idx_images_dhash_pending`（均从全表扫描变为索引扫描）；`cargo test` + `clippy` ✓。

### P1-2 · 为 `COALESCE(taken_at, mtime)` 加表达式索引
- **层**：DB
- **问题**：日期筛选、时间线、地图排序都用 `COALESCE(taken_at, mtime)`，裸列部分索引 `idx_images_taken_at` 命中不了表达式 → 全表。
- **位置**：[images_repo.rs:1036-1043](../src-tauri/src/repo/images_repo.rs#L1036-L1043)（日期筛选）、[:819](../src-tauri/src/repo/images_repo.rs#L819)（map 排序）
- **方案**：迁移加 `CREATE INDEX idx_images_effective_ts ON images(COALESCE(taken_at, mtime))`，或落 `effective_ts` 生成列 + 索引。四处（时间线/地图/日期筛选/排序）共享收益，与 P0-5 协同。
- **验证**：`cargo test`；`EXPLAIN QUERY PLAN` 确认命中。
- **状态**：✅ 已完成（2026-07-02）
- **实测 / 备注**：
  - 迁移 [0006_perf_indexes.sql](../src-tauri/migrations/0006_perf_indexes.sql) 加 `idx_images_effective_ts ON images(COALESCE(taken_at, mtime))`。
  - **关键确认**：`EXPLAIN QUERY PLAN` 证实查询里带别名的 `COALESCE(i.taken_at, i.mtime)` **能命中**无别名的表达式索引——日期范围过滤走 `SEARCH i USING INDEX idx_images_effective_ts (<expr>>? AND <expr><?)`（此前是全表扫描）。
  - 惠及日期筛选、地图排序（[[P0-5]] 时间线窗口排序亦可受益）。验证：`cargo test` + `clippy` ✓。

### P1-3 · 缩略图降采样解码
- **层**：后端
- **问题**：`image::open` 先把原图（可达 50MP）全解码进内存再缩放，CPU 与峰值内存双高。
- **位置**：[thumbnail_service.rs:306](../src-tauri/src/services/thumbnail_service.rs#L306)、[:327](../src-tauri/src/services/thumbnail_service.rs#L327)
- **方案**：JPEG 用 DCT scaled decode（`jpeg-decoder` 的 `scale`）或 `image` reader 带 limits；大图优先读 EXIF 内嵌预览。
- **验证**：`cargo test`；fixtures 含大 JPEG，对比缩略图生成耗时与内存峰值，确认缩略图质量无退化。
- **状态**：✅ 已完成（2026-07-02）
- **实测 / 备注**：
  - image 0.25 的 `JpegDecoder`（zune-jpeg 后端）**不暴露 `scale`**，zune-jpeg 0.4 也无公开降采样 API。故新增直接依赖 **`jpeg-decoder = "0.3"`**（Cargo.toml，⚠️ 新依赖，验收留意）用其 DCT `scale`。
  - 改动文件：[thumbnail_service.rs](../src-tauri/src/services/thumbnail_service.rs)。新增 `decode_for_thumbnail`（JPEG 走降采样、其余格式与失败回退 `image::open`）、`decode_jpeg_downscaled`（`Decoder::scale(THUMB_SIZE*2)` → RGB24/L8 构 `DynamicImage`；CMYK/L16 返 None 回退）、`oriented_native_dims`。
  - **正确性关键**：降采样后不能用解码尺寸当原生 `width/height`，改用 `image::image_dimensions`（只读图头）取原生尺寸并按 orientation(6/8) 交换宽高回填。
  - 新增测试：[thumbnail_tests.rs](../src-tauri/tests/thumbnail_tests.rs) `thumbnail_downscales_large_jpeg_and_keeps_native_dims`（1024×1024 JPEG 触发 1/2 DCT 降采样），断言缩略图 ≤256 且回填的仍是原生 1024×1024；原 PNG 测试覆盖非 JPEG 回退路径。
  - 验证：`cargo test --test thumbnail_tests` 2 通过 ✓、`cargo clippy --all-targets -- -D warnings` ✓。大 JPEG 的耗时/内存峰值实测对比留待实机 fixtures。

### P1-4 · `pending_*` 用窄投影
- **层**：后端
- **问题**：`pending_blake3/thumbnail/dhash_images` 都跑 30 列 `IMAGE_SELECT` + JOIN，仅为入队却 materialize 整表进内存。调用方只需 `id/full_path/root_path/orientation/thumb_hash`。
- **位置**：[images_repo.rs:614](../src-tauri/src/repo/images_repo.rs#L614)、[:628](../src-tauri/src/repo/images_repo.rs#L628)、[:640](../src-tauri/src/repo/images_repo.rs#L640)；调用方 [dedup_service.rs:260-300](../src-tauri/src/services/dedup_service.rs#L260-L300)
- **方案**：新增窄投影查询 + 轻量 struct，只取任务所需字段。
- **状态**：✅ 已完成（2026-07-02）
- **实测 / 备注**：
  - 改动文件：[images_repo.rs](../src-tauri/src/repo/images_repo.rs)。新增轻量 `PendingImage`（`id/root_id/rel_path/root_path/full_path/orientation/thumb_hash`）+ `PENDING_SELECT`（**6 列**，vs `IMAGE_SELECT` 30 列，丢弃无界 `ocr_text` 及所有 hash/status/gps/exif 列）+ `row_to_pending`（full_path 算法与 `row_to_record` 一致）+ `query_pending` 辅助。`pending_blake3_images` / `pending_thumbnail_images` / `pending_dhash_images` 全部改用它。
  - 三个调用方（dedup blake3/dhash 入队、缩略图启动重排）字段需求都被 `PendingImage` 覆盖，**无需改调用方**。
  - **效果**：fresh 库上入队哈希/缩略图任务时不再把整表 30 列 materialize 进内存，每行仅 6 列。
  - 验证：`cargo clippy --all-targets -- -D warnings` ✓；`dedup_tests`(8)/`images_tests`(2)/`thumbnail_tests`(2) 全通过（现有测试覆盖 `pending_blake3_images` 结果正确性：失败重试、软删跳过、新图入选）。

### P1-5 · refresh 只刷视口窗口而非全量重拉
- **层**：前端
- **问题**：扫描期每 1.5s（`SCAN_REFRESH_MS`）及每个 `scan:done` 触发 refresh，`limit = max(PAGE_SIZE, 已加载数量)` → 一次 invoke 重拉上千条并替换整个 `images` 数组。
- **位置**：[App.tsx:326-328](../src/App.tsx#L326-L328)、[:363-369](../src/App.tsx#L363-L369)（scheduleRefresh）、[:381-390](../src/App.tsx#L381-L390)（scan:done）
- **方案**：refresh 只拉第一页（或视口附近窗口）再 merge，而非全量。
- **验证**：`pnpm lint && pnpm typecheck && pnpm test`；`pnpm tauri dev` 扫描期观察网络/invoke 频次与列表更新是否正常。
- **状态**：⬜ 待办
- **实测 / 备注**：_（完成后填写）_

### P1-6 · `replaceImageRecord` 只更新活动视图
- **层**：前端
- **问题**：每个 `thumbnail:updated` 都对 `images`/`detail`/`previewImages`/`aiResults`/`timelineBuckets`(嵌套 map)/`mapPoints`/`advancedImages` 共 6+ 个数组各跑 O(n) `.map()`，初次扫描高频触发，即使视图未挂载。
- **位置**：[App.tsx:400-417](../src/App.tsx#L400-L417)
- **方案**：只更新当前活动视图对应的数组；或用 id→record 归一化 map 做 O(1) 更新。
- **验证**：`pnpm lint && pnpm typecheck && pnpm test`；Profiler 观察缩略图完成事件的更新成本。
- **状态**：⬜ 待办
- **实测 / 备注**：_（完成后填写）_

### P1-7 · 二级视图复用 VirtuosoGrid 虚拟化
- **层**：前端
- **问题**：人脸/OCR/智能相册/AI 搜索结果都 `images.map()` 直接渲染最多 200 张 `<img>` 卡片，DOM/reconcile 成本高。
- **位置**：[App.tsx:2102](../src/App.tsx#L2102)（PeopleView）、[:2331](../src/App.tsx#L2331)（AdvancedView）、[:1852](../src/App.tsx#L1852)（AiSearchView）、[:1907](../src/App.tsx#L1907)（TimelineView 嵌套 map）
- **方案**：抽出可复用的虚拟化网格组件（基于现有 `ImageGrid`/`VirtuosoGrid`），二级视图复用。
- **验证**：`pnpm lint && pnpm typecheck && pnpm test`；`pnpm tauri dev` 逐个视图手工验证滚动与交互。
- **状态**：⬜ 待办
- **实测 / 备注**：_（完成后填写）_

---

## P2 — 中收益

### P2-1 · 去重分组 insert 包事务
- **层**：后端
- **问题**：exact/visual 分组逐组 `insert_group` + `insert_items` 各自 autocommit，大库可产生成千上万 group/item，逐条 fsync。
- **位置**：[dedup_service.rs:627-635](../src-tauri/src/services/dedup_service.rs#L627-L635)（exact）、[:696-721](../src-tauri/src/services/dedup_service.rs#L696-L721)（visual）；[duplicates_repo.rs:50-64](../src-tauri/src/repo/duplicates_repo.rs#L50-L64)（insert_items）
- **方案**：整个分组 pass 外层包 `conn.transaction()`，或让 insert 接收 `&Transaction`。范式见已正确用事务的 [tags_repo.rs:64-92](../src-tauri/src/repo/tags_repo.rs#L64-L92)、[face_repo.rs:52-77](../src-tauri/src/repo/face_repo.rs#L52-L77)。
- **验证**：`cargo test --test dedup_tests`；fixtures 去重耗时对比。
- **状态**：✅ 已完成（2026-07-02）
- **实测 / 备注**：
  - 改动文件：[dedup_service.rs](../src-tauri/src/services/dedup_service.rs)。`run_exact_grouping_with` 与 `run_visual_grouping_with` 的 group/item 写入循环用 `conn.unchecked_transaction()` 包成一个事务批量提交（`&Connection` 无法调 `transaction()`，用 unchecked；已确认调用链无外层事务）。`drop_open_groups`/`all_dhashes` 等读/删保持在事务外，语义不变。
  - **事件时序确认（原顾虑已排除）**：`on_new_group` 仍逐组发，但前端 `useDedup` 的 `dedup:new_group` 走**尾防抖** `scheduleReload`——分组爆发期计时器不断重置，实际 reload 在爆发结束（提交）后才触发；且 `dedup:done` 兜底 reload。reload 用独立连接读**已提交**数据，不会读到未提交的组，流式渐显体验不变。
  - 验证：`cargo test --test dedup_tests` 8 通过（含 exact/visual 分组、rerun 替换）、`clippy` ✓。

### P2-2 · `batch_resolve` 改批量取详情
- **层**：后端
- **问题**：groups × items 逐 item 调 `get_detail`（每次一个 JOIN 查询），N+1。
- **位置**：[dedup_service.rs:423-435](../src-tauri/src/services/dedup_service.rs#L423-L435)；已存在批量接口 [images_repo.rs:191](../src-tauri/src/repo/images_repo.rs#L191)（`get_details_by_ids`）
- **方案**：收集全部 image_id，一次 `get_details_by_ids` 批量取。
- **验证**：`cargo test --test dedup_tests`。
- **状态**：⬜ 待办
- **实测 / 备注**：_（完成后填写）_

### P2-3 · 热路径改 `prepare_cached`
- **层**：后端
- **问题**：repo 全用 `conn.prepare`/`execute`/`query_row`，每次调用重编译 SQL。扫描 per-file 路径、缩略图协议 per-request 路径重复最狠。
- **位置**：[images_repo.rs:273-352](../src-tauri/src/repo/images_repo.rs#L273-L352)（upsert）、[:186-189](../src-tauri/src/repo/images_repo.rs#L186-L189)（get_detail）、[:212](../src-tauri/src/repo/images_repo.rs#L212)（get_thumb_hash）
- **方案**：重复语句改 `conn.prepare_cached(...)`。
- **验证**：`cargo test`；扫描耗时对比。
- **状态**：✅ 已完成（2026-07-02）
- **实测 / 备注**：
  - 改动文件：[images_repo.rs](../src-tauri/src/repo/images_repo.rs)。转为 `prepare_cached` 的语句：`upsert_scanned_image` 的 SELECT/UPDATE/INSERT（扫描每个变更文件执行）、`get_detail`（每 `thumbnail:updated` 事件 + 原图协议）、`get_thumb_hash`（每张缩略图 HTTP 请求）。
  - `prepare_cached` 通过 `&Transaction`（Deref 到 Connection）调用时仍命中 Connection 级语句缓存，跨事务持久，与 P0-1 的批量事务兼容。
  - 验证：`cargo clippy --all-targets -- -D warnings` ✓、`cargo test` 同上（除环境性 `thumbnail_tests`）。

### P2-4 · 缩略图完成事件不跑全量 JOIN
- **层**：后端
- **问题**：每张缩略图完成后 `emit_updated` 跑 30 列 `get_detail` JOIN 只为发 `thumbnail:updated`，初次导入数千次多余查询。
- **位置**：[thumbnail_service.rs:137-149](../src-tauri/src/services/thumbnail_service.rs#L137-L149)
- **方案**：只发 id + status 的轻量 payload；或合并 UI 刷新。与前端 P1-6 联动。
- **验证**：`cargo test`；确认前端仍能正确更新缩略图状态。
- **状态**：⬜ 待办
- **实测 / 备注**：_（完成后填写）_

### P2-5 · `probe_image` 单次读取
- **层**：后端
- **问题**：`image_dimensions` 与 EXIF 读取各 `open` 一次文件，网络盘每文件两次往返。
- **位置**：[scan_service.rs:561-570](../src-tauri/src/services/scan_service.rs#L561-L570)
- **方案**：单 decoder / 单次读取同时拿尺寸与 EXIF。
- **状态**：✅ 已完成（2026-07-02）
- **实测 / 备注**：
  - 改动文件：[scan_service.rs](../src-tauri/src/services/scan_service.rs)。`probe_image` 只 `File::open` 一次：先 `image::ImageReader::new(&mut reader).with_guessed_format().into_dimensions()` 解尺寸（等价于旧的 `image::image_dimensions`，其内部同样如此），再 `seek(0)` 复用同一句柄读 EXIF。原先是 `image_dimensions` + `File::open` 两次打开。
  - **效果**：每张待探测图少一次文件打开——网络盘上少一次往返。
  - 新增单测：`probe_image_reads_dimensions_single_open`（JPEG + PNG 各断言尺寸），守护"换成复用句柄的 ImageReader"后尺寸解析仍正确。`cargo test --lib` ✓、`clippy` ✓。

### P2-6 · 列表查询去掉 `ocr_text`
- **层**：DB
- **问题**：`IMAGE_SELECT` 30 列每行都带无界 `ocr_text`（主表唯一无界大字段），画廊/pending/timeline/map 全复用，加重行扫描与序列化。
- **位置**：[images_repo.rs:9-18](../src-tauri/src/repo/images_repo.rs#L9-L18)（`:16` 取 ocr_text）
- **方案**：列表用精简列集（不含 ocr_text），仅 `get_detail` 取；或把 `ocr_text` 拆副表 `image_ocr`，主表只留 `ocr_status`（FTS 已单独持有一份）。
- **状态**：✅ 已完成（2026-07-02）
- **实测 / 备注**：
  - 改动文件：[images_repo.rs](../src-tauri/src/repo/images_repo.rs)。新增 `IMAGE_SELECT_LIST`——列布局与 `IMAGE_SELECT` 完全一致，但第 28 列由 `i.ocr_text` 换成 `NULL`。`query()`（画廊/瀑布流分页）改用它，**复用同一个 `row_to_record`**（ocr_text 读到 NULL → None），零结构改动、零风险。详情页 `get_detail` 仍用 `IMAGE_SELECT` 取全文。
  - **效果**：列表每页 200 行不再读取无界 `ocr_text`（大文本走 overflow page，OCR 过的库收益明显；未 OCR 的库该列为 NULL，无副作用）。未拆副表（避免 schema 迁移），用 NULL 占位是等效的最小改动。
  - 验证：`cargo test --test images_tests`（query roundtrip + timeline）2 通过、`clippy` ✓。`row_to_record` 的 `ocr_text: row.get(28)` 与 NULL 位置对齐。

### P2-7 · MapView marker 聚类
- **层**：前端
- **问题**：一次 `imagesMapPoints(3000)` + 循环建 3000 个 `L.circleMarker`+popup，DOM/SVG 重。
- **位置**：[App.tsx:1976-1989](../src/App.tsx#L1976-L1989)、fetch [:896-907](../src/App.tsx#L896-L907)
- **方案**：引入 marker 聚类（如 leaflet.markercluster），或只渲当前视口 bounds 内的点。
- **验证**：`pnpm lint && pnpm typecheck`；`pnpm tauri dev` 地图视图交互与缩放。
- **状态**：⬜ 待办
- **实测 / 备注**：_（完成后填写）_

---

## P3 — 低收益 / 清理

### P3-1 · 缩略图加 `decoding="async"`
- **位置**：[App.tsx:2520](../src/App.tsx#L2520)、[:2634](../src/App.tsx#L2634)、[DedupView.tsx:640](../src/components/dedup/DedupView.tsx#L640)
- **方案**：`<img>` 加 `decoding="async"`，快速滚动时避免主线程同步解码。
- **验证**：`pnpm lint && pnpm typecheck`。
- **状态**：✅ 已完成（2026-07-02） · **实测 / 备注**：3 处 `<img>`（[App.tsx](../src/App.tsx) 网格卡片 + 详情 512px、[DedupView.tsx](../src/components/dedup/DedupView.tsx) 缩略图）均加 `decoding="async"`；DedupView 缩略图另补 `loading="lazy"`。`pnpm typecheck` + `pnpm lint` ✓。

### P3-2 · `selectedRecords` 用 Set
- **位置**：[App.tsx:1134](../src/App.tsx#L1134)
- **方案**：`selectedIds.includes` 改为先建 `Set` 再 `has`，O(n·m) → O(n)。
- **验证**：`pnpm lint && pnpm typecheck && pnpm test`。
- **状态**：✅ 已完成（2026-07-02） · **实测 / 备注**：[App.tsx](../src/App.tsx) `selectedRecords` 改为先建 `Set(selectedIds)` 再 `has`。`pnpm typecheck` + `pnpm lint` ✓。

### P3-3 · Lightbox `slides` 加 useMemo
- **位置**：[ImagePreviewLightbox.tsx:25-31](../src/components/browse/ImagePreviewLightbox.tsx#L25-L31)
- **方案**：`slides` 用 `useMemo` keyed on `images`，避免翻页时重复 map 大数组。
- **验证**：`pnpm lint && pnpm typecheck`。
- **状态**：✅ 已完成（2026-07-02） · **实测 / 备注**：[ImagePreviewLightbox.tsx](../src/components/browse/ImagePreviewLightbox.tsx) `slides` 用 `useMemo([images])` 包裹，翻页（`onIndexChange`）不再重建整份 slides 数组。`pnpm typecheck` + `pnpm lint` ✓。

### P3-4 · 清理重复 CSS 定义
- **位置**：`.image-grid`（[globals.css:485](../src/styles/globals.css#L485)、[:1478](../src/styles/globals.css#L1478)）、`.virtual-image-grid`（[:492](../src/styles/globals.css#L492)、[:1474](../src/styles/globals.css#L1474)）、`.image-card`（[:526](../src/styles/globals.css#L526)、[:1496](../src/styles/globals.css#L1496)）
- **方案**：合并重复规则，消除覆盖歧义。
- **验证**：`pnpm tauri dev` 目视确认样式无回归。
- **状态**：⏭️ 暂缓 · **实测 / 备注**：经比对，两处 `.image-card`/`.image-grid`/`.virtual-image-grid` **并非重复死规则，而是有意的层叠覆盖**（后块 `padding:0/background:transparent/rows auto 19px 16px` 覆盖前块 `padding:7px/background:var(--panel)`，像一层重设计）。合并需逐条比对中间所有交叠选择器并做视觉 QA，风险 > 收益；且重复 CSS 规则的运行时开销近乎为零（非真性能项）。留待有 `pnpm tauri dev` 环境时作为纯样式清理处理。

### P3-5 · 缩略图协议线程池
- **位置**：[lib.rs:326](../src-tauri/src/lib.rs#L326)
- **问题**：每协议请求 `std::thread::spawn`，无池（有 `Cache-Control` 缓存兜底故非致命）。
- **方案**：用有界线程池 / `spawn_blocking` 复用线程。
- **验证**：`cargo clippy --all-targets -- -D warnings && cargo test`。
- **状态**：⬜ 待办 · **实测 / 备注**：_

### P3-6 · LanceDB ANN 索引（规模化再做）
- **位置**：[lancedb_repo.rs:113-133](../src-tauri/src/repo/lancedb_repo.rs#L113-L133)（top_k）、[:219-247](../src-tauri/src/repo/lancedb_repo.rs#L219-L247)（建表）
- **问题**：从不 `create_index`，向量搜索 O(n) 暴力扫描。小库无碍，向量规模大时再加 IVF/HNSW。
- **验证**：`cargo test`；大向量集查询耗时对比。
- **状态**：⬜ 待办 · **实测 / 备注**：_

### P3-7 · BLAKE3 去掉冗余 BufReader
- **位置**：[hash_service.rs](../src-tauri/src/services/hash_service.rs) `hash_file`
- **方案**：已按 4MB 分块读，`BufReader` 冗余，直接读入 buf 省一次拷贝与 4MB/任务。
- **验证**：`cargo test`。
- **状态**：✅ 已完成（2026-07-02） · **实测 / 备注**：`hash_file` 去掉 `BufReader::with_capacity(4MB)`，直接 `File::read` 进 4MB `buf`（省一层同尺寸缓冲 + 每块一次拷贝）；移除未用的 `BufReader` 导入。既有 `hash_file_matches_known_value` 单测确认哈希值不变。`cargo test --lib` ✓、`clippy` ✓。

---

## 变更日志

| 日期 | 条目 | 摘要 | commit |
|------|------|------|--------|
| 2026-07-02 | — | 建立本文档，完成三层性能审查 | — |
| 2026-07-02 | P0-1 | 扫描导入改批量事务（探测事务外累积、单事务批量写、提交后入队缩略图） | 待提交 |
| 2026-07-02 | P2-3 | 扫描/详情/缩略图热路径改 `prepare_cached` | 待提交 |
| 2026-07-02 | P0-4 | `onPreviewImage` 用 `useCallback` 稳定，隔离虚拟网格重渲染 | 待提交 |
| 2026-07-02 | P0-3 | 部分：memo 化 `FilterBar`/`DetailPane` + 稳定其回调；深度 shell 拆分待实机 | 待提交 |
| 2026-07-02 | P0-5 | Timeline 改窗口函数 + 批量取详情，消除 N+1（241→~2 次扫描）；加测试 | 待提交 |
| 2026-07-02 | P0-6 | 部分：migration 0006 加 `idx_images_mtime` + COUNT 去 JOIN；FTS 搜索待实机 | 待提交 |
| 2026-07-02 | P1-1 | migration 0006：blake3/dhash pending 部分索引 + 纠正 phash→dhash 错配 | 待提交 |
| 2026-07-02 | P1-2 | migration 0006：`COALESCE(taken_at,mtime)` 表达式索引（EXPLAIN 确认命中） | 待提交 |
| 2026-07-02 | P0-2 | 人脸聚类去抖：仅在 pending 排空时重建一次（每轮 8→1 次 O(n²)） | 待提交 |
| 2026-07-02 | P3-1 | 3 处缩略图 `<img>` 加 `decoding="async"` | 待提交 |
| 2026-07-02 | P3-2 | `selectedRecords` 用 Set（O(n·m)→O(n)） | 待提交 |
| 2026-07-02 | P3-3 | Lightbox `slides` 用 `useMemo` | 待提交 |
| 2026-07-02 | P1-3 | 缩略图 JPEG DCT 降采样解码（新增 `jpeg-decoder` 依赖）；加测试 | 待提交 |
| 2026-07-02 | P1-4 | pending_* 改窄投影 `PendingImage`（6 列 vs 30 列，去 ocr_text） | 待提交 |
| 2026-07-02 | P2-5 | `probe_image` 单次打开文件（尺寸 + EXIF 复用句柄）；加单测 | 待提交 |
| 2026-07-02 | P3-7 | BLAKE3 去掉冗余 BufReader，直接读入 4MB buf | 待提交 |
| 2026-07-02 | P2-1 | 去重 exact/visual 分组 insert 包进单事务批量提交 | 待提交 |
| 2026-07-02 | P2-6 | 画廊列表查询用 `IMAGE_SELECT_LIST`（ocr_text 置 NULL），不读无界全文 | 待提交 |
