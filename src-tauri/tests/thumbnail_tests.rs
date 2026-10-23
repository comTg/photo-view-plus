use std::path::PathBuf;
use std::time::Duration;

use image::{ImageBuffer, Rgba};
use photo_view_plus_lib::queue::Scheduler;
use photo_view_plus_lib::repo::images_repo::{self, NewImageMetadata};
use photo_view_plus_lib::repo::roots_repo::{self, NewRoot};
use photo_view_plus_lib::services::thumbnail_service::{self, ThumbnailTask};

/// 可见队列是进程内全局的：用到它的测试串行跑，免得互相占名额、看到别的测试排的图。
static VISIBLE_QUEUE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn thumbnail_task_generates_jpeg_and_updates_db() {
    let _visible = VISIBLE_QUEUE.lock().await;
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
                assert_eq!(detail.thumb_ext, "jpg");
                let hash = detail.thumb_hash.expect("thumb hash");
                let thumb_path = thumbs_dir.join(&hash[0..2]).join(format!("{hash}.jpg"));
                assert!(thumb_path.exists(), "thumbnail file should exist");
                assert_eq!(
                    image::ImageFormat::from_path(&thumb_path).expect("format"),
                    image::ImageFormat::Jpeg
                );
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
    // docs/11 R4：R4 之前留下的 WebP 旧文件在重新生成后删掉。
    let legacy_path = thumbs_dir
        .join(&initial_hash[0..2])
        .join(format!("{initial_hash}.webp"));
    std::fs::write(&legacy_path, b"legacy").expect("legacy thumbnail");
    let (completion_tx, mut completion_rx) = tokio::sync::mpsc::unbounded_channel();
    let pending = thumbnail_service::enqueue_thumbnail_regeneration(
        &pool,
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
        .join(format!("{initial_hash}.jpg"));
    let regenerated_bytes = std::fs::read(regenerated_path).expect("read regenerated thumbnail");
    assert_ne!(regenerated_bytes, initial_bytes);
    assert!(!legacy_path.exists(), "legacy webp should be removed");
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
    let thumb_path = thumbs_dir.join(&hash[0..2]).join(format!("{hash}.jpg"));
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

// docs/11 R3：可见区域队列不经过批量调度器、独立生成；已排队或已就绪的图不会重复排队；
// 之后到来的批量任务发现图已就绪就跳过，不重复读原图。
#[tokio::test]
async fn visible_queue_generates_ahead_of_batch_and_dedupes() {
    let _visible = VISIBLE_QUEUE.lock().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = photo_view_plus_lib::db::open(&dir.path().join("app.sqlite")).expect("db");
    let root_dir = dir.path().join("photos");
    let thumbs_dir = dir.path().join("thumbs");
    std::fs::create_dir_all(&root_dir).expect("root dir");

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
    let mut ids = Vec::new();
    for name in ["a.png", "b.png"] {
        write_png(&root_dir.join(name), name == "b.png");
        let image = NewImageMetadata {
            root_id: root.id,
            rel_path: name.to_string(),
            filename: name.to_string(),
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
        ids.push(
            images_repo::upsert_scanned_image(&conn, &image, 2)
                .expect("insert image")
                .image_id(),
        );
    }
    drop(conn);

    let queued =
        thumbnail_service::prioritize_visible(&pool, &thumbs_dir, None, &ids).expect("prioritize");
    assert_eq!(queued, 2);
    // 同一屏再报一次：已在队列里或已就绪，不重复排。
    let again =
        thumbnail_service::prioritize_visible(&pool, &thumbs_dir, None, &ids).expect("prioritize");
    assert_eq!(again, 0);

    let hashes = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let conn = pool.get().expect("conn");
            let details = ids
                .iter()
                .map(|id| {
                    images_repo::get_detail(&conn, *id)
                        .expect("detail")
                        .expect("image")
                })
                .collect::<Vec<_>>();
            if details.iter().all(|detail| detail.thumb_status == "ready") {
                break details
                    .into_iter()
                    .map(|detail| detail.thumb_hash.expect("hash"))
                    .collect::<Vec<_>>();
            }
            assert!(
                details.iter().all(
                    |detail| detail.thumb_status == "pending" || detail.thumb_status == "ready"
                ),
                "visible thumbnail failed"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("visible thumbnails timed out");

    let thumb_path = thumbs_dir
        .join(&hashes[0][0..2])
        .join(format!("{}.jpg", hashes[0]));
    let modified = std::fs::metadata(&thumb_path)
        .expect("thumb")
        .modified()
        .expect("mtime");

    // 扫描时排进批量队列的同一张图：已就绪，跳过。
    let scheduler = Scheduler::start(1);
    scheduler
        .enqueue(ThumbnailTask::new(
            pool.clone(),
            ids[0],
            root.id,
            "a.png".to_string(),
            root_dir.join("a.png"),
            None,
            thumbs_dir.clone(),
            false,
        ))
        .expect("enqueue thumbnail");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(scheduler.status().completed, 1);
    let after = std::fs::metadata(&thumb_path)
        .expect("thumb")
        .modified()
        .expect("mtime");
    assert_eq!(
        modified, after,
        "batch task must not regenerate a ready thumbnail"
    );
}

// docs/11 R6：可见队列按上报的顺序生成（前端把视口里的排在前面）；新的上报替换掉滚出去的图；
// 手动重新生成排最前，之后的上报也不会把它挤掉。
#[tokio::test]
async fn visible_queue_follows_request_order() {
    let _visible = VISIBLE_QUEUE.lock().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = photo_view_plus_lib::db::open(&dir.path().join("app.sqlite")).expect("db");
    let root_dir = dir.path().join("photos");
    let thumbs_dir = dir.path().join("thumbs");
    std::fs::create_dir_all(&root_dir).expect("root dir");
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
    let mut ids = Vec::new();
    for name in ["a.png", "b.png", "c.png"] {
        write_png(&root_dir.join(name), name == "b.png");
        ids.push(insert_image(&conn, root.id, name, "png"));
    }
    drop(conn);
    let (a, b, c) = (ids[0], ids[1], ids[2]);

    // 先占住全部许可：工作者取不到任务，队列顺序可以直接看。
    let held = thumbnail_service::hold_visible_permits().await;
    let queued = thumbnail_service::prioritize_visible(&pool, &thumbs_dir, None, &[c, a, b])
        .expect("prioritize");
    assert_eq!(queued, 3);
    assert_eq!(thumbnail_service::visible_queue_snapshot(), vec![c, a, b]);
    let queued = thumbnail_service::prioritize_visible(&pool, &thumbs_dir, None, &[b, c])
        .expect("prioritize");
    assert_eq!(queued, 0, "b、c 只是换了顺序，不算新排进来的");
    assert_eq!(thumbnail_service::visible_queue_snapshot(), vec![b, c]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    thumbnail_service::enqueue_thumbnail_regeneration(&pool, &thumbs_dir, a, tx)
        .expect("regenerate");
    thumbnail_service::prioritize_visible(&pool, &thumbs_dir, None, &[c, b]).expect("prioritize");
    assert_eq!(thumbnail_service::visible_queue_snapshot(), vec![a, c, b]);

    drop(held);
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("regeneration timed out");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ready = {
                let conn = pool.get().expect("conn");
                ids.iter().all(|id| {
                    images_repo::thumb_status(&conn, *id)
                        .expect("status")
                        .as_deref()
                        == Some("ready")
                })
            };
            if ready && thumbnail_service::visible_queue_snapshot().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("visible queue drained");
}

fn insert_image(conn: &rusqlite::Connection, root_id: i64, name: &str, extension: &str) -> i64 {
    let image = NewImageMetadata {
        root_id,
        rel_path: name.to_string(),
        filename: name.to_string(),
        extension: extension.to_string(),
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
    images_repo::upsert_scanned_image(conn, &image, 2)
        .expect("insert image")
        .image_id()
}

// docs/11 R4：缩略图存 JPEG，带透明通道的 PNG（截图等）先铺到白底上，不会变成黑底。
#[tokio::test]
async fn transparent_png_thumbnail_is_flattened_onto_white() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = photo_view_plus_lib::db::open(&dir.path().join("app.sqlite")).expect("db");
    let root_dir = dir.path().join("photos");
    let thumbs_dir = dir.path().join("thumbs");
    std::fs::create_dir_all(&root_dir).expect("root dir");
    // 左半边完全透明，右半边不透明的深蓝。
    let png: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(64, 64, |x, _| {
        if x < 32 {
            Rgba([0, 0, 0, 0])
        } else {
            Rgba([20, 40, 120, 255])
        }
    });
    png.save(root_dir.join("shot.png")).expect("write png");

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
    let image_id = insert_image(&conn, root.id, "shot.png", "png");
    drop(conn);

    let scheduler = Scheduler::start(1);
    scheduler
        .enqueue(ThumbnailTask::new(
            pool.clone(),
            image_id,
            root.id,
            "shot.png".to_string(),
            root_dir.join("shot.png"),
            None,
            thumbs_dir.clone(),
            false,
        ))
        .expect("enqueue thumbnail");
    let hash = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let conn = pool.get().expect("conn");
            let detail = images_repo::get_detail(&conn, image_id)
                .expect("detail")
                .expect("image");
            if detail.thumb_status == "ready" {
                break detail.thumb_hash.expect("hash");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("thumbnail timed out");

    let thumb = image::open(thumbnail_service::thumb_path(&thumbs_dir, &hash, "jpg"))
        .expect("decode thumb")
        .to_rgb8();
    // 小图会被放大到缩略图尺寸，按比例取点：左边 1/8 处透明，右边 7/8 处不透明。
    let (width, height) = thumb.dimensions();
    let transparent = thumb.get_pixel(width / 8, height / 2).0;
    assert!(
        transparent.iter().all(|channel| *channel >= 245),
        "transparent area should be white, got {transparent:?}"
    );
    let opaque = thumb.get_pixel(width * 7 / 8, height / 2).0;
    assert!(
        opaque[2] > 90 && opaque[0] < 60,
        "opaque area keeps its color, got {opaque:?}"
    );
}

fn write_legacy_webp(path: &std::path::Path, seed: u8) {
    use image::ImageEncoder;
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    let image: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(64, 48, |x, y| {
        Rgba([(x * 4) as u8 ^ seed, (y * 5) as u8, seed, 255])
    });
    let mut bytes = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut bytes)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .expect("encode webp");
    std::fs::write(path, bytes).expect("write webp");
}

// docs/11 R4：旧的无损 WebP 缩略图逐批转成 JPEG。只读缓存，不碰原图；已有 dHash 的用新缩略图重算；
// 旧文件丢失的标回待生成；转完删掉旧文件，再转一次时没有可转的了。
#[test]
fn legacy_webp_thumbnails_are_converted_to_jpeg() {
    use photo_view_plus_lib::services::phash_service;
    use photo_view_plus_lib::utils::bk_tree::DhashIndex;

    let dir = tempfile::tempdir().expect("tempdir");
    let pool = photo_view_plus_lib::db::open(&dir.path().join("app.sqlite")).expect("db");
    let thumbs_dir = dir.path().join("thumbs");
    let conn = pool.get().expect("conn");
    let root = roots_repo::insert(
        &conn,
        &NewRoot {
            // 原图目录不存在：转换不能去读原图。
            path: dir
                .path()
                .join("missing-photos")
                .to_string_lossy()
                .to_string(),
            label: None,
            root_type: "local".to_string(),
        },
        1,
    )
    .expect("root");
    let with_dhash = insert_image(&conn, root.id, "a.jpg", "jpg");
    let without_dhash = insert_image(&conn, root.id, "b.jpg", "jpg");
    let missing_file = insert_image(&conn, root.id, "c.jpg", "jpg");
    let mut hashes = Vec::new();
    for (index, (id, name)) in [
        (with_dhash, "a.jpg"),
        (without_dhash, "b.jpg"),
        (missing_file, "c.jpg"),
    ]
    .into_iter()
    .enumerate()
    {
        let hash = thumbnail_service::thumb_hash_for(root.id, name);
        images_repo::update_thumbnail_ready(&conn, id, &hash, "webp", None, None).expect("ready");
        if id != missing_file {
            write_legacy_webp(
                &thumbnail_service::thumb_path(&thumbs_dir, &hash, "webp"),
                index as u8 * 70,
            );
        }
        hashes.push(hash);
    }
    images_repo::set_dhash(&conn, with_dhash, 0).expect("dhash");
    drop(conn);

    let index = DhashIndex::new();
    let outcome =
        thumbnail_service::convert_legacy_thumbnails(&pool, &thumbs_dir, Some(&index), 10)
            .expect("convert");
    assert_eq!(
        (outcome.converted, outcome.regenerate, outcome.skipped),
        (2, 1, 0)
    );

    let conn = pool.get().expect("conn");
    for (id, hash) in [(with_dhash, &hashes[0]), (without_dhash, &hashes[1])] {
        let detail = images_repo::get_detail(&conn, id)
            .expect("detail")
            .expect("image");
        assert_eq!(detail.thumb_status, "ready");
        assert_eq!(detail.thumb_ext, "jpg");
        let jpeg = thumbnail_service::thumb_path(&thumbs_dir, hash, "jpg");
        assert_eq!(
            image::ImageFormat::from_path(&jpeg).expect("format"),
            image::ImageFormat::Jpeg
        );
        assert!(jpeg.exists(), "jpeg written");
        assert!(
            !thumbnail_service::thumb_path(&thumbs_dir, hash, "webp").exists(),
            "legacy webp removed"
        );
    }
    let converted = images_repo::get_detail(&conn, with_dhash)
        .expect("detail")
        .expect("image");
    let expected = phash_service::dhash_from_thumb(&thumbnail_service::thumb_path(
        &thumbs_dir,
        &hashes[0],
        "jpg",
    ))
    .expect("dhash");
    assert_eq!(
        converted.dhash,
        Some(expected as i64),
        "dHash recomputed from the jpeg"
    );
    assert!(index
        .find_within(expected, 0)
        .iter()
        .any(|(id, _)| *id == with_dhash));
    let untouched = images_repo::get_detail(&conn, without_dhash)
        .expect("detail")
        .expect("image");
    assert_eq!(untouched.dhash, None, "images without dHash stay without");
    let requeued = images_repo::get_detail(&conn, missing_file)
        .expect("detail")
        .expect("image");
    assert_eq!(requeued.thumb_status, "pending");
    assert_eq!(requeued.thumb_hash, None);
    drop(conn);

    let again = thumbnail_service::convert_legacy_thumbnails(&pool, &thumbs_dir, None, 10)
        .expect("convert again");
    assert_eq!(again.processed(), 0);
}
