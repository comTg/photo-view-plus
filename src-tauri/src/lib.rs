use tauri::Manager;
use tracing_subscriber::EnvFilter;

mod commands;
mod config;
pub mod db;
pub mod error;
pub mod migrations;

pub use error::{AppError, AppResult};

pub fn run() {
    init_tracing();
    let profile = config::Profile::from_env();
    tracing::info!(?profile, "PhotoView+ starting");

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .manage(profile)
        .setup(|app| {
            let db_path = resolve_db_path(app)?;
            tracing::info!(path = ?db_path, "opening database");
            let pool = db::open(&db_path).map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
            app.manage(pool);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::ping,
            commands::db_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn resolve_db_path(app: &tauri::App) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    // Tauri 在每个平台返回 platform-appropriate app local data dir:
    //   Windows: %LOCALAPPDATA%\<identifier>
    //   macOS:   ~/Library/Application Support/<identifier>
    //   Linux:   ~/.local/share/<identifier>
    // identifier 已带 profile 后缀（com.vetoer.photoviewplus[.dev|.test]），不必再分目录。
    let base = app.path().app_local_data_dir()?;
    Ok(base.join("db").join("app.sqlite"))
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("PVP_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
