# 03 · MVP1 · 基础浏览

> 范围：选目录 → 扫描 → 缩略图 → 瀑布流 → 详情面板 → 基础筛选。  
> 不做：去重、AI、OCR、人脸、地图、智能相册。

## 0. 目标与验收

**做完 MVP1 后，应能：**

1. 在空安装上点击"添加目录"，选 `D:\Photos`（含 5 万张图），看到扫描进度
2. 扫描过程中瀑布流持续出图，已生成缩略图的卡片立刻可见
3. 滚动 10 万张图的瀑布流，FPS ≥ 55
4. 点击图片，右侧详情显示文件名、路径、尺寸、EXIF 时间、相机
5. 用筛选条件（格式 = HEIC、大小 > 5MB）实时过滤
6. 重启后所有数据保留，再次扫描只处理变更
7. 完整中文 UI；浅色/深色模式可切换

**不达标准的指标禁止进入 MVP2**：扫描中 UI 卡顿、缩略图过几分钟才出、关闭重启数据丢失。

## 1. 模块边界

涉及（按 `docs/00` 的目录骨架）：

| 路径 | 引入 |
|------|------|
| `src/app/page.tsx` `layout.tsx` | 主页 + 三栏布局 |
| `src/components/browse/`, `src/components/detail/` | 卡片、瀑布流、详情 |
| `src/stores/appStore.ts` `scanStore.ts` `browseStore.ts` | 三个 store |
| `src/lib/tauri.ts` `events.ts` `tauri-types.ts` | invoke 封装 |
| `src-tauri/src/commands/{roots,scan,images}.rs` | 命令层 |
| `src-tauri/src/services/{scan_service,thumbnail_service}.rs` | 业务 |
| `src-tauri/src/repo/{roots_repo,images_repo}.rs` | 数据访问 |
| `src-tauri/src/queue/` | 任务队列 |
| `src-tauri/src/migrations/0001_init.sql` | 表结构 |

**不引入**：`ai_client.rs`、`dedup_service.rs`、`face_*`、`ocr_*`、Python worker、LanceDB。

## 2. 任务清单（按依赖顺序，每条独立可执行）

### T1 项目骨架与多环境配置

**目标**：`pnpm tauri dev` 能起一个空窗口，cargo / pnpm 命令链路畅通。

具体：
- 手动建文件：`package.json` / `vite.config.ts` / `index.html` / `tsconfig.json` / `biome.json`
- `src/main.tsx` + `src/App.tsx` + `src/styles/globals.css` 三栏 placeholder
- 三套 `tauri.<profile>.json`（dev/test/prod，partial overlay 而非 extends）
- `package.json` 加 `tauri:dev` / `tauri:test` / `tauri:prod` 三条脚本，用 `cross-env PVP_PROFILE=<x>` 注入
- `src-tauri/Cargo.toml`：tauri 2 + tauri-plugin-{dialog,shell,fs} + serde + anyhow + thiserror + tracing
- `src-tauri/src/{main.rs,lib.rs,config.rs,commands.rs}`：最小骨架，ping 命令返回 profile
- `.cargo/config.toml`：sparse 镜像（项目本地，避免全局 git 协议慢）
- Biome + tsconfig 与 smart-search 对齐
- 提交：仅骨架，`pnpm tauri:dev` 可启动空白三栏页

**验收**：`pnpm tauri:dev` 起窗口；`cargo clippy --all-targets -- -D warnings` 通过。

### T2 SQLite 与 Migration 框架

**目标**：首次启动自动建库；后续可加 migration。

具体：
- 依赖：`rusqlite`（features = `bundled`）+ `r2d2_sqlite` + `r2d2`
- 写 `src-tauri/src/db.rs`：连接池 + 启动时 `PRAGMA foreign_keys=ON; journal_mode=WAL`
- 写 `src-tauri/src/migrations/runner.rs`：读取 `migrations/*.sql` 按编号执行，`schema_version` 表登记
- 写 `migrations/0001_init.sql`：包含 `schema_version` + `roots` + `images`（MVP1 字段集，无 hash/ai/ocr 列）+ `scan_tasks` + `undo_log`
- 在 `tauri::Builder::default().setup()` 里启动连接池与 migration
- 写 5 个单元测试：建表、二次启动幂等、加新 migration 生效、外键约束、WAL 模式生效

**验收**：`cargo test db::tests`；删 `app.sqlite` 后启动重建。

### T3 Roots 管理

**目标**：能加目录、列目录、删目录。

具体：
- `src-tauri/src/repo/roots_repo.rs`：`insert / list / remove / update_label`
- `src-tauri/src/utils/path_normalize.rs`：规范化绝对路径（详见 `docs/01` § 6）
- `src-tauri/src/commands/roots.rs`：
  - `roots_add(path: PathBuf) -> Root`：判断本地/网络、规范化、唯一冲突时返回错误
  - `roots_list() -> Vec<Root>`
  - `roots_remove(id: i64) -> ()`：级联删 images（FK CASCADE 自动）
  - `roots_update(id, label, enabled, settings) -> Root`
- `src/components/sidebar/RootList.tsx`：列出 roots，右键菜单
- `src/components/dialogs/AddRootDialog.tsx`：调 Tauri `dialog.open({directory: true})`

**验收**：UI 加 root → 数据库有记录；删 root → 记录消失；重复路径返回友好错误。

### T4 任务队列调度器

**目标**：通用的多优先级任务队列，扫描、缩略图、（未来的）哈希都用它。

具体：
- 新建 `src-tauri/src/queue/`：
  - `Task` trait：`fn priority(&self) -> Priority; async fn run(&self) -> Result<()>`
  - `Priority` enum：`P0..P7`（与 `docs/00` § 3 对齐）
  - `Scheduler`：每优先级一个 `tokio::sync::mpsc`，主循环 `select!` + 高优抢占
  - 并发控制：`tokio::sync::Semaphore`，本地盘 16、网络盘 4（按 root 区分）
  - 暂停 / 恢复 / 取消：用 `tokio_util::sync::CancellationToken`
- 测试：注入 50 个 mock 任务，断言高优先级先完成、cancel 立即生效
- emit 事件：每秒一次 `queue:status`（各队列长度）

**验收**：`cargo test queue::tests`；scheduler 在调试日志里能看到任务优先级跳跃。

### T5 目录扫描（递归 + 增量）

**目标**：把目录里的图片记到 `images` 表，二次扫只处理变更。

具体：
- 依赖：`walkdir` + `tokio::fs`
- 支持后缀：`jpg jpeg png webp heic heif bmp tiff gif`（heic 解码留到 T7）
- 增量识别：
  ```
  对每个文件：
    SELECT id, mtime, size_bytes FROM images WHERE root_id=? AND rel_path=?
    若不存在 → INSERT，提交缩略图任务
    若存在但 mtime/size 不同 → UPDATE，标 thumb_status='pending'，提交缩略图任务
    否则 → 跳过
  扫描完后，对该 root 下所有当前未访问的文件 → 标 deleted_at=now()（软删）
  ```
- 断点续扫：`scan_tasks.cursor_json` 存上次扫到的子目录路径，恢复时跳过已完成
- 网络盘特殊处理：并发≤4、IO 超时 30s、失败入 `scan_failures` 临时表后续重试

**验收**：
- 5 万图首扫 < 60s（本地 NVMe）
- 二次扫无变更 < 5s
- 改 1 个文件 mtime，扫描后只 update 1 行
- 网络盘断开 → 任务挂 paused，恢复后续扫

### T6 EXIF 读取

**目标**：扫描时顺手读 EXIF 写入 images 表。

具体：
- 依赖：`kamadak-exif`（rust）
- 字段：`taken_at`、`width/height`、`orientation`、`gps_lat/lng`、`camera_make/model`
- 解析失败不阻塞扫描，只记 warn
- 单元测试：5 张不同相机的样本图，断言字段对齐

**验收**：详情面板能看到相机型号与拍摄时间。

### T7 缩略图生成

**目标**：从原图生成 256px WebP，写入缓存目录。

具体：
- 依赖：`image` crate（jpeg/png/webp 默认开），HEIC 用 `libheif-rs`（feature 标 `heic`）
- 流程：
  ```
  1. 读 EXIF Orientation，决定旋转
  2. image::open 解码
  3. resize_with_filter(Lanczos3) 到长边 256
  4. WebP 编码（quality 78）写到 thumbs/<ab>/<hash>.webp
  5. update images: thumb_status='ready', thumb_hash=...
  ```
- 失败处理：
  - 解码失败 → `thumb_status='failed'`，记 error 列（新加在 0001？决定加 `thumb_error TEXT`）
  - 不支持格式 → `thumb_status='unsupported'`
- 优先级 P0，与扫描 P1 并发跑（扫描喂入新文件，缩略图立刻拣起来做）
- CPU 限流：缩略图生成是完整解码 + resize + WebP 编码，必须有专用 CPU semaphore；默认按逻辑核数取一半，1-2 核为 1，封顶 6，避免大图库首扫把 CPU 长时间打满导致 UI 卡顿

**验收**：
- 1 万张 JPG 出图 < 90s（CPU 8 核）
- 缩略图目录布局符合 `<ab>/<hash>.webp`
- 删原图、重扫，缩略图自动失效重建

### T8 扫描进度事件

**目标**：Rust 端把进度发给前端。

具体：
- 事件：`scan:progress` payload `{ root_id, processed, total, current_file, eta_sec }`，每秒 1 次
- 事件：`scan:done` payload `{ root_id, added, updated, removed }`
- 事件：`scan:error` payload `{ root_id, error, file? }`
- 队列状态：`queue:status` payload `{ thumb, hash, ai }`
- 前端在 `src/lib/events.ts` 封装 `useEvent(name, handler)` hook

**验收**：DevTools 网络面板能看到事件（Tauri 通过 IPC，devtools 看不到，改用 console.log 验证）。

### T9 镜像查询 API

**目标**：前端能按各种条件分页拉图片列表。

具体：
- `images_query(params) -> ImagePage`：
  ```ts
  params = {
    rootIds?: number[],
    tagIds?: number[],            // MVP1 不传
    formats?: string[],
    sizeMin?: number, sizeMax?: number,
    takenFrom?: number, takenTo?: number,
    sort: { field: 'taken_at'|'mtime'|'filename'|'size', dir: 'asc'|'desc' },
    offset: number, limit: number,
    includeDeleted: false,
  }
  → { items: Image[], total: number, offset, limit }
  ```
- 查询要走索引：`(root_id, taken_at DESC)`、`(extension)` 等
- `images_get_detail(id) -> ImageDetail`：含 EXIF 全字段

**验收**：10 万行数据，无 tag 时分页 < 50ms；按 extension 过滤 < 80ms（用 `EXPLAIN QUERY PLAN` 验证用了索引）。

### T10 缩略图 HTTP 协议

**目标**：前端 `<img src="...">` 能直接显示缩略图。

具体：
- 用 Tauri 2 的 `register_uri_scheme_protocol("thumb", ...)`：
  - URL：`thumb://<image_id>?size=256`
  - 处理器查 `images.thumb_hash`，读 `thumbs/<ab>/<hash>.webp` 返回
  - 加 `Cache-Control: max-age=86400`
- 前端工具：`thumbUrl(imageId, size=256)`

**验收**：浏览器 DevTools Network 看缩略图请求 < 5ms（本地 IO）；同一张图二次加载走 webview cache。

### T11 三栏布局与瀑布流

**目标**：跑 `pnpm tauri dev` 看到 `docs/02` 的主界面。

具体：
- 路由：单页 `src/app/page.tsx`，三栏 grid 布局
- 左侧栏：T3 的 `<RootList>` + 占位的"我的相册" / "标签" / "智能相册"（折叠分组，MVP1 内容空）
- 中间：`<ImageGrid>` 用 `react-virtuoso` 的 `VirtuosoGrid`
  - 卡片大小 = 当前视图档位（lg/md/sm 三档），瀑布流让宽度自动充满
  - 滚到底触发 `loadMore`
  - 选中态、悬停态、双击全屏
- 右侧：`<DetailPane>`，根据 `appStore.selection` 渲染
- 顶部工具栏：视图切换、排序、筛选弹层、搜索框占位（MVP1 仅文件名模糊搜）

**验收**：人工测试三栏均可拖宽；瀑布流滚动 60fps；详情面板内容跟随选中变化。

### T12 文件名搜索 + 基础筛选

**目标**：搜索框输入实时过滤；筛选面板支持多条件叠加。

具体：
- 搜索：`browseStore.filters.q = "<text>"`，触发 `images_query` 重查，前端 debounce 250ms
- 筛选弹层：
  - 格式（多选）
  - 文件大小（双向 slider）
  - 时间范围（react-day-picker）
  - 仅显示有 GPS / 仅显示无 GPS（三态切换）
- 工具栏 + 筛选面板用 Radix `Popover` 实现

**验收**：搜索 "IMG" 即时反应；筛选 "PNG + >5MB + 2025 拍摄" 返回正确结果。

### T13 多选与基础批量操作（无删除）

**目标**：能多选，能复制路径、能"打开所在文件夹"，**不**做删除（删除留给 MVP2 的回收站方案）。

具体：
- 选择模式开关在工具栏；进入后卡片显示 checkbox
- 快捷键：Click / Shift+Click / Ctrl+Click / Ctrl+A
- 选中态写 `appStore.selection: number[]`
- 操作：复制路径（拼系统路径）、打开所在文件夹（`tauri-plugin-shell` 的 `revealItemInDir`）
- 上下文菜单（右键）：同样的两项

**验收**：选 100 张后批量"复制路径"剪贴板得到 100 行。

### T14 详情面板完整化

**目标**：右侧显示 EXIF 完整字段；文件名可编辑（重命名走 OS）。

具体：
- `<DetailPane>` 分块：缩略图、基本、EXIF、（占位）标签、（占位）相似
- 文件名编辑：双击进入编辑态，回车提交；调 `images_rename` 命令，写 OS + DB + undo_log
- 地图：MVP1 用文字"上海 徐汇区"（暂留经纬度，反查城市可走 EXIF GPS → 离线 GeoIP？先空，标 TODO）

**验收**：在 5 张不同设备拍的图上目视核对 EXIF；重命名后磁盘文件与 DB 同步。

### T15 设置页（最小）

**目标**：基本设置可见可改。

具体：
- 路由 `/settings`
- 项：语言（zh/en）、主题（系统/浅/深）、缩略图大小上限（GB）、扫描并发（本地/网络）
- 持久化到 `config.json`（用 Rust `serde_json`，启动时读、修改时写）

**验收**：改完重启生效；缩略图缓存上限超出时触发 LRU 清理（实现入 T7 衍生）。

### T16 MVP1 端到端验收

**目标**：以"假装新用户"的视角跑一遍。

清单：
1. 全新安装（清空 `%LOCALAPPDATA%\PhotoViewPlus\dev\`）
2. 启动 → 空状态欢迎页
3. 点"添加目录"→ 选 `D:\Photos`（5 万张）
4. 立刻能在瀑布流看到占位卡片
5. 30 秒内能看到 ≥ 1000 张缩略图
6. 切到不同视图档位流畅
7. 选 1 张 → 详情正确
8. 搜 "IMG_2025" → 命中正确
9. 重启 → 数据齐全，二次扫 < 5s
10. 关闭应用 → 后台任务正确停止，不留僵尸进程

记录每条验收的耗时 / FPS / 内存峰值到 `docs/08-roadmap.md`。

## 3. 依赖关系（任务图）

```
T1 ─→ T2 ─→ T3
       │    │
       │    ▼
       │    T4 ─→ T5 ─→ T6
       │         │
       │         ▼
       │         T7 ─→ T10
       │         │
       │         ▼
       │         T8
       │              │
       │              ▼
       │              T9 ─→ T11 ─→ T12 ─→ T13 ─→ T14 ─→ T15 ─→ T16
       │                                                       ▲
       └───────────────────────────────────────────────────────┘
```

T1-T4 是单线依赖。T5-T9 是后端管线，可在 T11-T14 前后端并行推进时分头跑。

## 4. 关键技术点与坑

- **不要把扫描跑在 Tauri command handler 里**：command 只入队 + 返回，循环靠 scheduler
- **WebP 编码用 `image::codecs::webp::WebPEncoder`** 而不是外部 `cwebp` 进程，避免 Windows 上 dll 依赖
- **react-virtuoso 的 `VirtuosoGrid` 不会自动测量元素**：必须给固定 `itemHeight` 或精确实现 `useWindowScroll`
- **EXIF Orientation 必须在缩略图生成时旋转**：否则 lightbox 与列表显示不一致
- **HEIC 在 MVP1 内可降级**：若 `libheif` 集成成本大，先 `thumb_status='unsupported'` 占位，MVP1.1 补
- **r2d2 连接池上限设 8-16**：太小会卡，太大会 SQLite busy
- **WAL 模式下要定期 `PRAGMA wal_checkpoint(PASSIVE)`**：长时间运行不 checkpoint，WAL 文件会膨胀到 GB 级；调度器空闲时跑

## 5. 完成判据

- 所有 16 个任务的"验收"段都打勾
- `cargo clippy --all-targets -- -D warnings` 0 警告
- `pnpm typecheck` 0 错误
- `cargo test` 全绿
- 在 Windows 11 + 5 万图样本上跑完 T16 全部步骤，记录指标
- 写一条 ADR 到 `docs/08-roadmap.md`：MVP1 完成与遗留事项
