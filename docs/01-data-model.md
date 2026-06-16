# 01 · 数据模型

> 所有持久化数据的事实来源。改 schema 必须先改本文档 + 加 migration，再写代码。

## 1. 存储位置总览

| 数据 | 存储 | 路径 |
|------|------|------|
| 元数据 / 标签 / 任务 | SQLite | `%LOCALAPPDATA%\PhotoViewPlus\db\app.sqlite` |
| 向量 | LanceDB | `%LOCALAPPDATA%\PhotoViewPlus\vectors\` |
| 缩略图 | WebP 文件 | `%LOCALAPPDATA%\PhotoViewPlus\thumbs\<ab>\<hash>.webp` |
| 模型权重 | 本地 | `%LOCALAPPDATA%\PhotoViewPlus\models\` |
| 日志 | 文本 | `%LOCALAPPDATA%\PhotoViewPlus\logs\` |
| 用户设置 | JSON | `%LOCALAPPDATA%\PhotoViewPlus\config.json` |
| 撤销日志 | SQLite 表 | 同主库 `undo_log` 表 |

`%LOCALAPPDATA%` 在 Windows = `C:\Users\<user>\AppData\Local\`。dev profile 在子目录 `dev\` 下。

## 2. SQLite 表结构

下面每张表给出 **创建语句 + 字段说明 + 索引**。表名全小写，字段全 snake_case，外键开启 `PRAGMA foreign_keys = ON`。

### 2.1 `roots`（用户添加的根目录）

```sql
CREATE TABLE roots (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    path            TEXT NOT NULL UNIQUE,        -- 规范化后的绝对路径
    label           TEXT,                        -- 用户起的名字
    type            TEXT NOT NULL,               -- 'local' | 'network'
    enabled         INTEGER NOT NULL DEFAULT 1,
    last_scan_at    INTEGER,                     -- Unix 秒
    created_at      INTEGER NOT NULL,
    settings_json   TEXT                         -- 每根目录独立设置（并发数、忽略规则）
);
```

| 字段 | 说明 |
|------|------|
| `type` | `'local'` 用于本地盘，扫描并发 8-16；`'network'` 用于 SMB / 映射盘，并发 ≤4 |
| `path` | 规范化（去除尾随分隔符、转小写盘符）后存储；SMB 用 UNC 形式 `\\server\share` |
| `settings_json` | 形如 `{"ignore_patterns": ["**/Thumbs.db"], "max_concurrency": 4}` |

### 2.2 `images`（图片元数据，主表）

```sql
CREATE TABLE images (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    root_id         INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
    rel_path        TEXT NOT NULL,               -- 相对 root 的路径，含文件名
    filename        TEXT NOT NULL,               -- 仅文件名（冗余，方便搜索）
    extension       TEXT NOT NULL,               -- 'jpg' / 'png' / 'webp' / 'heic' ...
    size_bytes      INTEGER NOT NULL,
    mtime           INTEGER NOT NULL,            -- 文件修改时间（Unix 秒）
    width           INTEGER,                     -- 解码后写入，未解码时为 NULL
    height          INTEGER,
    orientation     INTEGER,                     -- EXIF Orientation
    taken_at        INTEGER,                     -- EXIF DateTimeOriginal（Unix 秒）
    gps_lat         REAL,
    gps_lng         REAL,
    camera_make     TEXT,
    camera_model    TEXT,

    -- 缩略图
    thumb_status    TEXT NOT NULL DEFAULT 'pending',   -- 'pending' | 'ready' | 'failed' | 'unsupported'
    thumb_hash      TEXT,                        -- 缩略图文件名前缀（SHA1 前 16 字符）

    -- 去重相关（MVP2 加入，migration 0002）
    blake3          TEXT,                        -- 全文件 BLAKE3，hex
    phash           INTEGER,                     -- 64-bit pHash，按 INTEGER 存
    dhash           INTEGER,                     -- 64-bit dHash
    hash_status     TEXT NOT NULL DEFAULT 'pending',  -- 'pending' | 'ready' | 'failed'

    -- 标签 / 向量（MVP3 加入，migration 0003）
    tag_status      TEXT NOT NULL DEFAULT 'pending',
    embedding_status TEXT NOT NULL DEFAULT 'pending',

    -- OCR / 人脸（MVP4 加入，migration 0004）
    ocr_status      TEXT NOT NULL DEFAULT 'disabled',
    ocr_text        TEXT,
    face_status     TEXT NOT NULL DEFAULT 'disabled',

    indexed_at      INTEGER NOT NULL,
    deleted_at      INTEGER,                     -- 软删（移到回收站时写入）

    UNIQUE(root_id, rel_path)
);

CREATE INDEX idx_images_root_mtime   ON images(root_id, mtime DESC);
CREATE INDEX idx_images_extension    ON images(extension);
CREATE INDEX idx_images_size         ON images(size_bytes);
CREATE INDEX idx_images_taken_at     ON images(taken_at) WHERE taken_at IS NOT NULL;
CREATE INDEX idx_images_blake3       ON images(blake3) WHERE blake3 IS NOT NULL;       -- MVP2
CREATE INDEX idx_images_phash        ON images(phash) WHERE phash IS NOT NULL;         -- MVP2
CREATE INDEX idx_images_thumb_status ON images(thumb_status) WHERE thumb_status='pending';
CREATE INDEX idx_images_deleted      ON images(deleted_at) WHERE deleted_at IS NOT NULL;
```

设计要点：
- **路径策略**：`root_id + rel_path` 是唯一键，盘符变化时只需更新 `roots.path`，所有图片不动
- **状态字段**：`thumb_status` / `hash_status` / `tag_status` / `embedding_status` / `ocr_status` / `face_status` 让每个流水线独立推进，不互相阻塞
- **软删**：`deleted_at` 不为 NULL = 已移到回收站，但元数据保留 30 天用于撤销
- **拍摄时间**：用 EXIF 而不是 mtime，因为很多图被处理过后 mtime 变了

### 2.3 `tags`（标签字典）

```sql
CREATE TABLE tags (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT NOT NULL UNIQUE,
    source          TEXT NOT NULL,               -- 'ai' | 'user' | 'ocr' | 'face_cluster'
    category        TEXT,                        -- 'object' | 'scene' | 'person' | 'color' | ...
    created_at      INTEGER NOT NULL
);

CREATE INDEX idx_tags_source ON tags(source);
```

### 2.4 `image_tags`（多对多关联）

```sql
CREATE TABLE image_tags (
    image_id        INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    tag_id          INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    score           REAL NOT NULL DEFAULT 1.0,   -- AI 置信度 0..1
    source          TEXT NOT NULL,               -- 'ai' / 'user'（user 标记会覆盖 ai）
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (image_id, tag_id, source)
);

CREATE INDEX idx_image_tags_tag ON image_tags(tag_id, score DESC);
```

### 2.5 `duplicate_groups` / `duplicate_items`（去重分组，MVP2）

```sql
CREATE TABLE duplicate_groups (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    method          TEXT NOT NULL,               -- 'exact' | 'phash' | 'semantic'
    threshold       REAL,                        -- phash hamming 阈值；exact 为 NULL
    keep_image_id   INTEGER,                     -- 用户确认保留的图
    status          TEXT NOT NULL DEFAULT 'open', -- 'open' | 'resolved' | 'dismissed'
    created_at      INTEGER NOT NULL,
    resolved_at     INTEGER
);

CREATE INDEX idx_dup_groups_status ON duplicate_groups(status);

CREATE TABLE duplicate_items (
    group_id        INTEGER NOT NULL REFERENCES duplicate_groups(id) ON DELETE CASCADE,
    image_id        INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    similarity      REAL,                        -- 0..1（1=完全相同）
    PRIMARY KEY (group_id, image_id)
);

CREATE INDEX idx_dup_items_image ON duplicate_items(image_id);
```

设计要点：
- 一张图可同时属于多个 group（exact 一个、phash 一个）
- `keep_image_id` 用于"保留最大分辨率"等批量策略的预选
- `resolved` = 用户处理过；`dismissed` = 用户标记"不是重复"

### 2.6 `scan_tasks`（扫描任务历史）

```sql
CREATE TABLE scan_tasks (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    root_id         INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
    status          TEXT NOT NULL,               -- 'queued' | 'running' | 'paused' | 'done' | 'failed'
    total_files     INTEGER,
    processed       INTEGER DEFAULT 0,
    started_at      INTEGER,
    finished_at     INTEGER,
    error           TEXT,
    cursor_json     TEXT                         -- 断点续扫的游标（路径 + offset）
);

CREATE INDEX idx_scan_tasks_root ON scan_tasks(root_id, started_at DESC);
```

### 2.7 `undo_log`（撤销日志，跨 MVP）

```sql
CREATE TABLE undo_log (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    action          TEXT NOT NULL,               -- 'trash' | 'move' | 'rename' | 'tag_remove'
    payload_json    TEXT NOT NULL,               -- 行为参数
    can_undo_until  INTEGER NOT NULL,            -- Unix 秒
    undone_at       INTEGER,
    created_at      INTEGER NOT NULL
);

CREATE INDEX idx_undo_until ON undo_log(can_undo_until) WHERE undone_at IS NULL;
```

### 2.8 `smart_albums`（智能相册，MVP4）

```sql
CREATE TABLE smart_albums (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT NOT NULL,
    filter_json     TEXT NOT NULL,               -- 保存的筛选条件（DSL）
    icon            TEXT,
    sort_order      INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL
);
```

### 2.9 `face_clusters`（人脸聚类，MVP4）

```sql
CREATE TABLE face_clusters (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    label           TEXT,                        -- 用户起名
    sample_image_id INTEGER REFERENCES images(id),
    face_count      INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL
);

CREATE TABLE faces (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    image_id        INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    cluster_id      INTEGER REFERENCES face_clusters(id),
    bbox_json       TEXT NOT NULL,               -- {x,y,w,h}
    confidence      REAL NOT NULL,
    embedding_ref   TEXT                         -- 在 LanceDB 中的 row id
);

CREATE INDEX idx_faces_image   ON faces(image_id);
CREATE INDEX idx_faces_cluster ON faces(cluster_id);
```

## 3. LanceDB 向量库（MVP3 起）

LanceDB 用作向量列存。每个模型独立 table，因为不同模型维度可能不同。

### 3.1 Table `image_embeddings_clip`

| 列名 | 类型 | 说明 |
|------|------|------|
| `image_id` | int64 | 与 SQLite `images.id` 关联 |
| `model` | string | 模型版本，如 `'clip-vit-b-32'` |
| `embedding` | fixed_size_list<float16, 512> | 归一化后的向量 |
| `created_at` | timestamp | 写入时间 |

索引：在 `embedding` 上建 IVF_PQ 或 HNSW（数据量 < 50 万用 HNSW，> 50 万用 IVF_PQ）。

### 3.2 Table `face_embeddings`（MVP4）

| 列名 | 类型 |
|------|------|
| `face_id` | int64（关联 `faces.id`） |
| `embedding` | fixed_size_list<float16, 512> |

### 3.3 一致性策略

- 写：先写 SQLite（事务内更新 `embedding_status='ready'`），再写 LanceDB。失败把 status 改回 `pending` 并入 `outbox` 表重试
- 读：永远先查 SQLite 的 status，确认 ready 再去 LanceDB
- 删：图片软删时，向量保留 30 天（与撤销窗口对齐）

## 4. 缩略图缓存

### 4.1 路径布局

```
%LOCALAPPDATA%\PhotoViewPlus\thumbs\
    ab\
        abcdef0123456789.webp        # 256px 主缩略图
        abcdef0123456789@512.webp    # 512px（按需，详情页使用）
    cd\
        cd1234abcd5678ef.webp
```

- 前两位作为子目录，避免单目录过万文件
- 文件名 = `thumb_hash`（SHA1(root_id || rel_path) 前 16 字符的 hex）
- 格式 WebP，quality 78，无 alpha 的图用 lossy，有 alpha 的用 lossless

### 4.2 失效策略

- 原图 `mtime` 变化 → 旧缩略图删除，重新生成
- 用户清缓存 → 整个 thumbs 目录可安全删除（数据库会重新标 `thumb_status='pending'`）
- 缓存大小上限默认 5GB（设置可改），LRU 淘汰

## 5. Migration 约定

### 5.1 文件命名

```
src-tauri/src/migrations/
    0001_init.sql                   # MVP1 基础表
    0002_hash_columns.sql           # MVP2 加 blake3 / phash / dhash / hash_status
    0003_ai_columns.sql             # MVP3 加 tag_status / embedding_status + tags / image_tags
    0004_ocr_face.sql               # MVP4 加 ocr / face 相关
    0005_smart_albums.sql           # MVP4
```

四位编号 + 简短描述。**禁止改已合并的 migration 文件**，错了发新 migration 修。

### 5.2 运行机制

启动时检查 `schema_version` 表（migration 0001 创建）：

```sql
CREATE TABLE schema_version (
    version INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);
```

按编号顺序运行所有未应用的 migration，每个 migration 在事务内执行。失败回滚并报错退出。

### 5.3 数据迁移

只改结构 → 单 `.sql` 文件。  
需要遍历数据回填 → `.sql` + 对应的 `.rs`（Rust 函数），migration runner 按编号配对调用。

## 6. 路径规范化（避免重复扫描）

不同输入要规范成同一个 root：

| 输入 | 规范化后 |
|------|----------|
| `D:\Photos`、`d:\photos\` | `D:\Photos` |
| `\\nas\share\` | `\\nas\share` |
| `Z:\` 映射到 `\\nas\share` | 询问用户、记两条 root 并标记 `aliased_to` |

实现：`src-tauri/src/utils/path_normalize.rs`，所有 root 写入前必经此函数。

## 7. 容量与性能预算

| 维度 | 目标 |
|------|------|
| 单库图片上限 | 100 万张 |
| SQLite 文件大小 | < 2 GB（10 万图约 200MB） |
| LanceDB 大小 | 10 万图 × 512 × float16 = 100MB 左右 |
| 缩略图 5GB 缓存能装 | 约 30 万张 256px WebP |
| 主键查询 | < 1ms |
| 按目录 + 时间排序分页 | < 50ms（覆盖索引） |
| pHash hamming top-100 | < 200ms（BK-tree） |
| CLIP 文本搜 top-100 | < 100ms（10 万向量，HNSW） |

超出目标记录到 `docs/08-roadmap.md` 风险表。

## 8. 接下来读

- 这些表怎么在 UI 里呈现 → [`02-ui-design.md`](./02-ui-design.md)
- 怎么开始建第一个表 → [`03-mvp1-browse.md`](./03-mvp1-browse.md) 任务 2
