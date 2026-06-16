use std::path::PathBuf;
use std::time::Duration;

use image::{ImageBuffer, Rgba};
use photo_view_plus_lib::queue::Scheduler;
use photo_view_plus_lib::repo::images_repo::{self, NewImageMetadata};
use photo_view_plus_lib::repo::roots_repo::{self, NewRoot};
use photo_view_plus_lib::services::thumbnail_service::ThumbnailTask;

#[tokio::test]
async fn thumbnail_task_generates_webp_and_updates_db() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("app.sqlite");
    let pool = photo_view_plus_lib::db::open(&db_path).expect("db");
    let root_dir = dir.path().join("photos");
    let thumbs_dir = dir.path().join("thumbs");
    std::fs::create_dir_all(&root_dir).expect("root dir");
    let image_path = root_dir.join("sample.png");
    write_png(&image_path);

    let conn = pool.get().expect("conn");
    let root = roots_repo::insert(
        &conn,
        &NewRoot {
            path: root_dir.to_string_lossy().to_string(),
            label: None,
            root_type: "local".to_string(),
        },
        1,
    )
    .expect("root");
    let image = NewImageMetadata {
        root_id: root.id,
        rel_path: "sample.png".to_string(),
        filename: "sample.png".to_string(),
        extension: "png".to_string(),
        size_bytes: 16,
        mtime: 1,
        width: None,
        height: None,
        orientation: None,
        taken_at: None,
        gps_lat: None,
        gps_lng: None,
        camera_make: None,
        camera_model: None,
    };
    let image_id = images_repo::upsert_scanned_image(&conn, &image, 2)
        .expect("insert image")
        .image_id();
    drop(conn);

    let scheduler = Scheduler::start(1);
    scheduler
        .enqueue(ThumbnailTask::new(
            pool.clone(),
            image_id,
            root.id,
            "sample.png".to_string(),
            image_path,
            None,
            thumbs_dir.clone(),
        ))
        .expect("enqueue thumbnail");

    for _ in 0..20 {
        let conn = pool.get().expect("conn");
        let detail = images_repo::get_detail(&conn, image_id)
            .expect("detail")
            .expect("image");
        if detail.thumb_status == "ready" {
            let hash = detail.thumb_hash.expect("thumb hash");
            let thumb_path = thumbs_dir.join(&hash[0..2]).join(format!("{hash}.webp"));
            assert!(thumb_path.exists(), "thumbnail file should exist");
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    panic!("thumbnail task did not finish");
}

fn write_png(path: &PathBuf) {
    let image: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(32, 32, |x, y| {
        if (x + y) % 2 == 0 {
            Rgba([220, 80, 80, 255])
        } else {
            Rgba([80, 160, 220, 255])
        }
    });
    image.save(path).expect("write png");
}
