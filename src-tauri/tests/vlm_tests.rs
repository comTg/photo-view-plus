//! docs/10 A6：llama-server 监管状态机（复用外部服务、启动并等待加载、显存不足拒绝、启动次数上限、
//! 槽位空闲才停、空闲卸载、进程退出回收）与 VLM 流水线的数据库部分（候选顺序、大小写重复行、
//! 标签替换、失败重试上限、提示词升级手动重跑、路径解析、读原图错误分类）。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use photo_view_plus_lib::config::AppSettings;
use photo_view_plus_lib::db::{self, Pool};
use photo_view_plus_lib::repo::ai_jobs_repo::{self, KIND_VLM_ANALYSIS, MAX_ATTEMPTS};
use photo_view_plus_lib::repo::analysis_repo::{self, ImageAnalysis, SOURCE_MEMORA, SOURCE_VLM};
use photo_view_plus_lib::repo::images_repo::{self, NewImageMetadata};
use photo_view_plus_lib::repo::roots_repo::{self, NewRoot};
use photo_view_plus_lib::repo::tags_repo;
use photo_view_plus_lib::services::ai_client::{VlmResult, VlmTag};
use photo_view_plus_lib::services::analysis_pipeline::{
    self, job_model, read_original, record_failure, resolve_vlm_paths, save_vlm_result, Failure,
};
use photo_view_plus_lib::services::vlm_supervisor::{
    LaunchSpec, VlmPaths, VlmStartError, VlmSupervisor,
};
use rusqlite::params;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---------------------------------------------------------------------------
// 假 llama-server：/health（加载中 503）与 /slots
// ---------------------------------------------------------------------------

struct FakeLlama {
    port: u16,
    healthy: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
}

async fn fake_llama(healthy: bool) -> FakeLlama {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let healthy = Arc::new(AtomicBool::new(healthy));
    let busy = Arc::new(AtomicBool::new(false));
    let (healthy_flag, busy_flag) = (healthy.clone(), busy.clone());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let healthy = healthy_flag.clone();
            let busy = busy_flag.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 4096];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, body) = match path.as_str() {
                    "/health" if healthy.load(Ordering::SeqCst) => ("200 OK", r#"{"status":"ok"}"#),
                    "/health" => (
                        "503 Service Unavailable",
                        r#"{"error":{"message":"Loading model"}}"#,
                    ),
                    "/slots" if busy.load(Ordering::SeqCst) => (
                        "200 OK",
                        r#"[{"id":0,"is_processing":true},{"id":1,"is_processing":false}]"#,
                    ),
                    "/slots" => (
                        "200 OK",
                        r#"[{"id":0,"is_processing":false},{"id":1,"is_processing":false}]"#,
                    ),
                    _ => ("404 Not Found", "{}"),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    FakeLlama {
        port,
        healthy,
        busy,
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

/// 一直活着的假服务进程（真正的 HTTP 由 FakeLlama 提供）。
fn long_running(seconds: u32) -> LaunchSpec {
    if cfg!(windows) {
        LaunchSpec {
            program: "ping".into(),
            args: vec!["-n".into(), seconds.to_string(), "127.0.0.1".into()],
        }
    } else {
        LaunchSpec {
            program: "sleep".into(),
            args: vec![seconds.to_string()],
        }
    }
}

/// 立刻退出的假服务进程（模拟模型加载失败）。
fn exits_immediately() -> LaunchSpec {
    if cfg!(windows) {
        LaunchSpec {
            program: "cmd".into(),
            args: vec!["/C".into(), "exit 3".into()],
        }
    } else {
        LaunchSpec {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 3".into()],
        }
    }
}

fn supervisor(port: u16, free_vram_mb: Option<u64>) -> VlmSupervisor {
    VlmSupervisor::new(port, None)
        .expect("supervisor")
        .with_vram_probe(Arc::new(move || free_vram_mb))
}

#[test]
fn launch_args_match_memora() {
    let paths = VlmPaths {
        exe: "C:/llama/llama-server.exe".into(),
        model: "C:/m/q.gguf".into(),
        mmproj: "C:/m/mm.gguf".into(),
    };
    let spec = LaunchSpec::llama(&paths, 4, 8081);
    assert_eq!(spec.program, Path::new("C:/llama/llama-server.exe"));
    assert_eq!(
        spec.args,
        [
            "-m",
            "C:/m/q.gguf",
            "--mmproj",
            "C:/m/mm.gguf",
            "-c",
            "24576",
            "-np",
            "4",
            "-ngl",
            "99",
            "-fa",
            "on",
            "--reasoning",
            "off",
            "--host",
            "127.0.0.1",
            "--port",
            "8081"
        ]
    );
}

#[tokio::test]
async fn reuses_healthy_external_server_without_owning_it() {
    let fake = fake_llama(true).await;
    // 显存很少也没关系：复用现成的服务不需要再加载模型。
    let vlm = supervisor(fake.port, Some(1000));
    let missing = LaunchSpec {
        program: "does-not-exist.exe".into(),
        args: Vec::new(),
    };
    let status = vlm.ensure_running(&missing).await.expect("reuse");
    assert_eq!(status.state, "ready");
    assert!(!status.owned);
    assert_eq!(status.pid, None);
    assert!(
        !vlm.stop(true).await,
        "never stops a server it did not start"
    );
}

#[tokio::test]
async fn spawns_waits_for_model_load_and_stops_only_when_slots_idle() {
    let fake = fake_llama(false).await;
    let vlm = supervisor(fake.port, Some(10_000));
    let healthy = fake.healthy.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        healthy.store(true, Ordering::SeqCst);
    });
    let status = vlm.ensure_running(&long_running(60)).await.expect("start");
    assert_eq!(status.state, "ready");
    assert!(status.owned);
    assert!(status.pid.is_some());
    assert_eq!(status.starts_last_minute, 1);
    // 已就绪：再调用不会重复启动。
    let again = vlm.ensure_running(&long_running(60)).await.expect("ready");
    assert_eq!(again.pid, status.pid);
    assert_eq!(again.starts_last_minute, 1);

    // 有槽位在处理：不停（可能是别的程序在复用这个服务）。
    fake.busy.store(true, Ordering::SeqCst);
    assert!(!vlm.stop(false).await);
    assert!(vlm.is_ready().await);
    fake.busy.store(false, Ordering::SeqCst);
    assert!(vlm.stop(false).await);
    let stopped = vlm.status().await;
    assert_eq!(stopped.state, "stopped");
    assert_eq!(stopped.pid, None);
}

#[tokio::test]
async fn refuses_to_start_without_enough_vram() {
    let fake = fake_llama(false).await;
    let vlm = supervisor(fake.port, Some(4096));
    let error = vlm
        .ensure_running(&long_running(60))
        .await
        .expect_err("not enough VRAM");
    assert_eq!(error, VlmStartError::Vram { free_mb: 4096 });
    assert!(error.to_string().contains("4.0GB"), "{error}");
    let status = vlm.status().await;
    assert_eq!(status.starts_last_minute, 0);
    assert!(status.last_error.is_some());
}

#[tokio::test]
async fn failed_starts_hit_the_restart_limit() {
    let vlm = supervisor(free_port(), None);
    for _ in 0..3 {
        let error = vlm
            .ensure_running(&exits_immediately())
            .await
            .expect_err("exits");
        assert!(matches!(error, VlmStartError::Failed(_)), "{error:?}");
    }
    let error = vlm
        .ensure_running(&exits_immediately())
        .await
        .expect_err("limit");
    assert_eq!(error, VlmStartError::RestartLimit);
    let status = vlm.status().await;
    assert_eq!(status.state, "degraded");
    assert_eq!(status.starts_last_minute, 3);
}

#[tokio::test]
async fn idle_unload_waits_for_quiet_period_and_reaps_dead_process() {
    let fake = fake_llama(false).await;
    let vlm = supervisor(fake.port, None);
    let healthy = fake.healthy.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        healthy.store(true, Ordering::SeqCst);
    });
    vlm.ensure_running(&long_running(60)).await.expect("start");
    vlm.touch();
    assert!(!vlm.stop_if_idle(Duration::from_secs(60)).await);
    assert!(vlm.stop_if_idle(Duration::ZERO).await);

    // 自己的进程意外退出：状态回到 stopped，并记下原因。
    fake.healthy.store(false, Ordering::SeqCst);
    let healthy = fake.healthy.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        healthy.store(true, Ordering::SeqCst);
    });
    vlm.ensure_running(&long_running(2))
        .await
        .expect("start short-lived");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let status = vlm.status().await;
    assert_eq!(status.state, "stopped");
    assert!(status
        .last_error
        .as_deref()
        .is_some_and(|error| error.contains("已退出")));
}

// ---------------------------------------------------------------------------
// 流水线的数据库部分
// ---------------------------------------------------------------------------

fn open(dir: &Path) -> Pool {
    db::open(&dir.join("app.sqlite")).expect("db")
}

fn add_images(pool: &Pool, root: &str, entries: &[(&str, Option<i64>)]) -> Vec<i64> {
    let conn = pool.get().expect("conn");
    let root_id = match roots_repo::list(&conn)
        .expect("roots")
        .into_iter()
        .find(|existing| existing.path == root)
    {
        Some(existing) => existing.id,
        None => {
            roots_repo::insert(
                &conn,
                &NewRoot {
                    path: root.to_string(),
                    label: None,
                    root_type: "local".to_string(),
                },
                1,
            )
            .expect("root")
            .id
        }
    };
    entries
        .iter()
        .map(|(rel, taken_at)| {
            let filename = rel.rsplit('\\').next().unwrap_or(rel).to_string();
            images_repo::upsert_scanned_image(
                &conn,
                &NewImageMetadata {
                    root_id,
                    rel_path: (*rel).to_string(),
                    filename,
                    extension: "jpg".to_string(),
                    size_bytes: 1,
                    mtime: 1,
                    width: None,
                    height: None,
                    orientation: None,
                    taken_at: *taken_at,
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

fn result(description: &str) -> VlmResult {
    VlmResult {
        description: description.to_string(),
        categories: vec!["宠物".to_string(), "日常生活".to_string()],
        tags: vec![
            VlmTag {
                name: "猫".to_string(),
                score: 0.95,
            },
            VlmTag {
                name: "窗台".to_string(),
                score: 0.93,
            },
        ],
        keywords: vec!["猫咪".to_string(), "宠物".to_string()],
        people_count: 0,
        visible_text: String::new(),
        sensitive: false,
        raw_json: Some("{}".to_string()),
    }
}

fn pending_ids(pool: &Pool) -> Vec<i64> {
    let conn = pool.get().expect("conn");
    ai_jobs_repo::pending_vlm_analysis(&conn, &job_model(), 100)
        .expect("pending")
        .into_iter()
        .map(|candidate| candidate.image_id)
        .collect()
}

#[test]
fn candidates_prefer_undescribed_camera_photos_and_newer_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = open(dir.path());
    let ids = add_images(
        &pool,
        r"D:\photos",
        &[
            (r"WeiXin\wx_1.jpg", Some(300)),       // 0：微信，最后
            (r"misc\old.jpg", Some(100)),          // 1：其他目录
            (r"misc\new.jpg", Some(200)),          // 2：其他目录，更新
            (r"phone\Camera\IMG_1.jpg", Some(50)), // 3：相机目录
            (r"WeiXin\wx_camera_1.jpg", Some(10)), // 4：微信里自己拍的
            (r"phone\Camera\IMG_2.jpg", Some(60)), // 5：相机目录，但已有 memora 描述
        ],
    );
    {
        let conn = pool.get().expect("conn");
        analysis_repo::upsert(
            &conn,
            &ImageAnalysis {
                image_id: ids[5],
                source: SOURCE_MEMORA.to_string(),
                model: "qwen3.5-9b".to_string(),
                prompt_version: "memora-m1".to_string(),
                description: "一只猫".to_string(),
                categories: vec!["宠物".to_string()],
                keywords: Vec::new(),
                people_count: None,
                visible_text: None,
                sensitive: false,
                raw_json: None,
                duration_ms: None,
                analyzed_at: 1,
            },
        )
        .expect("memora");
    }
    assert_eq!(
        pending_ids(&pool),
        vec![ids[3], ids[4], ids[2], ids[1], ids[0], ids[5]]
    );
    let conn = pool.get().expect("conn");
    assert_eq!(
        ai_jobs_repo::count_pending_vlm_analysis(&conn, &job_model()).expect("count"),
        6
    );
}

#[test]
fn result_is_written_to_case_duplicates_and_replaces_memora_tags() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = open(dir.path());
    let ids = add_images(
        &pool,
        r"D:\photos",
        &[
            (r"camera\a.jpg", None),
            (r"Camera\a.jpg", None),
            ("b.jpg", None),
        ],
    );
    {
        // memora 导入过的类别标签：会被 VLM 结果替换。
        let conn = pool.get().expect("conn");
        let tag_id =
            tags_repo::upsert_tag(&conn, "旅行", SOURCE_MEMORA, Some("category"), 1).expect("tag");
        conn.execute(
            "INSERT INTO image_tags(image_id, tag_id, score, source, created_at) VALUES (?1, ?2, 0.9, 'memora', 1)",
            params![ids[0], tag_id],
        )
        .expect("memora tag");
    }
    let candidates = {
        let conn = pool.get().expect("conn");
        ai_jobs_repo::pending_vlm_analysis(&conn, &job_model(), 10).expect("pending")
    };
    let first = candidates
        .iter()
        .find(|candidate| candidate.image_id == ids[0])
        .expect("candidate");
    let written = {
        let mut conn = pool.get().expect("conn");
        save_vlm_result(
            &mut conn,
            first,
            &result("一只橘猫趴在窗台上。"),
            Some(1700),
            2,
        )
        .expect("save")
    };
    assert_eq!(written, 2, "both case variants share one analysis");
    assert_eq!(pending_ids(&pool), vec![ids[2]]);

    let conn = pool.get().expect("conn");
    for id in &ids[..2] {
        let analysis = analysis_repo::get(&conn, *id).expect("get").expect("row");
        assert_eq!(analysis.source, SOURCE_VLM);
        assert_eq!(analysis.prompt_version, analysis_pipeline::PROMPT_VERSION);
        assert_eq!(analysis.people_count, Some(0));
        assert_eq!(analysis.visible_text, None);
        let tags = tags_repo::tags_for_image(&conn, *id).expect("tags");
        let names = tags
            .iter()
            .map(|tag| (tag.name.as_str(), tag.source.as_str()))
            .collect::<Vec<_>>();
        assert!(names.contains(&("猫", "vlm")));
        assert!(names.contains(&("宠物", "vlm")), "categories become tags");
        assert!(!names.iter().any(|(_, source)| *source == "memora"));
    }
    let categories = tags_repo::get_tag_by_name(&conn, "宠物")
        .expect("tag")
        .expect("exists");
    assert_eq!(categories.category.as_deref(), Some("category"));
    // 描述有了：描述向量进入待处理（A4 按输入指纹自动重算）。
    assert_eq!(
        ai_jobs_repo::count_pending_text_embedding(&conn, "bge-small-zh-v1.5").expect("text"),
        2
    );
}

#[test]
fn failures_retry_up_to_the_limit_and_can_be_reset() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = open(dir.path());
    let ids = add_images(&pool, r"D:\photos", &[("a.jpg", None), ("b.jpg", None)]);
    let conn = pool.get().expect("conn");
    for attempt in 1..=MAX_ATTEMPTS {
        record_failure(&conn, ids[0], "无法解码", attempt).expect("failure");
        let still_pending = pending_ids(&pool).contains(&ids[0]);
        assert_eq!(still_pending, attempt < MAX_ATTEMPTS, "attempt {attempt}");
    }
    let counts = ai_jobs_repo::counts(&conn, KIND_VLM_ANALYSIS, &job_model()).expect("counts");
    assert_eq!(counts.failed, 1);
    assert_eq!(ai_jobs_repo::retry_failed_vlm(&conn).expect("retry"), 1);
    assert!(pending_ids(&pool).contains(&ids[0]));
}

#[test]
fn prompt_upgrade_needs_manual_requeue() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = open(dir.path());
    let ids = add_images(&pool, r"D:\photos", &[("a.jpg", None)]);
    let conn = pool.get().expect("conn");
    conn.execute(
        "INSERT INTO ai_jobs(image_id, kind, model, status, attempts, updated_at)
         VALUES (?1, 'vlm_analysis', 'qwen3.5-9b-q4@search-v0', 'ready', 1, 1)",
        [ids[0]],
    )
    .expect("old job");
    // 旧版本的结果仍然有效，不自动重跑。
    assert!(pending_ids(&pool).is_empty());
    assert_eq!(
        ai_jobs_repo::requeue_outdated_vlm(&conn, &job_model()).expect("requeue"),
        1
    );
    assert_eq!(pending_ids(&pool), vec![ids[0]]);
}

#[test]
fn vlm_paths_come_from_settings_or_detection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("PhotoViewPlus");
    let models_dir = root.join("models");
    let expected = VlmPaths::under(&root);
    // 设置里填了但文件不存在：说清楚缺哪个。
    let mut settings = AppSettings {
        vlm_server_exe: dir.path().join("nope.exe").to_string_lossy().to_string(),
        ..AppSettings::default()
    };
    let error = resolve_vlm_paths(&settings, &models_dir).expect_err("missing");
    assert!(error.contains("llama-server"), "{error}");

    for path in [&expected.exe, &expected.model, &expected.mmproj] {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, b"x").expect("file");
    }
    settings.vlm_server_exe.clear();
    assert_eq!(
        resolve_vlm_paths(&settings, &models_dir).expect("detected"),
        expected
    );
    assert_eq!(
        analysis_pipeline::detect_vlm_paths(&models_dir),
        Some(expected.clone())
    );

    settings.vlm_server_exe = expected.exe.to_string_lossy().to_string();
    settings.vlm_model_path = expected.model.to_string_lossy().to_string();
    settings.vlm_mmproj_path = expected.mmproj.to_string_lossy().to_string();
    assert_eq!(
        resolve_vlm_paths(&settings, &models_dir).expect("configured"),
        expected
    );
}

#[test]
fn missing_original_is_permanent_but_missing_root_is_transient() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        read_original(dir.path(), &dir.path().join("missing.jpg")),
        Err(Failure::Permanent("文件不存在".to_string()))
    );
    let file = dir.path().join("a.jpg");
    std::fs::write(&file, b"jpeg bytes").expect("write");
    assert_eq!(
        read_original(dir.path(), &file).expect("read"),
        b"jpeg bytes"
    );
    // 整个图库目录不在（盘符拔掉、NAS 断开）：临时错误，不记失败次数。
    let gone = dir.path().join("unplugged-drive");
    assert!(matches!(
        read_original(&gone, &gone.join("a.jpg")),
        Err(Failure::Transient(_))
    ));
}

// ---------------------------------------------------------------------------
// A7：RAM-plus 下线
// ---------------------------------------------------------------------------

fn vlm_result(categories: &[&str], tags: &[&str]) -> VlmResult {
    VlmResult {
        description: format!("{}的照片", tags.join("、")),
        categories: categories.iter().map(|c| c.to_string()).collect(),
        tags: tags
            .iter()
            .enumerate()
            .map(|(index, name)| VlmTag {
                name: name.to_string(),
                score: 0.95 - index as f64 * 0.02,
            })
            .collect(),
        keywords: Vec::new(),
        people_count: 0,
        visible_text: String::new(),
        sensitive: false,
        raw_json: None,
    }
}

fn save(pool: &Pool, image_id: i64, result: &VlmResult) {
    let mut conn = pool.get().expect("conn");
    let candidate = ai_jobs_repo::pending_vlm_analysis(&conn, &job_model(), 100)
        .expect("pending")
        .into_iter()
        .find(|candidate| candidate.image_id == image_id)
        .expect("candidate");
    save_vlm_result(&mut conn, &candidate, result, None, 1).expect("save");
}

fn ram_tags(pool: &Pool, image_id: i64, names: &[&str]) {
    let mut conn = pool.get().expect("conn");
    let tags = names
        .iter()
        .map(|name| tags_repo::NewTagScore {
            name: name.to_string(),
            score: 0.9,
            source: "ai".to_string(),
            category: None,
        })
        .collect::<Vec<_>>();
    tags_repo::replace_image_tags(&mut conn, image_id, &tags, 1).expect("ram tags");
}

#[test]
fn tag_cloud_groups_tags_by_dominant_vlm_category() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = open(dir.path());
    let ids = add_images(
        &pool,
        r"D:\photos",
        &[
            ("a.jpg", None),
            ("b.jpg", None),
            ("c.jpg", None),
            ("d.jpg", None),
        ],
    );
    save(&pool, ids[0], &vlm_result(&["宠物"], &["猫", "窗台"]));
    save(&pool, ids[1], &vlm_result(&["宠物"], &["猫"]));
    save(&pool, ids[2], &vlm_result(&["日常生活"], &["窗台", "沙发"]));
    ram_tags(&pool, ids[3], &["沙发", "地毯"]);
    let conn = pool.get().expect("conn");
    let tags = tags_repo::list_tags(&conn, 50).expect("tags");
    let group = |name: &str| {
        tags.iter()
            .find(|tag| tag.name == name)
            .unwrap_or_else(|| panic!("tag {name}"))
            .group
            .clone()
    };
    assert_eq!(group("猫").as_deref(), Some("宠物"));
    // 类别标签自成一组。
    assert_eq!(group("宠物").as_deref(), Some("宠物"));
    assert_eq!(group("日常生活").as_deref(), Some("日常生活"));
    // 平票时按类别名排序取第一个，结果稳定。
    assert_eq!(group("窗台").as_deref(), Some("宠物"));
    // VLM 和 RAM 都打过的标签按 VLM 的图归组；只有 RAM 打过的没有分组。
    assert_eq!(group("沙发").as_deref(), Some("日常生活"));
    assert_eq!(group("地毯"), None);
}

#[test]
fn ram_tag_counts_and_vlm_coverage_drive_the_cleanup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = open(dir.path());
    let ids = add_images(
        &pool,
        r"D:\photos",
        &[
            ("a.jpg", None),
            ("b.jpg", None),
            ("c.jpg", None),
            ("d.jpg", None),
        ],
    );
    ram_tags(&pool, ids[0], &["沙发", "地毯"]);
    ram_tags(&pool, ids[1], &["沙发"]);
    save(&pool, ids[0], &vlm_result(&["日常生活"], &["沙发"]));
    let mut conn = pool.get().expect("conn");
    assert_eq!(tags_repo::ram_tag_counts(&conn).expect("ram"), (2, 2));
    assert_eq!(ai_jobs_repo::vlm_coverage(&conn).expect("coverage"), (1, 4));
    // VLM 写入不碰 RAM 的标签（来源不同），清理只删 RAM 的。
    tags_repo::clear_ai_tags_and_reset(&mut conn).expect("cleanup");
    assert_eq!(tags_repo::ram_tag_counts(&conn).expect("ram"), (0, 0));
    let remaining = tags_repo::tags_for_image(&conn, ids[0]).expect("tags");
    assert!(remaining.iter().all(|tag| tag.source == "vlm"));
    assert!(remaining.iter().any(|tag| tag.name == "沙发"));
}

#[test]
fn vlm_tagger_switches_ram_tagging_off() {
    use photo_view_plus_lib::config::{AppPaths, AppSettingsPatch, TAGGER_VLM};
    use photo_view_plus_lib::queue::Scheduler;
    use photo_view_plus_lib::services::ai_pipeline::AiPipeline;
    use photo_view_plus_lib::services::ai_supervisor::AiSupervisor;
    use photo_view_plus_lib::services::settings_service;

    let dir = tempfile::tempdir().expect("tempdir");
    let paths = AppPaths::from_data_dir(dir.path().to_path_buf());
    std::fs::create_dir_all(paths.db_path.parent().expect("db dir")).expect("mkdir");
    let pool = db::open(&paths.db_path).expect("db");
    let ids = add_images(&pool, r"D:\photos", &[("a.jpg", None)]);
    {
        let conn = pool.get().expect("conn");
        images_repo::update_thumbnail_ready(&conn, ids[0], "abcd", "jpg", Some(10), Some(10))
            .expect("thumb");
    }
    let supervisor = std::sync::Arc::new(
        AiSupervisor::new(dir.path().join("worker"), dir.path().join("models")).expect("sup"),
    );
    let pipeline = AiPipeline::new(pool.clone(), Scheduler::start(1), supervisor, &paths);
    assert_eq!(pipeline.status().tagging_pending, 1);

    let mut settings = AppSettings::default();
    settings.apply_patch(AppSettingsPatch {
        ai_tagger_model: Some(TAGGER_VLM.to_string()),
        ..AppSettingsPatch::default()
    });
    assert_eq!(settings.ai_tagger_model, TAGGER_VLM);
    // 不认识的值不改设置。
    settings.apply_patch(AppSettingsPatch {
        ai_tagger_model: Some("siglip".to_string()),
        ..AppSettingsPatch::default()
    });
    assert_eq!(settings.ai_tagger_model, TAGGER_VLM);
    settings_service::write(&paths, &settings).expect("write settings");
    assert_eq!(pipeline.status().tagging_pending, 0);
}
