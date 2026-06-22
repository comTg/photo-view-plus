use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;

use crate::config::AppPaths;
use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupExportResult {
    pub path: String,
    pub files: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupImportResult {
    pub restored_dir: String,
    pub files: usize,
    pub bytes: u64,
}

pub fn export_backup(
    paths: &AppPaths,
    destination: &Path,
    include_thumbs: bool,
    include_models: bool,
) -> AppResult<BackupExportResult> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(destination)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut files = 0usize;
    let mut bytes = 0u64;

    add_file_if_exists(
        &mut zip,
        &paths.db_path,
        "db/app.sqlite",
        options,
        &mut files,
        &mut bytes,
    )?;
    add_file_if_exists(
        &mut zip,
        &paths.db_path.with_extension("sqlite-wal"),
        "db/app.sqlite-wal",
        options,
        &mut files,
        &mut bytes,
    )?;
    add_file_if_exists(
        &mut zip,
        &paths.db_path.with_extension("sqlite-shm"),
        "db/app.sqlite-shm",
        options,
        &mut files,
        &mut bytes,
    )?;
    add_file_if_exists(
        &mut zip,
        &paths.config_path,
        "config.json",
        options,
        &mut files,
        &mut bytes,
    )?;
    add_dir(
        &mut zip,
        &paths.vectors_dir,
        "vectors",
        options,
        &mut files,
        &mut bytes,
    )?;
    if include_thumbs {
        add_dir(
            &mut zip,
            &paths.thumbs_dir,
            "thumbs",
            options,
            &mut files,
            &mut bytes,
        )?;
    }
    if include_models {
        add_dir(
            &mut zip,
            &paths.models_dir,
            "models",
            options,
            &mut files,
            &mut bytes,
        )?;
    }
    zip.finish()
        .map_err(|error| AppError::Other(format!("zip: {error}")))?;
    Ok(BackupExportResult {
        path: destination.to_string_lossy().to_string(),
        files,
        bytes,
    })
}

pub fn import_backup(paths: &AppPaths, source: &Path) -> AppResult<BackupImportResult> {
    let file = File::open(source)?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|error| AppError::Other(format!("zip: {error}")))?;
    let restore_dir = paths
        .data_dir
        .join("restore-staging")
        .join(now_unix().to_string());
    fs::create_dir_all(&restore_dir)?;
    let mut files = 0usize;
    let mut bytes = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| AppError::Other(format!("zip: {error}")))?;
        let out_path = restore_dir.join(entry.mangled_name());
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&out_path)?;
        let copied = std::io::copy(&mut entry, &mut out)?;
        files += 1;
        bytes = bytes.saturating_add(copied);
    }
    Ok(BackupImportResult {
        restored_dir: restore_dir.to_string_lossy().to_string(),
        files,
        bytes,
    })
}

fn add_dir(
    zip: &mut zip::ZipWriter<File>,
    dir: &Path,
    prefix: &str,
    options: SimpleFileOptions,
    files: &mut usize,
    bytes: &mut u64,
) -> AppResult<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(dir).follow_links(false).into_iter() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(%error, "backup walkdir entry failed");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(dir)
            .map_err(|error| AppError::Other(error.to_string()))?;
        let zip_name = normalize_zip_path(Path::new(prefix).join(relative));
        add_file(zip, entry.path(), &zip_name, options, files, bytes)?;
    }
    Ok(())
}

fn add_file_if_exists(
    zip: &mut zip::ZipWriter<File>,
    path: &Path,
    zip_name: &str,
    options: SimpleFileOptions,
    files: &mut usize,
    bytes: &mut u64,
) -> AppResult<()> {
    if path.exists() {
        add_file(zip, path, zip_name, options, files, bytes)?;
    }
    Ok(())
}

fn add_file(
    zip: &mut zip::ZipWriter<File>,
    path: &Path,
    zip_name: &str,
    options: SimpleFileOptions,
    files: &mut usize,
    bytes: &mut u64,
) -> AppResult<()> {
    zip.start_file(zip_name, options)
        .map_err(|error| AppError::Other(format!("zip: {error}")))?;
    let mut input = File::open(path)?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        zip.write_all(&buffer[..read])?;
        *bytes = bytes.saturating_add(read as u64);
    }
    *files += 1;
    Ok(())
}

fn normalize_zip_path(path: PathBuf) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/")
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
