-- MVP4 smart albums.

CREATE TABLE smart_albums (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL,
    filter_json TEXT NOT NULL,
    icon        TEXT,
    sort_order  INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);

CREATE INDEX idx_smart_albums_sort ON smart_albums(sort_order, id);
