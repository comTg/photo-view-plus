use std::path::PathBuf;
use std::time::Duration;

use image::{ImageBuffer, Rgba};
use photo_view_plus_lib::queue::Scheduler;
use photo_view_plus_lib::repo::images_repo::{self, NewImageMetadata};
use photo_view_plus_lib::repo::roots_repo::{self, NewRoot};
use photo_view_plus_lib::services::thumbnail_service::{self, ThumbnailTask};

#[tokio::test]
async fn thumbnail_task_generates_webp_and_updates_db() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("app.sqlite");
    let pool = photo_view_plus_lib::db::open(&db_path).expect("db");
    let root_dir = dir.path().join("photos");
    let thumbs_dir = dir.path().join("thumbs");
    std::fs::create_dir_all(&root_dir).expect("root dir");
    let image_path = root_dir.join("sample.png");
    write_png(&image_path, false);

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
            image_path.clone(),
            None,
            thumbs_dir.clone(),
            false,
        ))
        .expect("enqueue thumbnail");

    let (initial_hash, initial_bytes) = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let conn = pool.get().expect("conn");
            let detail = images_repo::get_detail(&conn, image_id)
                .expect("detail")
                .expect("image");
            if detail.thumb_status == "ready" {
                let hash = detail.thumb_hash.expect("thumb hash");
                let thumb_path = thumbs_dir.join(&hash[0..2]).join(format!("{hash}.webp"));
                assert!(thumb_path.exists(), "thumbnail file should exist");
                break (
                    hash,
                    std::fs::read(thumb_path).expect("read initial thumbnail"),
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("initial thumbnail timed out");

    write_png(&image_path, true);
    let (completion_tx, mut completion_rx) = tokio::sync::mpsc::unbounded_channel();
    let pending = thumbnail_service::enqueue_thumbnail_regeneration(
        &pool,
        &scheduler,
        &thumbs_dir,
        image_id,
        completion_tx,
    )
    .expect("enqueue thumbnail regeneration");
    assert_eq!(pending.thumb_status, "pending");
    assert!(pending.thumb_hash.is_none());

    tokio::time::timeout(Duration::from_secs(2), completion_rx.recv())
        .await
        .expect("regeneration timed out")
        .expect("completion sender dropped");

    let conn = pool.get().expect("conn");
    let regenerated = images_repo::get_detail(&conn, image_id)
        .expect("detail")
        .expect("image");
    assert_eq!(regenerated.thumb_status, "ready");
    assert_eq!(
        regenerated.thumb_hash.as_deref(),
        Some(initial_hash.as_str())
    );
    let regenerated_path = thumbs_dir
        .join(&initial_hash[0..2])
        .join(format!("{initial_hash}.webp"));
    let regenerated_bytes = std::fs::read(regenerated_path).expect("read regenerated thumbnail");
    assert_ne!(regenerated_bytes, initial_bytes);
}

fn write_png(path: &PathBuf, alternate: bool) {
    let image: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(32, 32, |x, y| {
        if alternate {
            Rgba([40, 200, 110, 255])
        } else if (x + y) % 2 == 0 {
            Rgba([220, 80, 80, 255])
        } else {
            Rgba([80, 160, 220, 255])
        }
    });
    image.save(path).expect("write png");
}

// P1-3：大 JPEG 走 jpeg-decoder DCT 降采样解码，但仍应记录原生尺寸并产出正确缩略图。
// 1024×1024 会触发 1/2 降采样（解码到 ~512），借此验证降采样路径 + 原生尺寸回填。
#[tokio::test]
async fn thumbnail_downscales_large_jpeg_and_keeps_native_dims() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = photo_view_plus_lib::db::open(&dir.path().join("app.sqlite")).expect("db");
    let root_dir = dir.path().join("photos");
    let thumbs_dir = dir.path().join("thumbs");
    std::fs::create_dir_all(&root_dir).expect("root dir");
    let image_path = root_dir.join("large.jpg");
    write_jpeg(&image_path, 1024, 1024);

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
    // width/height 留空，让 update_thumbnail_ready 用缩略图路径回填——借此验证回填的是原生尺寸。
    let image = NewImageMetadata {
        root_id: root.id,
        rel_path: "large.jpg".to_string(),
        filename: "large.jpg".to_string(),
        extension: "jpg".to_string(),
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
            "large.jpg".to_string(),
            image_path.clone(),
            None,
            thumbs_dir.clone(),
            false,
        ))
        .expect("enqueue thumbnail");

    let detail = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let conn = pool.get().expect("conn");
            let detail = images_repo::get_detail(&conn, image_id)
                .expect("detail")
                .expect("image");
            match detail.thumb_status.as_str() {
                "ready" => break detail,
                "failed" | "unsupported" => panic!("thumbnail failed: {:?}", detail.thumb_error),
                _ => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    })
    .await
    .expect("thumbnail timed out");

    // 关键：即使 JPEG 被 DCT 降采样解码，回填的仍是原生尺寸 1024×1024。
    assert_eq!(detail.width, Some(1024));
    assert_eq!(detail.height, Some(1024));

    // 缩略图文件存在且已缩到 <= 256。
    let hash = detail.thumb_hash.expect("thumb hash");
    let thumb_path = thumbs_dir.join(&hash[0..2]).join(format!("{hash}.webp"));
    assert!(thumb_path.exists(), "thumbnail file should exist");
    let thumb = image::open(&thumb_path).expect("decode thumb");
    assert!(
        thumb.width() <= 256 && thumb.height() <= 256,
        "thumb should be downscaled to <= 256, got {}x{}",
        thumb.width(),
        thumb.height()
    );
}

fn write_jpeg(path: &PathBuf, width: u32, height: u32) {
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    });
    image.save(path).expect("write jpeg");
}
