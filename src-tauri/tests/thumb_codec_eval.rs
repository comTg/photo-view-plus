//! R4 缩略图编码评测（docs/11 §6.2）：从正式库快照抽样已有缩略图（无损 WebP），重新编码成
//! JPEG q80 / q85，对比体积、编码耗时和 dHash 的变化；再在库里真实的视觉近似对上，
//! 看默认阈值下的分组是否保持。
//!
//! 全部是 `#[ignore]`，需要真实数据，手动运行（release，编码耗时才有参考价值）：
//!
//! ```powershell
//! cd src-tauri
//! cargo test --release --test thumb_codec_eval -- --ignored --nocapture --test-threads 1
//! ```
//!
//! 只读：数据库用评测快照（`immutable=1` 打开，不建 -wal / -shm），缩略图目录只读不写。
//! 环境变量：
//! - `PVP_EVAL_DB`：默认 `%LOCALAPPDATA%\PhotoViewPlus\eval\snapshot\app.sqlite`；
//! - `PVP_EVAL_THUMBS`：默认正式版 profile 的 `thumbs`；
//! - `PVP_EVAL_SAMPLE`：体积 / 耗时抽样张数，默认 200。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::webp::WebPEncoder;
use image::{DynamicImage, ImageEncoder, ImageFormat, RgbImage};
use photo_view_plus_lib::services::dedup_service::VISUAL_THRESHOLD_DEFAULT;
use photo_view_plus_lib::services::phash_service::{dhash_of_image, hamming};
use photo_view_plus_lib::utils::bk_tree::DhashIndex;
use rusqlite::{Connection, OpenFlags};

const QUALITIES: [u8; 2] = [80, 85];

fn local_app_data() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").expect("LOCALAPPDATA"))
}

fn open_snapshot() -> Connection {
    let path = std::env::var("PVP_EVAL_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            local_app_data()
                .join("PhotoViewPlus")
                .join("eval")
                .join("snapshot")
                .join("app.sqlite")
        });
    assert!(path.exists(), "缺少评测快照：{}", path.display());
    let uri = format!(
        "file:{}?immutable=1",
        path.to_string_lossy().replace('\\', "/")
    );
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open snapshot read-only")
}

fn thumbs_dir() -> PathBuf {
    std::env::var("PVP_EVAL_THUMBS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            local_app_data()
                .join("com.vetoer.photoviewplus")
                .join("thumbs")
        })
}

fn legacy_path(dir: &Path, hash: &str) -> PathBuf {
    dir.join(&hash[..2]).join(format!("{hash}.webp"))
}

/// 已就绪、库里记了 dHash 的缩略图：(id, thumb_hash, dhash)。
fn ready_thumbs(conn: &Connection) -> Vec<(i64, String, u64)> {
    let mut stmt = conn
        .prepare(
            "SELECT id, thumb_hash, dhash FROM images
             WHERE deleted_at IS NULL AND thumb_status = 'ready'
               AND thumb_hash IS NOT NULL AND dhash IS NOT NULL
             ORDER BY id",
        )
        .expect("prepare");
    stmt.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? as u64))
    })
    .expect("query")
    .map(|row| row.expect("row"))
    .collect()
}

/// 透明像素铺到白底上（带透明通道的 PNG 截图），与 thumbnail_service 的做法一致。
fn flatten(img: &DynamicImage) -> RgbImage {
    let rgba = img.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let blend =
            |c: u8| ((u16::from(c) * u16::from(a) + 255 * (255 - u16::from(a))) / 255) as u8;
        image::Rgb([blend(r), blend(g), blend(b)])
    })
}

fn encode_jpeg(rgb: &RgbImage, quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality)
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .expect("encode jpeg");
    out
}

fn encode_webp_lossless(img: &DynamicImage) -> Vec<u8> {
    let rgba = img.to_rgba8();
    let mut out = Vec::new();
    WebPEncoder::new_lossless(&mut out)
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )
        .expect("encode webp");
    out
}

fn jpeg_dhash(img: &DynamicImage, quality: u8) -> u64 {
    let bytes = encode_jpeg(&flatten(img), quality);
    let decoded = image::load_from_memory_with_format(&bytes, ImageFormat::Jpeg).expect("decode");
    dhash_of_image(&decoded).expect("dhash")
}

fn percentile(values: &mut [f64], p: f64) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let index = ((values.len() as f64 - 1.0) * p).round() as usize;
    values[index]
}

#[test]
#[ignore]
fn thumbnail_codec_sizes_and_dhash() {
    let conn = open_snapshot();
    let dir = thumbs_dir();
    let want: usize = std::env::var("PVP_EVAL_SAMPLE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);
    let all: Vec<_> = ready_thumbs(&conn)
        .into_iter()
        .filter(|(_, hash, _)| legacy_path(&dir, hash).exists())
        .collect();
    assert!(all.len() >= want, "可用缩略图只有 {} 张", all.len());
    // 等间隔抽样，覆盖不同目录和时间段。
    let step = all.len() / want;
    let sample: Vec<_> = all.iter().step_by(step).take(want).collect();

    let mut webp_bytes = 0usize;
    let mut webp_ms = Vec::new();
    let mut jpeg_bytes = HashMap::<u8, usize>::new();
    let mut jpeg_ms = HashMap::<u8, Vec<f64>>::new();
    let mut diffs = HashMap::<u8, Vec<u32>>::new();
    let mut stored_mismatch = 0usize;
    let mut with_alpha = 0usize;
    // 按原图格式分：(张数, 无损 WebP 字节, JPEG q85 字节)。截图类 PNG 的纯色块多，可能和照片不一样。
    let mut by_format = HashMap::<String, (usize, usize, usize)>::new();
    for (id, hash, stored) in &sample {
        let bytes = std::fs::read(legacy_path(&dir, hash)).expect("read thumb");
        webp_bytes += bytes.len();
        let format: String = conn
            .query_row("SELECT extension FROM images WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .expect("extension");
        let entry = by_format.entry(format).or_default();
        entry.0 += 1;
        entry.1 += bytes.len();
        let img = image::load_from_memory_with_format(&bytes, ImageFormat::WebP).expect("decode");
        if img.to_rgba8().pixels().any(|px| px.0[3] < 255) {
            with_alpha += 1;
        }
        let lossless = dhash_of_image(&img).expect("dhash");
        if lossless != *stored {
            stored_mismatch += 1;
        }

        let started = Instant::now();
        let _ = encode_webp_lossless(&img);
        webp_ms.push(started.elapsed().as_secs_f64() * 1000.0);

        for quality in QUALITIES {
            let started = Instant::now();
            let encoded = encode_jpeg(&flatten(&img), quality);
            jpeg_ms
                .entry(quality)
                .or_default()
                .push(started.elapsed().as_secs_f64() * 1000.0);
            *jpeg_bytes.entry(quality).or_default() += encoded.len();
            if quality == 85 {
                entry.2 += encoded.len();
            }
            let decoded =
                image::load_from_memory_with_format(&encoded, ImageFormat::Jpeg).expect("decode");
            let lossy = dhash_of_image(&decoded).expect("dhash");
            diffs
                .entry(quality)
                .or_default()
                .push(hamming(lossless, lossy));
        }
    }

    let n = sample.len() as f64;
    println!(
        "样本 {} 张（共 {} 张可用），带透明通道 {} 张；库里 dHash 与重算不一致 {} 张",
        sample.len(),
        all.len(),
        with_alpha,
        stored_mismatch
    );
    println!(
        "无损 WebP：平均 {:.1} KB，编码 p50 {:.2} ms / p95 {:.2} ms",
        webp_bytes as f64 / n / 1024.0,
        percentile(&mut webp_ms.clone(), 0.5),
        percentile(&mut webp_ms, 0.95)
    );
    for quality in QUALITIES {
        let size = jpeg_bytes[&quality] as f64;
        let mut ms = jpeg_ms[&quality].clone();
        let quality_diffs = &diffs[&quality];
        let histogram = (0..=3)
            .map(|bits| {
                let count = if bits == 3 {
                    quality_diffs.iter().filter(|d| **d >= 3).count()
                } else {
                    quality_diffs.iter().filter(|d| **d == bits).count()
                };
                format!("{}{}:{count}", bits, if bits == 3 { "+" } else { "" })
            })
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "JPEG q{quality}：平均 {:.1} KB（WebP 的 {:.0}%，缩小 {:.1} 倍），编码 p50 {:.2} ms / p95 {:.2} ms；dHash 变化位数 {histogram}",
            size / n / 1024.0,
            size / webp_bytes as f64 * 100.0,
            webp_bytes as f64 / size,
            percentile(&mut ms.clone(), 0.5),
            percentile(&mut ms, 0.95),
        );
    }
    let mut formats: Vec<_> = by_format.into_iter().collect();
    formats.sort_by_key(|(_, (count, _, _))| std::cmp::Reverse(*count));
    for (format, (count, webp, jpeg)) in formats {
        println!(
            "  原图 {format}：{count} 张，WebP 平均 {:.1} KB → JPEG q85 平均 {:.1} KB（{:.0}%）",
            webp as f64 / count as f64 / 1024.0,
            jpeg as f64 / count as f64 / 1024.0,
            jpeg as f64 / webp as f64 * 100.0
        );
    }
}

fn union_find_groups(ids: &[i64], pairs: &[(i64, i64)]) -> HashMap<i64, i64> {
    fn find(parent: &mut HashMap<i64, i64>, id: i64) -> i64 {
        let next = parent[&id];
        if next == id {
            return id;
        }
        let root = find(parent, next);
        parent.insert(id, root);
        root
    }
    let mut parent: HashMap<i64, i64> = ids.iter().map(|id| (*id, *id)).collect();
    for (a, b) in pairs {
        let (ra, rb) = (find(&mut parent, *a), find(&mut parent, *b));
        if ra != rb {
            parent.insert(ra.max(rb), ra.min(rb));
        }
    }
    ids.iter().map(|id| (*id, find(&mut parent, *id))).collect()
}

fn pairs_within(hashes: &[(i64, u64)], threshold: u32) -> Vec<(i64, i64)> {
    let index = DhashIndex::new();
    index.rebuild_from(hashes.iter().copied());
    let mut pairs = Vec::new();
    for (id, hash) in hashes {
        for (other, _) in index.find_within(*hash, threshold) {
            if other > *id {
                pairs.push((*id, other));
            }
        }
    }
    pairs
}

/// 成对 F1：真值 = 用现有（无损）缩略图的 dHash 分组，预测 = 用 JPEG 缩略图的 dHash 分组。
fn pairwise_f1(
    ids: &[i64],
    truth: &HashMap<i64, i64>,
    predicted: &HashMap<i64, i64>,
) -> (f64, f64, f64) {
    let (mut tp, mut fp, mut fnn) = (0usize, 0usize, 0usize);
    for i in 0..ids.len() {
        for j in (i + 1)..ids.len() {
            let same_truth = truth[&ids[i]] == truth[&ids[j]];
            let same_predicted = predicted[&ids[i]] == predicted[&ids[j]];
            match (same_truth, same_predicted) {
                (true, true) => tp += 1,
                (false, true) => fp += 1,
                (true, false) => fnn += 1,
                (false, false) => {}
            }
        }
    }
    let precision = if tp + fp == 0 {
        1.0
    } else {
        tp as f64 / (tp + fp) as f64
    };
    let recall = if tp + fnn == 0 {
        1.0
    } else {
        tp as f64 / (tp + fnn) as f64
    };
    let f1 = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };
    (precision, recall, f1)
}

#[test]
#[ignore]
fn visual_grouping_survives_jpeg_thumbnails() {
    let conn = open_snapshot();
    let dir = thumbs_dir();
    let threshold = VISUAL_THRESHOLD_DEFAULT;
    let all: Vec<_> = ready_thumbs(&conn)
        .into_iter()
        .filter(|(_, hash, _)| legacy_path(&dir, hash).exists())
        .collect();
    let stored: Vec<(i64, u64)> = all.iter().map(|(id, _, dhash)| (*id, *dhash)).collect();
    let near_pairs = pairs_within(&stored, threshold);

    // 子集：现有分组里出现过的图（同组的都在），再加等间隔抽的 800 张"无关"图检验误合并。
    let mut subset: HashSet<i64> = near_pairs.iter().flat_map(|(a, b)| [*a, *b]).collect();
    let paired = subset.len();
    let step = (all.len() / 800).max(1);
    subset.extend(all.iter().step_by(step).map(|(id, _, _)| *id));
    let by_id: HashMap<i64, &str> = all
        .iter()
        .map(|(id, hash, _)| (*id, hash.as_str()))
        .collect();
    let stored_by_id: HashMap<i64, u64> = stored.iter().copied().collect();
    let mut ids: Vec<i64> = subset.into_iter().collect();
    ids.sort_unstable();

    let subset_stored: Vec<(i64, u64)> = ids.iter().map(|id| (*id, stored_by_id[id])).collect();
    let truth = union_find_groups(&ids, &pairs_within(&subset_stored, threshold));
    let exact_pairs = near_pairs
        .iter()
        .filter(|(a, b)| stored_by_id[a] == stored_by_id[b])
        .count();
    println!(
        "阈值 {threshold}：全库 {} 张里有 {} 对近似（其中 {exact_pairs} 对 dHash 完全相同），涉及 {paired} 张；评测子集 {} 张",
        all.len(),
        near_pairs.len(),
        ids.len()
    );

    // 每张图只解码一次，两种质量的 dHash 一起算，只留哈希值（几千张解码后的图放内存里太大）。
    let mut lossy_by_quality: HashMap<u8, Vec<(i64, u64)>> = HashMap::new();
    for id in &ids {
        let bytes = std::fs::read(legacy_path(&dir, by_id[id])).expect("read thumb");
        let img = image::load_from_memory_with_format(&bytes, ImageFormat::WebP).expect("decode");
        for quality in QUALITIES {
            lossy_by_quality
                .entry(quality)
                .or_default()
                .push((*id, jpeg_dhash(&img, quality)));
        }
    }
    for quality in QUALITIES {
        let lossy = &lossy_by_quality[&quality];
        let lossy_by_id: HashMap<i64, u64> = lossy.iter().copied().collect();
        let predicted = union_find_groups(&ids, &pairs_within(lossy, threshold));
        let (precision, recall, f1) = pairwise_f1(&ids, &truth, &predicted);
        // 原本是"近似但不完全相同"的那些对，换成 JPEG 后还在阈值内的比例。
        let near_only: Vec<_> = near_pairs
            .iter()
            .filter(|(a, b)| stored_by_id[a] != stored_by_id[b])
            .collect();
        let kept = near_only
            .iter()
            .filter(|(a, b)| hamming(lossy_by_id[a], lossy_by_id[b]) <= threshold)
            .count();
        println!(
            "JPEG q{quality}：分组成对 precision {precision:.3} / recall {recall:.3} / F1 {f1:.3}；非完全相同的近似对保留 {kept}/{}",
            near_only.len()
        );
        assert!(f1 >= 0.92, "q{quality} 的分组 F1 {f1:.3} 低于 0.92");
    }
}

fn brighten(img: &DynamicImage, delta: i16) -> DynamicImage {
    let mut rgb = img.to_rgb8();
    for px in rgb.pixels_mut() {
        for channel in px.0.iter_mut() {
            *channel = (i16::from(*channel) + delta).clamp(0, 255) as u8;
        }
    }
    DynamicImage::ImageRgb8(rgb)
}

fn reencode(img: &DynamicImage, quality: u8) -> DynamicImage {
    let bytes = encode_jpeg(&flatten(img), quality);
    image::load_from_memory_with_format(&bytes, ImageFormat::Jpeg).expect("decode")
}

fn scaled(img: &DynamicImage, ratio: f32) -> DynamicImage {
    let width = ((img.width() as f32) * ratio).round() as u32;
    let height = ((img.height() as f32) * ratio).round() as u32;
    img.resize_exact(width, height, image::imageops::FilterType::Lanczos3)
}

/// 用真实缩略图造"近似重复"：每张原图配 4 个变体（重压缩 q70、缩到 85%、调亮 8、缩到 92% 再存 q90），
/// 同组 5 张应被分到一起。分别按"缩略图存无损"和"缩略图存 JPEG q85"算 dHash，在默认阈值下分组，
/// 看成对 F1 有没有因为改用 JPEG 而下降（库里现成的近似对太少，见上一个测试）。
#[test]
#[ignore]
fn synthetic_near_duplicates_keep_f1_with_jpeg_thumbnails() {
    let conn = open_snapshot();
    let dir = thumbs_dir();
    let threshold = VISUAL_THRESHOLD_DEFAULT;
    let all: Vec<_> = ready_thumbs(&conn)
        .into_iter()
        .filter(|(_, hash, _)| legacy_path(&dir, hash).exists())
        .collect();
    let sources = 200usize.min(all.len());
    let step = (all.len() / sources).max(1);

    let mut truth = HashMap::new();
    let mut lossless = Vec::new();
    let mut lossy = HashMap::<u8, Vec<(i64, u64)>>::new();
    let mut next_id = 0i64;
    for (group, (_, hash, _)) in all.iter().step_by(step).take(sources).enumerate() {
        let bytes = std::fs::read(legacy_path(&dir, hash)).expect("read thumb");
        let source =
            image::load_from_memory_with_format(&bytes, ImageFormat::WebP).expect("decode");
        let variants = [
            source.clone(),
            reencode(&source, 70),
            scaled(&source, 0.85),
            brighten(&source, 8),
            reencode(&scaled(&source, 0.92), 90),
        ];
        for variant in variants {
            next_id += 1;
            truth.insert(next_id, group as i64);
            lossless.push((next_id, dhash_of_image(&variant).expect("dhash")));
            for quality in QUALITIES {
                lossy
                    .entry(quality)
                    .or_default()
                    .push((next_id, jpeg_dhash(&variant, quality)));
            }
        }
    }
    let ids: Vec<i64> = (1..=next_id).collect();
    let report = |label: &str, hashes: &[(i64, u64)]| -> f64 {
        let predicted = union_find_groups(&ids, &pairs_within(hashes, threshold));
        let (precision, recall, f1) = pairwise_f1(&ids, &truth, &predicted);
        println!("{label}：precision {precision:.3} / recall {recall:.3} / F1 {f1:.3}");
        f1
    };
    println!("{sources} 组 × 5 张，阈值 {threshold}");
    let baseline = report("缩略图存无损 WebP（现状）", &lossless);
    for quality in QUALITIES {
        let f1 = report(&format!("缩略图存 JPEG q{quality}"), &lossy[&quality]);
        println!("  相对现状 F1 变化 {:+.3}", f1 - baseline);
    }
}
