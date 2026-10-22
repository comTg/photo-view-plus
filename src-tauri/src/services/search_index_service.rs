//! 检索专用全文索引 `search_fts` 的维护（docs/10 §5.4、§7，A2）。
//!
//! - 影响检索文本的写入（标签、OCR、重命名、扫描入库 / 删除、回收站 / 撤销、分析结果）都由
//!   SQLite 触发器把图片 id 记进 `search_fts_dirty`（见 migration 0008）；
//! - [`flush_dirty`] 批量取出这些 id，按当前分词规则重写 `search_fts` 的对应行（已删除的图只删不写）；
//!   后台循环定期调用，搜索前也会顺手处理少量积压，保证刚写入的标签马上能搜到；
//! - [`rebuild_all`] 全量重建（改了分词规则时用，命令 `search_index_rebuild`）。
//!
//! 每批在一个 IMMEDIATE 事务里完成"读积压 → 读文本 → 写索引 → 清积压"：写锁期间别的写入进不来，
//! 不会出现"读完文本后又有新标签写入、却被当成已处理清掉"的竞态。

use std::collections::HashMap;
use std::time::Instant;

use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, TransactionBehavior};
use serde::Serialize;

use crate::error::AppResult;
use crate::repo::analysis_repo;
use crate::utils::text_tokens::{self, Tokens};

/// 后台每批处理的图片数。
pub const FLUSH_BATCH: usize = 2000;

/// 一张图参与检索的原始文本（分词前）。也用于搜索结果的命中来源和描述片段。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchTexts {
    /// 标签名（所有来源）。RAM 的"站/矗立/摊位"这类带斜杠的歧义译名拆成多个。
    pub tags: Vec<String>,
    /// 检索用的同义词 / 上位词（VLM 的 keywords，A3 起）。
    pub keywords: Vec<String>,
    /// 画面描述（A3 起：memora 导入或 VLM 分析）。
    pub description: String,
    /// 画面里的主要文字（VLM 的 visible_text）。
    pub visible_text: String,
    /// OCR 全文。
    pub ocr_text: String,
    /// 文件名（不含扩展名）。
    pub filename: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchIndexStatus {
    /// 已建索引的图片数。
    pub indexed: i64,
    /// 等待重建的图片数。
    pub pending: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RebuildOutcome {
    pub indexed: usize,
    pub elapsed_ms: u64,
}

pub fn status(conn: &Connection) -> AppResult<SearchIndexStatus> {
    Ok(SearchIndexStatus {
        indexed: conn.query_row("SELECT COUNT(*) FROM search_fts", [], |row| row.get(0))?,
        pending: pending_count(conn)?,
    })
}

pub fn pending_count(conn: &Connection) -> AppResult<i64> {
    Ok(
        conn.query_row("SELECT COUNT(*) FROM search_fts_dirty", [], |row| {
            row.get(0)
        })?,
    )
}

/// 处理最多 `limit` 张积压的图，返回处理的张数（0 表示没有积压）。
pub fn flush_dirty(conn: &mut Connection, limit: usize) -> AppResult<usize> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let ids = {
        let mut stmt =
            tx.prepare_cached("SELECT image_id FROM search_fts_dirty ORDER BY image_id LIMIT ?1")?;
        let rows = stmt.query_map([limit.max(1) as i64], |row| row.get::<_, i64>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    if ids.is_empty() {
        return Ok(0);
    }
    reindex_in(&tx, &ids)?;
    for chunk in ids.chunks(500) {
        tx.execute(
            &format!(
                "DELETE FROM search_fts_dirty WHERE image_id IN ({})",
                placeholders(chunk.len())
            ),
            params_from_iter(chunk.iter().copied().map(Value::Integer)),
        )?;
    }
    tx.commit()?;
    Ok(ids.len())
}

/// 把积压全部处理完（分批提交，每批之间释放写锁）。
pub fn flush_all(conn: &mut Connection) -> AppResult<usize> {
    let mut total = 0;
    loop {
        let done = flush_dirty(conn, FLUSH_BATCH)?;
        if done == 0 {
            return Ok(total);
        }
        total += done;
    }
}

/// 全量重建：清空索引、整库重新排队，再分批处理完。
pub fn rebuild_all(conn: &mut Connection) -> AppResult<RebuildOutcome> {
    let started = Instant::now();
    {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM search_fts", [])?;
        tx.execute(
            "INSERT OR IGNORE INTO search_fts_dirty(image_id)
             SELECT id FROM images WHERE deleted_at IS NULL",
            [],
        )?;
        tx.commit()?;
    }
    let indexed = flush_all(conn)?;
    Ok(RebuildOutcome {
        indexed,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// 立即重写这些图的索引行（不经过积压队列）。
pub fn reindex(conn: &Connection, image_ids: &[i64]) -> AppResult<()> {
    reindex_in(conn, image_ids)
}

fn reindex_in(conn: &Connection, image_ids: &[i64]) -> AppResult<()> {
    for chunk in image_ids.chunks(500) {
        conn.execute(
            &format!(
                "DELETE FROM search_fts WHERE rowid IN ({})",
                placeholders(chunk.len())
            ),
            params_from_iter(chunk.iter().copied().map(Value::Integer)),
        )?;
        let texts = load_texts(conn, chunk)?;
        let mut insert = conn.prepare_cached(
            "INSERT INTO search_fts(rowid, tags, keywords, description, visible_text, ocr_text, filename, chars)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for (image_id, texts) in &texts {
            let doc = IndexDoc::from_texts(texts);
            insert.execute(rusqlite::params![
                image_id,
                doc.tags,
                doc.keywords,
                doc.description,
                doc.visible_text,
                doc.ocr_text,
                doc.filename,
                doc.chars
            ])?;
        }
    }
    Ok(())
}

/// 读出这些图（未删除的）参与检索的原始文本。已删除 / 不存在的图不在结果里。
pub fn load_texts(conn: &Connection, image_ids: &[i64]) -> AppResult<HashMap<i64, SearchTexts>> {
    let mut out: HashMap<i64, SearchTexts> = HashMap::new();
    for chunk in image_ids.chunks(500) {
        let marks = placeholders(chunk.len());
        let values = || params_from_iter(chunk.iter().copied().map(Value::Integer));
        {
            let mut stmt = conn.prepare(&format!(
                "SELECT id, filename, ocr_text FROM images
                 WHERE id IN ({marks}) AND deleted_at IS NULL"
            ))?;
            let rows = stmt.query_map(values(), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?;
            for row in rows {
                let (id, filename, ocr_text) = row?;
                out.insert(
                    id,
                    SearchTexts {
                        filename: filename_stem(&filename).to_string(),
                        ocr_text: ocr_text.unwrap_or_default(),
                        ..SearchTexts::default()
                    },
                );
            }
        }
        {
            let mut stmt = conn.prepare(&format!(
                "SELECT it.image_id, t.name FROM image_tags it
                 JOIN tags t ON t.id = it.tag_id
                 WHERE it.image_id IN ({marks})
                 ORDER BY it.image_id, it.score DESC, t.name"
            ))?;
            let rows = stmt.query_map(values(), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, name) = row?;
                if let Some(texts) = out.get_mut(&id) {
                    for part in split_tag_name(&name) {
                        if !texts.tags.iter().any(|existing| existing == part) {
                            texts.tags.push(part.to_string());
                        }
                    }
                }
            }
        }
        for (id, analysis) in analysis_repo::texts_for(conn, chunk)? {
            if let Some(texts) = out.get_mut(&id) {
                texts.description = analysis.description;
                texts.keywords = analysis.keywords;
                texts.visible_text = analysis.visible_text;
            }
        }
    }
    Ok(out)
}

/// 标签名拆成检索用的词：RAM 的歧义译名用斜杠、顿号连接（"站/矗立/摊位"）。
pub fn split_tag_name(name: &str) -> impl Iterator<Item = &str> {
    name.split(['/', '／', '、'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
}

fn filename_stem(filename: &str) -> &str {
    match filename.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 5 => stem,
        _ => filename,
    }
}

/// 写进 search_fts 的一行：每列是空格连接的词，`chars` 是所有字段的单字。
struct IndexDoc {
    tags: String,
    keywords: String,
    description: String,
    visible_text: String,
    ocr_text: String,
    filename: String,
    chars: String,
}

impl IndexDoc {
    fn from_texts(texts: &SearchTexts) -> Self {
        let mut chars = Vec::new();
        let mut column = |text: &str| {
            let Tokens {
                terms,
                chars: column_chars,
                ..
            } = text_tokens::tokenize(text);
            chars.extend(column_chars);
            terms.join(" ")
        };
        let tags = column(&texts.tags.join("，"));
        let keywords = column(&texts.keywords.join("，"));
        let description = column(&texts.description);
        let visible_text = column(&texts.visible_text);
        let ocr_text = column(&texts.ocr_text);
        let filename = column(&texts.filename);
        Self {
            tags,
            keywords,
            description,
            visible_text,
            ocr_text,
            filename,
            chars: chars.join(" "),
        }
    }
}

fn placeholders(count: usize) -> String {
    std::iter::repeat("?")
        .take(count)
        .collect::<Vec<_>>()
        .join(",")
}
