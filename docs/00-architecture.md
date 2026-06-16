# 00 · 总体架构

> 这是项目的"地图"。改任何跨模块的事先读这里，再去具体的 MVP 文档。

## 1. 分层架构

```
┌───────────────────────────────────────────────────────────────┐
│  L1 UI 层（Next.js 15 SSG + React 19）                        │
│  ───────────────────────────────────────────────────────────  │
│  瀑布流 · 详情面板 · 去重对比 · 搜索 · 设置 · 进度面板        │
│  Zustand 状态 · Radix + Tailwind · react-virtuoso 虚拟列表    │
└───────────────────────────────────────────────────────────────┘
                       ▲   │ Tauri invoke / Tauri event
                       │   ▼
┌───────────────────────────────────────────────────────────────┐
│  L2 Core Service（Rust, src-tauri/）                          │
│  ───────────────────────────────────────────────────────────  │
│  目录扫描 · 缩略图生成 · 文件操作 · 回收站 · 文件监听         │
│  任务队列调度（priority queue）· EXIF 读取                    │
└───────────────────────────────────────────────────────────────┘
                       ▲   │
                       │   ▼
┌───────────────────────────────────────────────────────────────┐
│  L3 Index Service（Rust）                                     │
│  ───────────────────────────────────────────────────────────  │
│  SQLite 元数据 · pHash / dHash 索引（BK-tree）                │
│  LanceDB 向量索引 · 标签倒排                                  │
└───────────────────────────────────────────────────────────────┘
                       ▲   │ HTTP 127.0.0.1:port / IPC
                       │   ▼
┌───────────────────────────────────────────────────────────────┐
│  L4 AI Worker（Python，ai-worker/，独立子进程）               │
│  ───────────────────────────────────────────────────────────  │
│  CLIP embedding · RAM/Florence 标签 · PaddleOCR · InsightFace │
│  PyTorch CUDA / ONNX Runtime CUDA / DirectML / CPU 回退       │
└───────────────────────────────────────────────────────────────┘
```

每一层只依赖下层，反向通过事件。L1 永远不直接读文件、不直接连数据库。L4 永远不直接 emit 事件给 L1，必须经 L3/L2 转发。

## 2. 进程模型

| 进程 | 启动方 | 生命周期 | 说明 |
|------|--------|----------|------|
| **Tauri Main**（Rust） | 用户点击图标 | 直到用户退出 | 持有窗口、菜单、托盘；Tauri 1 个，承载 L2/L3 |
| **WebView2** | Tauri Main 启动 | 跟随主进程 | 渲染 L1 |
| **AI Worker**（Python） | Tauri Main 按需启动 | 进入需要 AI 的页面时拉起，闲置 N 分钟后回收 | 独立子进程；崩了主程序不死 |
| **Scan Worker**（Rust 线程池） | Tauri Main 内部 | 跟随主进程 | tokio 任务池，本地盘 8-16 并发，网络盘 ≤4 |

AI Worker 通过 `127.0.0.1:<random port>` 的 HTTP 服务暴露能力。端口随机以避免冲突，启动后由 Rust 端记录。

## 3. 任务队列与优先级

后台任务全部入队，按优先级抢占。**永远不在 Tauri command handler 里同步做这些事**。

```
P0 缩略图生成        ←  浏览体验依赖，最高优先
P1 元数据扫描        ←  发现新文件 / 增量扫描
P2 BLAKE3 哈希       ←  去重需要
P3 pHash / dHash     ←  去重需要
P4 OCR               ←  文档/截图搜索
P5 AI 标签           ←  非关键
P6 CLIP embedding    ←  非关键
P7 人脸 embedding    ←  默认关
```

实现方式：单进程多优先级队列（`crossbeam-channel` 多通道 + 主调度循环），不要做成多优先级混跑。低优先级任务在高优先级队列非空时让出 CPU。

## 4. 数据流（用户加目录到能搜图）

```
[User] 选目录 → L1 入口
     │
     ▼
[L2] add_root() → 写 SQLite roots 表 → 提交 P1 扫描任务
     │
     ▼
[Scan Worker] walk_dir → 检测新增/变更 → 写 images 表（id / path / size / mtime）
                                              │
                                              ├─→ 提交 P0 缩略图任务
                                              ├─→ 提交 P2 BLAKE3 任务
                                              └─→ 提交 P3 pHash 任务
     │
     ▼
[L1] 订阅 scan:progress 事件，瀑布流增量出图（先占位 → 缩略图就绪 → 替换）
     │
     ▼
[MVP3 起] 缩略图入库后 → 提交 P5 标签任务 + P6 CLIP 任务到 AI Worker
                              │
                              ▼
                        [L4] CLIP forward → 返回 512 维向量
                              │
                              ▼
                        [L3] 写 LanceDB
     │
     ▼
[User] 输入"海边日落" → L1 → L2 forward → L4 文本编码 → L3 LanceDB 查询 → top-k 返回 → L1 渲染
```

## 5. 技术栈决策表

| 维度 | 选定 | 备选 | 理由 |
|------|------|------|------|
| 桌面壳 | **Tauri 2** | Electron | 体积 10-20MB vs 100MB+；内存占用低；用户已有 smart-search 经验 |
| 前端框架 | **Next.js 15 + SSG** | Vite + React | 与 smart-search 一致，可复用组件库与构建经验 |
| 状态管理 | **Zustand** | Redux Toolkit | smart-search 同选；样板代码少 |
| UI 库 | **Radix UI + Tailwind** | shadcn / MUI | 无障碍好；与 smart-search 同 |
| 虚拟列表 | **react-virtuoso（grid）** | masonic / react-window | grid 模式直接支持瀑布流；维护活跃 |
| Rust 异步 | **tokio** | async-std | 生态绝对主流 |
| 数据库 | **SQLite（rusqlite + r2d2 连接池）** | Postgres / DuckDB | 单文件、零运维；够撑 100 万行 |
| 向量库 | **LanceDB** | sqlite-vec / Qdrant local | LanceDB 列存性能好且嵌入式；sqlite-vec 在 50 万向量后明显变慢；Qdrant 要独立进程 |
| 缩略图编码 | **WebP（libwebp via image-rs）** | AVIF / JPEG | 体积/速度平衡最好；浏览器原生解 |
| 全文索引（MVP4） | **SQLite FTS5** | Tantivy | FTS5 与主库同事务；smart-search 用 Tantivy 但本项目场景 FTS5 足够 |
| 图片解码 | **image-rs + libheif（HEIC）** | libvips | image-rs 纯 Rust，无 dll 依赖；HEIC 单独走 libheif |
| AI 框架 | **PyTorch CUDA → ONNX Runtime CUDA** | TensorRT / DirectML | 开发期 PyTorch，产品化转 ONNX；DirectML 仅作 CPU 回退之上的备选 |
| Python 进程托管 | **tauri-plugin-shell + tokio::process** | sidecar 二进制打包 | 模型路径解耦；开发期可热重启 |
| 包管理 | **pnpm + cargo + uv（Python）** | npm / pip | uv 装 Python 依赖快且锁定 |
| 测试 | **cargo test + vitest** | nextest | nextest 后续可加 |

### 为什么不选 Electron + Node

- 内存占用：100 万张图扫描时 Electron 主进程峰值 1.5GB+；Tauri Rust 端在 500MB 以内
- 文件 IO：Rust + tokio 比 Node 的 fs 快 2-3 倍，扫描大目录差距明显
- AI 集成：Python worker 与 Rust 通过 HTTP 解耦，与 Node 通过 child_process 复杂度相同
- 用户已熟悉 Tauri 2 / Rust，不需要为开发速度妥协

### 为什么 SQLite + LanceDB 而不是单一存储

- **SQLite** 处理关系数据（图片元数据、标签、扫描任务、用户设置）天生最优
- **LanceDB** 处理向量列存查询，10 万向量级别比 sqlite-vec 快 5-10 倍
- 两者都是嵌入式、单文件目录，没有运维成本
- 一致性靠应用层：写完 SQLite 再写 LanceDB，失败时记 outbox 重试

## 6. 目录结构骨架

```
photo-view-plus/
├── CLAUDE.md
├── README.md
├── docs/                            # 本目录
├── package.json                     # pnpm workspace 根
├── pnpm-workspace.yaml
├── next.config.ts                   # output: 'export'
├── biome.json
├── tsconfig.json
├── src/                             # 前端
│   ├── app/                         # Next.js App Router（SSG）
│   │   ├── layout.tsx
│   │   ├── page.tsx                 # 主浏览页（瀑布流）
│   │   ├── dedup/page.tsx           # 去重界面（MVP2）
│   │   ├── search/page.tsx          # 高级搜索（MVP3）
│   │   └── settings/page.tsx
│   ├── components/
│   │   ├── browse/                  # 瀑布流、卡片
│   │   ├── detail/                  # 右侧详情
│   │   ├── dedup/                   # 去重对比
│   │   ├── search/                  # 搜索框
│   │   ├── progress/                # 任务进度
│   │   └── ui/                      # Radix 封装
│   ├── stores/                      # Zustand
│   │   ├── appStore.ts
│   │   ├── scanStore.ts
│   │   ├── dedupStore.ts            # MVP2
│   │   └── aiStore.ts               # MVP3
│   ├── hooks/
│   ├── lib/
│   │   ├── tauri.ts                 # invoke 封装
│   │   └── events.ts                # event 订阅
│   └── styles/
│
├── src-tauri/                       # Rust 后端
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── src/
│   │   ├── main.rs
│   │   ├── lib.rs
│   │   ├── commands/                # Tauri command handlers
│   │   │   ├── roots.rs
│   │   │   ├── scan.rs
│   │   │   ├── images.rs
│   │   │   ├── dedup.rs             # MVP2
│   │   │   └── ai.rs                # MVP3
│   │   ├── services/                # 业务服务
│   │   │   ├── scan_service.rs
│   │   │   ├── thumbnail_service.rs
│   │   │   ├── hash_service.rs      # MVP2
│   │   │   ├── phash_service.rs     # MVP2
│   │   │   ├── trash_service.rs     # MVP2，Windows IFileOperation
│   │   │   └── ai_client.rs         # MVP3，调 Python worker
│   │   ├── repo/                    # 数据访问层
│   │   │   ├── mod.rs
│   │   │   ├── roots_repo.rs
│   │   │   ├── images_repo.rs
│   │   │   └── duplicates_repo.rs   # MVP2
│   │   ├── queue/                   # 优先级任务队列
│   │   ├── events/                  # emit 帮助
│   │   ├── migrations/              # SQLite migrations，按编号
│   │   │   ├── 0001_init.sql
│   │   │   ├── 0002_hash_columns.sql
│   │   │   └── ...
│   │   ├── error.rs                 # 统一 anyhow + thiserror
│   │   └── config.rs
│   └── tests/                       # 集成测试
│
├── ai-worker/                       # Python AI 服务（MVP3 起）
│   ├── pyproject.toml
│   ├── uv.lock
│   ├── src/
│   │   ├── main.py                  # FastAPI 入口
│   │   ├── models/
│   │   │   ├── clip.py
│   │   │   ├── tagger.py
│   │   │   ├── ocr.py
│   │   │   └── face.py
│   │   ├── api/
│   │   └── runtime.py               # CUDA / DirectML / CPU 选择
│   └── scripts/
│       └── download_models.py
│
├── tests/                           # 端到端 / 基准测试
│   ├── fixtures/                    # 测试用图片集
│   └── e2e/
│
├── scripts/                         # 开发脚本
│   ├── dev-data.ts                  # 生成测试数据
│   └── windows-setup.ps1            # Windows 环境检查
│
└── .github/
    └── workflows/                   # CI（仅 windows-latest）
```

## 7. 模块边界（哪个层做哪些事）

| 模块 | 负责 | 不负责 |
|------|------|--------|
| `src/app/` | 路由、页面骨架 | 不写业务逻辑 |
| `src/stores/` | 客户端状态 | 不直接 invoke |
| `src/lib/tauri.ts` | invoke 封装、类型映射 | 不管业务 |
| `src-tauri/commands/` | 参数校验、调用 service、返回结果 | 不写业务循环、不直接读 DB |
| `src-tauri/services/` | 业务逻辑、调用 repo 与 queue | 不直接接触 Tauri runtime |
| `src-tauri/repo/` | 仅 SQL / LanceDB IO | 不做业务判断 |
| `src-tauri/queue/` | 调度、优先级、并发控制 | 不知道任务内容 |
| `ai-worker/` | 模型推理 | 不读项目数据库 |

## 8. 配置与环境

环境分三档（与 smart-search 一致）：

| profile | 数据库位置 | 模型位置 | 用途 |
|---------|----------|----------|------|
| `dev` | `%LOCALAPPDATA%\PhotoViewPlus\dev\` | 同上 | 日常开发 |
| `test` | 临时目录（每次清空） | mock | 集成测试 |
| `prod` | `%LOCALAPPDATA%\PhotoViewPlus\` | 同上 | 用户运行 |

切换：`pnpm tauri:dev` / `pnpm tauri:test` / `pnpm tauri:prod`，对应 `tauri.conf.<profile>.json`。

## 9. 错误处理与日志

- Rust 端：`anyhow::Result<T>` 用于业务，`thiserror` 自定义关键错误类型（`ScanError`、`DedupError`、`AiError`）
- 前端：所有 invoke 返回 `Result<T, AppError>`，AppError 是带 `code` + `message` + `hint` 的判别联合
- 日志：`tracing` + `tracing-subscriber`，写到 `%LOCALAPPDATA%\PhotoViewPlus\logs\YYYY-MM-DD.log`，按天轮转保留 14 天
- 严重错误（AI worker 反复崩溃、磁盘满）通过托盘通知 + 设置页红点

## 10. 接下来读什么

- 数据库表的具体字段 → [`01-data-model.md`](./01-data-model.md)
- 界面长什么样 → [`02-ui-design.md`](./02-ui-design.md)
- 现在动手写什么 → [`03-mvp1-browse.md`](./03-mvp1-browse.md)
