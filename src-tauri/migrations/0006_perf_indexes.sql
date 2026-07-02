-- docs/09 性能优化：查询—索引对齐。
-- 全部为 CREATE / DROP INDEX，纯性能优化，无表结构/语义变更。
-- 用 IF NOT EXISTS / IF EXISTS 保证幂等，可安全重复应用。

-- P0-6：全库"所有照片"视图默认 ORDER BY mtime DESC。此时不带 root_id 过滤，
-- 复合索引 idx_images_root_mtime(root_id, mtime) 的最左列用不上 → 全表 sort。
-- 补一个 mtime 独立索引支撑无 root 过滤时的排序（asc/desc 均可反向扫描）。
CREATE INDEX IF NOT EXISTS idx_images_mtime ON images(mtime);

-- P1-2：日期筛选 / 时间线 / 地图排序都用 COALESCE(taken_at, mtime) 这个表达式，
-- 裸列部分索引 idx_images_taken_at 命中不了表达式 → 全表。加表达式索引，四处共享。
CREATE INDEX IF NOT EXISTS idx_images_effective_ts ON images(COALESCE(taken_at, mtime));

-- P1-1：哈希流水线的 pending 扫描找的是 IS NULL 的行，而 0002 建的 idx_images_blake3 /
-- idx_images_phash 都是 WHERE ... IS NOT NULL，正好排除了要找的行 → 全表扫描。
-- 补上与查询谓词匹配的部分索引：
--   pending_blake3_images: WHERE deleted_at IS NULL AND blake3 IS NULL
CREATE INDEX IF NOT EXISTS idx_images_blake3_pending
    ON images(id) WHERE blake3 IS NULL AND deleted_at IS NULL;
--   pending_dhash_images:  WHERE deleted_at IS NULL AND dhash IS NULL AND thumb_status = 'ready'
--   （索引谓词被查询谓词蕴含即可命中，deleted_at 由查询侧再过滤）
CREATE INDEX IF NOT EXISTS idx_images_dhash_pending
    ON images(id) WHERE dhash IS NULL AND thumb_status = 'ready';

-- P1-1：视觉去重实际用的是 dhash（all_dhashes / pending_dhash），
-- 而 0002 却把索引建在了从不按值查询的 phash 列上。纠正这处错配：
DROP INDEX IF EXISTS idx_images_phash;
CREATE INDEX IF NOT EXISTS idx_images_dhash ON images(dhash) WHERE dhash IS NOT NULL;
