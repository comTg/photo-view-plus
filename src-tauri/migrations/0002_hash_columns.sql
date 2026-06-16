-- MVP2 去重所需 schema 扩展。
-- 与 docs/01-data-model.md § 2.2 / § 2.5 对齐：
--   * images 追加哈希列（blake3 / phash / dhash / hash_status）
--   * 新增 duplicate_groups / duplicate_items 两张表
-- SQLite 限制：ADD COLUMN 一次只能加一列，因此拆 4 条 ALTER。

ALTER TABLE images ADD COLUMN blake3      TEXT;
ALTER TABLE images ADD COLUMN phash       INTEGER;
ALTER TABLE images ADD COLUMN dhash       INTEGER;
ALTER TABLE images ADD COLUMN hash_status TEXT NOT NULL DEFAULT 'pending';

CREATE INDEX idx_images_blake3      ON images(blake3) WHERE blake3 IS NOT NULL;
CREATE INDEX idx_images_phash       ON images(phash) WHERE phash IS NOT NULL;
CREATE INDEX idx_images_hash_status ON images(hash_status) WHERE hash_status = 'pending';

CREATE TABLE duplicate_groups (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    method        TEXT NOT NULL,                  -- 'exact' | 'phash' | 'semantic'
    threshold     REAL,                            -- phash hamming 阈值；exact 为 NULL
    keep_image_id INTEGER,                         -- 用户确认保留的图（可空）
    status        TEXT NOT NULL DEFAULT 'open',    -- 'open' | 'resolved' | 'dismissed'
    created_at    INTEGER NOT NULL,
    resolved_at   INTEGER
);

CREATE INDEX idx_dup_groups_status ON duplicate_groups(status);

CREATE TABLE duplicate_items (
    group_id   INTEGER NOT NULL REFERENCES duplicate_groups(id) ON DELETE CASCADE,
    image_id   INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    similarity REAL,                                -- 0..1（1=完全相同）
    PRIMARY KEY (group_id, image_id)
);

CREATE INDEX idx_dup_items_image ON duplicate_items(image_id);
