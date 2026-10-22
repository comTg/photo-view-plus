-- docs/10 §7（A3）：图片的画面分析结果（描述、类别、检索同义词、人数、画面文字、是否敏感）。
-- 来源：'memora'（一次性导入 memora 已有的描述）或 'vlm'（本项目的大模型打标，A6）；
-- vlm 的结果优先，memora 导入不会覆盖它。ai_jobs 表在 0007 里已经建好。

CREATE TABLE image_analysis (
    image_id       INTEGER PRIMARY KEY REFERENCES images(id) ON DELETE CASCADE,
    source         TEXT NOT NULL,              -- 'vlm' | 'memora'
    model          TEXT NOT NULL,              -- 'qwen3.5-9b-q4'
    prompt_version TEXT NOT NULL,              -- 'search-v1' | 'memora-m1'
    description    TEXT NOT NULL,
    categories     TEXT NOT NULL DEFAULT '[]', -- JSON 数组
    keywords       TEXT NOT NULL DEFAULT '[]', -- JSON 数组
    people_count   INTEGER,
    visible_text   TEXT,
    sensitive      INTEGER NOT NULL DEFAULT 0,
    raw_json       TEXT,
    duration_ms    INTEGER,
    analyzed_at    INTEGER NOT NULL
);

CREATE INDEX idx_image_analysis_source ON image_analysis(source);

-- 描述、同义词、画面文字参与检索：变动时记进检索索引的积压表（见 0008）。
CREATE TRIGGER search_fts_dirty_analysis_ai AFTER INSERT ON image_analysis BEGIN
    INSERT OR IGNORE INTO search_fts_dirty(image_id) VALUES (new.image_id);
END;

CREATE TRIGGER search_fts_dirty_analysis_au AFTER UPDATE ON image_analysis BEGIN
    INSERT OR IGNORE INTO search_fts_dirty(image_id) VALUES (new.image_id);
END;

CREATE TRIGGER search_fts_dirty_analysis_ad AFTER DELETE ON image_analysis BEGIN
    INSERT OR IGNORE INTO search_fts_dirty(image_id) VALUES (old.image_id);
END;
