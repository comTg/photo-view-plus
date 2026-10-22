//! VLM 打标的离线验证（docs/10 A6、G3）：在评测库的副本上用真实的 llama-server（Qwen3.5-9B）
//! 和 AI worker 跑样本、对比输入尺寸、跑全量。评测库本身（`<评测目录>\rust`，memora 描述）不动，
//! VLM 的结果写进另一份副本 `<评测目录>\vlm\app.sqlite`（之后 R6 用它评测）。
//!
//! 全部是 `#[ignore]`，手动运行（llama-server 加载约 7GB 显存）：
//!
//! ```powershell
//! cd src-tauri
//! cargo test --test vlm_eval vlm_sample -- --ignored --nocapture
//! ```
//!
//! 环境变量：
//! - `PVP_EVAL_DIR`：评测目录，默认 `%LOCALAPPDATA%\PhotoViewPlus\eval`；
//! - `PVP_VLM_SAMPLE`：样本张数（默认 200）；`PVP_VLM_SEED`：抽样种子（默认 20261023）；
//! - `PVP_VLM_EDGE`：送模型的长边（默认 1024）；`PVP_VLM_SAVE=0`：只输出、不写库；
//! - `PVP_VLM_OUT`：输出文件名（默认 `sample_<长边>.jsonl`，写在 `<评测目录>\vlm`）；
//! - `PVP_VLM_PARALLEL`：并发（默认 4，与 `-np` 一致）；
//! - `PVP_VLM_HOURS`：全量运行的最长小时数（默认 8）。

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use photo_view_plus_lib::config::AppSettings;
use photo_view_plus_lib::db::{self, Pool};
use photo_view_plus_lib::repo::ai_jobs_repo::VlmCandidate;
use photo_view_plus_lib::services::ai_client::{VlmAnalyzeParams, VlmAnalyzeResponse};
use photo_view_plus_lib::services::ai_supervisor::AiSupervisor;
use photo_view_plus_lib::services::analysis_pipeline::{
    self, read_original, save_vlm_result, AnalysisPipeline,
};
use photo_view_plus_lib::services::resource_governor::{
    self, GpuGate, GpuPolicy, ModeThresholds, OverrideMode, ResourceGovernor,
};
use photo_view_plus_lib::services::vlm_supervisor::{
    nvidia_smi_free_mb, LaunchSpec, VlmSupervisor, DEFAULT_PORT,
};
use photo_view_plus_lib::utils::win_activity::SystemActivity;
use serde::Serialize;

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn local_app_data() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").expect("LOCALAPPDATA"))
}

fn eval_dir() -> PathBuf {
    std::env::var("PVP_EVAL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| local_app_data().join("PhotoViewPlus").join("eval"))
}

fn models_dir() -> PathBuf {
    std::env::var("PVP_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| local_app_data().join("PhotoViewPlus").join("models"))
}

/// VLM 用的评测库副本：第一次从 `<评测目录>\rust` 复制（库和向量目录），之后一直用它。
fn vlm_pool() -> (PathBuf, Pool) {
    let dir = eval_dir().join("vlm");
    let db_path = dir.join("app.sqlite");
    if !db_path.exists() {
        let source = eval_dir().join("rust");
        assert!(
            source.join("app.sqlite").exists(),
            "缺少 {}：先跑 search_eval 的 eval_prepare",
            source.join("app.sqlite").display()
        );
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::copy(source.join("app.sqlite"), &db_path).expect("copy db");
        copy_dir(&source.join("vectors"), &dir.join("vectors"));
        println!("copied {} -> {}", source.display(), dir.display());
    }
    let pool = db::open(&db_path).expect("open vlm eval db (runs migrations)");
    (dir, pool)
}

fn copy_dir(from: &Path, to: &Path) {
    if !from.exists() {
        return;
    }
    std::fs::create_dir_all(to).expect("mkdir");
    for entry in std::fs::read_dir(from).expect("read_dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy");
        }
    }
}

fn worker_dir() -> PathBuf {
    std::env::var("PVP_AI_WORKER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("repo root")
                .join("ai-worker")
        })
}

async fn services(parallel: u8) -> (Arc<AiSupervisor>, Arc<VlmSupervisor>, f64) {
    let paths = analysis_pipeline::detect_vlm_paths(&models_dir())
        .expect("找不到 llama-server 和模型（docs/10 §12.1）");
    println!("llama-server: {}", paths.exe.display());
    let vlm = Arc::new(
        VlmSupervisor::new(DEFAULT_PORT, Some(eval_dir().join("vlm").join("logs")))
            .expect("vlm supervisor"),
    );
    let started = Instant::now();
    let status = vlm
        .ensure_running(&LaunchSpec::llama(&paths, parallel, DEFAULT_PORT))
        .await
        .expect("llama-server");
    let load_secs = started.elapsed().as_secs_f64();
    println!("llama-server ready: {status:?} in {load_secs:.1}s");
    let worker =
        Arc::new(AiSupervisor::new(worker_dir(), models_dir()).expect("AI worker supervisor"));
    worker.start().await.expect("AI worker");
    (worker, vlm, load_secs)
}

/// 每秒采一次剩余显存，记最小值（= 峰值占用）。
fn vram_sampler(stop: Arc<AtomicBool>, min_free: Arc<AtomicU64>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if let Some(free) = nvidia_smi_free_mb() {
                min_free.fetch_min(free, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    })
}

#[derive(Serialize)]
struct SampleRecord {
    key: String,
    image_id: i64,
    thumb_hash: Option<String>,
    read_ms: u128,
    total_ms: u128,
    ok: bool,
    error: Option<String>,
    response: Option<VlmAnalyzeResponse>,
}

struct SampleImage {
    candidate: VlmCandidate,
    thumb_hash: Option<String>,
    key: String,
}

/// 有缩略图（便于人工核对）、按忽略大小写的路径去重后，用固定种子随机抽样。
/// 设了 `PVP_VLM_IDS`（逗号分隔的图片 id）时只取这些图。
fn sample_images(pool: &Pool, count: usize, seed: u64) -> Vec<SampleImage> {
    let only_ids = std::env::var("PVP_VLM_IDS").ok().map(|raw| {
        raw.split(',')
            .filter_map(|id| id.trim().parse::<i64>().ok())
            .collect::<HashSet<_>>()
    });
    let conn = pool.get().expect("conn");
    let mut stmt = conn
        .prepare(
            "SELECT i.id, r.path, i.rel_path, i.taken_at, i.thumb_hash FROM images i
             JOIN roots r ON r.id = i.root_id
             WHERE i.deleted_at IS NULL AND i.thumb_status = 'ready'
             ORDER BY i.id",
        )
        .expect("prepare");
    let rows = stmt
        .query_map([], |row| {
            Ok(SampleImage {
                candidate: VlmCandidate {
                    image_id: row.get(0)?,
                    root_path: row.get(1)?,
                    rel_path: row.get(2)?,
                    taken_at: row.get(3)?,
                    has_analysis: false,
                },
                thumb_hash: row.get(4)?,
                key: String::new(),
            })
        })
        .expect("query");
    let mut seen = HashSet::new();
    let mut images = rows
        .map(|row| row.expect("row"))
        .filter(|image| {
            only_ids
                .as_ref()
                .map_or(true, |ids| ids.contains(&image.candidate.image_id))
        })
        .filter_map(|mut image| {
            image.key = format!(
                "{}\\{}",
                image.candidate.root_path, image.candidate.rel_path
            )
            .to_lowercase();
            seen.insert(image.key.clone()).then_some(image)
        })
        .collect::<Vec<_>>();
    // Fisher–Yates，xorshift64 作随机源（不引入 rand 依赖）。
    let mut state = seed.max(1);
    for index in (1..images.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let other = (state % (index as u64 + 1)) as usize;
        images.swap(index, other);
    }
    images.truncate(count);
    images
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

/// G3 样本：随机 N 张，并发 = 槽位数；输出每张的结果、耗时，汇总吞吐、延迟和显存峰值。
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore]
async fn vlm_sample() {
    let count = env_or("PVP_VLM_SAMPLE", 200usize);
    let seed = env_or("PVP_VLM_SEED", 20_261_023u64);
    let long_edge = env_or("PVP_VLM_EDGE", 1024u16);
    let parallel = env_or("PVP_VLM_PARALLEL", 4u8);
    let save = env_or("PVP_VLM_SAVE", 1u8) == 1;
    let (dir, pool) = vlm_pool();
    let images = sample_images(&pool, count, seed);
    println!(
        "sample: {} images, long edge {long_edge}, parallel {parallel}",
        images.len()
    );

    let baseline_free = nvidia_smi_free_mb();
    let (worker, vlm, load_secs) = services(parallel).await;
    let stop = Arc::new(AtomicBool::new(false));
    let min_free = Arc::new(AtomicU64::new(u64::MAX));
    let sampler = vram_sampler(stop.clone(), min_free.clone());

    let started = Instant::now();
    let base_url = vlm.base_url().to_string();
    let records = futures::stream::iter(images.into_iter().map(|image| {
        let worker = worker.clone();
        let pool = pool.clone();
        let base_url = base_url.clone();
        async move {
            let begin = Instant::now();
            let path = PathBuf::from(&image.candidate.root_path).join(&image.candidate.rel_path);
            let bytes = tokio::task::spawn_blocking(move || read_original(&path))
                .await
                .expect("join");
            let read_ms = begin.elapsed().as_millis();
            let bytes = match bytes {
                Ok(bytes) => bytes,
                Err(failure) => {
                    return SampleRecord {
                        key: image.key,
                        image_id: image.candidate.image_id,
                        thumb_hash: image.thumb_hash,
                        read_ms,
                        total_ms: begin.elapsed().as_millis(),
                        ok: false,
                        error: Some(format!("{failure:?}")),
                        response: None,
                    }
                }
            };
            let params = VlmAnalyzeParams {
                server_url: base_url,
                long_edge,
                taken_at: image.candidate.taken_at,
            };
            let response = worker.vlm_analyze(bytes, &params).await;
            let total_ms = begin.elapsed().as_millis();
            match response {
                Ok(response) => {
                    if save {
                        if let Some(result) = response.result.as_ref() {
                            let mut conn = pool.get().expect("conn");
                            save_vlm_result(
                                &mut conn,
                                &image.candidate,
                                result,
                                response.duration_ms,
                                chrono::Utc::now().timestamp(),
                            )
                            .expect("save");
                        }
                    }
                    SampleRecord {
                        key: image.key,
                        image_id: image.candidate.image_id,
                        thumb_hash: image.thumb_hash,
                        read_ms,
                        total_ms,
                        ok: response.ok,
                        error: response.error.clone(),
                        response: Some(response),
                    }
                }
                Err(error) => SampleRecord {
                    key: image.key,
                    image_id: image.candidate.image_id,
                    thumb_hash: image.thumb_hash,
                    read_ms,
                    total_ms,
                    ok: false,
                    error: Some(error.to_string()),
                    response: None,
                },
            }
        }
    }))
    .buffer_unordered(usize::from(parallel))
    .collect::<Vec<_>>()
    .await;
    let wall = started.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    let _ = sampler.join();

    let out_name =
        std::env::var("PVP_VLM_OUT").unwrap_or_else(|_| format!("sample_{long_edge}.jsonl"));
    let mut out = std::fs::File::create(dir.join(&out_name)).expect("out");
    for record in &records {
        writeln!(out, "{}", serde_json::to_string(record).expect("json")).expect("write");
    }
    let ok = records.iter().filter(|record| record.ok).count();
    let mut latency = records
        .iter()
        .filter(|record| record.ok)
        .map(|record| record.total_ms as f64 / 1000.0)
        .collect::<Vec<_>>();
    latency.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let mut read = records
        .iter()
        .map(|record| record.read_ms as f64)
        .collect::<Vec<_>>();
    read.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let tokens = records
        .iter()
        .filter_map(|record| record.response.as_ref())
        .filter_map(|response| response.prompt_tokens.zip(response.predicted_tokens))
        .collect::<Vec<_>>();
    let min_free = min_free.load(Ordering::Relaxed);
    println!("=== VLM sample (long edge {long_edge}) ===");
    println!("images: {} ok / {} total", ok, records.len());
    println!("model load: {load_secs:.1}s");
    println!(
        "wall: {wall:.1}s -> {:.2} s/image (parallel {parallel})",
        wall / records.len().max(1) as f64
    );
    println!(
        "latency per image: p50 {:.2}s p95 {:.2}s; read original p50 {:.0}ms p95 {:.0}ms",
        percentile(&latency, 0.5),
        percentile(&latency, 0.95),
        percentile(&read, 0.5),
        percentile(&read, 0.95)
    );
    if !tokens.is_empty() {
        let prompt = tokens.iter().map(|(p, _)| *p as f64).sum::<f64>() / tokens.len() as f64;
        let predicted = tokens.iter().map(|(_, p)| *p as f64).sum::<f64>() / tokens.len() as f64;
        println!("tokens: prompt avg {prompt:.0}, predicted avg {predicted:.0}");
    }
    if let (Some(baseline), true) = (baseline_free, min_free != u64::MAX) {
        println!(
            "VRAM: free before {baseline} MiB, min free during run {min_free} MiB (peak use +{} MiB)",
            baseline.saturating_sub(min_free)
        );
    }
    let failures = records
        .iter()
        .filter(|record| !record.ok)
        .map(|record| record.error.clone().unwrap_or_default())
        .collect::<Vec<_>>();
    if !failures.is_empty() {
        println!("failures: {failures:#?}");
    }
    if std::env::var("PVP_VLM_KEEP").is_err() {
        vlm.stop(true).await;
    }
}

/// 全量：用真正的 `AnalysisPipeline`（治理器读真实的键盘鼠标状态，有人用电脑时自动让路）
/// 跑完 VLM 评测库副本里的全部待分析图。开头先测一次"降档 → 在途清空"的耗时（docs/11 §8：≤ 10 秒）。
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore]
async fn vlm_full() {
    let hours = env_or("PVP_VLM_HOURS", 8.0f64);
    let (dir, pool) = vlm_pool();
    // 流水线读设置：在评测目录里放一份打开了 VLM 的配置（不碰应用自己的配置）。
    let data_dir = dir.join("app-data");
    std::fs::create_dir_all(&data_dir).expect("mkdir");
    let mut paths = photo_view_plus_lib::config::AppPaths::from_data_dir(data_dir);
    paths.models_dir = models_dir();
    let settings = AppSettings {
        vlm_enabled: true,
        ..AppSettings::default()
    };
    std::fs::write(
        &paths.config_path,
        serde_json::to_string_pretty(&settings).expect("json"),
    )
    .expect("config");

    let gpu = GpuGate::new(GpuPolicy::Unlimited);
    let (budget, thresholds) = resource_governor::settings_from(&settings, 6);
    let governor = ResourceGovernor::new(
        Box::new(SystemActivity),
        budget,
        ModeThresholds {
            idle_after: thresholds.idle_after,
            ..ModeThresholds::default()
        },
        gpu,
        Box::new(|_| {}),
    );
    let worker =
        Arc::new(AiSupervisor::new(worker_dir(), models_dir()).expect("AI worker supervisor"));
    let vlm =
        Arc::new(VlmSupervisor::new(DEFAULT_PORT, Some(dir.join("logs"))).expect("vlm supervisor"));
    let pipeline = AnalysisPipeline::new(
        pool.clone(),
        paths,
        worker.clone(),
        vlm.clone(),
        governor.clone(),
    );

    let log_path = dir.join("full_progress.log");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("log");
    let started = Instant::now();
    let deadline = started + Duration::from_secs_f64(hours * 3600.0);

    // 1) 全速跑到有 4 张在途，然后切"暂停"，量在途清空要多久。
    governor.set_override(OverrideMode::FullSpeed, None, Instant::now());
    let mut measured_drain = false;
    let mut last_print = Instant::now() - Duration::from_secs(60);
    loop {
        governor.tick(Instant::now());
        pipeline.tick().await;
        let progress = pipeline.progress().await;
        if !measured_drain && progress.inflight >= 4 && progress.session_done >= 8 {
            let pause_at = Instant::now();
            governor.set_override(OverrideMode::Paused, None, pause_at);
            loop {
                pipeline.tick().await;
                if pipeline.progress().await.inflight == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let drain = pause_at.elapsed().as_secs_f64();
            let line = format!("downgrade drain: {drain:.1}s (in-flight requests completed)");
            println!("{line}");
            writeln!(log, "{line}").expect("log");
            measured_drain = true;
            // 之后交给自动模式：有人用电脑时让路，空闲时全速。
            governor.set_override(OverrideMode::Auto, None, Instant::now());
        }
        if last_print.elapsed() >= Duration::from_secs(30) {
            last_print = Instant::now();
            let line = format!(
                "{:>6.0}s state={} pending={} analyzed={} failed={} inflight={} done={} rate={:?} eta={:?} tier={:?} err={:?}",
                started.elapsed().as_secs_f64(),
                progress.state,
                progress.pending,
                progress.analyzed,
                progress.failed,
                progress.inflight,
                progress.session_done,
                progress.secs_per_image.map(|rate| (rate * 100.0).round() / 100.0),
                progress.eta_secs,
                governor.status().tier,
                progress.last_error,
            );
            println!("{line}");
            writeln!(log, "{line}").expect("log");
        }
        if progress.pending == 0 && progress.inflight == 0 && measured_drain {
            break;
        }
        if Instant::now() >= deadline {
            println!("time limit reached");
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let progress = pipeline.progress().await;
    let line = format!(
        "finished after {:.1} min: analyzed={} failed={} pending={} session_done={} session_failed={}",
        started.elapsed().as_secs_f64() / 60.0,
        progress.analyzed,
        progress.failed,
        progress.pending,
        progress.session_done,
        progress.session_failed
    );
    println!("{line}");
    writeln!(log, "{line}").expect("log");
    vlm.stop(true).await;
}
