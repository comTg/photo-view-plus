-- MVP1 初始 schema。
-- 字段集与 docs/01-data-model.md § 2 对齐，但仅含 MVP1 需要的列；
-- MVP2/3/4 的列（blake3 / phash / tag_status / face_* 等）由后续 migration 追加。

CREATE TABLE schema_version (
    version    INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

CREATE TABLE roots (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    path          TEXT NOT NULL UNIQUE,
    label         TEXT,
    type          TEXT NOT NULL,                 -- 'local' | 'network'
    enabled       INTEGER NOT NULL DEFAULT 1,
    last_scan_at  INTEGER,
    created_at    INTEGER NOT NULL,
    settings_json TEXT
);

CREATE TABLE images (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    root_id      INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
    rel_path     TEXT NOT NULL,
    filename     TEXT NOT NULL,
    extension    TEXT NOT NULL,
    size_bytes   INTEGER NOT NULL,
    mtime        INTEGER NOT NULL,

    -- 解码后写入（扫描时只填基础字段，EXIF 读取阶段补 width/height/taken_at 等）
    width        INTEGER,
    height       INTEGER,
    orientation  INTEGER,
    taken_at     INTEGER,
    gps_lat      REAL,
    gps_lng      REAL,
    camera_make  TEXT,
    camera_model TEXT,

    -- 缩略图状态
    thumb_status TEXT NOT NULL DEFAULT 'pending',  -- pending | ready | failed | unsupported
    thumb_hash   TEXT,
    thumb_error  TEXT,

    indexed_at   INTEGER NOT NULL,
    deleted_at   INTEGER,                          -- 软删（移到回收站时写入）

    UNIQUE(root_id, rel_path)
);

CREATE INDEX idx_images_root_mtime ON images(root_id, mtime DESC);
CREATE INDEX idx_images_extension  ON images(extension);
CREATE INDEX idx_images_size       ON images(size_bytes);
CREATE INDEX idx_images_taken_at   ON images(taken_at) WHERE taken_at IS NOT NULL;
CREATE INDEX idx_images_thumb_pending ON images(thumb_status) WHERE thumb_status = 'pending';
CREATE INDEX idx_images_deleted    ON images(deleted_at) WHERE deleted_at IS NOT NULL;

CREATE TABLE scan_tasks (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    root_id      INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
    status       TEXT NOT NULL,                    -- queued | running | paused | done | failed
    total_files  INTEGER,
    processed    INTEGER DEFAULT 0,
    started_at   INTEGER,
    finished_at  INTEGER,
    error        TEXT,
    cursor_json  TEXT                              -- 断点续扫的游标
);

CREATE INDEX idx_scan_tasks_root ON scan_tasks(root_id, started_at DESC);

CREATE TABLE undo_log (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    action         TEXT NOT NULL,                  -- trash | move | rename | tag_remove
    payload_json   TEXT NOT NULL,
    can_undo_until INTEGER NOT NULL,
    undone_at      INTEGER,
    created_at     INTEGER NOT NULL
);

CREATE INDEX idx_undo_until ON undo_log(can_undo_until) WHERE undone_at IS NULL;
