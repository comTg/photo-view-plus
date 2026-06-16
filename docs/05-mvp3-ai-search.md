# 05 · MVP3 · AI 标签与语义搜索

> 范围：Python AI Worker 进程 + CLIP 向量化 + LanceDB + 自然语言搜图 + 自动标签 + 以图搜图。  
> 前置：MVP1 + MVP2 完成。

## 0. 目标与验收

**做完 MVP3 后，应能：**

1. 主程序按需启动 / 停止 AI Worker（独立 Python 子进程）
2. 自动检测 CUDA 12.8 + RTX 5070，启用 GPU 加速；不可用时回退 CPU
3. 输入"海边日落"，5 万图库中 top-20 命中率 ≥ 0.85（在 fixture query 集上）
4. 拖图进搜索框做"以图搜图"，相似结果合理
5. 每张图自动打 5-15 个标签，标签准确率人工抽查 ≥ 85%
6. AI Worker 崩溃时主程序仍可正常浏览，3 秒内自动重启
7. 模型按需下载（不打包进安装包），可在设置页管理

## 1. 模块边界

引入：

| 路径 | 引入 |
|------|------|
| `ai-worker/` | 整个 Python 项目 |
| `src/app/search/page.tsx` | 高级搜索 |
| `src/components/search/`, `src/components/ai/` | 搜索 UI、模型状态 |
| `src/stores/aiStore.ts` | AI 状态 |
| `src-tauri/src/commands/ai.rs` | 命令 |
| `src-tauri/src/services/ai_client.rs` | 调 Python worker 的 HTTP client |
| `src-tauri/src/services/ai_supervisor.rs` | 子进程托管 + 健康检查 + 自动重启 |
| `src-tauri/src/repo/lancedb_repo.rs` | LanceDB 读写（用 `lancedb` Rust crate） |
| `src-tauri/src/migrations/0003_ai_columns.sql` | schema |

## 2. 任务清单

### T1 Migration 0003

具体：
- `ALTER TABLE images ADD COLUMN tag_status TEXT NOT NULL DEFAULT 'pending';`
- `ALTER TABLE images ADD COLUMN embedding_status TEXT NOT NULL DEFAULT 'pending';`
- 新建 `tags` 表（id / name / source / category / created_at）
- 新建 `image_tags` 表（image_id / tag_id / score / source）
- 索引按 `docs/01` § 2.3 / 2.4

**验收**：migration 在已含 MVP2 数据的库上幂等执行。

### T2 AI Worker 脚手架

**目标**：能独立跑起来的 FastAPI 服务，提供 `/health`、`/ready`。

具体：
- 工具：`uv`（速度比 pip 快 10 倍）
- `pyproject.toml`：Python 3.11，依赖 `fastapi`、`uvicorn`、`pydantic`、`torch`（CUDA 12.8 索引）、`onnxruntime-gpu`、`pillow`、`open_clip_torch`
- 目录：
  ```
  ai-worker/
    pyproject.toml
    uv.lock
    src/
      main.py            # FastAPI 入口
      api/
        clip.py
        tagger.py
        health.py
      models/
        clip_loader.py
        tagger_loader.py
      runtime.py         # 运行时选择（CUDA / DirectML / CPU）
      logging.py
    scripts/
      download_models.py
  ```
- `runtime.py`：探测 `torch.cuda.is_available()` + `torch.cuda.get_device_capability()`，返回 device + 精度（float16 on CUDA）
- 启动：`uv run uvicorn src.main:app --host 127.0.0.1 --port 0`（端口 0 = 系统分配，启动时打到 stdout 让 Rust 端读取）

**验收**：`pnpm ai:dev` 启动 worker，`curl :PORT/health` 返回 `{"status":"ok","device":"cuda","compute":"8.9"}`。

### T3 进程托管（supervisor）

**目标**：Rust 端可靠地拉起 / 健康检查 / 重启 AI Worker。

具体（`src-tauri/src/services/ai_supervisor.rs`）：
- 启动：`tokio::process::Command::new("python")` 指向 `ai-worker/.venv/python.exe`，捕获 stdout 读端口
- 健康：每 5s ping `/health`，连续 3 次失败 → 触发重启
- 自动重启：最多 3 次/分钟，超过则进入"degraded" 状态通知前端
- 优雅停止：发 `/shutdown` → 收 `done` → 1s 超时强杀
- 重要事件 emit：`ai:worker_status` payload `{ status, device, vram, pid, last_error }`
- 闲置策略：N 分钟（默认 10）没有请求 → 自动 stop；下次请求时按需 start

**验收**：人为 `kill -9` worker 进程，3 秒内自动起来；连杀 4 次进入 degraded。

### T4 CUDA / 模型可用性自检

**目标**：用户在设置页能看到 GPU 状态与模型状态。

具体：
- `/diagnostics` 端点：返回
  ```json
  {
    "torch_version": "2.5.0+cu128",
    "cuda_available": true,
    "cuda_version": "12.8",
    "device_name": "NVIDIA GeForce RTX 5070",
    "compute_capability": "12.0",
    "vram_total_gb": 12,
    "vram_free_gb": 11,
    "models": {
      "clip-vit-b-32": {"downloaded": true, "size_mb": 580, "loaded": false},
      "ram-plus": {"downloaded": false, "size_mb": 1200}
    }
  }
  ```
- 前端设置 → AI 页展示
- 处理 sm_120 兼容：若 PyTorch 不识别 RTX 50 系列，提示用户升级 PyTorch nightly（详见 `docs/07` § 1）

**验收**：能正确报告 RTX 5070 信息；GPU 占用时可见 VRAM 数字变化。

### T5 模型下载管理

**目标**：模型按需下载，可断点续传。

具体：
- 模型清单 `ai-worker/models.json`：
  ```json
  {
    "clip-vit-b-32": { "hf_repo": "openai/clip-vit-base-patch32", "format": "transformers", "size_mb": 580 },
    "ram-plus": { "hf_repo": "xinyu1205/recognize-anything-plus-model", "size_mb": 1200 },
    "siglip-so400m": { "hf_repo": "google/siglip-so400m-patch14-384", "size_mb": 3700 }
  }
  ```
- 下载脚本：`scripts/download_models.py`，用 `huggingface_hub.snapshot_download`
- 落盘到 `%LOCALAPPDATA%\PhotoViewPlus\models\<model_key>\`
- API `/models/download` + `/models/cancel`：返回 SSE 进度
- 前端：设置页"模型管理" 列表，下载 / 取消 / 删除

**验收**：断网中断下载，重连恢复（HF Hub 默认支持）。

### T6 CLIP 图片向量化

**目标**：批量把图片转 512 维向量。

具体（`ai-worker/src/api/clip.py`）：
- 模型加载：`open_clip` 加载 `ViT-B-32` weights `openai`；torch.compile（如可用）
- API `POST /clip/embed`：
  ```json
  request:  {"items": [{"id": 123, "thumb_path": "..."} , ...]}  // 最多 32
  response: {"items": [{"id": 123, "embedding": [..]}, ...]}     // float16
  ```
- 优化：
  - 输入用缩略图（256px），CLIP 内部会 resize 到 224；不要喂原图
  - batch=32 在 RTX 5070 上 < 200ms
  - 输出归一化（L2），方便后续 cosine = dot product
- 错误处理：单张失败不影响整 batch，返回错误项

**验收**：1000 张图 batch 化处理 < 30s（RTX 5070）。

### T7 LanceDB 写入与索引

**目标**：把 CLIP 向量持久化、可查询。

具体（`src-tauri/src/repo/lancedb_repo.rs`）：
- 依赖：`lancedb` rust crate
- 启动时打开（首次自动创建）`vectors/image_embeddings_clip` 表，schema 见 `docs/01` § 3.1
- 写入 API：批量 upsert
- 查询 API：`top_k(query_vec, k=100, filter?)`，filter 可加 SQL 谓词（`root_id IN (1,2)`）
- 索引：数据 > 1 万行后建 HNSW（`m=16, ef_construction=200`）
- 一致性：先 SQLite 事务设 `embedding_status='ready'`，再写 LanceDB；失败则回滚 + 入 outbox

**验收**：10 万向量 top-100 检索 < 100ms。

### T8 主进程拉取流水线

**目标**：自动化"待 embed 的图 → AI Worker → LanceDB"链路。

具体：
- 任务：`EmbedTask { image_ids: Vec<i64> }`，优先级 P6
- 触发条件：`thumb_status='ready' AND embedding_status='pending'`
- 调度循环：每秒扫一次取最多 32 个，构造 batch 调 AI Worker，写 LanceDB，更新状态
- 失败重试：3 次后标 `embedding_status='failed'`
- 暂停：用户在设置页关 AI → 调度器跳过

**验收**：扫一遍图库后约几小时（取决于规模）全部 embed 完成；中途关 AI 立即停止。

### T9 自然语言搜图

**目标**：搜索框输入文本，返回相关图片。

具体：
- AI Worker `POST /clip/encode_text` → 单个 512 维向量
- 主进程命令 `ai_search({ text, root_ids?, limit })`：
  1. 调 worker 拿文本向量
  2. 调 LanceDB top_k
  3. JOIN SQLite 拿图片完整信息
  4. 返回 `SearchResult[]` 含 `score`
- 前端 `<SearchPanel>`：输入框 + 实时 hint + 历史搜索
- 路由：在主页搜索框直接显示结果列表（替换瀑布流）；或独立 `/search?q=...`
- 取消：用户改 query 时取消上一次（abort signal）

**验收**：固定 50 条 query 在 5 万图库的 fixture 上 top-20 命中率 ≥ 0.85（人工标注）。

### T10 以图搜图

**目标**：用一张图找相似图。

具体：
- 命令 `ai_search_by_image(image_id, limit)`：
  1. 读 `image_id` 的 embedding（从 LanceDB）
  2. top_k 查同表（排除 self）
- 前端：详情面板"相似"区点击 / 搜索框拖入图片（先要把图片入库或临时 embed）
- 拖入未入库图：调 worker `/clip/embed` 一次性，不写库

**验收**：选一张 "海边日落" 的图，top-10 都是海边/日落主题。

### T11 自动标签（RAM 或 Florence-2）

**目标**：每张图给 5-15 个英文/中文标签。

具体：
- 选择：`RAM++`（Recognize Anything Plus）当默认（标签覆盖广、英文+中文）
- AI Worker `/tagger/run`：批量 16，输出 `[{id, tags: [{name, score}]}]`
- 主进程：
  - 任务 `TagTask`，优先级 P5
  - 收到结果后 upsert `tags`（未见过的 name 新增）+ `image_tags`
- 翻译：可选启用"标签翻译"，把英文标签转中文（用本地 dict 文件而不是调 LLM）

**验收**：100 张随机图人工抽查，每张前 5 标签命中率 ≥ 85%。

### T12 标签筛选与标签云

**目标**：用户可按标签筛选 / 浏览。

具体：
- 左侧栏"标签"分组改为真实树（按 category 分组：物体 / 场景 / 颜色 / 风格）
- 点击标签 → `browseStore.currentTagIds` 更新，重查
- 标签详情页（点击标签名）：显示该标签下所有图 + 子标签建议
- 设置：标签置信度阈值（默认 0.6）

**验收**：左侧"日落" → 中间显示该标签所有图。

### T13 搜索 DSL 升级

**目标**：搜索框支持自然语言 + 标签 + 元数据混合。

具体：
- 解析器：把输入拆成 token：
  - `#tag` → 标签条件
  - `size:>5MB`、`taken:2026-06`、`format:heic`、`root:nas` → SQL 过滤
  - `is:duplicate`、`has:gps` → 谓词
  - 剩下的纯文本 → 调 CLIP 文本搜
- 流程：先 SQL 过滤候选集，候选集 ID 传给 LanceDB top_k 重排
- 实现：`src-tauri/src/utils/search_dsl.rs` 解析器（解析为 AST）

**验收**：`日落 #风景 size:>5MB taken:2026-06` 返回正确结果。

### T14 AI 状态面板

**目标**：用户能看到 AI 在做什么。

具体（参考 `docs/02` § 2.3）：
- 状态栏 → 展开 AI 区
  - Worker 状态徽章
  - CUDA / 模型加载状态
  - 队列：embedding pending / tagging pending
  - 速度：images/sec
  - 估计剩余
- 控制：暂停 AI / 设置每日时段 / 仅充电时跑（笔记本场景）

**验收**：所有状态准确实时。

### T15 AI Worker 容错

**目标**：worker 崩了主程序不死。

具体：
- 调 worker 全用 `tokio::time::timeout(30s, ...)`
- 网络错误返回友好提示，不冒泡到 UI 报错
- 重启策略见 T3
- 健康检查不通过时，搜索/标签 命令直接报"AI 暂不可用，请检查 GPU 或重启"

**验收**：模拟 worker 进程 panic / OOM / GPU 错误三种场景，主程序行为正确。

### T16 MVP3 端到端验收

1. 全新启动 → 主程序起、AI Worker 起、检测到 RTX 5070
2. 自动下载 CLIP + RAM 模型（首次约 1.8GB）
3. 5 万图库 embedding 完成 < 30 分钟
4. 50 条 query top-20 命中率 ≥ 0.85
5. 自动标签准确率 ≥ 85%
6. 拖入新图以图搜图返回合理结果
7. 杀掉 worker，3s 内重启
8. 关掉 worker 用户仍能正常浏览

## 3. 依赖关系

```
T1 ─→ T2 ─→ T3 ─→ T4 ─→ T5
                          │
                          ▼
                          T6 ─→ T7 ─→ T8 ─→ T9 ─→ T13
                                              │     │
                                              │     ▼
                                              │     T14
                                              ▼
                                              T10
                                              T11 ─→ T12
                                              T15 ─→ T16
```

## 4. 关键技术点与坑

- **RTX 5070 sm_120 兼容**：PyTorch nightly 才完全支持，stable 2.5 可能报"unsupported architecture"。若遇到，临时回退 sm_89（hack 环境变量 `TORCH_CUDA_ARCH_LIST="8.9"` 让 PyTorch 用 Ada 内核跑 Blackwell——能跑但慢 20%）。正式版盯 PyTorch 2.6+
- **不要每次请求都加载模型**：模型常驻 worker 进程；切模型重启 worker
- **batch 大小自适应**：根据 VRAM 余量动态调整（RTX 5070 12GB ≈ batch 32-64）
- **CLIP 与 RAM 共享 worker 还是分开**：建议同 worker 但用 `concurrency_limiter`，避免 CLIP 占 VRAM 导致 RAM OOM。VRAM 不够时单独子 worker
- **LanceDB 索引重建时机**：每新增 1 万向量后台 reindex 一次，不要每条都重建
- **HNSW 召回不是 100%**：top_k 取 200 然后业务层过滤到 100，避免边缘漏掉
- **中文搜索**：CLIP 原版主要是英文，中文需要 SigLIP 或 ChineseCLIP 才稳。MVP3 先 CLIP；遇到中文 query 效果差时升级 SigLIP（已在 models.json 备好）
- **隐私**：不上传任何图片到 HuggingFace 或任何远端，AI Worker 不开外网（除模型下载）

## 5. 完成判据

- 16 个任务"验收"段全部打勾
- benchmark 文件 `tests/ai_bench.md` 记录 RTX 5070 上的实际性能数据
- ADR 写入 `docs/08-roadmap.md`：CLIP vs SigLIP 选型与切换路径
