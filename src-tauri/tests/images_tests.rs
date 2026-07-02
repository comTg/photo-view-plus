use photo_view_plus_lib::repo::images_repo::{
    self, ImageQueryParams, NewImageMetadata, UpsertOutcome,
};
use photo_view_plus_lib::repo::roots_repo::{self, NewRoot};

#[test]
fn upsert_and_query_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = photo_view_plus_lib::db::open(&dir.path().join("app.sqlite")).expect("db");
    let conn = pool.get().expect("conn");
    let root = roots_repo::insert(
        &conn,
        &NewRoot {
            path: dir.path().to_string_lossy().to_string(),
            label: None,
            root_type: "local".to_string(),
        },
        1,
    )
    .expect("root");

    let image = NewImageMetadata {
        root_id: root.id,
        rel_path: "IMG_001.JPG".to_string(),
        filename: "IMG_001.JPG".to_string(),
        extension: "jpg".to_string(),
        size_bytes: 42,
        mtime: 2,
        width: Some(10),
        height: Some(20),
        orientation: None,
        taken_at: None,
        gps_lat: None,
        gps_lng: None,
        camera_make: Some("Camera".to_string()),
        camera_model: None,
    };
    let outcome = images_repo::upsert_scanned_image(&conn, &image, 3).expect("insert");
    assert!(matches!(outcome, UpsertOutcome::Added(_)));

    let page = images_repo::query(
        &conn,
        &ImageQueryParams {
            root_ids: Some(vec![root.id]),
            formats: Some(vec!["jpg".to_string()]),
            q: Some("IMG".to_string()),
            size_min: Some(1),
            size_max: Some(100),
            taken_from: None,
            taken_to: None,
            has_gps: Some(false),
            sort: None,
            offset: Some(0),
            limit: Some(10),
            include_deleted: Some(false),
        },
    )
    .expect("query");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].filename, "IMG_001.JPG");
}

// 月中正午 UTC 的时间戳：任何 ±14h 时区偏移下仍落在同一年月，测试与本机时区无关。
const TS_2023_06_15: i64 = 1_686_830_400; // 2023-06-15 12:00:00 UTC
const TS_2023_06_20: i64 = 1_687_262_400; // 2023-06-20 12:00:00 UTC
const TS_2024_01_15: i64 = 1_705_320_000; // 2024-01-15 12:00:00 UTC

#[test]
fn timeline_buckets_groups_by_month_with_samples() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = photo_view_plus_lib::db::open(&dir.path().join("app.sqlite")).expect("db");
    let conn = pool.get().expect("conn");
    let root = roots_repo::insert(
        &conn,
        &NewRoot {
            path: dir.path().to_string_lossy().to_string(),
            label: None,
            root_type: "local".to_string(),
        },
        1,
    )
    .expect("root");

    let insert = |rel: &str, taken_at: i64| -> i64 {
        let image = NewImageMetadata {
            root_id: root.id,
            rel_path: rel.to_string(),
            filename: rel.to_string(),
            extension: "jpg".to_string(),
            size_bytes: 10,
            mtime: 1,
            width: None,
            height: None,
            orientation: None,
            taken_at: Some(taken_at),
            gps_lat: None,
            gps_lng: None,
            camera_make: None,
            camera_model: None,
        };
        images_repo::upsert_scanned_image(&conn, &image, 1)
            .expect("insert")
            .image_id()
    };

    let june_older = insert("a.jpg", TS_2023_06_15);
    let june_newer = insert("b.jpg", TS_2023_06_20);
    let _jan = insert("c.jpg", TS_2024_01_15);

    let buckets = images_repo::timeline_buckets(&conn, 240).expect("timeline");
    assert_eq!(buckets.len(), 2, "两个月应产生两个桶");

    // 按年月倒序：2024-01 在前，2023-06 在后。
    assert_eq!((buckets[0].year, buckets[0].month), (2024, 1));
    assert_eq!(buckets[0].count, 1);
    assert_eq!(buckets[0].samples.len(), 1);

    assert_eq!((buckets[1].year, buckets[1].month), (2023, 6));
    assert_eq!(buckets[1].count, 2);
    assert_eq!(buckets[1].samples.len(), 2);
    // 桶内按时间倒序：较新的 06-20 应排在前。
    assert_eq!(buckets[1].samples[0].id, june_newer);
    assert_eq!(buckets[1].samples[1].id, june_older);
}
