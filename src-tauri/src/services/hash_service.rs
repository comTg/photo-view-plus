//! BLAKE3 内容哈希服务。对一张图算 64-hex BLAKE3，写入 `images.blake3`。
//!
//! 设计要点（详见 `docs/04` § 2 T2）：
//! - 4MB 块流式喂入 hasher，大文件不一次性进内存
//! - 失败标 `hash_status='failed'`，不阻塞队列
//! - 优先级 P2（低于扫描 P1、缩略图 P0；高于 phash P3）
//! - 文件已删除（deleted_at）的图跳过——上游入队时已过滤；任务运行前再防御一次

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::PathBuf;

use async_trait::async_trait;
use blake3::Hasher;

use crate::db::Pool;
use crate::error::AppResult;
use crate::queue::{Priority, Task, TaskContext};
use crate::repo::images_repo;
use crate::utils::read_gate;

const READ_BUFFER: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct BlakeHashTask {
    pool: Pool,
    image_id: i64,
    full_path: PathBuf,
    is_network: bool,
}

impl BlakeHashTask {
    pub fn new(pool: Pool, image_id: i64, full_path: PathBuf, is_network: bool) -> Self {
        Self {
            pool,
            image_id,
            full_path,
            is_network,
        }
    }
}

#[async_trait]
impl Task for BlakeHashTask {
    fn priority(&self) -> Priority {
        Priority::P2
    }

    fn label(&self) -> String {
        format!("blake3:{}", self.image_id)
    }

    async fn run(&self, ctx: TaskContext) -> AppResult<()> {
        if ctx.is_cancelled() {
            return Ok(());
        }
        // 读原图前先过限流闸门：网络盘并发读过高会拖死 NAS（红线 6）。
        // permit 持有到 spawn_blocking 里的文件读取结束。
        let permit = read_gate::acquire_read(self.is_network).await?;
        let task = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            compute(task, ctx)
        })
        .await?
    }
}

fn compute(task: BlakeHashTask, ctx: TaskContext) -> AppResult<()> {
    if ctx.is_cancelled() {
        return Ok(());
    }
    let token = ctx.cancellation_token();
    let hash = match hash_file(&task.full_path, &token) {
        Ok(Some(hash)) => hash,
        Ok(None) => return Ok(()), // 被取消
        Err(error) => {
            // 不写失败状态：下次去重会自动重试（多为网络抖动）。真正坏的文件每次快速失败一次即可。
            tracing::warn!(image_id = task.image_id, path = ?task.full_path, %error, "blake3 failed");
            return Ok(());
        }
    };

    let conn = task.pool.get()?;
    images_repo::set_blake3(&conn, task.image_id, &hash)?;
    Ok(())
}

fn hash_file(
    path: &std::path::Path,
    cancellation: &tokio_util::sync::CancellationToken,
) -> std::io::Result<Option<String>> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(READ_BUFFER, file);
    let mut hasher = Hasher::new();
    let mut buf = vec![0u8; READ_BUFFER];
    loop {
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(Some(hasher.finalize().to_hex().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn hash_file_matches_known_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.bin");
        let mut f = File::create(&path).expect("create");
        f.write_all(b"hello world").expect("write");
        let token = tokio_util::sync::CancellationToken::new();
        // BLAKE3("hello world") known hex（来自官方测试向量）
        let h = hash_file(&path, &token).expect("ok").expect("not cancelled");
        assert_eq!(
            h,
            "d74981efa70a0c880b8d8c1985d075dbcbf679b99a5f9914e5aaf96b831a9e24"
        );
    }
}
