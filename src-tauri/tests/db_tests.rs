//! T2 验收测试：
//! 1. 建表：迁移后 5 张核心表存在
//! 2. 二次启动幂等：同一目录二次 open 不报错且版本不变
//! 3. 加新 migration 生效（通过对 schema_version 写入再 open 模拟）
//! 4. 外键约束生效：删除 root 级联删 images
//! 5. WAL 模式与 PRAGMA 生效

use photo_view_plus_lib::repo::smart_albums_repo::{self, SmartAlbumInput};
use photo_view_plus_lib::repo::tags_repo::{self, NewTagScore};
use photo_view_plus_lib::*;
use rusqlite::params;
use tempfile::TempDir;

fn fresh() -> (db::Pool, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::open(&dir.path().join("app.sqlite")).unwrap();
    (pool, dir)
}

#[test]
fn test_001_tables_created() {
    let (pool, _dir) = fresh();
    let conn = pool.get().unwrap();
    for table in [
        "schema_version",
        "roots",
        "images",
        "scan_tasks",
        "undo_log",
    ] {
        assert!(
            db::table_exists(&conn, table).unwrap(),
            "table {table} should exist"
        );
    }
}

#[test]
fn test_002_idempotent_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("app.sqlite");

    let pool1 = db::open(&db_path).unwrap();
    let v1 = migrations::current_version(&pool1.get().unwrap()).unwrap();
    drop(pool1);

    let pool2 = db::open(&db_path).unwrap();
    let v2 = migrations::current_version(&pool2.get().unwrap()).unwrap();
    assert_eq!(v1, v2, "schema_version unchanged on reopen");
    assert!(v1 >= 1, "at least migration 0001 applied");
}

#[test]
fn test_003_current_version_after_init() {
    // 首次启动后版本号 = 已注册的最高 migration 编号。
    // MVP1 起步是 1；MVP2 加 migration 0002 后变 2。继续往后只升不降。
    let (pool, _dir) = fresh();
    let conn = pool.get().unwrap();
    let version = migrations::current_version(&conn).unwrap();
    assert!(
        version >= 5,
        "expected version >= 5 after MVP4 schema, got {version}"
    );
}

#[test]
fn test_007_mvp3_tags_schema_and_repo_roundtrip() {
    let (pool, _dir) = fresh();
    let mut conn = pool.get().unwrap();

    for table in ["tags", "image_tags"] {
        assert!(
            db::table_exists(&conn, table).unwrap(),
            "table {table} should exist after 0003"
        );
    }

    conn.execute(
        "INSERT INTO roots(path, type, created_at) VALUES (?1, 'local', 0)",
        params!["/tmp/ai-tags"],
    )
    .unwrap();
    let root_id: i64 = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO images(root_id, rel_path, filename, extension, size_bytes, mtime, indexed_at)
         VALUES (?1, 'sunset.jpg', 'sunset.jpg', 'jpg', 100, 0, 0)",
        params![root_id],
    )
    .unwrap();
    let image_id = conn.last_insert_rowid();

    let tag_status: String = conn
        .query_row(
            "SELECT tag_status FROM images WHERE id = ?1",
            [image_id],
            |r| r.get(0),
        )
        .unwrap();
    let embedding_status: String = conn
        .query_row(
            "SELECT embedding_status FROM images WHERE id = ?1",
            [image_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tag_status, "pending");
    assert_eq!(embedding_status, "pending");

    let tags = tags_repo::replace_image_tags(
        &mut conn,
        image_id,
        &[
            NewTagScore {
                name: "sunset".to_string(),
                score: 0.93,
                source: "ai".to_string(),
                category: Some("scene".to_string()),
            },
            NewTagScore {
                name: "sea".to_string(),
                score: 0.84,
                source: "ai".to_string(),
                category: Some("scene".to_string()),
            },
        ],
        10,
    )
    .unwrap();
    assert_eq!(tags.len(), 2);

    let fetched = tags_repo::tags_for_image(&conn, image_id).unwrap();
    assert_eq!(fetched.len(), 2);
    assert_eq!(fetched[0].name, "sunset");

    let cloud = tags_repo::list_tags(&conn, 10).unwrap();
    assert_eq!(cloud.len(), 2);
}

#[test]
fn test_008_mvp4_ocr_face_fts_and_smart_album_schema() {
    let (pool, _dir) = fresh();
    let conn = pool.get().unwrap();

    for table in ["faces", "face_clusters", "smart_albums"] {
        assert!(
            db::table_exists(&conn, table).unwrap(),
            "table {table} should exist after MVP4 migrations"
        );
    }

    conn.execute(
        "INSERT INTO roots(path, type, created_at) VALUES (?1, 'local', 0)",
        params!["/tmp/mvp4"],
    )
    .unwrap();
    let root_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO images(root_id, rel_path, filename, extension, size_bytes, mtime, indexed_at, ocr_text)
         VALUES (?1, 'error.png', 'error.png', 'png', 100, 0, 0, 'Fatal Error 报错')",
        params![root_id],
    )
    .unwrap();
    let image_id = conn.last_insert_rowid();
    let page = photo_view_plus_lib::repo::images_repo::search_text(&conn, "Error", 10, 0).unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].id, image_id);

    let album = smart_albums_repo::save(
        &conn,
        &SmartAlbumInput {
            name: "PNG".to_string(),
            filter_json: r#"{"formats":["png"],"offset":0,"limit":200}"#.to_string(),
            icon: Some("sparkles".to_string()),
            sort_order: Some(1),
        },
    )
    .unwrap();
    assert_eq!(album.name, "PNG");
    assert_eq!(smart_albums_repo::list(&conn).unwrap().len(), 1);
}

#[test]
fn test_004_foreign_key_cascade() {
    let (pool, _dir) = fresh();
    let conn = pool.get().unwrap();

    // 插入 root
    conn.execute(
        "INSERT INTO roots(path, label, type, created_at) VALUES (?1, ?2, 'local', 0)",
        params!["/tmp/photos", "test"],
    )
    .unwrap();
    let root_id: i64 = conn.last_insert_rowid();

    // 插入挂在 root 下的图片
    conn.execute(
        "INSERT INTO images(root_id, rel_path, filename, extension, size_bytes, mtime, indexed_at)
         VALUES (?1, 'a.jpg', 'a.jpg', 'jpg', 100, 0, 0)",
        params![root_id],
    )
    .unwrap();

    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM images", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);

    // 删 root → images 级联删
    conn.execute("DELETE FROM roots WHERE id=?1", params![root_id])
        .unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM images", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "image should be cascade-deleted with its root");
}

#[test]
fn test_005_wal_and_pragmas_applied() {
    let (pool, _dir) = fresh();
    let conn = pool.get().unwrap();

    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");

    let fk: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk, 1);

    let busy: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .unwrap();
    assert_eq!(busy, 5000);
}

#[test]
fn test_006_unique_root_path() {
    let (pool, _dir) = fresh();
    let conn = pool.get().unwrap();

    conn.execute(
        "INSERT INTO roots(path, type, created_at) VALUES (?1, 'local', 0)",
        params!["/tmp/photos"],
    )
    .unwrap();

    let err = conn.execute(
        "INSERT INTO roots(path, type, created_at) VALUES (?1, 'local', 0)",
        params!["/tmp/photos"],
    );
    assert!(
        err.is_err(),
        "duplicate root path should fail UNIQUE constraint"
    );
}
