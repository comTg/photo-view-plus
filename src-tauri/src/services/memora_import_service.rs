//! 一次性导入 memora 已有的画面描述（docs/10 §7、A3）。
//!
//! memora（`D:\code\mygit\memora`）用同一个 Qwen3.5-9B 给同一批 NAS 照片写过中文描述和类别。
//! 按图片的绝对路径匹配本库的图，写进 `image_analysis`（source = memora），类别写成标签
//! （image_tags.source = memora），作为冷启动数据。之后跑本项目的 VLM 时，这些行会被覆盖。
//!
//! **路径匹配忽略大小写**：NAS 上 `camera` / `Camera` 两个目录名只差大小写（docs/11 §6.1），
//! 本库按两条路径各入了一次，而 Windows 经 SMB 访问时两条路径指向同一个文件。只做精确匹配的话，
//! 描述可能落到读不出缩略图的那一行，画廊里能看到的那一行反而没有描述。

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;

use crate::error::{AppError, AppResult};
use crate::repo::analysis_repo::{self, ImageAnalysis, SOURCE_MEMORA};
use crate::repo::tags_repo;

/// memora 默认的数据库位置（本机）；界面里可以另选。
pub const DEFAULT_MEMORA_DB: &str = r"D:\code\mygit\memora\data\memora.db";
/// 类别标签的 category。
pub const CATEGORY_TAG: &str = "category";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoraImportOutcome {
    /// memora 里有描述的照片数。
    pub memora_rows: usize,
    /// 其中在本库找到对应图片的照片数。
    pub matched_photos: usize,
    /// 写入（新增或更新）的本库图片数（同一路径大小写不同的重复行各算一张）。
    pub images_written: usize,
    /// 已有本项目 VLM 结果、没有覆盖的图片数。
    pub skipped_vlm: usize,
    pub elapsed_ms: u64,
}

struct MemoraPhoto {
    path: String,
    description: String,
    categories: Vec<String>,
    sensitive: bool,
    model: String,
    prompt_version: String,
    analyzed_at: Option<String>,
    analysis_s: Option<f64>,
}

/// 从 memora 数据库导入。memora 库只读打开；本库的写入在一个事务里完成。
pub fn import_memora(
    conn: &mut Connection,
    memora_db: &Path,
    now: i64,
) -> AppResult<MemoraImportOutcome> {
    let started = Instant::now();
    let photos = read_memora(memora_db)?;
    let mut outcome = MemoraImportOutcome {
        memora_rows: photos.len(),
        ..MemoraImportOutcome::default()
    };
    let by_path = image_ids_by_path(conn)?;

    let tx = conn.transaction()?;
    for photo in &photos {
        let Some(image_ids) = by_path.get(&path_key(&photo.path)) else {
            continue;
        };
        outcome.matched_photos += 1;
        for &image_id in image_ids {
            let analysis = ImageAnalysis {
                image_id,
                source: SOURCE_MEMORA.to_string(),
                model: photo.model.clone(),
                prompt_version: format!("memora-{}", photo.prompt_version),
                description: photo.description.clone(),
                categories: photo.categories.clone(),
                keywords: Vec::new(),
                people_count: None,
                visible_text: None,
                sensitive: photo.sensitive,
                raw_json: None,
                duration_ms: photo.analysis_s.map(|secs| (secs * 1000.0).round() as i64),
                analyzed_at: photo
                    .analyzed_at
                    .as_deref()
                    .and_then(parse_local_time)
                    .unwrap_or(now),
            };
            if analysis_repo::upsert(&tx, &analysis)? {
                write_category_tags(&tx, image_id, &photo.categories, now)?;
                outcome.images_written += 1;
            } else {
                outcome.skipped_vlm += 1;
            }
        }
    }
    tx.commit()?;
    outcome.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(outcome)
}

/// 类别写成标签（source = memora）：先清掉这张图之前导入的 memora 标签，主类别分数高一点。
fn write_category_tags(
    conn: &Connection,
    image_id: i64,
    categories: &[String],
    now: i64,
) -> AppResult<()> {
    conn.prepare_cached("DELETE FROM image_tags WHERE image_id = ?1 AND source = 'memora'")?
        .execute([image_id])?;
    for (index, name) in categories.iter().enumerate() {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let tag_id = tags_repo::upsert_tag(conn, name, SOURCE_MEMORA, Some(CATEGORY_TAG), now)?;
        conn.prepare_cached(
            "INSERT OR REPLACE INTO image_tags(image_id, tag_id, score, source, created_at)
             VALUES (?1, ?2, ?3, 'memora', ?4)",
        )?
        .execute(params![image_id, tag_id, 0.9 - index as f64 * 0.05, now])?;
    }
    Ok(())
}

fn read_memora(path: &Path) -> AppResult<Vec<MemoraPhoto>> {
    if !path.is_file() {
        return Err(AppError::Other(format!(
            "找不到 memora 数据库：{}",
            path.display()
        )));
    }
    let memora = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let columns = {
        let mut stmt = memora.prepare("SELECT name FROM pragma_table_info('photos')")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let required = [
        "path",
        "status",
        "description",
        "categories",
        "sensitive",
        "vlm_model",
        "prompt_version",
        "analyzed_at",
        "analysis_s",
    ];
    if let Some(missing) = required
        .iter()
        .find(|name| !columns.iter().any(|column| column == *name))
    {
        return Err(AppError::Other(format!(
            "{} 不像 memora 数据库（photos 表缺少 {missing} 列）",
            path.display()
        )));
    }
    let mut stmt = memora.prepare(
        "SELECT path, description, categories, sensitive, vlm_model, prompt_version,
                analyzed_at, analysis_s
         FROM photos
         WHERE status = 'done' AND description IS NOT NULL AND TRIM(description) <> ''",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(MemoraPhoto {
            path: row.get(0)?,
            description: row.get::<_, String>(1)?.trim().to_string(),
            categories: analysis_repo::from_json(
                &row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ),
            sensitive: row.get::<_, Option<i64>>(3)?.unwrap_or(0) != 0,
            model: row
                .get::<_, Option<String>>(4)?
                .filter(|model| !model.trim().is_empty())
                .unwrap_or_else(|| "qwen3.5-9b".to_string()),
            prompt_version: row
                .get::<_, Option<String>>(5)?
                .unwrap_or_else(|| "unknown".to_string()),
            analyzed_at: row.get(6)?,
            analysis_s: row.get(7)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// 本库未删除的图：忽略大小写的绝对路径 → image id（同一路径可能有大小写不同的几行）。
fn image_ids_by_path(conn: &Connection) -> AppResult<HashMap<String, Vec<i64>>> {
    let mut stmt = conn.prepare(
        "SELECT i.id, r.path, i.rel_path FROM images i
         JOIN roots r ON r.id = i.root_id
         WHERE i.deleted_at IS NULL",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut out: HashMap<String, Vec<i64>> = HashMap::new();
    for row in rows {
        let (id, root, rel) = row?;
        let full = format!("{}\\{}", root.trim_end_matches(['\\', '/']), rel);
        out.entry(path_key(&full)).or_default().push(id);
    }
    Ok(out)
}

/// 路径比较用的 key：统一成反斜杠、转小写。
fn path_key(path: &str) -> String {
    path.replace('/', "\\").to_lowercase()
}

/// memora 的时间是本地时间字符串（"2026-10-14 19:35:51"）。
fn parse_local_time(raw: &str) -> Option<i64> {
    use chrono::{Local, NaiveDateTime, TimeZone};
    let naive = NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|time| time.timestamp())
}
