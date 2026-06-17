-- MVP3 AI 搜索所需 schema 扩展。
-- 与 docs/01-data-model.md § 2.2 / § 2.3 / § 2.4 对齐：
--   * images 追加标签与向量处理状态
--   * 新增 tags / image_tags 两张表

ALTER TABLE images ADD COLUMN tag_status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE images ADD COLUMN embedding_status TEXT NOT NULL DEFAULT 'pending';

CREATE INDEX idx_images_tag_status
    ON images(tag_status)
    WHERE tag_status = 'pending';

CREATE INDEX idx_images_embedding_status
    ON images(embedding_status)
    WHERE embedding_status = 'pending';

CREATE TABLE tags (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL UNIQUE,
    source     TEXT NOT NULL,                    -- 'ai' | 'user' | 'ocr' | 'face_cluster'
    category   TEXT,                             -- 'object' | 'scene' | 'person' | 'color' | ...
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_tags_source ON tags(source);

CREATE TABLE image_tags (
    image_id   INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    tag_id     INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    score      REAL NOT NULL DEFAULT 1.0,
    source     TEXT NOT NULL,                    -- 'ai' | 'user'
    created_at INTEGER NOT NULL,
    PRIMARY KEY (image_id, tag_id, source)
);

CREATE INDEX idx_image_tags_tag ON image_tags(tag_id, score DESC);
