//! docs/10 A5：`ai_jobs` 任务状态（换模型自动全量重算、失败重试上限、文件变化清任务）、
//! migration 0007 继承旧 CLIP 状态、设置迁移（旧默认 clip-vit-b-32 → Chinese-CLIP）。

use photo_view_plus_lib::config::{
    AppSettings, AppSettingsPatch, DEFAULT_CLIP_MODEL, LEGACY_CLIP_MODEL, SETTINGS_VERSION,
};
use photo_view_plus_lib::db::{self, Pool};
use photo_view_plus_lib::migrations;
use photo_view_plus_lib::repo::ai_jobs_repo::{
    self, JobResult, KIND_IMAGE_EMBEDDING, KIND_TEXT_EMBEDDING, MAX_ATTEMPTS, STATUS_FAILED,
    STATUS_READY,
};
use photo_view_plus_lib::repo::images_repo::{self, NewImageMetadata};
use photo_view_plus_lib::repo::roots_repo::{self, NewRoot};

const NEW: &str = "chinese-clip-vit-b-16";
const OLD: &str = "clip-vit-b-32";

fn open(dir: &tempfile::TempDir) -> Pool {
    db::open(&dir.path().join("app.sqlite")).expect("db")
}

fn image(root_id: i64, name: &str, mtime: i64) -> NewImageMetadata {
    NewImageMetadata {
        root_id,
        rel_path: name.to_string(),
        filename: name.to_string(),
        extension: "jpg".to_string(),
        size_bytes: 100,
        mtime,
        width: None,
        height: None,
        orientation: None,
        taken_at: None,
        gps_lat: None,
        gps_lng: None,
        camera_make: None,
        camera_model: None,
    }
}

/// 插入 n 张缩略图已就绪的图，返回 (root_id, ids)。
fn seed(pool: &Pool, n: usize) -> (i64, Vec<i64>) {
    let conn = pool.get().expect("conn");
    let root = roots_repo::insert(
        &conn,
        &NewRoot {
            path: "D:/photos".to_string(),
            label: None,
            root_type: "local".to_string(),
        },
        1,
    )
    .expect("root")
    .id;
    let ids = (0..n)
        .map(|i| {
            let id =
                images_repo::upsert_scanned_image(&conn, &image(root, &format!("{i}.jpg"), 1), 1)
                    .expect("insert")
                    .image_id();
            images_repo::update_thumbnail_ready(&conn, id, &format!("hash{i:04}"), None, None)
                .expect("thumb");
            id
        })
        .collect();
    (root, ids)
}

fn record(pool: &Pool, kind: &str, model: &str, image_id: i64, status: &str) {
    let conn = pool.get().expect("conn");
    ai_jobs_repo::record(
        &conn,
        kind,
        model,
        &JobResult {
            image_id,
            status,
            error: (status == STATUS_FAILED).then_some("boom"),
            input_hash: None,
        },
        10,
    )
    .expect("record");
}

fn pending(pool: &Pool, model: &str) -> Vec<i64> {
    let conn = pool.get().expect("conn");
    ai_jobs_repo::pending_image_embedding(&conn, model, 100).expect("pending")
}

fn attempts(pool: &Pool, image_id: i64, kind: &str) -> i64 {
    let conn = pool.get().expect("conn");
    conn.query_row(
        "SELECT attempts FROM ai_jobs WHERE image_id = ?1 AND kind = ?2",
        rusqlite::params![image_id, kind],
        |row| row.get(0),
    )
    .expect("attempts")
}

#[test]
fn image_embedding_pending_follows_current_model() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = open(&dir);
    let (_, ids) = seed(&pool, 4);
    assert_eq!(pending(&pool, NEW), ids, "一开始全部待处理");

    for id in &ids[..3] {
        record(&pool, KIND_IMAGE_EMBEDDING, NEW, *id, STATUS_READY);
    }
    assert_eq!(pending(&pool, NEW), vec![ids[3]]);
    {
        let conn = pool.get().expect("conn");
        assert_eq!(
            ai_jobs_repo::count_pending_image_embedding(&conn, NEW).expect("count"),
            1
        );
        let counts = ai_jobs_repo::counts(&conn, KIND_IMAGE_EMBEDDING, NEW).expect("counts");
        assert_eq!((counts.ready, counts.failed, counts.skipped), (3, 0, 0));
    }

    // 换回旧模型：当前模型下一张都没算过 → 全部待处理（"换模型自动全量重算"）。
    assert_eq!(pending(&pool, OLD), ids);
    // 别的任务类型互不影响。
    record(
        &pool,
        KIND_TEXT_EMBEDDING,
        "bge-small-zh-v1.5",
        ids[3],
        STATUS_READY,
    );
    assert_eq!(pending(&pool, NEW), vec![ids[3]]);
}

#[test]
fn failed_jobs_retry_until_max_attempts_and_reset_on_model_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = open(&dir);
    let (_, ids) = seed(&pool, 1);
    let id = ids[0];

    for attempt in 1..MAX_ATTEMPTS {
        record(&pool, KIND_IMAGE_EMBEDDING, NEW, id, STATUS_FAILED);
        assert_eq!(attempts(&pool, id, KIND_IMAGE_EMBEDDING), attempt);
        assert_eq!(
            pending(&pool, NEW),
            vec![id],
            "第 {attempt} 次失败后仍会重试"
        );
    }
    record(&pool, KIND_IMAGE_EMBEDDING, NEW, id, STATUS_FAILED);
    assert_eq!(attempts(&pool, id, KIND_IMAGE_EMBEDDING), MAX_ATTEMPTS);
    assert!(pending(&pool, NEW).is_empty(), "失败满 3 次不再自动重试");

    // 换模型：行里的 model 对不上 → 重新待处理；记录时 attempts 从 1 重新计。
    assert_eq!(pending(&pool, OLD), vec![id]);
    record(&pool, KIND_IMAGE_EMBEDDING, OLD, id, STATUS_FAILED);
    assert_eq!(attempts(&pool, id, KIND_IMAGE_EMBEDDING), 1);
    record(&pool, KIND_IMAGE_EMBEDDING, OLD, id, STATUS_READY);
    assert!(pending(&pool, OLD).is_empty());
}

#[test]
fn deleted_or_thumbless_images_are_not_pending_and_changed_files_are_recomputed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = open(&dir);
    let (root, ids) = seed(&pool, 3);
    {
        let conn = pool.get().expect("conn");
        images_repo::mark_deleted_by_id(&conn, ids[0], 5).expect("delete");
        images_repo::update_thumbnail_failed(&conn, ids[1], "failed", "decode").expect("thumb");
    }
    assert_eq!(pending(&pool, NEW), vec![ids[2]]);

    record(&pool, KIND_IMAGE_EMBEDDING, NEW, ids[2], STATUS_READY);
    record(
        &pool,
        KIND_TEXT_EMBEDDING,
        "bge-small-zh-v1.5",
        ids[2],
        STATUS_READY,
    );
    assert!(pending(&pool, NEW).is_empty());

    // 文件内容变了（mtime 变化）：这张图的所有 AI 任务行清掉，缩略图重新生成后再算。
    {
        let conn = pool.get().expect("conn");
        let outcome =
            images_repo::upsert_scanned_image(&conn, &image(root, "2.jpg", 99), 6).expect("rescan");
        assert!(matches!(outcome, images_repo::UpsertOutcome::Updated(_)));
        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ai_jobs WHERE image_id = ?1",
                [ids[2]],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(left, 0);
        images_repo::update_thumbnail_ready(&conn, ids[2], "newhash", None, None).expect("thumb");
    }
    assert_eq!(pending(&pool, NEW), vec![ids[2]]);
}

#[test]
fn recording_a_job_for_a_removed_image_is_ignored() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = open(&dir);
    seed(&pool, 1);
    // 不存在的 image_id（例如任务进行中整个目录被移除）：静默跳过，不撞外键。
    record(&pool, KIND_IMAGE_EMBEDDING, NEW, 9_999, STATUS_READY);
    let conn = pool.get().expect("conn");
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM ai_jobs", [], |row| row.get(0))
        .expect("count");
    assert_eq!(rows, 0);
}

#[test]
fn migration_0007_inherits_ready_legacy_clip_embeddings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ids = {
        // 模拟升级前（v6）的库：旧流水线把 embedding_status 标成 ready，还没有 ai_jobs 表。
        let mut conn = rusqlite::Connection::open(dir.path().join("app.sqlite")).expect("open");
        migrations::run_to(&mut conn, 6).expect("migrate to v6");
        let root = roots_repo::insert(
            &conn,
            &NewRoot {
                path: "D:/photos".to_string(),
                label: None,
                root_type: "local".to_string(),
            },
            1,
        )
        .expect("root")
        .id;
        let ids = (0..3)
            .map(|i| {
                let id = images_repo::upsert_scanned_image(
                    &conn,
                    &image(root, &format!("{i}.jpg"), 1),
                    1,
                )
                .expect("insert")
                .image_id();
                images_repo::update_thumbnail_ready(&conn, id, &format!("hash{i}"), None, None)
                    .expect("thumb");
                id
            })
            .collect::<Vec<_>>();
        conn.execute(
            "UPDATE images SET embedding_status = 'ready' WHERE id IN (?1, ?2)",
            [ids[0], ids[1]],
        )
        .expect("status");
        images_repo::mark_deleted_by_id(&conn, ids[1], 5).expect("delete");
        ids
    };
    let pool = open(&dir);
    assert_eq!(
        pending(&pool, OLD),
        vec![ids[2]],
        "旧模型下已就绪的不重算（已删除的不继承）"
    );
    assert_eq!(
        pending(&pool, NEW),
        vec![ids[0], ids[2]],
        "新模型全部待处理"
    );
}

#[test]
fn settings_migrate_legacy_clip_default_to_chinese_clip() {
    // 旧版配置文件：没有 settingsVersion，保存的是旧默认模型；缺的字段用默认值补齐而不是整份作废。
    let raw = r#"{
        "locale": "zh-CN",
        "theme": "dark",
        "aiEnabled": false,
        "aiClipModel": "clip-vit-b-32",
        "aiTaggerModel": "ram-plus"
    }"#;
    let mut settings: AppSettings = serde_json::from_str(raw).expect("parse old config");
    assert_eq!(settings.settings_version, 0);
    assert_eq!(settings.theme, "dark", "已有字段保留");
    assert!(!settings.ai_enabled);
    assert!(settings.file_watcher_enabled, "缺的字段取默认值");
    assert!(settings.migrate(), "需要迁移");
    assert_eq!(settings.ai_clip_model, DEFAULT_CLIP_MODEL);
    assert_eq!(settings.settings_version, SETTINGS_VERSION);
    assert!(!settings.migrate(), "迁移只做一次");

    // 迁移之后用户主动切回旧模型：保持不变。
    settings.apply_patch(AppSettingsPatch {
        ai_clip_model: Some(LEGACY_CLIP_MODEL.to_string()),
        ..AppSettingsPatch::default()
    });
    assert_eq!(settings.ai_clip_model, LEGACY_CLIP_MODEL);
    let roundtrip: AppSettings =
        serde_json::from_str(&serde_json::to_string(&settings).expect("json")).expect("parse");
    let mut roundtrip = roundtrip;
    assert!(!roundtrip.migrate());
    assert_eq!(roundtrip.ai_clip_model, LEGACY_CLIP_MODEL);

    // 不认识的模型（例如旧界面里的 siglip-so400m）：不接受补丁；旧文件里的值迁移时改回默认。
    settings.apply_patch(AppSettingsPatch {
        ai_clip_model: Some("siglip-so400m".to_string()),
        ..AppSettingsPatch::default()
    });
    assert_eq!(settings.ai_clip_model, LEGACY_CLIP_MODEL);
    let mut odd: AppSettings =
        serde_json::from_str(r#"{"aiClipModel": "siglip-so400m"}"#).expect("parse");
    assert!(odd.migrate());
    assert_eq!(odd.ai_clip_model, DEFAULT_CLIP_MODEL);

    assert_eq!(AppSettings::default().ai_clip_model, DEFAULT_CLIP_MODEL);
    assert!(!AppSettings::default().migrate());
}
