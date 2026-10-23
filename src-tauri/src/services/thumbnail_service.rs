use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ExtendedColorType, ImageEncoder, Rgb, RgbImage};
use sha1::{Digest, Sha1};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::db::Pool;
use crate::error::{AppError, AppResult};
use crate::queue::{Priority, Scheduler, Task, TaskContext};
use crate::repo::images_repo::{self, ImageRecord};
use crate::services::phash_service;
use crate::utils::bk_tree::DhashIndex;
use crate::utils::limiter::AdjustableLimiter;
use crate::utils::priority::BackgroundPriorityGuard;
use crate::utils::read_gate;

const THUMB_SIZE: u32 = 256;
const MAX_THUMBNAIL_CPU_WORKERS: usize = 6;
/// 可见区域队列的固定并发（docs/11 §5.2：全屏 / 游戏时由治理器降到 1）。
pub const VISIBLE_THUMBNAIL_CONCURRENCY: usize = 2;
/// 一次最多接受多少张可见区域的图（一屏加上下预加载区）。
const VISIBLE_REQUEST_LIMIT: usize = 160;
/// 缩略图完成事件的合批窗口（docs/11 R5）。
pub const EVENT_BATCH_WINDOW: Duration = Duration::from_millis(250);
/// 单张缩略图生成的时间上限（docs/11 R6）。读 NAS 原图或解码一旦卡住，不能让它一直占着并发名额：
/// 使用中档只有 1 个名额，一张卡住整条队列就停了（R6 测量时见过队列停住，根因未确认）。
/// 正常情况下单张远低于这个值（大 PNG 也只要几秒）。
const GENERATE_TIMEOUT: Duration = Duration::from_secs(120);
/// 新生成的缩略图格式（docs/11 R4）：有损 JPEG。
pub const THUMB_EXT: &str = "jpg";
/// R4 之前生成的缩略图是无损 WebP，空闲时由 [`convert_legacy_thumbnails`] 逐批转成 JPEG。
pub const LEGACY_THUMB_EXT: &str = "webp";
/// JPEG 质量：抽样实测体积约为无损 WebP 的 25%，dHash 分组效果不变（docs/11 R4）。
const JPEG_QUALITY: u8 = 85;

static THUMBNAIL_CPU_LIMITER: OnceLock<Arc<AdjustableLimiter>> = OnceLock::new();
static VISIBLE_LIMITER: OnceLock<Arc<AdjustableLimiter>> = OnceLock::new();
static VISIBLE_STATE: OnceLock<Mutex<VisibleState>> = OnceLock::new();
/// 正在生成的缩略图文件（批量队列和可见区域队列共用），同一张图不会同时生成两次。
/// 按输出文件路径记，而不是图片 id：路径同时确定了库（缩略图目录）和图片。
static GENERATING: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// 可见区域队列的状态。
#[derive(Default)]
struct VisibleState {
    /// 前端最近一次报告的可见区域：排队期间用户滚走了的图留给批量队列。
    wanted: HashSet<i64>,
    /// 已在可见队列里（排队或生成中）的图。
    queued: HashSet<i64>,
}

/// 占住"正在生成"的名额，drop 时释放。
struct GeneratingClaim(PathBuf);

impl GeneratingClaim {
    fn try_claim(task: &ThumbnailTask) -> Option<Self> {
        Self::try_claim_path(thumb_path(
            &task.thumbs_dir,
            &thumb_hash_for(task.root_id, &task.rel_path),
            THUMB_EXT,
        ))
    }

    fn try_claim_path(path: PathBuf) -> Option<Self> {
        let mut generating = GENERATING
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .ok()?;
        generating.insert(path.clone()).then_some(Self(path))
    }
}

impl Drop for GeneratingClaim {
    fn drop(&mut self) {
        if let Some(generating) = GENERATING.get() {
            if let Ok(mut generating) = generating.lock() {
                generating.remove(&self.0);
            }
        }
    }
}

/// 缩略图完成事件合批（docs/11 R5）。每张图完成时只记下 id；第一张完成后等 250ms，把这段时间里
/// 完成的图一起读出窄字段（id、状态、hash、宽高、错误），发一个 `thumbnails:updated`。
/// 以前每张图各发一次 `thumbnail:updated`，每次还要回读一整条 30 列的记录。
#[derive(Clone)]
pub struct ThumbnailEvents {
    tx: mpsc::UnboundedSender<i64>,
}

impl ThumbnailEvents {
    pub fn spawn(app: AppHandle, pool: Pool) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tauri::async_runtime::spawn(async move {
            while let Some(ids) = next_event_batch(&mut rx, EVENT_BATCH_WINDOW).await {
                let pool = pool.clone();
                let updates = tokio::task::spawn_blocking(move || -> AppResult<_> {
                    let conn = pool.get()?;
                    images_repo::thumbnail_updates(&conn, &ids)
                })
                .await;
                match updates {
                    Ok(Ok(updates)) if !updates.is_empty() => {
                        let _ = app.emit("thumbnails:updated", updates);
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => tracing::warn!(%error, "failed to read thumbnail updates"),
                    Err(error) => tracing::warn!(%error, "thumbnail update task join failed"),
                }
            }
        });
        Self { tx }
    }

    /// 记下一张缩略图状态变了（生成完成、失败或被取消），在下一批事件里发出去。
    pub fn notify(&self, image_id: i64) {
        let _ = self.tx.send(image_id);
    }
}

/// 等到第一个 id，再收 `window` 时间内陆续到达的其余 id（去重，保持先后顺序）。通道关闭时返回 `None`。
pub async fn next_event_batch(
    rx: &mut mpsc::UnboundedReceiver<i64>,
    window: Duration,
) -> Option<Vec<i64>> {
    let first = rx.recv().await?;
    let mut ids = vec![first];
    let mut seen = HashSet::from([first]);
    let deadline = tokio::time::sleep(window);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            next = rx.recv() => match next {
                Some(id) => {
                    if seen.insert(id) {
                        ids.push(id);
                    }
                }
                None => break,
            },
        }
    }
    Some(ids)
}

fn visible_state() -> &'static Mutex<VisibleState> {
    VISIBLE_STATE.get_or_init(|| Mutex::new(VisibleState::default()))
}

fn visible_limiter() -> Arc<AdjustableLimiter> {
    VISIBLE_LIMITER
        .get_or_init(|| AdjustableLimiter::new(VISIBLE_THUMBNAIL_CONCURRENCY))
        .clone()
}

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
    completion: Option<mpsc::UnboundedSender<i64>>,
    event_app: Option<AppHandle>,
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
            completion: None,
            event_app: None,
        }
    }

    pub fn with_completion(mut self, completion: mpsc::UnboundedSender<i64>) -> Self {
        self.completion = Some(completion);
        self
    }

    pub fn with_event_app(mut self, app: AppHandle) -> Self {
        self.event_app = Some(app);
        self
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
        let result = self.run_inner(ctx).await;
        if let Err(error) = &result {
            if let Ok(conn) = self.pool.get() {
                let _ = images_repo::update_thumbnail_failed(
                    &conn,
                    self.image_id,
                    "failed",
                    &error.to_string(),
                );
            }
        }
        if let Some(completion) = &self.completion {
            let _ = completion.send(self.image_id);
        }
        self.emit_updated();
        result
    }
}

impl ThumbnailTask {
    fn emit_updated(&self) {
        let Some(app) = &self.event_app else {
            return;
        };
        if let Some(events) = app.try_state::<ThumbnailEvents>() {
            events.notify(self.image_id);
        }
    }

    async fn run_inner(&self, ctx: TaskContext) -> AppResult<()> {
        let limiter = thumbnail_cpu_limiter();
        let cancellation = ctx.cancellation_token();
        let permit = tokio::select! {
            permit = limiter.acquire() => permit,
            _ = cancellation.cancelled() => return Ok(()),
        };
        // 可见区域队列可能已经生成了这张图（或正在生成）：跳过，不重复读原图。
        let Some(claim) = GeneratingClaim::try_claim(self) else {
            return Ok(());
        };
        if !self.still_pending()? {
            return Ok(());
        }
        // 读原图前再过一道原图读取闸门：网络盘并发读过高会拖死 NAS（红线 6）。
        // 与 BLAKE3 共用同一组闸门，约束的是"读原图"的总并发。
        let read_permit = tokio::select! {
            permit = read_gate::acquire_read(self.is_network) => permit?,
            _ = cancellation.cancelled() => return Ok(()),
        };
        // 许可和"正在生成"标记留在这里，不交给阻塞线程：生成超时后照常归还。
        let _held = (permit, read_permit, claim);
        run_generate(self.clone(), cancellation, true).await
    }

    fn still_pending(&self) -> AppResult<bool> {
        let conn = self.pool.get()?;
        Ok(images_repo::thumb_status(&conn, self.image_id)?.as_deref() == Some("pending"))
    }

    /// 可见区域队列里的一张图：固定并发，不占批量名额，以正常优先级运行（用户正在等）。
    async fn run_visible(self, require_wanted: bool) {
        let image_id = self.image_id;
        let result = self.run_visible_inner(require_wanted).await;
        if let Ok(mut state) = visible_state().lock() {
            state.queued.remove(&image_id);
        }
        let generated = match result {
            Ok(generated) => generated,
            Err(error) => {
                if let Ok(conn) = self.pool.get() {
                    let _ = images_repo::update_thumbnail_failed(
                        &conn,
                        image_id,
                        "failed",
                        &error.to_string(),
                    );
                }
                true
            }
        };
        if let Some(completion) = &self.completion {
            let _ = completion.send(image_id);
        }
        if generated {
            self.emit_updated();
        }
    }

    /// 返回是否真的生成了（排队期间滚出可见区域、或已被批量队列处理时为 false）。
    async fn run_visible_inner(&self, require_wanted: bool) -> AppResult<bool> {
        let permit = visible_limiter().acquire().await;
        if require_wanted {
            let wanted = visible_state()
                .lock()
                .map(|state| state.wanted.contains(&self.image_id))
                .unwrap_or(false);
            if !wanted {
                return Ok(false);
            }
        }
        let Some(claim) = GeneratingClaim::try_claim(self) else {
            return Ok(false);
        };
        if !self.still_pending()? {
            return Ok(false);
        }
        let read_permit = read_gate::acquire_read(self.is_network).await?;
        let _held = (permit, read_permit, claim);
        run_generate(self.clone(), CancellationToken::new(), false).await?;
        Ok(true)
    }
}

/// 可见区域优先（docs/11 §5.4、R3）：前端报告当前可见范围里还没有缩略图的图，这里用独立的
/// 固定并发生成，不占批量名额，也不受"使用中"的批量限速影响。返回新排进可见队列的张数。
///
/// 每次调用都会替换"可见区域"：之前排队、但已经滚出可见区域的图不再生成，留给批量队列。
pub fn prioritize_visible(
    pool: &Pool,
    thumbs_dir: &Path,
    app: Option<&AppHandle>,
    image_ids: &[i64],
) -> AppResult<usize> {
    let ids = image_ids
        .iter()
        .copied()
        .take(VISIBLE_REQUEST_LIMIT)
        .collect::<Vec<_>>();
    let pending = {
        let conn = pool.get()?;
        images_repo::pending_thumbnail_images_by_ids(&conn, &ids)?
    };
    let fresh = {
        let mut state = visible_state()
            .lock()
            .map_err(|_| AppError::Other("可见区域缩略图状态不可用".to_string()))?;
        state.wanted = ids.iter().copied().collect();
        pending
            .into_iter()
            .filter(|image| state.queued.insert(image.id))
            .collect::<Vec<_>>()
    };
    let count = fresh.len();
    for image in fresh {
        let is_network = crate::utils::path_normalize::is_network_path(Path::new(&image.root_path));
        let mut task = ThumbnailTask::new(
            pool.clone(),
            image.id,
            image.root_id,
            image.rel_path,
            PathBuf::from(image.full_path),
            image.orientation,
            thumbs_dir.to_path_buf(),
            is_network,
        );
        if let Some(app) = app {
            task = task.with_event_app(app.clone());
        }
        tauri::async_runtime::spawn(task.run_visible(true));
    }
    Ok(count)
}

/// 资源治理器调整可见区域队列的并发（全屏 / 游戏时 1，其他 2）。
pub fn set_visible_limit(limit: usize) {
    visible_limiter().set_limit(limit.clamp(1, VISIBLE_THUMBNAIL_CONCURRENCY));
}

/// 用户手动"重新生成缩略图"：走可见区域队列（用户正在等，不排在批量任务后面，也不受暂停影响）。
pub fn enqueue_thumbnail_regeneration(
    pool: &Pool,
    thumbs_dir: &Path,
    image_id: i64,
    completion: mpsc::UnboundedSender<i64>,
) -> AppResult<images_repo::ImageRecord> {
    let conn = pool.get()?;
    let mut image = images_repo::get_detail(&conn, image_id)?
        .ok_or_else(|| AppError::Other(format!("图片不存在：{image_id}")))?;
    images_repo::update_thumbnail_pending(&conn, image_id)?;
    drop(conn);

    let is_network = crate::utils::path_normalize::is_network_path(Path::new(&image.root_path));
    let task = ThumbnailTask::new(
        pool.clone(),
        image.id,
        image.root_id,
        image.rel_path.clone(),
        PathBuf::from(&image.full_path),
        image.orientation,
        thumbs_dir.to_path_buf(),
        is_network,
    )
    .with_completion(completion);
    if let Ok(mut state) = visible_state().lock() {
        state.queued.insert(image_id);
    }
    tauri::async_runtime::spawn(task.run_visible(false));

    image.thumb_status = "pending".to_string();
    image.thumb_hash = None;
    image.thumb_error = None;
    Ok(image)
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

fn thumbnail_cpu_limiter() -> Arc<AdjustableLimiter> {
    THUMBNAIL_CPU_LIMITER
        .get_or_init(|| {
            let limit = thumbnail_cpu_limit();
            tracing::info!(limit, "thumbnail CPU worker limit configured");
            AdjustableLimiter::new(limit)
        })
        .clone()
}

/// 资源治理器按负载模式调整批量缩略图并发（docs/11 §5.2），不超过机器的 CPU 上限。
/// 0 = 全屏 / 暂停时批量生成全部等待（可见区域队列不受影响）。
pub fn set_batch_limit(limit: usize) {
    thumbnail_cpu_limiter().set_limit(limit.min(thumbnail_cpu_limit()));
}

/// 启动时把仍处于 pending 的缩略图重新入队（P0）。覆盖"扫描被中断后缩略图卡在生成中"
/// 的场景——这些图不会被随后的重复扫描重新触发（未改动文件会被跳过）。
pub fn requeue_pending_thumbnails(
    pool: &Pool,
    scheduler: &Scheduler,
    thumbs_dir: &Path,
    app: &AppHandle,
) -> AppResult<usize> {
    let conn = pool.get()?;
    let pending = images_repo::pending_thumbnail_images(&conn)?;
    drop(conn);

    let mut count = 0usize;
    for img in pending {
        let is_network = crate::utils::path_normalize::is_network_path(Path::new(&img.root_path));
        scheduler.enqueue(
            ThumbnailTask::new(
                pool.clone(),
                img.id,
                img.root_id,
                img.rel_path,
                PathBuf::from(img.full_path),
                img.orientation,
                thumbs_dir.to_path_buf(),
                is_network,
            )
            .with_event_app(app.clone()),
        )?;
        count += 1;
    }
    if count > 0 {
        tracing::info!(count, "requeued pending thumbnails on startup");
    }
    Ok(count)
}

/// 一批旧缩略图转换的结果（docs/11 R4）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LegacyConversion {
    /// 转成 JPEG 的张数。
    pub converted: usize,
    /// 旧文件丢失或损坏、标回待生成的张数（由可见区域队列或下次启动的重排重新生成）。
    pub regenerate: usize,
    /// 正在被生成任务处理、或转换期间被重扫重刷过的张数，这批跳过。
    pub skipped: usize,
}

impl LegacyConversion {
    pub fn processed(&self) -> usize {
        self.converted + self.regenerate + self.skipped
    }
}

/// 把一批 R4 之前的无损 WebP 缩略图转成 JPEG（docs/11 R4）。只读本地缓存，不读原图；转完删掉旧文件。
/// 已有 dHash 的图顺带用新缩略图重算，全库的 dHash 都出自同一种缩略图，视觉去重前后可比。
pub fn convert_legacy_thumbnails(
    pool: &Pool,
    thumbs_dir: &Path,
    dhash_index: Option<&DhashIndex>,
    limit: usize,
) -> AppResult<LegacyConversion> {
    let legacy = {
        let conn = pool.get()?;
        images_repo::legacy_webp_thumbnails(&conn, limit)?
    };
    let _bg = BackgroundPriorityGuard::enter();
    let mut outcome = LegacyConversion::default();
    for (image_id, hash, has_dhash) in legacy {
        let target = thumb_path(thumbs_dir, &hash, THUMB_EXT);
        let Some(_claim) = GeneratingClaim::try_claim_path(target.clone()) else {
            outcome.skipped += 1;
            continue;
        };
        let source = thumb_path(thumbs_dir, &hash, LEGACY_THUMB_EXT);
        let image = match image::open(&source) {
            Ok(image) => image,
            Err(error) => {
                tracing::warn!(image_id, path = ?source, %error, "legacy thumbnail unreadable; regenerating");
                let conn = pool.get()?;
                if images_repo::requeue_legacy_thumbnail(&conn, image_id, &hash)? {
                    outcome.regenerate += 1;
                } else {
                    outcome.skipped += 1;
                }
                continue;
            }
        };
        write_jpeg(&image, &target)?;
        let dhash = if has_dhash {
            Some(phash_service::dhash_from_thumb(&target)?)
        } else {
            None
        };
        let conn = pool.get()?;
        let converted = images_repo::mark_thumbnail_converted(
            &conn,
            image_id,
            &hash,
            THUMB_EXT,
            dhash.map(|value| value as i64),
        )?;
        if !converted {
            outcome.skipped += 1;
            continue;
        }
        if let (Some(index), Some(dhash)) = (dhash_index, dhash) {
            index.insert(image_id, dhash);
        }
        let _ = fs::remove_file(&source);
        outcome.converted += 1;
    }
    Ok(outcome)
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

/// 缩略图文件：`<thumbs>/<hash 前两位>/<hash>.<ext>`，`ext` 见 [`THUMB_EXT`] / [`LEGACY_THUMB_EXT`]。
pub fn thumb_path(thumbs_dir: &Path, thumb_hash: &str, ext: &str) -> PathBuf {
    let prefix = thumb_hash.get(0..2).unwrap_or("00");
    thumbs_dir.join(prefix).join(format!("{thumb_hash}.{ext}"))
}

/// 一条图片记录的缩略图文件；还没有缩略图时为 `None`。
pub fn record_thumb_path(thumbs_dir: &Path, record: &ImageRecord) -> Option<PathBuf> {
    record
        .thumb_hash
        .as_deref()
        .map(|hash| thumb_path(thumbs_dir, hash, &record.thumb_ext))
}

/// 缩略图编码成 JPEG 写到 `path`；带透明通道的（PNG 截图等）先铺到白底上。
fn write_jpeg(thumb: &DynamicImage, path: &Path) -> AppResult<()> {
    let rgb = flatten_on_white(thumb);
    let mut writer = BufWriter::new(File::create(path)?);
    JpegEncoder::new_with_quality(&mut writer, JPEG_QUALITY).write_image(
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        ExtendedColorType::Rgb8,
    )?;
    writer.flush()?;
    Ok(())
}

fn flatten_on_white(image: &DynamicImage) -> RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let alpha = u16::from(a);
        let blend =
            |channel: u8| ((u16::from(channel) * alpha + 255 * (255 - alpha) + 127) / 255) as u8;
        Rgb([blend(r), blend(g), blend(b)])
    })
}

/// 在阻塞线程池里跑 `work`，最多等 `limit`；超时返回 `None`。卡住的线程留在池里，
/// 调用方手上的许可不受它牵连，照常释放。
async fn blocking_with_timeout<T: Send + 'static>(
    limit: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<Result<T, tokio::task::JoinError>> {
    tokio::time::timeout(limit, tokio::task::spawn_blocking(work))
        .await
        .ok()
}

/// 生成一张缩略图，超过 [`GENERATE_TIMEOUT`] 就放弃这一张：标记失败（用户可以重新生成），
/// 让出并发名额，后面的图继续做。
async fn run_generate(
    task: ThumbnailTask,
    cancel: CancellationToken,
    background: bool,
) -> AppResult<()> {
    let image_id = task.image_id;
    let pool = task.pool.clone();
    // 超时后取消这一次尝试：卡住的线程以后醒过来也不再写文件、改状态，免得和重新生成撞车。
    let attempt = cancel.child_token();
    let worker_cancel = attempt.clone();
    match blocking_with_timeout(GENERATE_TIMEOUT, move || {
        generate(task, &worker_cancel, background)
    })
    .await
    {
        Some(joined) => joined?,
        None => {
            attempt.cancel();
            tracing::warn!(
                image_id,
                timeout_secs = GENERATE_TIMEOUT.as_secs(),
                "thumbnail generation timed out; giving up on this image"
            );
            let conn = pool.get()?;
            images_repo::update_thumbnail_failed(
                &conn,
                image_id,
                "failed",
                "缩略图生成超时（读取原图或解码卡住），可以稍后重新生成",
            )
        }
    }
}

/// `background`：批量队列整段降到后台优先级；可见区域队列用正常优先级（用户正在等这张图）。
fn generate(task: ThumbnailTask, cancel: &CancellationToken, background: bool) -> AppResult<()> {
    if cancel.is_cancelled() {
        return Ok(());
    }
    let hash = thumb_hash_for(task.root_id, &task.rel_path);
    let path = thumb_path(&task.thumbs_dir, &hash, THUMB_EXT);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // 读原图 / 解码 / 缩放 / JPEG 编码都很重，批量生成这一段降到后台优先级，避免拖卡前台 UI。
    // 写库不在后台模式里（见 utils/priority.rs：I/O 优先级被压低后持写锁的时间会变长）。
    let encoded = {
        let _bg = background.then(BackgroundPriorityGuard::enter);
        // 原生尺寸只需读图头（便宜），与解码分辨率解耦：JPEG 会被 DCT 降采样解码，
        // 不能拿降采样后的像素尺寸当原始 width/height 回填。
        let native_dims = image::image_dimensions(&task.full_path).ok();
        match decode_for_thumbnail(&task.full_path) {
            Ok(image) => {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                let oriented = apply_orientation(image, task.orientation);
                // width/height 用原生尺寸（按 orientation 交换），拿不到时退回解码后的尺寸。
                let dims = oriented_native_dims(native_dims, task.orientation)
                    .unwrap_or((i64::from(oriented.width()), i64::from(oriented.height())));
                write_jpeg(&oriented.thumbnail(THUMB_SIZE, THUMB_SIZE), &path)?;
                Ok(dims)
            }
            Err(error) => Err(error),
        }
    };
    let (width, height) = match encoded {
        Ok(dims) => dims,
        Err(_) if cancel.is_cancelled() => return Ok(()),
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

    let conn = task.pool.get()?;
    images_repo::update_thumbnail_ready(
        &conn,
        task.image_id,
        &hash,
        THUMB_EXT,
        Some(width),
        Some(height),
    )?;
    // 重新生成的图，R4 之前留下的 WebP 旧文件已经没用了。
    let _ = fs::remove_file(thumb_path(&task.thumbs_dir, &hash, LEGACY_THUMB_EXT));
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

/// 缩略图解码入口：JPEG 走 DCT 降采样（只解到略大于缩略图目标的分辨率，省 CPU/内存），
/// 非 JPEG 格式或降采样失败时回退到通用全量解码，保证正确性。
fn decode_for_thumbnail(path: &Path) -> Result<DynamicImage, image::ImageError> {
    let is_jpeg = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg"))
        .unwrap_or(false);
    if is_jpeg {
        if let Some(image) = decode_jpeg_downscaled(path) {
            return Ok(image);
        }
        // 降采样解码失败（打开失败 / CMYK 等少见格式）→ 落回通用解码。
    }
    image::open(path)
}

/// 用 jpeg-decoder 的 DCT 缩放，把大 JPEG 只解到略大于缩略图目标的分辨率。
/// 任何异常（打开失败、非 RGB/灰度格式、尺寸非法）返回 `None`，由上层回退到通用解码。
fn decode_jpeg_downscaled(path: &Path) -> Option<DynamicImage> {
    let file = File::open(path).ok()?;
    let mut decoder = jpeg_decoder::Decoder::new(BufReader::new(file));
    // 目标略大于缩略图尺寸，给旋转与后续缩放滤波留余量；解码器按 1/1,1/2,1/4,1/8 取最合适档。
    let target = THUMB_SIZE.saturating_mul(2).min(u32::from(u16::MAX)) as u16;
    decoder.scale(target, target).ok()?;
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            image::RgbImage::from_raw(width, height, pixels).map(DynamicImage::ImageRgb8)
        }
        jpeg_decoder::PixelFormat::L8 => {
            image::GrayImage::from_raw(width, height, pixels).map(DynamicImage::ImageLuma8)
        }
        // L16 / CMYK32 少见，交给通用解码处理，避免自行做色彩转换出错。
        _ => None,
    }
}

/// 原生尺寸按 EXIF orientation 交换宽高（6/8 为旋转 90°/270°），供回填 width/height 用。
fn oriented_native_dims(dims: Option<(u32, u32)>, orientation: Option<i64>) -> Option<(i64, i64)> {
    let (width, height) = dims?;
    let (width, height) = (i64::from(width), i64::from(height));
    Some(match orientation.unwrap_or(1) {
        6 | 8 => (height, width),
        _ => (width, height),
    })
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

    #[tokio::test]
    async fn stuck_blocking_work_times_out() {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        // 模拟卡死的读取：一直等到测试结束才放行。
        let stuck = blocking_with_timeout(Duration::from_millis(50), move || {
            let _ = wait.recv();
            1
        })
        .await;
        assert!(stuck.is_none(), "卡住的工作应当超时");
        drop(release);
        let done = blocking_with_timeout(Duration::from_secs(5), || 7).await;
        assert_eq!(done.map(|joined| joined.expect("join")), Some(7));
    }

    #[tokio::test]
    async fn event_batches_merge_ids_within_window() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        for id in [3, 1, 3, 2] {
            tx.send(id).expect("send");
        }
        // 窗口内稍后到达的也并进同一批。
        let late = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            late.send(5).expect("send");
        });
        assert_eq!(
            next_event_batch(&mut rx, Duration::from_millis(300)).await,
            Some(vec![3, 1, 2, 5])
        );

        tx.send(7).expect("send");
        assert_eq!(
            next_event_batch(&mut rx, Duration::from_millis(30)).await,
            Some(vec![7])
        );
        drop(tx);
        assert_eq!(
            next_event_batch(&mut rx, Duration::from_millis(30)).await,
            None
        );
    }

    #[test]
    fn thumb_path_uses_two_char_prefix() {
        let path = thumb_path(Path::new("thumbs"), "abcdef0123456789", THUMB_EXT);
        assert!(path.ends_with(Path::new("ab").join("abcdef0123456789.jpg")));
        let legacy = thumb_path(Path::new("thumbs"), "abcdef0123456789", LEGACY_THUMB_EXT);
        assert!(legacy.ends_with(Path::new("ab").join("abcdef0123456789.webp")));
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
