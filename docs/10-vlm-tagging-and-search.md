# 10 · 本地视觉大模型打标与中文自然语言搜索

> 状态：方案（2026-10-22），待评审。实施按 §10 的阶段逐项推进，每项完成后更新进度表里的状态与实测数据（做法同 [docs/09](09-performance-optimization.md)：一项一提交，提交信息引用条目 ID）。
> 关联：[docs/05](05-mvp3-ai-search.md)（MVP3 原设计）、[docs/11](11-background-resource-governance.md)（后台资源治理，VLM 批处理受它调度）、参考实现 memora（`D:\code\mygit\memora`）。

## 0. 结论

1. **打标可以照搬 memora 的方案。** memora 用 llama.cpp 的 `llama-server` 跑 Qwen3.5-9B（Q4 量化，加视觉投影约 7.2GB 显存）。它在**同一台机器**（RTX 5070 12GB）上全量分析过 12,609 张照片，并发 4 时约 1.7 秒/张。`llama-server` 程序和模型文件都已经在本机，直接复用，不用重新下载。
2. **能明显提升中文搜索，但前提是搜索也要改。** 现在搜索框里的普通文字只走 OpenAI 原版 CLIP，而这个模型只懂英文；标签也只能用 `#标签` 精确匹配。大模型打出的中文标签和描述再好，搜索不改就用不上。需要新增一条"文字检索通道"：对标签、描述、OCR 做全文检索，再加上中文文本向量，最后和图像向量融合排序。
3. **收益已经用离线评测验证（§9.1）。** memora 已经给同一个 NAS 目录（`唐唐手机备份`）里的 8,827 张照片写好了中文描述，路径和本库逐条对得上。用这些描述跑了 40 个中文查询的评测，nDCG@20 如下：
   - 现状：0.079；
   - 单换 Chinese-CLIP：0.817；
   - 单用描述的文本向量：0.841；
   - 加权融合：0.900。

   换 CLIP 是收益最大、成本最低的一步；大模型描述在此基础上还能再提升一截。
4. **代价是 GPU 时间和整机负载。** 去重后全库约 1 万张，全量分析约 4.8 小时，期间 GPU 满载；导入 memora 后只剩约 1,350 张，约 40 分钟。大模型推理会占满同时负责桌面显示的显卡，必须只在空闲时运行，由 docs/11 的资源治理器控制启停。

## 1. 现状（2026-10-22，正式版数据库只读统计）

| 项 | 现状 | 影响 |
|----|------|------|
| 图像向量 | open_clip ViT-B/32 + OpenAI 权重，512 维（[clip_loader.py:28](../ai-worker/src/models/clip_loader.py#L28)） | 只懂英文，中文查询的结果接近随机 |
| 标签 | RAM-plus 中文输出：2,252 个不同标签，已打标的 4,886 张平均每张 20 个；大量动词和带斜杠的歧义译名（"站/矗立/摊位""握着/抓着""坐/放置/坐落"）；没有置信度 | 标签云噪声大；而且普通文字搜索根本不查标签 |
| 全文索引 | `images_fts(filename, ocr_text)`，分词器 `unicode61`（[0004_ocr_face.sql:16](../src-tauri/migrations/0004_ocr_face.sql#L16)） | 连续中文会整段当成一个词，OCR 中文无法按词检索 |
| 搜索逻辑 | `ai_search`：拆 DSL → CLIP 取前 limit×5 → 内存里后置过滤（[commands/ai.rs:84](../src-tauri/src/commands/ai.rs#L84)） | 只输 `#标签` 时只看默认排序的前 400 行；选了目录或带标签时结果会漏；退回文件名匹配时丢掉标签条件 |
| 兜底向量 | worker 缺依赖时返回哈希伪向量，模型名照写 `clip-vit-b-32`，Rust 侧不看 `fallback`，照常入库并标 ready（[ai_pipeline.rs:255](../src-tauri/src/services/ai_pipeline.rs#L255)） | 换成真模型后这些图不会重算 |
| 向量维度 | `CLIP_DIMS = 512` 写死，人脸向量表也复用了这个常量（[lancedb_repo.rs:21](../src-tauri/src/repo/lancedb_repo.rs#L21)、[face_service.rs:12](../src-tauri/src/services/face_service.rs#L12)） | 换模型会连带弄坏人脸功能 |
| 覆盖率 | 13,960 条在库记录中，缩略图失败 3,001（其中 2,512 条源于 NAS 上 `camera`/`Camera` 两个目录名大小写冲突，见 [docs/11 §6.1](11-background-resource-governance.md#61-nas-目录大小写冲突数据问题建议最先处理)），HEIC 不支持 1,503 | 约三分之一的图没有缩略图，进不了现有的 AI 流水线 |
| 设置 | 正式版配置 `aiEnabled=false` | AI 目前处于关闭状态 |

## 2. 参考 memora：借什么、不借什么

| 方面 | memora 的做法 | 本项目怎么用 |
|------|--------------|-------------|
| 推理运行时 | `llama-server`（conda-forge build 11351，CUDA 12.9），OpenAI 兼容接口，只监听 127.0.0.1 | 直接复用该 exe，路径做成设置项 |
| 模型 | Qwen3.5-9B UD-Q4_K_XL（5.97GB）+ mmproj-F16（0.92GB），从 ModelScope 下载并核对过 sha256 | 复用同一文件，不复制 |
| 启动参数 | `-ngl 99 -fa on --reasoning off`，`-np 4`，每个槽位上下文 6144 | 相同；`-np` 随负载模式调整（docs/11） |
| 输入图 | 按 EXIF 方向校正，长边缩到 1024，JPEG 质量 88（约 800 个图像 token） | 相同；A6 再对比长边 768 的速度与质量 |
| 输出约束 | JSON Schema（llama.cpp grammar）保证格式合法；偶发截断时重试 | 相同，temperature 降到 0.2 |
| 生命周期 | 有活才启动，轮询 `/health`（加载中返回 503）；用完关闭、释放显存；用 Windows 作业对象保证父进程退出时子进程一起结束 | 相同，由 Rust 侧监管（§4） |
| 实测 | 并发 2 / 4 / 6：2.46 / 1.64 / 1.99 秒每张；模型约 7.2GB，加桌面共约 9.5–9.9 / 12GB | 作为基线 |

不借的部分：回忆度/美观度评分、上墙文案、家庭相框导向的 16 个类别。本项目要的是检索导向的输出（§6）。

**显存预算**：总共 12GB，桌面常驻约 1.5GB（本机 `nvidia-smi` 实测 1,510MiB）。`llama-server` 的 7.2GB 和现在 worker 里的 RAM-plus（Swin-L，FP32）不能同时驻留。由 VLM 接管打标之后，RAM-plus 下线（A7）。

## 3. 目标与非目标

**目标**
- 中文自然语言查询的检索质量可量化提升，指标与门槛见 §9。
- 标签干净、可浏览：名词为主，不带斜杠。
- 打标只在空闲时自动进行，不让整机卡（验收标准见 docs/11 §8）。
- 支持断点续跑，可以按模型版本和提示词版本重跑。

**非目标**
- "和图库聊天"（docs/08 §8 已明确不做）。
- 云端模型。
- 视频。

## 4. 总体设计

```
            ┌──────────────── 离线：空闲时批处理 ────────────────┐
原图(NAS) ─▶ worker /vlm/analyze：读原图（含 HEIC）→ 长边 1024 → llama-server（Qwen3.5-9B）
                     │  JSON：描述 / 类别 / 标签 / 同义词 / 人数 / 可见文字 / 敏感
                     ▼
            Rust：image_analysis 表 + image_tags(source='vlm')
                     ├─▶ search_fts（jieba 分词后写入 FTS5）
                     └─▶ worker /text/embed → LanceDB image_text_emb_<model>
缩略图 ───▶ worker /clip/embed（Chinese-CLIP）→ LanceDB image_clip_emb_<model>
            └────────────────────────────────────────────────────┘

查询 ─▶ 解析 DSL ─▶ SQL 预过滤候选集 ─▶ 四路召回 ─▶ RRF 融合 ─▶ 结果 + 命中来源
                                       ├ 标签（精确 / 同义词）
                                       ├ 全文 BM25（标签 / 同义词 / 描述 / OCR / 文件名）
                                       ├ 文本向量（描述）
                                       └ 图像向量（Chinese-CLIP：文本 → 图像）
```

**职责划分**

| 组件 | 负责 |
|------|------|
| Rust | 调度；负载模式（docs/11）；`llama-server` 启停与健康检查；数据库写入；索引维护；搜索融合 |
| Python worker | 准备输入图（PIL + pillow-heif，HEIC 也能读）；调用 `llama-server`；校验并规范化输出；计算文本和图像向量 |
| llama-server | 只做推理 |

让 worker 去调用 `llama-server`、而不是 Rust 直接调用，有三个原因：
- 读 HEIC 需要 pillow-heif。
- memora 的 `images.prepare`、`VLM.chat_json` 和 Schema 代码几乎可以原样移植。
- Rust 侧不必新增图像解码依赖。

worker 从 NAS 读原图，这条读取路径也必须遵守"网络盘并发 ≤ 4"的红线。做法是由 Rust 先从 [read_gate](../src-tauri/src/utils/read_gate.rs) 拿到读取许可，拿到几个就发几张给 worker，worker 返回后再释放许可。

## 5. 模型选型

### 5.1 视觉大模型（打标）

- 默认用 Qwen3.5-9B，复用 memora 的文件。新增三个设置项：`vlmServerExe`、`vlmModelPath`、`vlmMmprojPath`。
  - 默认值指向 `%LOCALAPPDATA%\PhotoViewPlus\models\qwen3.5-9b\`。
  - 本机直接填 memora 的路径。memora 在 D: 盘、模型目录在 C: 盘，跨盘不能建硬链接，所以靠配置指过去，不复制 7GB 文件。
- 新模型一律从 ModelScope 下载。本机访问国外线路约 10KB/s，HuggingFace 和 GitHub 实际下不动（见 memora 方案 §1 的实测）。现有的 [model_registry.py](../ai-worker/src/model_registry.py) 只支持 HuggingFace，需要补上 ModelScope 来源。
- 吞吐不够时的备选，等 A6 实测后再定：同系列更小尺寸的视觉模型、把输入长边降到 768、调整 `-np`。

### 5.2 中文文本向量（用于描述检索）

| 候选 | 维度 | 体积（约） | 说明 |
|------|------|-----------|------|
| bge-small-zh-v1.5 | 512 | 100MB | 默认首选；单条查询在 CPU 上就很快 |
| bge-base-zh-v1.5 | 768 | 400MB | 评测时对比 |
| bge-m3 | 1024 | 2.2GB | 多语言、支持长文本；评测时对比 |
| Qwen3-Embedding-0.6B | 1024 | 1.2GB | 多语言；有 GGUF 版本，也可以跑在 llama.cpp 上 |

- 运行在 worker 新增的 `/text/embed` 端点里（`transformers` 已经是现有依赖）。
- 查询文本按模型卡的建议加检索指令前缀。
- 表中数值是约数，实施时以模型卡为准，最终用 §9 的评测结果来选。

### 5.3 图像向量（ADR-008 提议）

- 换成 Chinese-CLIP（OFA-Sys，先用 ViT-B/16，通过 `transformers` 的 `ChineseCLIPModel` 加载）。它支持直接用中文文本检索图片，全库 1.4 万张缩略图重算向量只需几分钟。
- **更正 docs/05 §注意事项**：`models.json` 里备着的 `siglip-so400m` 是英文模型，换上它也解决不了中文问题。如果想试 SigLIP 系列，应选多语言的 SigLIP 2。
- 向量表按"模型 + 维度"命名，去掉写死的 512；人脸表改用自己的维度常量。

### 5.4 全文检索

- 用 SQLite FTS5，写入和查询前都用 jieba-rs（MIT 许可）先分词，再用 `unicode61` 按空格切开已经分好的词。
  - **更新（§9.1）**：A0 原型用不依赖词典的"CJK 双字 + 单字"切分加 BM25，已经有效（描述全文 nDCG@20 0.692）。A2 先实现同样的双字方案，不引入 jieba-rs；以后如果需要，再用评测集对比 jieba。
- 另加一列"单字"，权重设得很低，用来兜住"猫""车"这类单字查询：jieba 会把"猫咪"切成一个词，只查"猫"时匹配不到。
- 同义词有两个来源：VLM 输出的 `keywords` 字段；另外维护一份小同义词表用于查询扩展（可选，A2b）。

## 6. VLM 输出 Schema 与提示词（初版，A6 样本试跑后调整）

**系统提示词**要固定不变，这样 `llama-server` 能复用前缀缓存。它的要点：

```
你是一个"照片检索标注助手"。你的输出会用于本地相册的中文搜索：用户会用口语描述想找的照片，
例如"海边的小孩""生日吃蛋糕""发票截图"。请客观、具体地描述画面里能确定的内容，
不要猜测人物身份和关系，不要编造画面里没有的东西。

1. description：60~150 字。依次写：照片类型（照片/截图/文档等）；主体和人物（人数、大致年龄段、
   动作、表情、显著的穿着）；场景和地点类型（室内/室外、海边/餐厅/街道等）；时间与天气
   （白天/夜晚/雪天等）；显著的物体、动物、食物；显著时再写主要颜色和光线。
2. categories：从给定列表里选 1~2 个，第一个是主类别。
3. tags：5~15 个中文标签。用最常见的说法（"猫"，不要"猫科动物"），每个 1~6 个字，以名词为主
   （物体、场景、活动、事件、动物、食物、地标类型）；不要动词短语，不要斜杠，
   不要"照片""图片""画面"这类词。
4. keywords：3~10 个检索用的同义词或上位词，帮助换个说法也能搜到
   （例如 猫 → 猫咪、宠物；婚礼 → 婚纱照、新娘）。不要和 tags 重复。
5. people_count：画面中清晰可见的人数（没有人写 0，超过 10 写 10）。
6. visible_text：画面里最主要的文字，不超过 40 字。证件号、卡号、手机号、姓名、住址等个人信息
   不要抄录，只写类别，例如"身份证号码"。没有文字写空字符串。
7. sensitive：是否包含证件、银行卡、病历、合同票据等个人信息，或裸露、私密的画面。
只输出 JSON。
```

**用户消息**：只给拍摄时间作参考（做法同 memora：不给地点，避免模型被地点带偏），然后附上图片。

**Schema**：

```json
{
  "type": "object",
  "properties": {
    "description":  { "type": "string" },
    "categories":   { "type": "array", "items": { "type": "string", "enum": ["人物", "合影", "自拍", "孩子", "宠物", "动物", "旅行", "风景", "建筑街景", "美食", "聚会活动", "运动", "日常生活", "花草植物", "交通工具", "物品", "文档票据", "截图", "网图表情包", "其他"] }, "minItems": 1, "maxItems": 2 },
    "tags":         { "type": "array", "items": { "type": "string" }, "minItems": 3, "maxItems": 15 },
    "keywords":     { "type": "array", "items": { "type": "string" }, "maxItems": 10 },
    "people_count": { "type": "integer" },
    "visible_text": { "type": "string" },
    "sensitive":    { "type": "boolean" }
  },
  "required": ["description", "categories", "tags", "keywords", "people_count", "visible_text", "sensitive"]
}
```

- 约束只用 llama.cpp 支持的 Schema 子集，越界的值在代码里夹取，例如 `people_count` 限制在 0–10。
- 推理参数：`temperature` 0.2，`max_tokens` 约 500；JSON 解析失败时最多重试 2 次（同 memora）。
- 提示词带版本号（如 `search-v1`）。改提示词就升版本号，是否重跑由用户手动触发。

**规范化**（在 worker 里做，带单元测试）：
- 去首尾空白；按 `/`、`、` 拆开；去掉标点。
- 丢掉停用词（照片、图片、画面、背景……），只保留 1–8 个字的标签，去重后最多保留 15 个。
- 按同义词表归一，例如"小猫""猫咪"统一成"猫"，原词放进 `keywords`。
- VLM 不给置信度，标签分数按顺序赋值：0.95 − 0.02 × 序号，最低 0.6。

## 7. 数据模型（全部走 migration）

```sql
-- 0007_search_fts.sql（A2）：检索专用全文索引，由 Rust 写入已分词的文本（词与词之间用空格）
CREATE VIRTUAL TABLE search_fts USING fts5(
    tags, keywords, description, visible_text, ocr_text, filename, chars,
    tokenize = 'unicode61'
);   -- rowid = images.id；chars 是单字序列，用于单字查询兜底

-- 0008_image_analysis.sql（A3）
CREATE TABLE image_analysis (
    image_id       INTEGER PRIMARY KEY REFERENCES images(id) ON DELETE CASCADE,
    source         TEXT NOT NULL,              -- 'vlm' | 'memora'
    model          TEXT NOT NULL,              -- 'qwen3.5-9b-q4'
    prompt_version TEXT NOT NULL,              -- 'search-v1' | 'memora-m1'
    description    TEXT NOT NULL,
    categories     TEXT NOT NULL DEFAULT '[]', -- JSON 数组
    keywords       TEXT NOT NULL DEFAULT '[]',
    people_count   INTEGER,
    visible_text   TEXT,
    sensitive      INTEGER NOT NULL DEFAULT 0,
    raw_json       TEXT,
    duration_ms    INTEGER,
    analyzed_at    INTEGER NOT NULL
);

-- 新任务的状态统一放一张表：按"任务类型"记一行，换模型时不必再给 images 加列
CREATE TABLE ai_jobs (
    image_id   INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,      -- 'vlm_analysis' | 'text_embedding' | 'image_embedding'
    model      TEXT NOT NULL,      -- 带版本：'qwen3.5-9b-q4@search-v1'、'bge-small-zh-v1.5'
    status     TEXT NOT NULL,      -- 'ready' | 'failed' | 'skipped'
    attempts   INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    input_hash TEXT,               -- 输入指纹（例如描述文本的哈希），变了就要重算
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (image_id, kind)
);
CREATE INDEX idx_ai_jobs_kind_status ON ai_jobs(kind, status);
```

- **怎么算待处理**：某张图没有"当前模型且 status=ready"的 `ai_jobs` 行，就算待处理（用 LEFT JOIN 计算，不预先插行）。`failed` 且 `attempts < 3` 的会重试。NAS 断开这类 I/O 错误算临时错误，不累加 `attempts`。
- **文件内容变了**（size/mtime 变化）：删掉这张图的 `ai_jobs` 行。旧的 `image_analysis` 先留着，搜索照常可用，等新结果出来再覆盖。
- **VLM 标签**写进 `image_tags`，`source='vlm'`；表的主键本来就含 `source`，可以和 RAM-plus 的 `'ai'` 标签并存。顺带修正 [tags_repo.rs:36](../src-tauri/src/repo/tags_repo.rs#L36)：`upsert_tag` 遇到同名标签会覆盖 `tags.source`，以后以 `image_tags.source` 为准。
- **LanceDB**：
  - 新增 `image_text_emb_<model>`、`image_clip_emb_<model>` 两类表，维度跟随模型。
  - 旧表 `image_embeddings_clip` 保留，直到 A5 评测通过，便于回退。
  - 写入前检查 worker 返回的 `fallback`：兜底向量一律不入库，任务记为 `skipped`。
- **search_fts 的维护**：标签、分析结果、OCR、重命名、扫描删除这几类写入完成后，由 `search_index_service::reindex(image_id)` 重写对应行；另提供 `search_index_rebuild` 命令，可以随时全量重建（以后改分词规则时要用）。
- **memora 导入**（A3）：
  - 一次性命令 `analysis_import_memora(db_path)`，按绝对路径精确匹配 memora 的 `photos.path`，写入 `image_analysis`（`source='memora'`），并把它的类别写成标签。
  - 本库可匹配 8,827 张，其中 6,878 张已有缩略图。
  - 之后正式跑 VLM 时，这些行会被 `source='vlm'` 的结果覆盖。

## 8. 搜索管线 v2

逻辑从命令里移到 `services/search_service.rs`（CLAUDE.md 反模式：不在一个 command 里堆全部逻辑）。`ai_search` 命令保留，作为对外接口。

1. **解析**：沿用 `#标签`、`format:`、`size:`，其余文字用 jieba 分词，再按同义词表扩展。
2. **SQL 预过滤**：目录、格式、大小、标签、删除状态先在 SQL 里求出候选 id 集合。候选集小于某个阈值时，所有召回通道都只在候选集里检索：FTS 用 `rowid IN (…)`，LanceDB 用 `only_if` 预过滤。这一步修掉 §1 里列出的三处后置过滤问题。
3. **四路召回**，每路取前 200：
   - **标签**：精确匹配加同义词匹配，按标签分数排序。
   - **全文**：用 `bm25()` 加列权重，初始值为 tags 4、keywords 3、description 2、visible_text 1.5、ocr 1、filename 1、chars 0.3。先要求所有词都出现（AND）；命中少于 20 条时放宽为任一词出现（OR）。
   - **文本向量**：查询向量与描述向量做近邻检索。
   - **图像向量**：Chinese-CLIP 的文本向量与图像向量做近邻检索。
4. **融合**：RRF，`score = Σ w_c / (60 + rank_c)`，各通道权重 `w_c` 用 §9 的评测来调。向量通道设一个相似度下限：低于下限的命中，只有在文字通道没有结果时才保留。
5. **返回**：每条结果带 `matchedBy`（命中了哪几路）和描述片段（用 FTS 的 `snippet()` 高亮）。界面上显示"命中：描述 / 标签 / 文字"。
6. **降级**：worker 没就绪时只跑标签和全文两路，响应里带 `degraded` 字段。前端提示"语义检索启动中"；收到 `ai:worker_status` 变为 ready 的事件后，自动重跑上一次查询。搜索时会顺带触发 worker 启动，但最多只等约 1.5 秒，不阻塞返回。
7. **性能目标**：1.5 万张的库，p95 ≤ 300ms（不含 worker 冷启动）。FTS 只需几毫秒，暴力向量检索十几毫秒，两路查询编码并行执行。

## 9. 评测：回答"能不能提升"

**查询集**：40 个中文查询（`tests/search_eval/queries.json`），覆盖以下类型：
- 物体：猫、蛋糕、自行车
- 场景：海边、雪地、夜景、游乐园
- 活动：生日、婚礼、吃火锅、毕业
- 人物：两个孩子、全家福、自拍、婴儿
- 文字与截图：聊天记录、发票、二维码、火车票
- 属性：红衣服、逆光
- 组合：海边的小孩、夜里放烟花
- 长尾

查询的具体内容由 Claude 按图库的实际内容来定：参考 RAM 标签频次、memora 的类别分布，并随机看几屏缩略图，保证每个查询在库里都有相关的图。这样选查询会偏向图库里常见的内容，汇总报告里要注明这一点。

**标注**（由 Claude 完成，见 §12）：
- 采用池化标注：把各个对比方案的前 20 条结果合并去重。
- 脚本把每个查询待标的图拼成带编号的拼图（每张 30 格），Claude 逐张看图，记下相关的编号。只做二值判定：相关 / 不相关。
- 以后新增方案时，只补标之前没标过的图。
- 拼图、判定结果、每次运行的输出都放在本机评测目录 `%LOCALAPPDATA%\PhotoViewPlus\eval\`，不进仓库，因为里面有私人照片的路径。仓库里只放查询集和汇总后的指标。
- 判定结果的键是"忽略大小写的相对路径"，不依赖数据库 id。
- 语料按忽略大小写的路径去重，原因见 docs/11 §6.1。

**原型与线上**：
- A0–A5 阶段先用 Python 离线原型计算各个通道（`scripts/search_eval/`，用 ai-worker 的虚拟环境），这样能在动线上代码之前就判断门槛。
- 从 A2 开始，Rust `search_service` 实现的各个通道要用同一份查询集和判定结果复核，指标不能低于原型。

**指标**：Recall@20、nDCG@20、MRR；另外统计 P@10，看"前 10 条里有没有明显错的"。

**对比方案**

| 编号 | 方案 |
|------|------|
| S0 | 现状（OpenAI CLIP） |
| S1 | Chinese-CLIP |
| S2 | RAM 标签 + OCR 全文 |
| S3 | 描述全文（先用 memora 导入的描述） |
| S4 | 描述文本向量 |
| S5 | 融合（标签 + 全文 + 文本向量 + Chinese-CLIP） |
| S6 | 融合，描述换成 VLM `search-v1` 的结果 |

**执行方式**：
- A0–A5：用 `scripts/search_eval/` 下的 Python 原型。语料从正式库的只读副本构建，各通道在本机离线计算，再由计分脚本算出指标。
- A2 起：线上逻辑复核写成 `#[ignore]` 的 Rust 集成测试，直接调 `search_service` 并输出每一路的排名，用 `cargo test --test search_eval -- --ignored` 运行；数据库路径和 worker 地址通过环境变量传入。
- 汇总指标（不含任何路径）写进 `tests/search_eval/report.md`，可以提交到仓库。

### 9.1 A0 评测结果（2026-10-22，离线原型）

- **语料**：正式库去重后 10,174 张。其中 8,181 张有缩略图，参与图像向量通道；8,827 张有 memora 描述。
- **规模**：40 个查询，池化后共 3,653 条判定，1,809 条相关。各方案前 20 名全部标注过。
- **怎么看指标**：多数查询的相关图超过 20 张，Recall@20 的理论上限只有 0.494，所以主要看 nDCG@20 和 P@10。完整表格见 [tests/search_eval/report.md](../tests/search_eval/report.md)。

| 方案 | nDCG@20 | P@10 | MRR |
|------|---------|------|-----|
| S0 OpenAI CLIP（现状） | 0.079 | 0.082 | 0.147 |
| S1 Chinese-CLIP | 0.817 | 0.857 | 0.933 |
| S2 RAM 标签全文 | 0.492 | 0.530 | 0.597 |
| S3 描述全文（双字 BM25） | 0.692 | 0.708 | 0.816 |
| S4 描述文本向量（bge-small-zh） | 0.841 | 0.853 | 0.971 |
| S5 四路等权融合 | 0.881 | 0.920 | 0.942 |
| S5b Chinese-CLIP + RAM 标签（不用描述） | 0.759 | 0.792 | 0.876 |
| **S5w 加权融合（S1×1、S4×1、S3×0.6、S2×0.3）** | **0.900** | **0.920** | **0.975** |

**结论**：
1. **现状确实不可用。** 中文查询下，前 10 条里平均只有 0.8 条相关。
2. **最大的一步是把 CLIP 换成 Chinese-CLIP。** 只需几分钟算力，不依赖大模型，nDCG 就从 0.079 升到 0.817。门槛 G2 在原型上已通过，A5 应提前到 A1 之后马上做。
3. **大模型描述有独立价值。** 仅描述向量（S4）就和 Chinese-CLIP 不相上下。两者互补：
   - 描述明显更好的查询：熊猫（真熊猫照片和熊猫表情包，图像向量分不清）、自拍、穿红衣服的人、雪景、医院。
   - 图像向量明显更好的查询：黑白照片、一桌菜、火车站、在车里。
   - 加权融合在 Chinese-CLIP 基础上，nDCG 再提高 0.08，P@10 再提高 0.06。门槛 G1（S5 比 S0 的 Recall@20 高 0.20 以上）通过：0.408 对比 0.024。
4. **RAM 标签最弱**，等权融合时反而拖累结果：S5b 不如单用 S1。融合里给它低权重（0.3），由 VLM 标签接替后再评估（A7）。
5. **全文检索**：不用词典的"CJK 双字 + 单字"BM25 已经有效。A2 先实现同样的双字方案，不引入 jieba，日后需要时再对比。
6. **A2 的默认融合权重**：图像向量 1.0、描述向量 1.0、描述全文 0.6、标签 0.3。上线实现要用同一套判定复核（§9 原型与线上）。

注意事项：
- 查询由 Claude 按库内常见内容选定，结果会偏向常见内容。
- 描述只覆盖 memora 分析过的 8,827 张，S6（VLM `search-v1` 描述）要等 A6 之后才能评测。
- 模型从 hf-mirror.com 下载：当天 ModelScope 的接口持续返回 504。下载脚本要支持镜像源（`HF_ENDPOINT`）。

**门槛**（决定要不要继续投入）：
- **G1**（A3 之后）：S3 或 S5 的 Recall@20 比 S0 至少高 0.20（绝对值），才做 A4–A6。预期差距会远大于这个数。
- **G2**（A5 之后）：S1 比 S0 好，才把 Chinese-CLIP 设为默认，并记录 ADR-008。
- **G3**（A6 跑完 200 张样本之后）：
  - 人工抽查前 5 个标签的命中率 ≥ 85%（与 docs/05 的验收口径一致）。
  - 吞吐 ≤ 2.5 秒/张。
  - 运行期间满足 docs/11 §8 的"使用中"指标。
  - 三条都满足，才放行全量分析。

## 10. 实施阶段与进度

**状态图例**：⬜ 待办 · 🔄 进行中 · ✅ 已完成 · ⏭️ 已跳过（附原因）

| ID | 内容 | 依赖 | 状态 |
|----|------|------|------|
| A0 | 评测基线：查询集、拼图标注、计分脚本；跑出 S0。顺带用原型跑了 S1–S5，结果见 §9.1 | — | ✅ |
| A1 | 搜索正确性修复：SQL 预过滤、`#标签`单独搜索、兜底时保留过滤条件、兜底向量不入库、逻辑移到 `search_service` | — | ✅ |
| A2 | 全文通道与融合框架：jieba-rs、`search_fts`、索引维护与重建、RRF、界面显示命中来源；先用 RAM 标签和 OCR 喂数据 | A1 | ⬜ |
| A2b | 同义词表与查询扩展（可选） | A2 | ⬜ |
| A3 | 导入 memora 描述（`image_analysis` + `ai_jobs`）；跑 S2/S3，过 G1 | A2 | ⬜ |
| A4 | 文本向量通道：worker `/text/embed`、LanceDB `image_text_emb_<model>`；跑 S4/S5 | A3、G1 | ⬜ |
| A5 | Chinese-CLIP：worker 图像模型可插拔、新表、全量重算、人脸维度解耦；跑 S1/S5，过 G2，写 ADR-008 | A1 | ⬜ |
| A6 | VLM 打标流水线：`llama-server` 监管、worker `/vlm/analyze`、`AnalysisPipeline`（受 docs/11 R2 调度）、进度界面；200 张样本过 G3 后再全量 | R2（docs/11）、G1 | ⬜ |
| A6b | 夜间定时：Windows 计划任务在 03:00 运行 `photo-view-plus.exe --background-analyze`（无窗口、单实例）；应用已经开着时交给正在运行的实例；07:00 前处理完或到点退出；与 memora 共用 8081 服务（§12） | A6 | ⬜ |
| A7 | RAM-plus 下线：tagger 设置增加 `vlm`；VLM 覆盖率 ≥ 95% 后，经用户确认再清理旧的 `'ai'` 标签；标签云按类别分组 | A6 | ⬜ |
| A8 | 收尾：更新 docs/05、docs/08（ADR-008 / ADR-010）、设置页、测试 | 全部 | ⬜ |

**与 docs/11 交错的执行顺序**（2026-10-22 按 §9.1 的结果调整：A5 收益最大、成本最低，提前做）：
1. R0 测量，再做 R1 止血。
2. A5：换 Chinese-CLIP（几分钟算力）。
3. A2、A3：全文通道与融合框架，导入 memora 描述。
4. A4：描述文本向量。
5. 用 Rust `search_service` 复核 G1、G2。
6. 做 R2 资源治理器。
7. 做 A6，过 G3。
8. 做 A7。
9. 最后做 R3–R6。

### A0 评测基线
- **改动**：
  - `tests/search_eval/queries.json`。
  - `scripts/search_eval/`：构建语料、离线计算各通道、池化并生成拼图、计分。
  - Rust 复核用的 `src-tauri/tests/search_eval.rs` 放到 A2 再做。
- **验证**：用正式库的只读副本跑出 S0；顺带把离线就能算的 S2、S3 一起跑出来，提前看到 G1 的趋势。
- **通过标准**：40 个查询全部标完，S0 指标入表。

### A1 搜索正确性修复
- **实测 / 备注**（2026-10-22 完成）：
  - 新增 `services/search_service.rs`：内含 `QueryEncoder` trait，真实现包一层 `AiSupervisor`；`ai_search`、`ai_search_by_image` 改为薄包装，命令名和返回结构都没变。
  - 标签过滤进了 SQL：`ImageQueryParams.tag_names_all`，AND 语义，同名标签去重。
  - 有过滤条件时，先用 `filtered_ids` 求出候选集，再交给 `top_k_filtered` 在 LanceDB 里预过滤。已核实 lancedb 0.30 默认就是 prefilter。
  - 文件名兜底保留全部过滤条件。
  - 兜底处理：worker 返回 `fallback` 时，不写向量和占位标签，该类任务暂停 10 分钟（worker 重启后立即解除）；查询时跳过语义通道。手动"生成标签"遇到兜底也直接报错，不再写占位标签。
  - 新增 `tests/search_tests.rs`，12 个用例。`cargo clippy --all-targets -- -D warnings` 无警告；`cargo test` 44 个全部通过，`thumbnail_tests` 这次也通过了。
  - 遗留问题：库里已有的伪向量无法和真向量区分，等 A5 全量重算时自然覆盖；兜底期间界面上看不出待处理数为什么不减少。
- **改动**：
  - 新增 [services/search_service.rs](../src-tauri/src/services/)，`ai_search` 改为调用它。
  - 预过滤候选集：替换 [ai.rs:108-144](../src-tauri/src/commands/ai.rs#L108-L144) 的"先取 400 行再筛"。
  - 兜底的文件名匹配也带上全部过滤条件（[ai.rs:184-212](../src-tauri/src/commands/ai.rs#L184-L212)）。
  - `EmbedBatchTask` 检查 `fallback`，兜底向量不入库。
- **验证**：`cargo test`，新增用例：
  - `#标签`单独搜索能找到排在第 400 行以后的图；
  - 选中目录时语义搜索不漏；
  - 兜底返回时不写 LanceDB。
- **通过标准**：上述用例通过；S0 的指标不下降。

### A2 全文通道与融合框架
- **改动**：
  - Cargo 依赖 `jieba-rs`；migration `0007_search_fts.sql`；`search_index_service`（单条 reindex + 全量重建）。
  - 在标签、OCR、重命名、删除这些写入路径上挂 reindex。
  - RRF 融合，返回结果带 `matchedBy`；前端显示命中来源和高亮片段。
- **验证**：
  - 单测：分词、单字兜底、AND→OR 放宽、列权重排序。
  - 用 fixtures 跑端到端。
  - 跑 S2。
- **通过标准**：
  - S2 的 Recall@20 高于 S0。
  - 1.5 万张的库上全文通道 p95 < 30ms。
  - 全量重建 < 30 秒。

### A3 导入 memora 描述
- **改动**：migration `0008_image_analysis.sql`（`image_analysis` + `ai_jobs`）；命令 `analysis_import_memora`；导入后重建 `search_fts`。
- **验证**：
  - 导入数为 8,827（与只读统计一致）。
  - 抽 20 条，核对路径与描述对应正确。
  - 跑 S3/S5（此时还没有文本向量通道，S5 先用标签 + 全文 + CLIP）。
- **通过标准**：过 G1。

### A4 文本向量通道
- **改动**：
  - worker 端点 `/text/embed`（批量，返回模型名与维度）；Rust 侧任务类型 `text_embedding`，用 `input_hash` 判断是否需要重算。
  - 新 LanceDB 表；融合时加入这一路。
- **验证**：
  - 全库描述向量化耗时（预期几分钟）。
  - 查询编码 p95。
  - 跑 S4/S5，并对比 bge-small 和 bge-base。
- **通过标准**：S5 不低于 S3；模型选型结果写进本文件。

### A5 Chinese-CLIP
- **改动**：
  - worker 的 CLIP 运行时可按设置 `aiClipModel` 切换实现。
  - `lancedb_repo` 去掉写死的 512，改为按表定维度；人脸表用独立常量。
  - 任务类型 `image_embedding`，换模型后自动全量重算。
- **验证**：
  - 现有 `lancedb_tests` 和人脸相关测试通过。
  - 全库重算耗时（预期几分钟）。
  - 跑 S1/S5。
- **通过标准**：过 G2；写 ADR-008。

### A6 VLM 打标流水线
- **改动**：
  - `services/vlm_supervisor.rs`：
    - 按设置拼启动参数（同 memora `llama.command`），隐藏窗口、低于正常优先级，放进作业对象（见 docs/11 R1-4）；
    - 轮询 `/health`，超时 300 秒；1 分钟内最多重启 3 次，超过就进入 degraded 状态；
    - 没活或负载模式降档后 5 分钟停掉；进入全屏/游戏模式时立刻停掉；
    - 启动前检查剩余显存 ≥ 8.5GB，不够就拒绝启动并提示原因。
  - worker `ai-worker/src/api/vlm.py`：`/vlm/analyze`，每个请求一张图。新增依赖 `pillow-heif`。
  - `services/analysis_pipeline.rs`：
    - 只在空闲模式，或用户点了"全速"时投递任务；
    - 并发数 = min(`-np`, 拿到的读闸门许可数)；
    - 排序参考 memora：相机目录优先，微信等目录最后，同目录内新照片在前；
    - 结果在一个事务里写入 `image_analysis`、`image_tags`、`ai_jobs`、`search_fts`，并把这张图的 `text_embedding` 标为待重算；
    - 进度事件 `analysis:progress` 每秒最多发一次。
  - 降档时立刻停止投递新任务，在途的最多 4 张自然完成（约 7 秒）。可选优化：改用流式请求，降档时主动断开连接，让 `llama-server` 立即停掉对应槽位（实施时验证这个行为）。
- **验证**：
  - Python 单测：规范化规则、Schema 校验、HEIC 读取。
  - Rust 测试：监管状态机（启动、健康、重启上限、按模式停止）。
  - 跑 200 张样本：人工抽查标签与描述，测吞吐和显存峰值，同时跑 docs/11 的探针。
  - 长边 1024 和 768 各跑 30 张做对比。
- **通过标准**：过 G3 后，在空闲时段全量运行。

### A7 RAM-plus 下线
- **改动**：`aiTaggerModel` 增加 `vlm`，选中时 `TagBatchTask` 停用、不加载 RAM-plus；VLM 覆盖率 ≥ 95% 后，在界面里提示用户确认，再清理 `source='ai'` 的旧标签；标签云按 VLM 类别分组。
- **验证**：显存占用对比；标签云抽查。

## 11. 风险与对策

| 风险 | 对策 |
|------|------|
| 显存争用：VLM、torch worker 和桌面合计超过 12GB 后会溢出到共享内存，整机变慢 | 启动前检查显存；VLM 接管后 RAM-plus 不再加载；只在空闲模式运行 |
| llama.cpp 与 Qwen3.5 的兼容性 | 固定使用 memora 验证过的 build 11351，并在文档里注明版本 |
| VLM 幻觉：描述里写了画面中没有的东西，导致全文误召回 | 多路融合降低单路误差；界面显示命中来源；A6 样本抽查 |
| 耗时长 | 先导入 memora，剩余工作量减少约 87%；只在空闲时跑，并显示预计剩余时间 |
| NAS 不可用 | I/O 错误按临时错误处理，不累加失败次数，下轮重试 |
| 读原图超过网络盘并发红线 | VLM 读原图也走 Rust 的读闸门（§4） |
| 隐私 | 描述和可见文字只存本地 SQLite；`sensitive` 的图在搜索结果里默认打码（沿用 memora 的做法，可选） |
| 许可证 | Qwen3.5 Apache-2.0；llama.cpp MIT；Chinese-CLIP MIT；bge MIT；jieba-rs MIT |

## 12. 已确认的决定（2026-10-22）

1. **复用 memora 的 `llama-server` 和模型文件**：只在设置里写路径，不复制。本机路径：
   - `vlmServerExe = D:\code\mygit\memora\runtime\llama\Library\bin\llama-server.exe`
   - `vlmModelPath = D:\code\mygit\memora\models\qwen3.5-9b\Qwen3.5-9B-UD-Q4_K_XL.gguf`
   - `vlmMmprojPath = D:\code\mygit\memora\models\qwen3.5-9b\mmproj-F16.gguf`
2. **导入 memora 已有的 8,827 条描述**（A3），用作冷启动数据和评测数据。
3. **评测由 Claude 标注**：方式见 §9，你可以随时抽查标注用的拼图。
4. **加夜间定时运行**（A6b，§10）。注意本机已有 memora 的计划任务 `\Memora\update`：每天 02:00 运行，按需在 127.0.0.1:8081 启动 `llama-server`、用完关闭。两边必须共用同一个服务，避免同时加载两份 7GB 模型把显存撑爆。规则如下：
   - 本项目使用同一端口 8081 和同一套启动参数。
   - 8081 上已经有健康的服务时，直接复用，并且不负责关闭它。
   - 自己启动的服务，关闭前先确认 `/slots` 全部空闲。
   - 夜间窗口默认 03:00–07:00，避开 memora 的 02:00。
5. **仍待处理**：NAS 上 `camera`/`Camera` 两个目录的合并（docs/11 §6.1），需要你在 NAS 上操作。处理之前，评测语料会按"忽略大小写的路径"去重。
