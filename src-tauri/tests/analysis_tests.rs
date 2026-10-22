//! docs/10 A3：memora 描述导入（路径忽略大小写匹配、VLM 结果优先、类别写成标签）、
//! 分析结果进入检索索引、标签来源不被后来者覆盖。

use std::path::Path;

use photo_view_plus_lib::db::{self, Pool};
use photo_view_plus_lib::repo::analysis_repo::{self, ImageAnalysis, SOURCE_MEMORA, SOURCE_VLM};
use photo_view_plus_lib::repo::images_repo::{self, ImageQueryParams, NewImageMetadata};
use photo_view_plus_lib::repo::roots_repo::{self, NewRoot};
use photo_view_plus_lib::repo::tags_repo;
use photo_view_plus_lib::services::memora_import_service;
use photo_view_plus_lib::services::search_index_service;
use photo_view_plus_lib::services::search_service;
use photo_view_plus_lib::utils::text_tokens;
use rusqlite::{params, Connection};

fn open(dir: &Path) -> Pool {
    db::open(&dir.join("app.sqlite")).expect("db")
}

/// 本库：根目录 D:\photos，含大小写不同的重复路径 camera\a.jpg / Camera\a.jpg。
fn seed_library(pool: &Pool) -> Vec<i64> {
    let conn = pool.get().expect("conn");
    let root = roots_repo::insert(
        &conn,
        &NewRoot {
            path: r"D:\photos".to_string(),
            label: None,
            root_type: "network".to_string(),
        },
        1,
    )
    .expect("root")
    .id;
    [
        r"camera\a.jpg",
        r"Camera\a.jpg",
        "b.jpg",
        "c.jpg",
        "gone.jpg",
    ]
    .iter()
    .map(|rel| {
        let filename = rel.rsplit('\\').next().unwrap_or(rel).to_string();
        images_repo::upsert_scanned_image(
            &conn,
            &NewImageMetadata {
                root_id: root,
                rel_path: (*rel).to_string(),
                filename,
                extension: "jpg".to_string(),
                size_bytes: 1,
                mtime: 1,
                width: None,
                height: None,
                orientation: None,
                taken_at: None,
                gps_lat: None,
                gps_lng: None,
                camera_make: None,
                camera_model: None,
            },
            1,
        )
        .expect("image")
        .image_id()
    })
    .collect()
}

/// 假的 memora 库：只建导入用到的列。
fn memora_db(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("memora.db");
    let conn = Connection::open(&path).expect("memora");
    conn.execute_batch(
        "CREATE TABLE photos (
             id INTEGER PRIMARY KEY, path TEXT, status TEXT, description TEXT, categories TEXT,
             sensitive INTEGER, vlm_model TEXT, prompt_version TEXT, analyzed_at TEXT, analysis_s REAL
         );",
    )
    .expect("schema");
    let rows: &[(&str, &str, &str, &str)] = &[
        (
            r"D:\photos\Camera\a.jpg",
            "done",
            "一只橘猫趴在沙发上打哈欠",
            r#"["宠物"]"#,
        ),
        (
            r"D:\photos\b.jpg",
            "done",
            "海边日落，沙滩上有两个小孩在堆沙堡",
            r#"["风景", "孩子"]"#,
        ),
        (r"D:\photos\gone.jpg", "done", "已经删除的图", r#"["其他"]"#),
        (
            r"D:\photos\missing.jpg",
            "done",
            "本库没有这张",
            r#"["其他"]"#,
        ),
        (
            r"D:\photos\c.jpg",
            "error",
            "分析失败的不导入",
            r#"["其他"]"#,
        ),
        (r"D:\photos\c.jpg", "done", "   ", r#"["其他"]"#),
    ];
    for (path, status, description, categories) in rows {
        conn.execute(
            "INSERT INTO photos(path, status, description, categories, sensitive, vlm_model,
                                prompt_version, analyzed_at, analysis_s)
             VALUES (?1, ?2, ?3, ?4, 0, 'qwen3.5-9b', 'm1', '2026-10-14 19:35:51', 4.5)",
            params![path, status, description, categories],
        )
        .expect("photo");
    }
    path
}

/// 全文通道的排名（按相关度）。
fn fulltext_ids(pool: &Pool, query: &str) -> Vec<i64> {
    let mut conn = pool.get().expect("conn");
    search_index_service::flush_all(&mut conn).expect("flush");
    search_service::fulltext_channel(
        &conn,
        &text_tokens::query_tokens(query),
        &ImageQueryParams {
            include_deleted: Some(false),
            ..ImageQueryParams::default()
        },
        None,
    )
    .expect("fulltext")
    .into_iter()
    .map(|(id, _)| id)
    .collect()
}

#[test]
fn memora_import_matches_paths_ignoring_case_and_indexes_descriptions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = open(dir.path());
    let ids = seed_library(&pool);
    {
        let conn = pool.get().expect("conn");
        images_repo::mark_deleted_by_id(&conn, ids[4], 9).expect("delete");
    }
    let memora = memora_db(dir.path());

    let outcome = {
        let mut conn = pool.get().expect("conn");
        memora_import_service::import_memora(&mut conn, &memora, 100).expect("import")
    };
    assert_eq!(outcome.memora_rows, 4, "只导入分析完成、描述非空的照片");
    assert_eq!(outcome.matched_photos, 2, "已删除和本库没有的不算");
    assert_eq!(
        outcome.images_written, 3,
        "camera / Camera 两行（Windows 下同一个文件）都写上"
    );
    assert_eq!(outcome.skipped_vlm, 0);

    let conn = pool.get().expect("conn");
    let a = analysis_repo::get(&conn, ids[0]).expect("get").expect("a");
    assert_eq!(a.source, SOURCE_MEMORA);
    assert_eq!(a.prompt_version, "memora-m1");
    assert_eq!(a.model, "qwen3.5-9b");
    assert_eq!(a.categories, vec!["宠物".to_string()]);
    assert_eq!(a.duration_ms, Some(4_500));
    assert!(a.analyzed_at > 1_700_000_000, "本地时间字符串转成时间戳");
    assert_eq!(
        analysis_repo::get(&conn, ids[1])
            .expect("get")
            .expect("A")
            .description,
        a.description
    );
    assert!(analysis_repo::get(&conn, ids[3]).expect("get").is_none());

    // 类别写成标签（来源 memora）。
    let tags = tags_repo::tags_for_image(&conn, ids[2]).expect("tags");
    let names = tags.iter().map(|tag| tag.name.as_str()).collect::<Vec<_>>();
    assert_eq!(names, vec!["风景", "孩子"]);
    assert!(tags.iter().all(|tag| tag.source == "memora"));
    assert_eq!(
        analysis_repo::counts_by_source(&conn).expect("counts"),
        vec![("memora".to_string(), 3)]
    );
    drop(conn);

    // 描述进入检索索引。
    let mut cat = fulltext_ids(&pool, "橘猫");
    cat.sort_unstable();
    assert_eq!(cat, vec![ids[0], ids[1]]);
    // "沙堡"只出现在 b 的描述里，排第一（命中太少时放宽为单字，"沙发"里的"沙"也会带进来，排在后面）。
    assert_eq!(fulltext_ids(&pool, "沙堡").first(), Some(&ids[2]));
}

#[test]
fn vlm_results_are_not_overwritten_by_memora_reimport() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = open(dir.path());
    let ids = seed_library(&pool);
    let memora = memora_db(dir.path());
    {
        let conn = pool.get().expect("conn");
        images_repo::mark_deleted_by_id(&conn, ids[4], 9).expect("delete");
        let vlm = ImageAnalysis {
            image_id: ids[2],
            source: SOURCE_VLM.to_string(),
            model: "qwen3.5-9b-q4".to_string(),
            prompt_version: "search-v1".to_string(),
            description: "VLM 写的描述：黄昏的海滩".to_string(),
            categories: vec!["风景".to_string()],
            keywords: vec!["夕阳".to_string(), "海岸".to_string()],
            people_count: Some(2),
            visible_text: None,
            sensitive: false,
            raw_json: None,
            duration_ms: Some(1_700),
            analyzed_at: 50,
        };
        assert!(analysis_repo::upsert(&conn, &vlm).expect("vlm"));
    }
    let outcome = {
        let mut conn = pool.get().expect("conn");
        memora_import_service::import_memora(&mut conn, &memora, 100).expect("import")
    };
    assert_eq!(outcome.skipped_vlm, 1);
    assert_eq!(outcome.images_written, 2);
    {
        let conn = pool.get().expect("conn");
        let b = analysis_repo::get(&conn, ids[2]).expect("get").expect("b");
        assert_eq!(b.source, SOURCE_VLM);
        assert!(b.description.starts_with("VLM"));
        assert_eq!(b.keywords, vec!["夕阳".to_string(), "海岸".to_string()]);
    }
    // VLM 的同义词也进检索索引（keywords 列）。
    assert_eq!(fulltext_ids(&pool, "海岸").first(), Some(&ids[2]));

    // 再导一次：幂等（更新已有的 memora 行，VLM 行仍然跳过）。
    let again = {
        let mut conn = pool.get().expect("conn");
        memora_import_service::import_memora(&mut conn, &memora, 200).expect("import")
    };
    assert_eq!((again.images_written, again.skipped_vlm), (2, 1));
    let conn = pool.get().expect("conn");
    assert_eq!(
        tags_repo::tags_for_image(&conn, ids[0])
            .expect("tags")
            .len(),
        1,
        "重复导入不会叠加类别标签"
    );
}

#[test]
fn import_rejects_files_that_are_not_memora_databases() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = open(dir.path());
    let mut conn = pool.get().expect("conn");
    let missing = memora_import_service::import_memora(&mut conn, &dir.path().join("nope.db"), 1);
    assert!(missing.is_err());

    let other = dir.path().join("other.db");
    Connection::open(&other)
        .expect("other")
        .execute_batch("CREATE TABLE photos (id INTEGER PRIMARY KEY, path TEXT);")
        .expect("schema");
    let error = memora_import_service::import_memora(&mut conn, &other, 1)
        .expect_err("not memora")
        .to_string();
    assert!(error.contains("不像 memora 数据库"), "{error}");
}

#[test]
fn upsert_tag_keeps_the_first_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = open(dir.path());
    let conn = pool.get().expect("conn");
    let first = tags_repo::upsert_tag(&conn, "猫", "ai", None, 1).expect("ai");
    let second = tags_repo::upsert_tag(&conn, "猫", "memora", Some("category"), 2).expect("memora");
    assert_eq!(first, second);
    let tag = tags_repo::get_tag_by_name(&conn, "猫")
        .expect("get")
        .expect("tag");
    assert_eq!(
        tag.source, "ai",
        "谁打的标签以 image_tags.source 为准，tags.source 不被覆盖"
    );
    assert_eq!(tag.category.as_deref(), Some("category"));
}

/// 回归：image_analysis 的 UPSERT 走 DO UPDATE 分支时，触发器里的 OR IGNORE 会被外层冲突策略
/// 覆盖。检索索引积压表里已经有这张图（还没被后台处理）时，更新描述不能报 UNIQUE 约束错误。
#[test]
fn updating_analysis_with_pending_index_rows_does_not_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = open(dir.path());
    let ids = seed_library(&pool);
    let memora = memora_db(dir.path());
    let mut conn = pool.get().expect("conn");
    // 新入库的图都在积压表里；连续导入两次（第二次全部走 DO UPDATE）。
    memora_import_service::import_memora(&mut conn, &memora, 1).expect("first import");
    memora_import_service::import_memora(&mut conn, &memora, 2).expect("re-import");
    // VLM 覆盖 memora 的描述（同样走 DO UPDATE）。
    let vlm = ImageAnalysis {
        image_id: ids[2],
        source: SOURCE_VLM.to_string(),
        model: "qwen3.5-9b-q4".to_string(),
        prompt_version: "search-v1".to_string(),
        description: "VLM 的描述".to_string(),
        categories: Vec::new(),
        keywords: Vec::new(),
        people_count: None,
        visible_text: None,
        sensitive: false,
        raw_json: None,
        duration_ms: None,
        analyzed_at: 3,
    };
    assert!(analysis_repo::upsert(&conn, &vlm).expect("vlm overwrite"));
    let pending: i64 = conn
        .query_row("SELECT COUNT(*) FROM search_fts_dirty", [], |row| {
            row.get(0)
        })
        .expect("pending");
    assert_eq!(pending, 5, "每张图只排一次");
}
