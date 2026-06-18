use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use http::header;
use tauri::{Emitter, Manager};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

mod commands;
mod config;
pub mod db;
pub mod error;
pub mod migrations;
pub mod queue;
pub mod repo;
pub mod services;
pub mod utils;

pub use error::{AppError, AppResult};

use services::dedup_service::DedupCoordinator;
use services::startup_gate::StartupGate;
use utils::bk_tree::DhashIndex;

static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// 前端迟迟不发「首屏就绪」信号时，启动期重活最多等多久就自行放行。
const STARTUP_GATE_FALLBACK: Duration = Duration::from_secs(30);

pub fn run() {
    let profile = config::Profile::from_env();
    init_tracing(profile);
    tracing::info!(?profile, "PhotoView+ starting");

    tauri::Builder::default()
        .register_asynchronous_uri_scheme_protocol("thumb", thumb_protocol)
        .register_asynchronous_uri_scheme_protocol("original", original_protocol)
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .manage(profile)
        .setup(move |app| {
            set_profile_window_title(app.handle(), profile);

            let data_dir = app.path().app_local_data_dir()?;
            let paths = config::AppPaths::from_data_dir(data_dir);
            std::fs::create_dir_all(&paths.data_dir)?;
            std::fs::create_dir_all(&paths.thumbs_dir)?;
            std::fs::create_dir_all(&paths.vectors_dir)?;
            std::fs::create_dir_all(&paths.models_dir)?;

            tracing::info!(path = ?paths.db_path, "opening database");
            let pool =
                db::open(&paths.db_path).map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
            services::scan_service::recover_interrupted_tasks(&pool)
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
            // 给缩略图（P0）设并发上限 = 缩略图 CPU 限额，避免一大批待生成缩略图占满全部
            // 并发槽、饿死扫描（P1）和去重哈希（P2/P3）。缩略图本就受 CPU 限流，不会因此变慢。
            let mut task_caps = [usize::MAX; 8];
            task_caps[queue::Priority::P0 as usize] =
                services::thumbnail_service::thumbnail_cpu_limit();
            // AI 后台任务（P5 标签 / P6 embedding）每类最多 1 个在飞：单个 worker/GPU 本就
            // 串行处理，放任并发只会让 worker 同时收到一堆请求、各开线程做 CPU 预处理，
            // 叠加缩略图把机器拖卡（worker 的 /tagger/run 是同步端点，并发会进 FastAPI 线程池）。
            task_caps[queue::Priority::P5 as usize] = 1;
            task_caps[queue::Priority::P6 as usize] = 1;
            let scheduler = queue::Scheduler::start_with_caps(16, task_caps);
            let queue_app = app.handle().clone();
            scheduler.spawn_status_loop(move |status| {
                let _ = queue_app.emit("queue:status", status);
            });

            // MVP2 状态：BK-tree（视觉哈希索引） + dedup 协调器。
            // BK-tree 体量随图库增长，全量加载放后台线程（见 spawn_background_init），
            // 否则会阻塞 setup 主线程的消息泵、导致窗口长时间白屏。
            let dhash_index = Arc::new(DhashIndex::new());
            let dedup_coord = Arc::new(DedupCoordinator::new());

            // MVP3 AI worker supervisor：按需启动 Python 子进程，后台 monitor 只负责已启动后的健康检查。
            let ai_worker_dir = resolve_ai_worker_dir();
            let ai_supervisor = Arc::new(
                services::ai_supervisor::AiSupervisor::new(ai_worker_dir, paths.models_dir.clone())
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?,
            );
            ai_supervisor.clone().spawn_monitor(app.handle().clone());
            let ai_pipeline = Arc::new(services::ai_pipeline::AiPipeline::new(
                pool.clone(),
                scheduler.clone(),
                ai_supervisor.clone(),
                &paths,
            ));
            ai_pipeline.clone().spawn_loop(app.handle().clone());

            let startup_gate = Arc::new(StartupGate::new());

            // BK-tree 全量加载：一次性查全库，放后台线程，让 setup 尽快返回。
            spawn_bktree_init(pool.clone(), dhash_index.clone());

            // 重排上次未完成的缩略图会瞬间灌入数千个解码任务，和 dev 下 WebView/Vite
            // 的首屏模块加载抢 CPU/磁盘（实测把白屏拖到近两分钟）。推迟到前端首屏绘制
            // 之后（frontend_ready）或兜底超时再做。
            spawn_deferred_thumbnail_requeue(
                pool.clone(),
                scheduler.clone(),
                paths.thumbs_dir.clone(),
                startup_gate.clone(),
            );

            app.manage(paths);
            app.manage(pool);
            app.manage(scheduler);
            app.manage(dhash_index);
            app.manage(dedup_coord);
            app.manage(ai_supervisor);
            app.manage(ai_pipeline);
            app.manage(startup_gate);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::system::ping,
            commands::system::db_status,
            commands::system::ui_perf,
            commands::system::frontend_ready,
            commands::roots::roots_add,
            commands::roots::roots_list,
            commands::roots::roots_remove,
            commands::roots::roots_update,
            commands::scan::scan_start,
            commands::scan::scan_pause,
            commands::scan::scan_resume,
            commands::scan::scan_cancel,
            commands::scan::scan_status,
            commands::scan::queue_status,
            commands::images::images_query,
            commands::images::images_get_detail,
            commands::images::images_rename,
            commands::images::images_reveal_in_dir,
            commands::images::thumbs_path,
            commands::settings::settings_get,
            commands::settings::settings_update,
            commands::dedup::dedup_start,
            commands::dedup::dedup_status,
            commands::dedup::dedup_groups,
            commands::dedup::dedup_group_detail,
            commands::dedup::dedup_resolve,
            commands::dedup::dedup_batch_resolve,
            commands::dedup::dedup_export_csv,
            commands::trash::trash_history,
            commands::trash::trash_undo,
            commands::ai::ai_worker_start,
            commands::ai::ai_worker_stop,
            commands::ai::ai_worker_status,
            commands::ai::ai_worker_diagnostics,
            commands::ai::ai_models_status,
            commands::ai::ai_model_download,
            commands::ai::ai_search,
            commands::ai::ai_search_by_image,
            commands::ai::ai_tag_image,
            commands::ai::ai_pipeline_status,
            commands::ai::ai_process_pending,
            commands::ai::ai_tags_list,
            commands::ai::ai_image_tags,
            commands::ai::ai_images_by_tag,
            commands::ai::ai_retag_all,
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|error| eprintln!("error while running tauri application: {error}"));
}

/// BK-tree 全量加载：查全库，放后台线程，不能放在 setup 主线程里跑，否则窗口会白屏到它结束。
fn spawn_bktree_init(pool: db::Pool, dhash_index: Arc<DhashIndex>) {
    std::thread::spawn(move || match pool.get() {
        Ok(conn) => match repo::images_repo::all_dhashes(&conn) {
            Ok(entries) => {
                let count = entries.len();
                dhash_index.rebuild_from(entries);
                tracing::info!(loaded = count, "dhash BK-tree loaded");
            }
            Err(error) => tracing::warn!(%error, "failed to load dhash BK-tree on startup"),
        },
        Err(error) => tracing::warn!(%error, "failed to get conn for BK-tree init"),
    });
}

/// 重排上次未完成的缩略图（扫描中断会留下 thumb_status='pending' 的图，重复扫描不会
/// 再触碰未改动文件，必须主动恢复）。但这会瞬间灌入大量解码任务，启动时会和首屏抢资源，
/// 所以挂在 [`StartupGate`] 上——等前端首屏绘制完成或兜底超时后再做。
fn spawn_deferred_thumbnail_requeue(
    pool: db::Pool,
    scheduler: queue::Scheduler,
    thumbs_dir: PathBuf,
    gate: Arc<StartupGate>,
) {
    tauri::async_runtime::spawn(async move {
        gate.wait(STARTUP_GATE_FALLBACK).await;
        // 重排本身是同步阻塞（查 pending + 入队），丢到 blocking 线程，别占 async runtime。
        let joined = tokio::task::spawn_blocking(move || {
            services::thumbnail_service::requeue_pending_thumbnails(&pool, &scheduler, &thumbs_dir)
        })
        .await;
        match joined {
            Ok(Ok(count)) if count > 0 => tracing::info!(count, "requeued pending thumbnails"),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(%error, "failed to requeue pending thumbnails"),
            Err(error) => tracing::warn!(%error, "thumbnail requeue task join failed"),
        }
    });
}

/// 定位 ai-worker 目录。优先 `PVP_AI_WORKER_DIR`，否则在若干候选位置里取第一个真实存在的。
/// 注意：`tauri dev` 下进程工作目录是 `src-tauri/`，所以必须向上找一级，
/// 不能直接 `current_dir()/ai-worker`（那会落到不存在的 `src-tauri/ai-worker`）。
fn resolve_ai_worker_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PVP_AI_WORKER_DIR") {
        return PathBuf::from(dir);
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("ai-worker"));
        if let Some(parent) = cwd.parent() {
            candidates.push(parent.join("ai-worker"));
        }
    }
    // 编译期已知的源码树位置（dev 兜底）：<repo>/src-tauri 的上一级 = <repo>。
    if let Some(repo_root) = Path::new(env!("CARGO_MANIFEST_DIR")).parent() {
        candidates.push(repo_root.join("ai-worker"));
    }

    candidates
        .iter()
        .find(|dir| dir.exists())
        .cloned()
        .or_else(|| candidates.into_iter().next())
        .unwrap_or_else(|| PathBuf::from("ai-worker"))
}

fn set_profile_window_title(app: &tauri::AppHandle<tauri::Wry>, profile: config::Profile) {
    let title = match profile {
        config::Profile::Dev => "PhotoView+ — dev",
        config::Profile::Test => "PhotoView+ — test",
        config::Profile::Prod => "PhotoView+",
    };

    match app.get_webview_window("main") {
        Some(window) => {
            if let Err(error) = window.set_title(title) {
                tracing::warn!(%error, "failed to set profile window title");
            }
        }
        None => tracing::warn!("main window not found while setting profile title"),
    }
}

// 缩略图协议必须异步：同步处理器在 WebView 的 UI 线程上执行（WebResourceRequested
// 就在该线程触发），而每次请求都要查 DB + 同步读 webp 文件。启动时网格会同时发出
// 几十个缩略图请求，串行占住 UI 线程会让窗口长时间白屏。挪到后台线程响应即可，
// 和 original_protocol 同构。
fn thumb_protocol(
    ctx: tauri::UriSchemeContext<'_, tauri::Wry>,
    request: http::Request<Vec<u8>>,
    responder: tauri::UriSchemeResponder,
) {
    let image_id = image_id_from_request(&request);
    let app = ctx.app_handle().clone();

    std::thread::spawn(move || {
        let response = match image_id {
            Some(image_id) => thumb_response(&app, image_id),
            None => text_response(http::StatusCode::BAD_REQUEST, "invalid thumbnail image id"),
        };
        responder.respond(response);
    });
}

fn thumb_response(app: &tauri::AppHandle<tauri::Wry>, image_id: i64) -> http::Response<Vec<u8>> {
    let pool = app.state::<db::Pool>();
    let paths = app.state::<config::AppPaths>();
    let conn = match pool.get() {
        Ok(conn) => conn,
        Err(error) => {
            return text_response(http::StatusCode::INTERNAL_SERVER_ERROR, &error.to_string())
        }
    };
    let hash = match repo::images_repo::get_thumb_hash(&conn, image_id) {
        Ok(Some(hash)) => hash,
        Ok(None) => return text_response(http::StatusCode::NOT_FOUND, "thumbnail is not ready"),
        Err(error) => {
            return text_response(http::StatusCode::INTERNAL_SERVER_ERROR, &error.to_string())
        }
    };
    drop(conn);
    let path = services::thumbnail_service::thumb_path(&paths.thumbs_dir, &hash);
    match std::fs::read(path) {
        Ok(bytes) => http::Response::builder()
            .status(http::StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/webp")
            .header(header::CACHE_CONTROL, "max-age=86400")
            .body(bytes)
            .unwrap_or_else(|_| http::Response::new(Vec::new())),
        Err(_) => text_response(http::StatusCode::NOT_FOUND, "thumbnail file missing"),
    }
}

fn original_protocol(
    ctx: tauri::UriSchemeContext<'_, tauri::Wry>,
    request: http::Request<Vec<u8>>,
    responder: tauri::UriSchemeResponder,
) {
    let image_id = image_id_from_request(&request);
    let method = request.method().clone();
    let app = ctx.app_handle().clone();

    std::thread::spawn(move || {
        let response = if method != http::Method::GET && method != http::Method::HEAD {
            text_response(
                http::StatusCode::METHOD_NOT_ALLOWED,
                "original protocol only supports GET and HEAD",
            )
        } else if let Some(image_id) = image_id {
            original_response(&app, image_id, method != http::Method::HEAD)
        } else {
            text_response(http::StatusCode::BAD_REQUEST, "invalid original image id")
        };
        responder.respond(response);
    });
}

fn original_response(
    app: &tauri::AppHandle<tauri::Wry>,
    image_id: i64,
    include_body: bool,
) -> http::Response<Vec<u8>> {
    let pool = app.state::<db::Pool>();
    let conn = match pool.get() {
        Ok(conn) => conn,
        Err(error) => {
            return text_response(http::StatusCode::INTERNAL_SERVER_ERROR, &error.to_string())
        }
    };
    let detail = match repo::images_repo::get_detail(&conn, image_id) {
        Ok(Some(detail)) if detail.deleted_at.is_none() => detail,
        Ok(_) => return text_response(http::StatusCode::NOT_FOUND, "image is not available"),
        Err(error) => {
            return text_response(http::StatusCode::INTERNAL_SERVER_ERROR, &error.to_string())
        }
    };
    let path = PathBuf::from(&detail.full_path);
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return text_response(http::StatusCode::NOT_FOUND, "image path is not a file"),
        Err(_) => return text_response(http::StatusCode::NOT_FOUND, "image file missing"),
    };
    let bytes = if include_body {
        match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => return text_response(http::StatusCode::NOT_FOUND, &error.to_string()),
        }
    } else {
        Vec::new()
    };

    http::Response::builder()
        .status(http::StatusCode::OK)
        .header(header::CONTENT_TYPE, image_content_type(&detail.extension))
        .header(header::CONTENT_LENGTH, metadata.len().to_string())
        .header(header::CACHE_CONTROL, "max-age=60")
        .body(bytes)
        .unwrap_or_else(|_| http::Response::new(Vec::new()))
}

fn image_id_from_request(request: &http::Request<Vec<u8>>) -> Option<i64> {
    request
        .uri()
        .path()
        .trim_matches('/')
        .parse::<i64>()
        .ok()
        .or_else(|| {
            request
                .uri()
                .host()
                .and_then(|host| host.parse::<i64>().ok())
        })
}

fn image_content_type(extension: &str) -> &'static str {
    match extension
        .trim_start_matches('.')
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "heif" => "image/heif",
        _ => "application/octet-stream",
    }
}

fn text_response(status: http::StatusCode, text: &str) -> http::Response<Vec<u8>> {
    http::Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(text.as_bytes().to_vec())
        .unwrap_or_else(|_| http::Response::new(Vec::new()))
}

fn init_tracing(profile: config::Profile) {
    // 依赖（尤其是 LanceDB）在 info 级别极其啰嗦：每次 embedding upsert 会刷十几行
    // lance::* 事件，写满日志文件、也让 dev 下同步写 stdout 的 fmt layer 互相抢锁。
    // 默认只放行本项目 crate 的 info，其余依赖压到 warn；需要排障时用 PVP_LOG 覆盖。
    let filter = EnvFilter::try_from_env("PVP_LOG").unwrap_or_else(|_| {
        EnvFilter::new("warn,photo_view_plus_lib=info,photo_view_plus=info")
    });
    let stdout_layer = tracing_subscriber::fmt::layer();

    if let Some(log_dir) = log_dir_for(profile) {
        if let Err(error) = std::fs::create_dir_all(&log_dir) {
            eprintln!("failed to create log dir {}: {error}", log_dir.display());
        } else {
            let file_appender = tracing_appender::rolling::daily(&log_dir, "photo-view-plus.log");
            let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
            let _ = LOG_GUARD.set(guard);
            let file_layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer);
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(stdout_layer)
                .with(file_layer)
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer)
        .try_init();
}

fn log_dir_for(profile: config::Profile) -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    let identifier = match profile {
        config::Profile::Dev => "com.vetoer.photoviewplus.dev",
        config::Profile::Test => "com.vetoer.photoviewplus.test",
        config::Profile::Prod => "com.vetoer.photoviewplus",
    };
    Some(base.join(identifier).join("logs"))
}
