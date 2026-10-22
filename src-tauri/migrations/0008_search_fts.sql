-- docs/10 §5.4 / §7（A2）：检索专用全文索引。
-- 由 Rust（search_index_service）写入已经分好词的文本，词与词之间用空格连接；unicode61 只按空格
-- 切开，CJK 双字词本身不含分隔符，保持完整。rowid = images.id。
-- chars 列是单字序列（权重很低），兜住"猫""车"这类单字查询。
CREATE VIRTUAL TABLE search_fts USING fts5(
    tags,
    keywords,
    description,
    visible_text,
    ocr_text,
    filename,
    chars,
    tokenize = 'unicode61'
);

-- 待重建队列：任何影响检索文本的写入都由触发器记下图片 id，search_index_service 批量重写对应行。
-- 这样标签、OCR、重命名、扫描删除、回收站 / 撤销等所有写入路径都不会漏掉。
CREATE TABLE search_fts_dirty (
    image_id INTEGER PRIMARY KEY
);

CREATE TRIGGER search_fts_dirty_images_ai AFTER INSERT ON images BEGIN
    INSERT OR IGNORE INTO search_fts_dirty(image_id) VALUES (new.id);
END;

CREATE TRIGGER search_fts_dirty_images_au AFTER UPDATE OF filename, ocr_text, deleted_at ON images BEGIN
    INSERT OR IGNORE INTO search_fts_dirty(image_id) VALUES (new.id);
END;

CREATE TRIGGER search_fts_dirty_images_ad AFTER DELETE ON images BEGIN
    INSERT OR IGNORE INTO search_fts_dirty(image_id) VALUES (old.id);
END;

CREATE TRIGGER search_fts_dirty_tags_ai AFTER INSERT ON image_tags BEGIN
    INSERT OR IGNORE INTO search_fts_dirty(image_id) VALUES (new.image_id);
END;

CREATE TRIGGER search_fts_dirty_tags_au AFTER UPDATE ON image_tags BEGIN
    INSERT OR IGNORE INTO search_fts_dirty(image_id) VALUES (old.image_id);
    INSERT OR IGNORE INTO search_fts_dirty(image_id) VALUES (new.image_id);
END;

CREATE TRIGGER search_fts_dirty_tags_ad AFTER DELETE ON image_tags BEGIN
    INSERT OR IGNORE INTO search_fts_dirty(image_id) VALUES (old.image_id);
END;

-- 建好后整库排队，启动后由后台批量建索引（不在 migration 里做：分词规则在 Rust 侧）。
INSERT INTO search_fts_dirty(image_id) SELECT id FROM images WHERE deleted_at IS NULL;
