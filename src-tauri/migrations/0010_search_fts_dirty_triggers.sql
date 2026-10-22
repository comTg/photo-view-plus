-- 修正 0008 / 0009 的检索索引积压触发器。
--
-- 原来用 INSERT OR IGNORE 防重复，但 SQLite 规定：触发器由带冲突子句的语句触发时（例如
-- image_analysis 的 UPSERT 走 DO UPDATE 分支），触发器内部的 OR IGNORE 会被外层语句的冲突策略
-- 覆盖成 ABORT——积压表里已经有这张图时，整条写入报 UNIQUE 约束错误（memora 重复导入、VLM 覆盖
-- 描述都会遇到）。改成先查再插，不产生冲突，跟外层语句用什么冲突策略无关。

DROP TRIGGER search_fts_dirty_images_ai;
DROP TRIGGER search_fts_dirty_images_au;
DROP TRIGGER search_fts_dirty_images_ad;
DROP TRIGGER search_fts_dirty_tags_ai;
DROP TRIGGER search_fts_dirty_tags_au;
DROP TRIGGER search_fts_dirty_tags_ad;
DROP TRIGGER search_fts_dirty_analysis_ai;
DROP TRIGGER search_fts_dirty_analysis_au;
DROP TRIGGER search_fts_dirty_analysis_ad;

CREATE TRIGGER search_fts_dirty_images_ai AFTER INSERT ON images BEGIN
    INSERT INTO search_fts_dirty(image_id) SELECT new.id
    WHERE NOT EXISTS (SELECT 1 FROM search_fts_dirty WHERE image_id = new.id);
END;

CREATE TRIGGER search_fts_dirty_images_au AFTER UPDATE OF filename, ocr_text, deleted_at ON images BEGIN
    INSERT INTO search_fts_dirty(image_id) SELECT new.id
    WHERE NOT EXISTS (SELECT 1 FROM search_fts_dirty WHERE image_id = new.id);
END;

CREATE TRIGGER search_fts_dirty_images_ad AFTER DELETE ON images BEGIN
    INSERT INTO search_fts_dirty(image_id) SELECT old.id
    WHERE NOT EXISTS (SELECT 1 FROM search_fts_dirty WHERE image_id = old.id);
END;

CREATE TRIGGER search_fts_dirty_tags_ai AFTER INSERT ON image_tags BEGIN
    INSERT INTO search_fts_dirty(image_id) SELECT new.image_id
    WHERE NOT EXISTS (SELECT 1 FROM search_fts_dirty WHERE image_id = new.image_id);
END;

CREATE TRIGGER search_fts_dirty_tags_au AFTER UPDATE ON image_tags BEGIN
    INSERT INTO search_fts_dirty(image_id) SELECT old.image_id
    WHERE NOT EXISTS (SELECT 1 FROM search_fts_dirty WHERE image_id = old.image_id);
    INSERT INTO search_fts_dirty(image_id) SELECT new.image_id
    WHERE NOT EXISTS (SELECT 1 FROM search_fts_dirty WHERE image_id = new.image_id);
END;

CREATE TRIGGER search_fts_dirty_tags_ad AFTER DELETE ON image_tags BEGIN
    INSERT INTO search_fts_dirty(image_id) SELECT old.image_id
    WHERE NOT EXISTS (SELECT 1 FROM search_fts_dirty WHERE image_id = old.image_id);
END;

CREATE TRIGGER search_fts_dirty_analysis_ai AFTER INSERT ON image_analysis BEGIN
    INSERT INTO search_fts_dirty(image_id) SELECT new.image_id
    WHERE NOT EXISTS (SELECT 1 FROM search_fts_dirty WHERE image_id = new.image_id);
END;

CREATE TRIGGER search_fts_dirty_analysis_au AFTER UPDATE ON image_analysis BEGIN
    INSERT INTO search_fts_dirty(image_id) SELECT new.image_id
    WHERE NOT EXISTS (SELECT 1 FROM search_fts_dirty WHERE image_id = new.image_id);
END;

CREATE TRIGGER search_fts_dirty_analysis_ad AFTER DELETE ON image_analysis BEGIN
    INSERT INTO search_fts_dirty(image_id) SELECT old.image_id
    WHERE NOT EXISTS (SELECT 1 FROM search_fts_dirty WHERE image_id = old.image_id);
END;
