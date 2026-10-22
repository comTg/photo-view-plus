//! `image_analysis`：图片的画面分析结果（docs/10 §7）。来源是 memora 导入或本项目的 VLM 打标，
//! VLM 的结果优先：memora 导入不会覆盖已有的 VLM 行。

use std::collections::HashMap;

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::Serialize;

use crate::error::AppResult;

pub const SOURCE_VLM: &str = "vlm";
pub const SOURCE_MEMORA: &str = "memora";

/// 一张图的分析结果。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageAnalysis {
    pub image_id: i64,
    pub source: String,
    pub model: String,
    pub prompt_version: String,
    pub description: String,
    pub categories: Vec<String>,
    pub keywords: Vec<String>,
    pub people_count: Option<i64>,
    pub visible_text: Option<String>,
    pub sensitive: bool,
    #[serde(skip_serializing)]
    pub raw_json: Option<String>,
    pub duration_ms: Option<i64>,
    pub analyzed_at: i64,
}

/// 参与检索的文本部分（search_index_service 用）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnalysisTexts {
    pub description: String,
    pub keywords: Vec<String>,
    pub visible_text: String,
}

impl AnalysisTexts {
    /// 送去算描述文本向量的文本，见 [`crate::db::embedding_text`]。
    pub fn embedding_text(&self) -> String {
        crate::db::embedding_text(&self.description, &self.keywords)
    }
}

/// 写入（或更新）一条分析结果，返回是否写入。
///
/// - VLM 结果总是写入（覆盖 memora 导入的行，也覆盖旧的 VLM 结果）；
/// - memora 导入只在没有行、或已有行也是 memora 时写入——不覆盖 VLM 的结果。
pub fn upsert(conn: &Connection, analysis: &ImageAnalysis) -> AppResult<bool> {
    let guard = if analysis.source == SOURCE_VLM {
        ""
    } else {
        "WHERE image_analysis.source <> 'vlm'"
    };
    let sql = format!(
        "INSERT INTO image_analysis(
             image_id, source, model, prompt_version, description, categories, keywords,
             people_count, visible_text, sensitive, raw_json, duration_ms, analyzed_at
         )
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13
         WHERE EXISTS (SELECT 1 FROM images WHERE id = ?1)
         ON CONFLICT(image_id) DO UPDATE SET
             source = excluded.source,
             model = excluded.model,
             prompt_version = excluded.prompt_version,
             description = excluded.description,
             categories = excluded.categories,
             keywords = excluded.keywords,
             people_count = excluded.people_count,
             visible_text = excluded.visible_text,
             sensitive = excluded.sensitive,
             raw_json = excluded.raw_json,
             duration_ms = excluded.duration_ms,
             analyzed_at = excluded.analyzed_at
         {guard}"
    );
    let changed = conn.prepare_cached(&sql)?.execute(params![
        analysis.image_id,
        analysis.source,
        analysis.model,
        analysis.prompt_version,
        analysis.description,
        to_json(&analysis.categories),
        to_json(&analysis.keywords),
        analysis.people_count,
        analysis.visible_text,
        analysis.sensitive,
        analysis.raw_json,
        analysis.duration_ms,
        analysis.analyzed_at
    ])?;
    Ok(changed > 0)
}

pub fn get(conn: &Connection, image_id: i64) -> AppResult<Option<ImageAnalysis>> {
    Ok(conn
        .prepare_cached(
            "SELECT image_id, source, model, prompt_version, description, categories, keywords,
                    people_count, visible_text, sensitive, raw_json, duration_ms, analyzed_at
             FROM image_analysis WHERE image_id = ?1",
        )?
        .query_row([image_id], row_to_analysis)
        .optional()?)
}

/// 各来源的条数（只统计未删除的图）。
pub fn counts_by_source(conn: &Connection) -> AppResult<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT a.source, COUNT(*) FROM image_analysis a
         JOIN images i ON i.id = a.image_id
         WHERE i.deleted_at IS NULL
         GROUP BY a.source ORDER BY a.source",
    )?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// 批量取参与检索的文本。
pub fn texts_for(conn: &Connection, image_ids: &[i64]) -> AppResult<HashMap<i64, AnalysisTexts>> {
    let mut out = HashMap::new();
    for chunk in image_ids.chunks(500) {
        let sql = format!(
            "SELECT image_id, description, keywords, visible_text FROM image_analysis
             WHERE image_id IN ({})",
            std::iter::repeat("?")
                .take(chunk.len())
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params_from_iter(chunk.iter().copied().map(Value::Integer)),
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    AnalysisTexts {
                        description: row.get(1)?,
                        keywords: from_json(&row.get::<_, String>(2)?),
                        visible_text: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    },
                ))
            },
        )?;
        for row in rows {
            let (image_id, texts) = row?;
            out.insert(image_id, texts);
        }
    }
    Ok(out)
}

fn row_to_analysis(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImageAnalysis> {
    Ok(ImageAnalysis {
        image_id: row.get(0)?,
        source: row.get(1)?,
        model: row.get(2)?,
        prompt_version: row.get(3)?,
        description: row.get(4)?,
        categories: from_json(&row.get::<_, String>(5)?),
        keywords: from_json(&row.get::<_, String>(6)?),
        people_count: row.get(7)?,
        visible_text: row.get(8)?,
        sensitive: row.get(9)?,
        raw_json: row.get(10)?,
        duration_ms: row.get(11)?,
        analyzed_at: row.get(12)?,
    })
}

fn to_json(values: &[String]) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "[]".to_string())
}

/// JSON 字符串数组 → Vec；格式不对时返回空（导入的旧数据可能不规范，不让它拖垮读取）。
pub fn from_json(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}
