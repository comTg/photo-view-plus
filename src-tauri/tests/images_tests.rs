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
