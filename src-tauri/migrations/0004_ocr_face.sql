-- MVP4 OCR / face schema.
-- 与 docs/01-data-model.md § 2.2 / § 2.9 对齐，并加入 docs/06 T3 的 FTS5 索引。

ALTER TABLE images ADD COLUMN ocr_status TEXT NOT NULL DEFAULT 'disabled';
ALTER TABLE images ADD COLUMN ocr_text   TEXT;
ALTER TABLE images ADD COLUMN face_status TEXT NOT NULL DEFAULT 'disabled';

CREATE INDEX idx_images_ocr_status
    ON images(ocr_status)
    WHERE ocr_status = 'pending';

CREATE INDEX idx_images_face_status
    ON images(face_status)
    WHERE face_status = 'pending';

CREATE VIRTUAL TABLE images_fts USING fts5(
    filename,
    ocr_text,
    content='images',
    content_rowid='id',
    tokenize='unicode61'
);

CREATE TRIGGER images_fts_ai AFTER INSERT ON images BEGIN
    INSERT INTO images_fts(rowid, filename, ocr_text)
    VALUES (new.id, new.filename, COALESCE(new.ocr_text, ''));
END;

CREATE TRIGGER images_fts_ad AFTER DELETE ON images BEGIN
    INSERT INTO images_fts(images_fts, rowid, filename, ocr_text)
    VALUES ('delete', old.id, old.filename, COALESCE(old.ocr_text, ''));
END;

CREATE TRIGGER images_fts_au AFTER UPDATE OF filename, ocr_text ON images BEGIN
    INSERT INTO images_fts(images_fts, rowid, filename, ocr_text)
    VALUES ('delete', old.id, old.filename, COALESCE(old.ocr_text, ''));
    INSERT INTO images_fts(rowid, filename, ocr_text)
    VALUES (new.id, new.filename, COALESCE(new.ocr_text, ''));
END;

INSERT INTO images_fts(rowid, filename, ocr_text)
SELECT id, filename, COALESCE(ocr_text, '')
FROM images;

CREATE TABLE face_clusters (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    label           TEXT,
    sample_image_id INTEGER REFERENCES images(id),
    face_count      INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL
);

CREATE TABLE faces (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    image_id      INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    cluster_id    INTEGER REFERENCES face_clusters(id),
    bbox_json     TEXT NOT NULL,
    confidence    REAL NOT NULL,
    embedding_ref TEXT
);

CREATE INDEX idx_faces_image   ON faces(image_id);
CREATE INDEX idx_faces_cluster ON faces(cluster_id);
