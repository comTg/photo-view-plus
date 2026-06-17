use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use image::codecs::webp::WebPEncoder;
use image::{DynamicImage, ImageEncoder};
use sha1::{Digest, Sha1};
use tokio::sync::Semaphore;

use crate::db::Pool;
use crate::error::{AppError, AppResult};
use crate::queue::{Priority, Scheduler, Task, TaskContext};
use crate::repo::images_repo;
use crate::utils::read_gate;

const THUMB_SIZE: u32 = 256;
const MAX_THUMBNAIL_CPU_WORKERS: usize = 6;

static THUMBNAIL_CPU_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

#[derive(Clone)]
pub struct ThumbnailTask {
    pool: Pool,
    image_id: i64,
    root_id: i64,
    rel_path: String,
    full_path: PathBuf,
    orientation: Option<i64>,
    thumbs_dir: PathBuf,
    is_network: bool,
}

impl ThumbnailTask {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: Pool,
        image_id: i64,
        root_id: i64,
        rel_path: String,
        full_path: PathBuf,
        orientation: Option<i64>,
        thumbs_dir: PathBuf,
        is_network: bool,
    ) -> Self {
        Self {
            pool,
            image_id,
            root_id,
            rel_path,
            full_path,
            orientation,
            thumbs_dir,
            is_network,
        }
    }
}

#[async_trait]
impl Task for ThumbnailTask {
    fn priority(&self) -> Priority {
        Priority::P0
    }

    fn label(&self) -> String {
        format!("thumb:{}", self.image_id)
    }

    async fn run(&self, ctx: TaskContext) -> AppResult<()> {
        if ctx.is_cancelled() {
            return Ok(());
        }

        let semaphore = thumbnail_cpu_semaphore();
        let cancellation = ctx.cancellation_token();
        let permit = tokio::select! {
            permit = semaphore.acquire_owned() => {
                permit.map_err(|_| AppError::Other("缩略图 CPU 限流器已关闭".to_string()))?
            }
            _ = cancellation.cancelled() => return Ok(()),
        };
        // 读原图前再过一道原图读取闸门：网络盘并发读过高会拖死 NAS（红线 6）。
        // 与 BLAKE3 共用同一组闸门，约束的是"读原图"的总并发。
        let read_permit = tokio::select! {
            permit = read_gate::acquire_read(self.is_network) => permit?,
            _ = cancellation.cancelled() => return Ok(()),
        };
        let task = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _read_permit = read_permit;
            generate(task, ctx)
        })
        .await?
    }
}

pub fn thumbnail_cpu_limit() -> usize {
    let logical_cpus = std::thread::available_parallelism()
        .map(|cpus| cpus.get())
        .unwrap_or(4);
    thumbnail_cpu_limit_for(logical_cpus)
}

fn thumbnail_cpu_limit_for(logical_cpus: usize) -> usize {
    match logical_cpus {
        0..=2 => 1,
        _ => (logical_cpus / 2).clamp(2, MAX_THUMBNAIL_CPU_WORKERS),
    }
}

fn thumbnail_cpu_semaphore() -> Arc<Semaphore> {
    THUMBNAIL_CPU_SEMAPHORE
        .get_or_init(|| {
            let limit = thumbnail_cpu_limit();
            tracing::info!(limit, "thumbnail CPU worker limit configured");
            Arc::new(Semaphore::new(limit))
        })
        .clone()
}

/// 启动时把仍处于 pending 的缩略图重新入队（P0）。覆盖"扫描被中断后缩略图卡在生成中"
/// 的场景——这些图不会被随后的重复扫描重新触发（未改动文件会被跳过）。
pub fn requeue_pending_thumbnails(
    pool: &Pool,
    scheduler: &Scheduler,
    thumbs_dir: &Path,
) -> AppResult<usize> {
    let conn = pool.get()?;
    let pending = images_repo::pending_thumbnail_images(&conn)?;
    drop(conn);

    let mut count = 0usize;
    for img in pending {
        let is_network = crate::utils::path_normalize::is_network_path(Path::new(&img.root_path));
        scheduler.enqueue(ThumbnailTask::new(
            pool.clone(),
            img.id,
            img.root_id,
            img.rel_path,
            PathBuf::from(img.full_path),
            img.orientation,
            thumbs_dir.to_path_buf(),
            is_network,
        ))?;
        count += 1;
    }
    if count > 0 {
        tracing::info!(count, "requeued pending thumbnails on startup");
    }
    Ok(count)
}

pub fn thumb_hash_for(root_id: i64, rel_path: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(root_id.to_le_bytes());
    hasher.update(b"|");
    hasher.update(rel_path.as_bytes());
    let digest = hasher.finalize();
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn thumb_path(thumbs_dir: &Path, thumb_hash: &str) -> PathBuf {
    let prefix = thumb_hash.get(0..2).unwrap_or("00");
    thumbs_dir.join(prefix).join(format!("{thumb_hash}.webp"))
}

fn generate(task: ThumbnailTask, ctx: TaskContext) -> AppResult<()> {
    if ctx.is_cancelled() {
        return Ok(());
    }

    let hash = thumb_hash_for(task.root_id, &task.rel_path);
    let path = thumb_path(&task.thumbs_dir, &hash);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let image = match image::open(&task.full_path) {
        Ok(image) => image,
        Err(error) => {
            let status = if matches!(error, image::ImageError::Unsupported(_)) {
                "unsupported"
            } else {
                "failed"
            };
            let conn = task.pool.get()?;
            images_repo::update_thumbnail_failed(&conn, task.image_id, status, &error.to_string())?;
            return Ok(());
        }
    };

    if ctx.is_cancelled() {
        return Ok(());
    }

    let oriented = apply_orientation(image, task.orientation);
    let width = i64::from(oriented.width());
    let height = i64::from(oriented.height());
    let thumb = oriented.thumbnail(THUMB_SIZE, THUMB_SIZE).to_rgba8();
    let mut writer = BufWriter::new(File::create(&path)?);
    let encoder = WebPEncoder::new_lossless(&mut writer);
    encoder.write_image(
        thumb.as_raw(),
        thumb.width(),
        thumb.height(),
        image::ColorType::Rgba8.into(),
    )?;

    let conn = task.pool.get()?;
    images_repo::update_thumbnail_ready(&conn, task.image_id, &hash, Some(width), Some(height))?;
    Ok(())
}

fn apply_orientation(image: DynamicImage, orientation: Option<i64>) -> DynamicImage {
    match orientation.unwrap_or(1) {
        3 => image.rotate180(),
        6 => image.rotate90(),
        8 => image.rotate270(),
        2 => image.fliph(),
        4 => image.flipv(),
        _ => image,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_short() {
        let a = thumb_hash_for(1, "a.jpg");
        let b = thumb_hash_for(1, "a.jpg");
        let c = thumb_hash_for(2, "a.jpg");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn thumb_path_uses_two_char_prefix() {
        let path = thumb_path(Path::new("thumbs"), "abcdef0123456789");
        assert!(path.ends_with(Path::new("ab").join("abcdef0123456789.webp")));
    }

    #[test]
    fn thumbnail_cpu_limit_scales_with_available_parallelism() {
        assert_eq!(thumbnail_cpu_limit_for(1), 1);
        assert_eq!(thumbnail_cpu_limit_for(2), 1);
        assert_eq!(thumbnail_cpu_limit_for(4), 2);
        assert_eq!(thumbnail_cpu_limit_for(8), 4);
        assert_eq!(thumbnail_cpu_limit_for(16), MAX_THUMBNAIL_CPU_WORKERS);
        assert_eq!(thumbnail_cpu_limit_for(64), MAX_THUMBNAIL_CPU_WORKERS);
    }
}
