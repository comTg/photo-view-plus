# 04 · MVP2 · 去重

> 范围：完全重复（BLAKE3）+ 视觉近似重复（pHash）+ 去重对比 UI + 回收站 + 撤销。  
> 前置：MVP1 全部完成且端到端验收通过。

## 0. 目标与验收

**做完 MVP2 后，应能：**

1. 对已扫描的图库一键开启"查找重复"，进度可见
2. 完全重复（不同路径、相同字节）100% 检出，0 误报
3. 视觉近似（重压缩、改尺寸、轻微裁剪）漏报率 < 5%，误报率 < 2%（在固定 fixture 集上）
4. 去重对比界面流畅切换，支持键盘操作
5. 批量保留策略（最大分辨率/最新文件/最大体积/手动）一键应用
6. 删除全部走 Windows 回收站，30 天内可撤销
7. 可导出重复报告（CSV）

**禁止行为**：直接 `DeleteFile`；任何不可撤销的操作不写入主流程。

## 1. 模块边界

引入：

| 路径 | 引入 |
|------|------|
| `src/app/dedup/page.tsx` | 去重界面路由 |
| `src/components/dedup/` | 分组列表、对比视图 |
| `src/stores/dedupStore.ts` | 去重状态 |
| `src-tauri/src/commands/dedup.rs` `trash.rs` | 命令 |
| `src-tauri/src/services/{hash_service,phash_service,dedup_service,trash_service}.rs` | 业务 |
| `src-tauri/src/repo/duplicates_repo.rs` | 数据访问 |
| `src-tauri/src/migrations/0002_hash_columns.sql` | schema |
| `src-tauri/src/utils/bk_tree.rs` | pHash 加速结构 |

## 2. 任务清单

### T1 Migration 0002

**目标**：images 表追加哈希列；建去重分组表。

具体（来自 `docs/01` § 2）：
- `ALTER TABLE images ADD COLUMN blake3 TEXT;`
- `ALTER TABLE images ADD COLUMN phash INTEGER;`
- `ALTER TABLE images ADD COLUMN dhash INTEGER;`
- `ALTER TABLE images ADD COLUMN hash_status TEXT NOT NULL DEFAULT 'pending';`
- 索引：`idx_images_blake3`、`idx_images_phash`、`idx_images_hash_status`
- 新建 `duplicate_groups`、`duplicate_items`

**验收**：migration runner 在已有 MVP1 数据库上幂等执行。

### T2 BLAKE3 哈希服务

**目标**：异步计算 BLAKE3，写回 `images.blake3`。

具体：
- 依赖：`blake3` crate（with `rayon` feature 可并行）
- 任务：`HashTask { image_id }`，优先级 P2
- 流程：
  ```
  读 images 拿绝对路径
  打开文件流，4MB 块迭代喂入 blake3::Hasher
  完成 → UPDATE images SET blake3=?, hash_status='ready'
  失败（文件已删/无权限）→ hash_status='failed'
  ```
- 并发：本地盘 8、网络盘 2（与 root.type 关联）
- 网络盘超时 60s

**性能目标**：本地 NVMe，1000 张图（平均 5MB）< 60s。

**验收**：所有 hash_status='ready' 的行 blake3 非空且 hex 长 64。

### T3 pHash / dHash 服务

**目标**：基于缩略图算 pHash 与 dHash。

**算法实现**：直接用 [`image_hasher`](https://crates.io/crates/image_hasher) crate，**不自己实现 DCT**。这是 czkawka（11.0.1, MIT, 2026-02 仍维护）使用的同一个 crate，pHash / dHash / aHash / Wavelet / Blockhash 全套都有，10 行代码出结果。详见 ADR-005（`docs/08-roadmap.md`）。

具体：
- 依赖：`image_hasher = "2.x"`（运行时只依赖 `image` 与 `transpose`，无额外 GUI / 音频 / PDF 依赖）
- 配置：
  ```rust
  use image_hasher::{HasherConfig, HashAlg, FilterType};

  // pHash 8x8（64-bit），与 czkawka 默认一致
  let phash_cfg = HasherConfig::new()
      .hash_alg(HashAlg::Mean)            // 占位，下面覆盖
      .hash_size(8, 8)
      .resize_filter(FilterType::Lanczos3)
      .preproc_dct()                       // ← 启用 DCT 即 pHash
      .to_hasher();

  let dhash_cfg = HasherConfig::new()
      .hash_alg(HashAlg::Gradient)         // dHash
      .hash_size(8, 8)
      .to_hasher();
  ```
- 输入：从 thumbs 缓存读 256px WebP（**不要重新解原图**），喂给 `hasher.hash_image(&dyn_img)` → `ImageHash`，再 `.to_bytes()` → 8 字节 → 当 `u64` 存
- 任务优先级 P3
- 任务依赖：缩略图 `thumb_status='ready'` 才能跑
- 并发：用 `rayon` 在 worker 内 batch 处理（czkawka 同方案）
- 失败处理：解码失败 → `hash_status='failed'`，不阻塞队列

**为什么不自己写 DCT**：DCT 是数学题不是工程题，自实现没有任何业务价值；`image_hasher` 在 czkawka 上跑过百万级图库的实战验证。

**验收**：用 `tests/fixtures/phash_pairs/`（10 组人工标注的"重压缩对"）计算 hamming distance（`hash_a.dist(&hash_b)`），全部 ≤ 6。

### T4 BK-tree 索引（pHash 加速查询）

**目标**：在 100 万张图中找"hamming ≤ N"的 top-K，亚秒级。

具体：
- 新建 `src-tauri/src/utils/bk_tree.rs`，纯 Rust BK-tree 实现
- 启动时若 `images.phash` 行数 > 1 万，从 SQL 加载所有 phash 建树
- 树常驻内存，新增图片增量插入
- API：
  ```rust
  fn find_within(&self, q: u64, max_dist: u32) -> Vec<(image_id, dist)>
  ```
- 复杂度：典型 O(log N)；备用线性扫不超过 100ms / 10 万

**验收**：100 万随机 64-bit 查询 < 5ms / query。

### T5 重复分组算法（完全重复）

**目标**：把 blake3 相同的图聚成 group。

具体：
- SQL：
  ```sql
  SELECT blake3, COUNT(*) c, GROUP_CONCAT(id) ids
  FROM images
  WHERE blake3 IS NOT NULL AND deleted_at IS NULL
  GROUP BY blake3 HAVING c > 1;
  ```
- 每组写 `duplicate_groups(method='exact', threshold=NULL)` + N 条 `duplicate_items(similarity=1.0)`
- 增量：第一次扫全表；后续只对新 hash 查"该 hash 是否已存在过 1 个"，是则加入对应 group 或新建

**验收**：人造 10 对完全相同文件（仅改文件名），全部聚为 10 个 group。

### T6 重复分组算法（视觉近似）

**目标**：把 phash hamming ≤ 阈值（默认 6）的图聚成 group。

具体：
- 算法（增量）：
  ```
  对新计算 phash 的图 X：
    1. BK-tree.find_within(X.phash, 6) → 候选 [(Y, dist)]
    2. 过滤掉与 X 已在同一个 exact group 的（已经完全重复，没必要再加近似）
    3. 候选不空 → 找候选 Y 已属于的 phash group；若有就把 X 加入；若无就建新 group
    4. 多组合并：若 X 桥接了两个原本独立的 phash group，合并两组
    5. 写 duplicate_items，similarity = 1 - dist/64
    6. 把 X.phash 插入 BK-tree
  ```
- 这个算法是"近似并查集"，BK-tree 配 union-find 实现
- 阈值可配置（设置页"近似严格度"：3/6/10）

**验收**：用 `tests/fixtures/phash_groups/` 30 组人工聚类，对比算法输出，F1 ≥ 0.92。

### T7 去重命令层

**目标**：前端能启动、暂停、查看去重。

具体（命令）：
- `dedup_start({ method: 'exact'|'phash'|'both', root_ids? })`：把没 hash 的图入队，hash 完后自动跑分组
- `dedup_status() -> { running, processed, total, groups_found }`
- `dedup_groups({ method, status, page }) -> DupGroupPage`
- `dedup_group_detail(group_id) -> { group, items: ImageWithThumb[] }`
- `dedup_resolve(group_id, keep_image_ids: number[], action: 'trash'|'keep_all'|'dismiss')`
- `dedup_undo(undo_id)`

事件：
- `dedup:progress` 每秒
- `dedup:new_group` 新 group 增量推送
- `dedup:resolved` 用户操作完成

### T8 回收站服务

**目标**：把文件移到 Windows 回收站，写撤销日志。

具体：
- 依赖：`trash` crate（v3+），底层调 Windows Shell `IFileOperation`
- 流程：
  ```
  对每个 image_id：
    1. 读绝对路径
    2. trash::delete(path)（系统会提示同名时自动处理）
    3. UPDATE images SET deleted_at=now()
    4. INSERT undo_log: action='trash', payload={image_id, original_path}
  ```
- 失败处理：文件已不在 → 标 deleted_at 但记 warn；权限拒绝 → 返回错误展示给用户
- 撤销：
  ```
  undo_log → 读 payload.original_path
  调 Shell 从回收站恢复（trash crate 不支持，要自己用 windows-rs 的 IFileOperation::MoveItem）
  UPDATE images SET deleted_at=NULL
  ```
- 撤销窗口：30 天，超时清理任务每天跑一次

**测试**：单元测试用临时目录，避免污染真实回收站；集成测试在 Windows VM 跑。

**验收**：删 10 张 → 回收站有 10 项；撤销 → 文件回原位置，DB 同步。

### T9 去重对比界面

**目标**：用户能高效逐组处理。

具体（参考 `docs/02` § 2.2）：
- 路由 `/dedup`
- 左侧：分组列表（虚拟列表，分组方法切换 Tab）
- 中间/右侧：A/B 大图对比 + 数据对照（分辨率、大小、时间、来源 root）
- 键盘：`↑↓` 切组、`←→` 切 A/B 焦点（多于 2 张时循环）、`Enter` 应用、`Space` 标 dismiss、`U` 撤销
- 底部固定栏：批量规则下拉、"应用到全部 N 组"按钮（含二次确认）
- 删除后即时移除该组并显示 toast 含撤销

**验收**：连续处理 50 组无卡顿；键盘可全程操作。

### T10 批量保留规则

**目标**：一次性处理多组，按规则自动选保留项。

具体：
- 规则：
  - `largest`：分辨率（width*height）最大
  - `newest`：mtime 最新
  - `biggest_file`：文件最大
  - `shortest_name`：filename 字符数最少
  - `prefer_root`：用户指定的 root 优先
- 应用前 UI 显示 dry-run 预览（哪些会被删）
- 二次确认 modal
- 批量删除任务化，进度条 + 中途可暂停

**验收**：dry-run 与实际删除结果一致；中途暂停不留半完成态。

### T11 撤销 UI

**目标**：30 天内任何删除都能撤销。

具体：
- 设置页或顶部菜单"历史" → 列出最近 undo_log（未 undone）
- 每条显示：动作、图片数、时间、占用空间
- 单条撤销 / 批量撤销
- 自动清理：每天启动时删 `can_undo_until < now` 的 undo_log + 真正从回收站清除（**可选，默认保持文件在回收站让 Windows 管**）

**验收**：删 → 重启 → 还能撤销。

### T12 导出 CSV 报告

**目标**：用户能在 Excel 里审查重复情况。

具体：
- 列：`group_id, method, similarity, image_id, root, rel_path, filename, size, width, height, mtime, blake3, phash, kept`
- UI：去重页右上角"导出"按钮，调 Tauri `dialog.save`
- 编码 UTF-8 BOM（Excel 友好）

**验收**：在 Excel 打开 1 万行 CSV 正常显示中文。

### T13 性能优化与监控

**目标**：去重大库不会拖死浏览。

具体：
- BLAKE3 与 pHash 队列让出 CPU 给 P0 缩略图
- 文件读取限速：本地盘 默认无限；网络盘 100MB/s 上限
- 进度面板增加"哈希中"、"近似检测中" 两项
- 内存监控：BK-tree 占用 = N * 24 字节（100 万 phash 约 24MB，可接受）

**验收**：扫 + 哈希 + pHash 三流水线并行时，UI FPS 不低于 30。

### T14 MVP2 端到端验收

清单：
1. MVP1 库（5 万张）启动去重 → 哈希完成 < 5 分钟
2. 完全重复 100% 命中
3. 视觉近似在 fixture 集上 F1 ≥ 0.92
4. 删 100 张 → 回收站有 100 项 → 全部撤销成功
5. 导出 CSV 在 Excel 打开正常
6. 24 小时不操作 → 流程不卡死

## 3. 依赖关系

```
T1 ─→ T2 ─→ T5
       │
       ▼
       T3 ─→ T4 ─→ T6
                   │
                   ▼
                   T7 ─→ T9 ─→ T10 ─→ T14
                          │
                          └─→ T8 ─→ T11 ─→ T12
                                       │
                                       └─→ T13
```

## 4. 关键技术点与坑

- **借鉴 czkawka，不依赖 czkawka_core**：见 ADR-005。我们用它依赖的 `image_hasher` crate，不要把整个 czkawka_core 引入——它包含音频/PDF/视频/压缩档大量我们不用的依赖，且 API 是"自带扫描器"而非 library 友好。算法细节、阈值经验、特殊格式跳过策略可以从它的 `czkawka_core/src/tools/similar_images.rs` 学习（MIT，保留 license 头可抄）
- **pHash 一定基于缩略图**：原图大 + 解码慢，每张图都重新解原图会让 pHash 任务慢 10 倍
- **BK-tree 启动时构建一次**：不要每次查询都重建，但插入要增量
- **回收站撤销在 Windows 上没有官方 Rust crate 全功能支持**：需要自己 binding `IFileOperation::MoveItem`；提前在 Windows VM 验证
- **同组阈值合并风险**：phash 阈值放太大（>10）会把不相关的图也聚一起。默认 6 是经验值，做成可调
- **Hash 与扫描的事务边界**：避免哈希计算时 image 被删除导致 NULL 写回；用 `WHERE id=? AND deleted_at IS NULL` 保护
- **批量删除的事务**：100 张图一个事务 vs 每张一个事务 → 选每张一个事务但用 prepared statement 复用，比批量事务更利于中途暂停
- **网络盘上的 BLAKE3 = 全文件读 = 流量爆炸**：用户可在 root 设置里"跳过此 root 哈希"或"仅本地盘哈希"

## 5. 完成判据

- 14 个任务的"验收"段全部打勾
- 在 fixture 集上自动跑误报/漏报基准（CI 上跑）
- MVP2 用户文档（怎么用、怎么撤销）写到 `README.md`
- 在 `docs/08-roadmap.md` 记 ADR：方法选型与阈值依据
