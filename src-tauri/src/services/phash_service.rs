//! 视觉哈希服务（默认 dHash / Gradient）。从缩略图缓存读 256px WebP，算 64-bit hash 写入 `images.dhash`。
//!
//! 设计要点（详见 `docs/04` § 2 T3 + ADR-005）：
//! - 用 `image_hasher` crate；默认参数与 czkawka 对齐（dHash, hash_size=8, Lanczos3, 不启用 DCT）
//! - **不**重解原图，从 `thumbs/<ab>/<hash>.webp` 读——快 10×
//! - 完成后塞进共享的 `DhashIndex`（BK-tree），dedup_service 增量查询用
//! - 优先级 P3，依赖 thumb_status='ready'

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use image_hasher::{FilterType, HashAlg, HasherConfig};

use crate::db::Pool;
use crate::error::{AppError, AppResult};
use crate::queue::{Priority, Task, TaskContext};
use crate::repo::images_repo;
use crate::services::thumbnail_service;
use crate::utils::bk_tree::DhashIndex;

#[derive(Clone)]
pub struct DhashTask {
    pool: Pool,
    image_id: i64,
    thumbs_dir: PathBuf,
    thumb_hash: String,
    index: Arc<DhashIndex>,
}

impl DhashTask {
    pub fn new(
        pool: Pool,
        image_id: i64,
        thumbs_dir: PathBuf,
        thumb_hash: String,
        index: Arc<DhashIndex>,
    ) -> Self {
        Self {
            pool,
            image_id,
            thumbs_dir,
            thumb_hash,
            index,
        }
    }
}

#[async_trait]
impl Task for DhashTask {
    fn priority(&self) -> Priority {
        Priority::P3
    }

    fn label(&self) -> String {
        format!("dhash:{}", self.image_id)
    }

    async fn run(&self, ctx: TaskContext) -> AppResult<()> {
        if ctx.is_cancelled() {
            return Ok(());
        }
        let task = self.clone();
        tokio::task::spawn_blocking(move || compute(task)).await?
    }
}

fn compute(task: DhashTask) -> AppResult<()> {
    let thumb_path = thumbnail_service::thumb_path(&task.thumbs_dir, &task.thumb_hash);
    let dhash = match dhash_from_thumb(&thumb_path) {
        Ok(value) => value,
        Err(error) => {
            // 不写失败状态：下次去重会自动重试（多为缩略图暂时缺失/损坏）。不连累 blake3。
            tracing::warn!(image_id = task.image_id, path = ?thumb_path, %error, "dhash failed");
            return Ok(());
        }
    };

    let conn = task.pool.get()?;
    images_repo::set_dhash(&conn, task.image_id, dhash)?;
    task.index.insert(task.image_id, dhash);
    Ok(())
}

/// 把缩略图 WebP 解码 → image_hasher dHash → u64。
/// hash_size=8 ⇒ as_bytes() 是 8 字节，按 little-endian 拼成 u64。
pub fn dhash_from_thumb(thumb_path: &std::path::Path) -> AppResult<u64> {
    let img = image::open(thumb_path)?;
    let hasher = HasherConfig::new()
        .hash_alg(HashAlg::Gradient)
        .hash_size(8, 8)
        .resize_filter(FilterType::Lanczos3)
        .to_hasher();
    let bytes = hasher.hash_image(&img).as_bytes().to_vec();
    let arr: [u8; 8] = bytes.as_slice().try_into().map_err(|_| {
        AppError::Other(format!(
            "image_hasher 返回 {} 字节，期望 8 字节",
            bytes.len()
        ))
    })?;
    Ok(u64::from_le_bytes(arr))
}

/// 64-bit hamming 距离。Rust 标准库内联到 `popcnt` 指令。
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};

    fn write_solid_thumb(path: &std::path::Path, r: u8, g: u8, b: u8) {
        let mut img = RgbImage::new(64, 64);
        for px in img.pixels_mut() {
            *px = image::Rgb([r, g, b]);
        }
        DynamicImage::ImageRgb8(img).save(path).expect("save");
    }

    #[test]
    fn dhash_identical_images_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.png");
        let b = dir.path().join("b.png");
        write_solid_thumb(&a, 120, 120, 120);
        write_solid_thumb(&b, 120, 120, 120);
        let ha = dhash_from_thumb(&a).expect("hash a");
        let hb = dhash_from_thumb(&b).expect("hash b");
        assert_eq!(ha, hb);
        assert_eq!(hamming(ha, hb), 0);
    }

    #[test]
    fn dhash_different_solids_have_low_distance_for_solid() {
        // 纯色图 dHash 都是 0（没有梯度），所以两张纯色 hamming=0。
        // 这条不要太强；主要确保 API 不 panic。
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.png");
        let b = dir.path().join("b.png");
        write_solid_thumb(&a, 0, 0, 0);
        write_solid_thumb(&b, 255, 255, 255);
        let ha = dhash_from_thumb(&a).expect("hash a");
        let hb = dhash_from_thumb(&b).expect("hash b");
        // hamming ≤ 16 即可（双纯色应该很近）
        assert!(hamming(ha, hb) <= 16, "hamming = {}", hamming(ha, hb));
    }
}
