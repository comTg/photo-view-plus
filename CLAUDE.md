# CLAUDE.md

本文件给 Claude Code（claude.ai/code）阅读，约束在本仓库内的工作方式。

## Project Overview

**photo-view-plus** 是本地优先的图片资产管理工具，目标平台 **Windows（RTX 5070，CUDA 本地推理）**。核心能力：目录扫描 → 缩略图缓存 → 浏览 → 去重（完全/视觉/语义三层）→ AI 标签 → 自然语言/以图搜图。

技术栈：Tauri 2 + React 19（Vite SPA）+ Rust + SQLite + LanceDB + Python AI Worker（PyTorch CUDA / ONNX Runtime CUDA）。

完整设计源在仓库外：`/Volumes/HK 1/vetoer/code/mygit/my-doc/APP灵感/图片相册浏览与去重.md`。本仓库的 `docs/` 是从该设计转译出的、可分步实施的开发文档。

## 文档地图（先读这个，再动手）

| 路径 | 你要在什么时候读 |
|------|----------------|
| `docs/00-architecture.md` | 改任何跨模块的事之前 |
| `docs/01-data-model.md` | 涉及 SQLite / 向量库 / 缩略图缓存的改动 |
| `docs/02-ui-design.md` | 涉及任何 UI / 交互 / 状态管理的改动 |
| `docs/03-mvp1-browse.md` | 当前阶段（基础浏览） |
| `docs/04-mvp2-dedup.md` | 第二阶段（去重） |
| `docs/05-mvp3-ai-search.md` | 第三阶段（AI / 语义搜索） |
| `docs/06-mvp4-advanced.md` | 第四阶段（OCR / 人脸 / 时间轴） |
| `docs/07-windows-platform.md` | 任何依赖 Windows 行为的代码（CUDA / HEIC / SMB / 回收站） |
| `docs/08-roadmap.md` | 规划新阶段、调里程碑、记 ADR |

文档没说的，**先开 issue / 改 docs 再写代码**，不要在代码里反向定规则。

## 环境要求

- Node.js 18+、pnpm 9+
- Rust 1.78+（stable）
- Python 3.11（仅 MVP3 之后需要；MVP1/2 可以不装）
- CUDA Toolkit 12.8+（仅 MVP3 之后需要）
- Windows 10 22H2 或 Windows 11；启用长路径支持（见 `docs/07`）
- 开发可在 macOS 上跑前端 + Rust 单元测试；AI Worker、HEIC、回收站等 Windows-only 能力必须在 Windows 上联调

## Common Development Commands

```bash
# 安装
pnpm install
cargo fetch --manifest-path src-tauri/Cargo.toml

# 开发
pnpm tauri dev                  # 启动 Tauri + Vite dev server
pnpm tauri:dev                  # 用 dev profile（指向开发数据库）
pnpm tauri:test                 # 用 test profile（临时数据库 + 测试目录）

# 构建（仅 Windows）
pnpm tauri build                # 输出到 src-tauri/target/release/bundle/

# 代码检查
pnpm lint                       # Biome
pnpm typecheck                  # tsc --noEmit
cargo clippy --all-targets -- -D warnings
cargo fmt --check

# 测试
cargo test                                      # 全部 Rust 单元测试
cargo test --test scan_tests                    # 扫描相关
cargo test --test dedup_tests                   # 去重相关
pnpm test                                       # 前端 vitest

# AI Worker（MVP3 起）
pnpm ai:dev                     # 启动 Python FastAPI worker（uvicorn）
pnpm ai:check                   # 检查 CUDA / 模型可用性
```

## Architecture（速览，详见 docs/00）

```
┌────────────────────────────────────────────────────────────┐
│  UI 层（Vite + React 19, 桌面 SPA）                        │
│  - 瀑布流 / 详情 / 去重对比 / 搜索 / 设置                  │
└────────────────────────────────────────────────────────────┘
                       │ Tauri invoke
┌────────────────────────────────────────────────────────────┐
│  Core Service（Rust，src-tauri/）                          │
│  - 目录扫描、缩略图、文件操作、回收站、文件监听            │
└────────────────────────────────────────────────────────────┘
                       │
┌────────────────────────────────────────────────────────────┐
│  Index Service（Rust + SQLite + LanceDB）                  │
│  - 元数据 / pHash / 标签 / 向量索引                        │
└────────────────────────────────────────────────────────────┘
                       │ HTTP / IPC
┌────────────────────────────────────────────────────────────┐
│  AI Worker（Python，ai-worker/，独立子进程）               │
│  - CLIP / RAM / OCR / 人脸                                 │
└────────────────────────────────────────────────────────────┘
```

进程模型与数据流详见 `docs/00-architecture.md`。

## 约束（红线）

继承用户全局 CLAUDE.md 的全部红线（删文件、动密钥、改 schema、git 强制操作等需先问）。**本项目特有红线**：

1. **MVP 阶段隔离**：当前阶段（MVP1 / MVP2）的代码里不要预埋 AI 相关代码、向量库依赖、Python worker 启动逻辑。每个 MVP 只引入它所需的最小依赖。
2. **不要直接永久删除图片**：所有"删除"操作必须走回收站（Windows Shell IFileOperation），并保留撤销日志。
3. **不要阻塞 UI 线程做扫描或 AI**：扫描、缩略图、pHash、AI 全部走后台队列，前端通过事件订阅进度。
4. **不要把图片绝对路径硬编码进数据库**：用 `roots.id + 相对路径` 存储，处理盘符变化。
5. **AI Worker 崩溃不能拖死主程序**：主进程对 AI worker 必须有健康检查 + 自动重启 + 超时熔断。
6. **网络盘（SMB / 映射盘）扫描并发上限 4**：超过会拖死 NAS。本地盘可以 8-16。
7. **不要把 AI 模型权重提交进 git**：用 `pnpm ai:download` 脚本按需下载到 `%LOCALAPPDATA%\PhotoViewPlus\models\`。
8. **数据库 schema 变更必须走 migration**：禁止直接 ALTER TABLE 后假装兼容。

## 验证要求（改完代码做这几件事）

- 改 Rust：`cargo clippy --all-targets -- -D warnings && cargo test`
- 改前端：`pnpm lint && pnpm typecheck && pnpm test`
- 改 UI 显示效果：在 `pnpm tauri dev` 里手工跑一遍受影响的页面
- 改扫描/去重/AI 流水线：用 `tests/fixtures/` 下的小数据集跑端到端

详细验证脚本与基准数据集见 `docs/08-roadmap.md`。

## 反模式（看到就改，不要保留）

- `unwrap()` / `expect()` 出现在非测试代码——用 `anyhow::Result` 或 `thiserror` 自定义错误
- 前端用 `useEffect + fetch` 替代 Tauri invoke——所有持久化数据走 Rust 命令
- `setInterval` 轮询扫描进度——用 Tauri event 订阅
- 写入数据库时不加索引——查询慢会立刻暴露
- 一个 Tauri command 里既扫描又落库又发事件——拆成 service 层
