use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

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
use utils::bk_tree::DhashIndex;

static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

pub fn run() {
    let profile = config::Profile::from_env();
    init_tracing(profile);
    tracing::info!(?profile, "PhotoView+ starting");

    tauri::Builder::default()
        .register_uri_scheme_protocol("thumb", thumb_protocol)
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .manage(profile)
        .setup(|app| {
            let data_dir = app.path().app_local_data_dir()?;
            let paths = config::AppPaths::from_data_dir(data_dir);
            std::fs::create_dir_all(&paths.data_dir)?;
            std::fs::create_dir_all(&paths.thumbs_dir)?;

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
            let scheduler = queue::Scheduler::start_with_caps(16, task_caps);
            let queue_app = app.handle().clone();
            scheduler.spawn_status_loop(move |status| {
                let _ = queue_app.emit("queue:status", status);
            });

            // MVP2 状态：BK-tree（视觉哈希索引） + dedup 协调器
            let dhash_index = Arc::new(DhashIndex::new());
            match pool.get() {
                Ok(conn) => match repo::images_repo::all_dhashes(&conn) {
                    Ok(entries) => {
                        let count = entries.len();
                        dhash_index.rebuild_from(entries);
                        tracing::info!(loaded = count, "dhash BK-tree loaded");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to load dhash BK-tree on startup");
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "failed to get conn for BK-tree init");
                }
            }
            let dedup_coord = Arc::new(DedupCoordinator::new());

            // 重排上次未完成的缩略图：扫描中断会留下 thumb_status='pending' 的图，
            // 而重复扫描不会再触碰未改动文件，必须在这里主动恢复，否则它们永远停在"生成中"。
            match services::thumbnail_service::requeue_pending_thumbnails(
                &pool,
                &scheduler,
                &paths.thumbs_dir,
            ) {
                Ok(count) if count > 0 => {
                    tracing::info!(count, "requeued pending thumbnails")
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "failed to requeue pending thumbnails"),
            }

            app.manage(paths);
            app.manage(pool);
            app.manage(scheduler);
            app.manage(dhash_index);
            app.manage(dedup_coord);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::system::ping,
            commands::system::db_status,
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
            commands::dedup::dedup_export_csv,
            commands::trash::trash_history,
            commands::trash::trash_undo,
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|error| eprintln!("error while running tauri application: {error}"));
}

fn thumb_protocol(
    ctx: tauri::UriSchemeContext<'_, tauri::Wry>,
    request: http::Request<Vec<u8>>,
) -> http::Response<Vec<u8>> {
    let image_id = request
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
        });

    let Some(image_id) = image_id else {
        return text_response(http::StatusCode::BAD_REQUEST, "invalid thumbnail image id");
    };

    let pool = ctx.app_handle().state::<db::Pool>();
    let paths = ctx.app_handle().state::<config::AppPaths>();
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

fn text_response(status: http::StatusCode, text: &str) -> http::Response<Vec<u8>> {
    http::Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(text.as_bytes().to_vec())
        .unwrap_or_else(|_| http::Response::new(Vec::new()))
}

fn init_tracing(profile: config::Profile) {
    let filter = EnvFilter::try_from_env("PVP_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
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
