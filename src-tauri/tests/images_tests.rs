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
            tag_names_all: None,
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

#[test]
fn query_filters_by_all_tags_in_sql_with_matching_total() {
    use photo_view_plus_lib::repo::tags_repo::{self, NewTagScore};

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = photo_view_plus_lib::db::open(&dir.path().join("app.sqlite")).expect("db");
    let mut conn = pool.get().expect("conn");
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

    // 300 张，mtime 递减；带"猫"的是最后 20 张（默认排序的第 280 行以后），其中 5 张还带"沙发"。
    let mut ids = Vec::new();
    for i in 0..300_i64 {
        let image = NewImageMetadata {
            root_id: root.id,
            rel_path: format!("IMG_{i:04}.jpg"),
            filename: format!("IMG_{i:04}.jpg"),
            extension: "jpg".to_string(),
            size_bytes: 10,
            mtime: 10_000 - i,
            width: None,
            height: None,
            orientation: None,
            taken_at: None,
            gps_lat: None,
            gps_lng: None,
            camera_make: None,
            camera_model: None,
        };
        ids.push(
            images_repo::upsert_scanned_image(&conn, &image, 1)
                .expect("insert")
                .image_id(),
        );
    }
    let score = |name: &str| NewTagScore {
        name: name.to_string(),
        score: 0.9,
        source: "ai".to_string(),
        category: None,
    };
    for (index, id) in ids.iter().enumerate().skip(280) {
        let tags = if index >= 295 {
            vec![score("猫"), score("沙发")]
        } else {
            vec![score("猫")]
        };
        tags_repo::replace_image_tags(&mut conn, *id, &tags, 1).expect("tags");
    }

    let params = |tags: &[&str], limit: i64| ImageQueryParams {
        tag_names_all: Some(tags.iter().map(|tag| tag.to_string()).collect()),
        limit: Some(limit),
        ..ImageQueryParams::default()
    };

    let page = images_repo::query(&conn, &params(&["猫"], 5)).expect("query");
    assert_eq!(page.total, 20, "total 与分页用同一个标签过滤");
    assert_eq!(
        page.items.iter().map(|image| image.id).collect::<Vec<_>>(),
        ids[280..285].to_vec()
    );

    let both = images_repo::query(&conn, &params(&["猫", "沙发", " 沙发 "], 50)).expect("query");
    assert_eq!(both.total, 5, "AND 语义；名字去空白、去重");
    assert_eq!(
        both.items.iter().map(|image| image.id).collect::<Vec<_>>(),
        ids[295..300].to_vec()
    );

    // 空标签列表 = 不过滤。
    let unfiltered = images_repo::query(&conn, &params(&[], 1)).expect("query");
    assert_eq!(unfiltered.total, 300);

    // 搜索预过滤用的候选集：与 query 同一套 WHERE，返回全部 id。
    let candidates = images_repo::filtered_ids(&conn, &params(&["沙发"], 1)).expect("filtered ids");
    assert_eq!(candidates, ids[295..300].to_vec());

    // JSON 里不带 tagNamesAll 的旧请求照常反序列化。
    let legacy: ImageQueryParams =
        serde_json::from_str(r#"{"rootIds":[1],"limit":10}"#).expect("legacy json");
    assert!(legacy.tag_names_all.is_none());
    let with_tags: ImageQueryParams =
        serde_json::from_str(r#"{"tagNamesAll":["猫"]}"#).expect("json");
    assert_eq!(with_tags.tag_names_all, Some(vec!["猫".to_string()]));
}

// docs/11 R5：thumbnails:updated 只带窄字段，按请求顺序返回，库里没有的 id 跳过。
// R1-5：count 与 query 的 total 用同一套过滤条件。
#[test]
fn thumbnail_updates_and_count_use_narrow_queries() {
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
    let insert = |rel: &str, extension: &str| -> i64 {
        let image = NewImageMetadata {
            root_id: root.id,
            rel_path: rel.to_string(),
            filename: rel.to_string(),
            extension: extension.to_string(),
            size_bytes: 10,
            mtime: 1,
            width: Some(4000),
            height: None,
            orientation: None,
            taken_at: None,
            gps_lat: None,
            gps_lng: None,
            camera_make: None,
            camera_model: None,
        };
        images_repo::upsert_scanned_image(&conn, &image, 1)
            .expect("insert")
            .image_id()
    };
    let ready = insert("a.jpg", "jpg");
    let failed = insert("b.png", "png");
    images_repo::update_thumbnail_ready(&conn, ready, "abcdef0123456789", Some(1), Some(3000))
        .expect("ready");
    images_repo::update_thumbnail_failed(&conn, failed, "failed", "boom").expect("failed");

    let updates = images_repo::thumbnail_updates(&conn, &[failed, 9_999, ready]).expect("updates");
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0].id, failed);
    assert_eq!(updates[0].thumb_status, "failed");
    assert_eq!(updates[0].thumb_error.as_deref(), Some("boom"));
    assert_eq!(updates[1].id, ready);
    assert_eq!(updates[1].thumb_status, "ready");
    assert_eq!(updates[1].thumb_hash.as_deref(), Some("abcdef0123456789"));
    // 已知的原生宽度不被缩略图回填覆盖，缺的高度会补上。
    assert_eq!(
        (updates[1].width, updates[1].height),
        (Some(4000), Some(3000))
    );

    let all = ImageQueryParams::default();
    let jpg_only = ImageQueryParams {
        formats: Some(vec!["jpg".to_string()]),
        ..ImageQueryParams::default()
    };
    assert_eq!(images_repo::count(&conn, &all).expect("count"), 2);
    assert_eq!(images_repo::count(&conn, &jpg_only).expect("count"), 1);
    assert_eq!(
        images_repo::query(&conn, &jpg_only).expect("query").total,
        1
    );
}
