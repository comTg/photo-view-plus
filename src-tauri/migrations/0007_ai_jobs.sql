-- docs/10 §7：AI 新任务的状态统一放一张表，按"任务类型"记一行；换模型时不必再给 images 加列。
-- 待处理 = 没有"当前模型且 status=ready"的行（LEFT JOIN 计算，不预先插行）；failed 且 attempts < 3 会重试。
-- 文件内容变化（size/mtime）时删掉这张图的全部 ai_jobs 行（见 images_repo::upsert_scanned_image）。

CREATE TABLE ai_jobs (
    image_id   INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,      -- 'image_embedding' | 'text_embedding' | 'vlm_analysis'
    model      TEXT NOT NULL,      -- 带版本：'chinese-clip-vit-b-16'、'qwen3.5-9b-q4@search-v1'
    status     TEXT NOT NULL,      -- 'ready' | 'failed' | 'skipped'
    attempts   INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    input_hash TEXT,               -- 输入指纹（例如描述文本的哈希），变了就要重算
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (image_id, kind)
);

CREATE INDEX idx_ai_jobs_kind_status ON ai_jobs(kind, status);

-- 继承旧的 CLIP 向量状态：这些图的向量在 LanceDB 旧表 image_embeddings_clip 里（docs/10 A5 保留它
-- 以便回退）。切回旧模型时不必重算；切到新模型时它们按"模型不同"自然进入待处理。
INSERT INTO ai_jobs(image_id, kind, model, status, attempts, updated_at)
SELECT id, 'image_embedding', 'clip-vit-b-32', 'ready', 1, CAST(strftime('%s', 'now') AS INTEGER)
FROM images
WHERE embedding_status = 'ready' AND deleted_at IS NULL;
