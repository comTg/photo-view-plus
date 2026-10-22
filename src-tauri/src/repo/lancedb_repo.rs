use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::OnceCell;

use arrow_array::types::Float32Type;
use arrow_array::{
    Array, FixedSizeListArray, Float32Array, Int64Array, RecordBatch, RecordBatchIterator,
    RecordBatchReader, StringArray,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use futures::TryStreamExt;
use lancedb::expr::{col, lit};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::DistanceType;
use serde::Serialize;

use crate::error::{AppError, AppResult};

/// 旧的 OpenAI CLIP 模型（只懂英文）。docs/10 A5 之后仅作回退，向量留在旧表里。
pub const LEGACY_CLIP_MODEL: &str = "clip-vit-b-32";
/// 旧模型的向量表：表名不带模型和维度，保留到 A5 评测通过之后，便于回退。
const LEGACY_CLIP_TABLE: &str = "image_embeddings_clip";
const FACE_TABLE_NAME: &str = "face_embeddings";
const VECTOR_COLUMN: &str = "embedding";
/// 人脸向量维度（insightface ArcFace）。以前复用 CLIP 的 512，换图像模型会连带弄坏人脸，现独立出来。
pub const FACE_DIMS: usize = 512;

/// 一张"按图片 id 存向量"的 LanceDB 表。表名带模型和维度（docs/10 §5.3、§7），换模型就换表，
/// 新旧向量互不覆盖；维度由模型决定，不再写死 512。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorTable {
    name: String,
    dims: usize,
}

impl VectorTable {
    /// 图像向量（CLIP 系，文本 → 图像检索）：`image_clip_emb_<model>_<dims>`。旧模型沿用旧表。
    pub fn image_clip(model: &str, dims: usize) -> Self {
        if model == LEGACY_CLIP_MODEL && dims == 512 {
            return Self {
                name: LEGACY_CLIP_TABLE.to_string(),
                dims,
            };
        }
        Self {
            name: format!("image_clip_emb_{}_{dims}", table_token(model)),
            dims,
        }
    }

    /// 描述文本向量（文本 → 描述检索）：`image_text_emb_<model>_<dims>`。
    pub fn image_text(model: &str, dims: usize) -> Self {
        Self {
            name: format!("image_text_emb_{}_{dims}", table_token(model)),
            dims,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn dims(&self) -> usize {
        self.dims
    }
}

/// 模型名 → 表名片段：只留小写字母、数字，其余换成下划线（LanceDB 表名限制）。
fn table_token(model: &str) -> String {
    model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct EmbeddingRecord {
    pub image_id: i64,
    pub model: String,
    pub embedding: Vec<f32>,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct FaceEmbeddingRecord {
    pub face_id: i64,
    pub embedding: Vec<f32>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VectorSearchHit {
    pub image_id: i64,
    /// 余弦相似度（向量都已归一化），越大越近。
    pub score: f32,
}

#[derive(Clone)]
pub struct LanceDbRepo {
    path: PathBuf,
    // 复用连接：lancedb::connect 每次都会重读命名空间 __manifest，按调用重连
    // 会在每批 upsert 上叠加大量冗余磁盘 I/O。连接是 Clone 且为复用而设计的。
    conn: Arc<OnceCell<lancedb::Connection>>,
}

impl LanceDbRepo {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            conn: Arc::new(OnceCell::new()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 懒初始化并缓存 LanceDB 连接。首次失败不会污染 cell，下次调用会重试。
    async fn connection(&self) -> AppResult<&lancedb::Connection> {
        self.conn
            .get_or_try_init(|| async {
                std::fs::create_dir_all(&self.path)?;
                lancedb::connect(&self.path.to_string_lossy())
                    .execute()
                    .await
                    .map_err(lance_error)
            })
            .await
    }

    pub async fn upsert_embeddings(
        &self,
        table: &VectorTable,
        records: &[EmbeddingRecord],
    ) -> AppResult<()> {
        if records.is_empty() {
            return Ok(());
        }
        validate_dims(
            table.dims,
            records
                .iter()
                .map(|record| (record.image_id, record.embedding.len())),
            "image",
        )?;
        let lance_table = self.open_or_create_table(table).await?;
        let ids: Vec<i64> = records.iter().map(|record| record.image_id).collect();
        let predicate = format!("image_id IN ({})", join_i64(&ids));
        let _ = lance_table.delete(predicate.as_str()).await;
        let batch = records_to_batch(table.dims, records)?;
        let schema = batch.schema();
        let data = RecordBatchIterator::new(vec![Ok(batch)].into_iter(), schema);
        let reader: Box<dyn RecordBatchReader + Send> = Box::new(data);
        lance_table
            .add(reader)
            .execute()
            .await
            .map_err(lance_error)?;
        Ok(())
    }

    /// 删掉这些图的向量（表不存在时什么也不做）。
    pub async fn delete_embeddings(&self, table: &VectorTable, image_ids: &[i64]) -> AppResult<()> {
        if image_ids.is_empty() {
            return Ok(());
        }
        let Ok(lance_table) = self.open_table(table).await else {
            return Ok(());
        };
        let predicate = format!("image_id IN ({})", join_i64(image_ids));
        lance_table
            .delete(predicate.as_str())
            .await
            .map_err(lance_error)?;
        Ok(())
    }

    pub async fn embedding_for_image(
        &self,
        table: &VectorTable,
        image_id: i64,
    ) -> AppResult<Option<Vec<f32>>> {
        let lance_table = match self.open_table(table).await {
            Ok(lance_table) => lance_table,
            Err(_) => return Ok(None),
        };
        let stream = lance_table
            .query()
            .only_if(format!("image_id = {image_id}"))
            .limit(1)
            .execute()
            .await
            .map_err(lance_error)?;
        let batches = stream.try_collect::<Vec<_>>().await.map_err(lance_error)?;
        Ok(batches
            .first()
            .and_then(|batch| vector_from_batch(batch, 0).ok())
            .filter(|vector| !vector.is_empty()))
    }

    pub async fn top_k(
        &self,
        table: &VectorTable,
        query: &[f32],
        limit: usize,
    ) -> AppResult<Vec<VectorSearchHit>> {
        self.top_k_filtered(table, query, limit, None).await
    }

    /// 近邻检索（余弦距离），按相似度从高到低返回。
    ///
    /// `candidate_ids` 为 `Some` 时只在这些 image_id 里检索：条件经 `only_if_expr` 交给
    /// LanceDB，而 lancedb 的查询默认是 **prefilter**（`QueryRequest::prefilter = true`，
    /// 除非显式调用 `postfilter()`）——先按条件筛行，再只在筛出的行里算距离取 top-k。
    /// 所以候选集之外的向量再近也挤不掉候选集里的结果。候选集为空时直接返回空，不访问 LanceDB。
    ///
    /// 条件用 DataFusion 表达式（`image_id IN (...)`）而非 SQL 字符串，候选集上万时也不用拼接、解析大段 SQL。
    pub async fn top_k_filtered(
        &self,
        table: &VectorTable,
        query: &[f32],
        limit: usize,
        candidate_ids: Option<&[i64]>,
    ) -> AppResult<Vec<VectorSearchHit>> {
        if query.len() != table.dims {
            return Err(AppError::Other(format!(
                "查询向量维度错误（表 {}）：expected {}, got {}",
                table.name,
                table.dims,
                query.len()
            )));
        }
        if limit == 0 || candidate_ids.is_some_and(|ids| ids.is_empty()) {
            return Ok(Vec::new());
        }
        let lance_table = match self.open_table(table).await {
            Ok(lance_table) => lance_table,
            Err(_) => return Ok(Vec::new()),
        };
        let mut vector_query = lance_table
            .query()
            .nearest_to(query)
            .map_err(lance_error)?
            .distance_type(DistanceType::Cosine)
            .limit(limit);
        if let Some(ids) = candidate_ids {
            let list = ids.iter().map(|id| lit(*id)).collect::<Vec<_>>();
            vector_query = vector_query.only_if_expr(col("image_id").in_list(list, false));
        }
        let stream = vector_query.execute().await.map_err(lance_error)?;
        let batches = stream.try_collect::<Vec<_>>().await.map_err(lance_error)?;
        let mut out = Vec::new();
        for batch in &batches {
            let image_ids = batch
                .column_by_name("image_id")
                .and_then(|column| column.as_any().downcast_ref::<Int64Array>())
                .ok_or_else(|| AppError::Other("LanceDB 结果缺少 image_id 列".to_string()))?;
            let distances = batch
                .column_by_name("_distance")
                .and_then(|column| column.as_any().downcast_ref::<Float32Array>());
            for row in 0..batch.num_rows() {
                if image_ids.is_null(row) {
                    continue;
                }
                let distance = distances
                    .filter(|array| !array.is_null(row))
                    .map(|array| array.value(row))
                    .unwrap_or(1.0);
                out.push(VectorSearchHit {
                    image_id: image_ids.value(row),
                    // 余弦距离 = 1 − 余弦相似度
                    score: (1.0 - distance).clamp(-1.0, 1.0),
                });
            }
        }
        out.sort_by(|a, b| b.score.total_cmp(&a.score));
        Ok(out)
    }

    pub async fn count(&self, table: &VectorTable) -> AppResult<usize> {
        let lance_table = match self.open_table(table).await {
            Ok(lance_table) => lance_table,
            Err(_) => return Ok(0),
        };
        lance_table.count_rows(None).await.map_err(lance_error)
    }

    pub async fn upsert_face_embeddings(&self, records: &[FaceEmbeddingRecord]) -> AppResult<()> {
        if records.is_empty() {
            return Ok(());
        }
        validate_dims(
            FACE_DIMS,
            records
                .iter()
                .map(|record| (record.face_id, record.embedding.len())),
            "face",
        )?;
        let table = self.open_or_create_face_table().await?;
        let ids = records
            .iter()
            .map(|record| record.face_id)
            .collect::<Vec<_>>();
        let predicate = format!("face_id IN ({})", join_i64(&ids));
        let _ = table.delete(predicate.as_str()).await;
        let batch = face_records_to_batch(records)?;
        let schema = batch.schema();
        let data = RecordBatchIterator::new(vec![Ok(batch)].into_iter(), schema);
        let reader: Box<dyn RecordBatchReader + Send> = Box::new(data);
        table.add(reader).execute().await.map_err(lance_error)?;
        Ok(())
    }

    pub async fn list_face_embeddings(&self) -> AppResult<Vec<(i64, Vec<f32>)>> {
        let table = match self.open_face_table().await {
            Ok(table) => table,
            Err(_) => return Ok(Vec::new()),
        };
        let stream = table.query().execute().await.map_err(lance_error)?;
        let batches = stream.try_collect::<Vec<_>>().await.map_err(lance_error)?;
        let mut out = Vec::new();
        for batch in &batches {
            let face_ids = batch
                .column_by_name("face_id")
                .and_then(|column| column.as_any().downcast_ref::<Int64Array>())
                .ok_or_else(|| AppError::Other("LanceDB 结果缺少 face_id 列".to_string()))?;
            for row in 0..batch.num_rows() {
                if face_ids.is_null(row) {
                    continue;
                }
                out.push((face_ids.value(row), vector_from_batch(batch, row)?));
            }
        }
        Ok(out)
    }

    async fn open_table(&self, table: &VectorTable) -> AppResult<lancedb::Table> {
        let db = self.connection().await?;
        db.open_table(table.name.as_str())
            .execute()
            .await
            .map_err(lance_error)
    }

    /// 打开（不存在就建）向量表，并核对已有表的维度：同名表维度对不上说明数据错乱，宁可报错也不写。
    async fn open_or_create_table(&self, table: &VectorTable) -> AppResult<lancedb::Table> {
        let db = self.connection().await?;
        if let Ok(existing) = db.open_table(table.name.as_str()).execute().await {
            let schema = existing.schema().await.map_err(lance_error)?;
            match vector_dims(&schema) {
                Some(dims) if dims == table.dims => return Ok(existing),
                found => {
                    return Err(AppError::Other(format!(
                        "向量表 {} 的维度是 {:?}，与模型维度 {} 不一致",
                        table.name, found, table.dims
                    )))
                }
            }
        }
        db.create_empty_table(table.name.as_str(), embedding_schema(table.dims))
            .execute()
            .await
            .map_err(lance_error)
    }

    async fn open_face_table(&self) -> AppResult<lancedb::Table> {
        let db = self.connection().await?;
        db.open_table(FACE_TABLE_NAME)
            .execute()
            .await
            .map_err(lance_error)
    }

    async fn open_or_create_face_table(&self) -> AppResult<lancedb::Table> {
        let db = self.connection().await?;
        if let Ok(table) = db.open_table(FACE_TABLE_NAME).execute().await {
            return Ok(table);
        }
        db.create_empty_table(FACE_TABLE_NAME, face_embedding_schema())
            .execute()
            .await
            .map_err(lance_error)
    }
}

fn vector_field(dims: usize) -> Field {
    Field::new(
        VECTOR_COLUMN,
        DataType::FixedSizeList(
            Arc::new(Field::new("item", DataType::Float32, true)),
            dims as i32,
        ),
        false,
    )
}

fn vector_dims(schema: &Schema) -> Option<usize> {
    match schema.field_with_name(VECTOR_COLUMN).ok()?.data_type() {
        DataType::FixedSizeList(_, size) => usize::try_from(*size).ok(),
        _ => None,
    }
}

fn embedding_schema(dims: usize) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("image_id", DataType::Int64, false),
        Field::new("model", DataType::Utf8, false),
        vector_field(dims),
        Field::new("created_at", DataType::Int64, false),
    ]))
}

fn face_embedding_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("face_id", DataType::Int64, false),
        vector_field(FACE_DIMS),
        Field::new("created_at", DataType::Int64, false),
    ]))
}

fn vectors_array<'a>(
    dims: usize,
    vectors: impl Iterator<Item = &'a Vec<f32>>,
) -> FixedSizeListArray {
    FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        vectors.map(|vector| Some(vector.iter().copied().map(Some).collect::<Vec<_>>())),
        dims as i32,
    )
}

fn records_to_batch(dims: usize, records: &[EmbeddingRecord]) -> AppResult<RecordBatch> {
    let schema = embedding_schema(dims);
    let image_ids = Int64Array::from_iter_values(records.iter().map(|record| record.image_id));
    let models = StringArray::from_iter_values(records.iter().map(|record| record.model.as_str()));
    let embeddings = vectors_array(dims, records.iter().map(|record| &record.embedding));
    let created_at = Int64Array::from_iter_values(records.iter().map(|record| record.created_at));
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(image_ids),
            Arc::new(models),
            Arc::new(embeddings),
            Arc::new(created_at),
        ],
    )
    .map_err(|error| AppError::Other(error.to_string()))
}

fn face_records_to_batch(records: &[FaceEmbeddingRecord]) -> AppResult<RecordBatch> {
    let schema = face_embedding_schema();
    let face_ids = Int64Array::from_iter_values(records.iter().map(|record| record.face_id));
    let embeddings = vectors_array(FACE_DIMS, records.iter().map(|record| &record.embedding));
    let created_at = Int64Array::from_iter_values(records.iter().map(|record| record.created_at));
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(face_ids),
            Arc::new(embeddings),
            Arc::new(created_at),
        ],
    )
    .map_err(|error| AppError::Other(error.to_string()))
}

fn vector_from_batch(batch: &RecordBatch, row: usize) -> AppResult<Vec<f32>> {
    let vectors = batch
        .column_by_name(VECTOR_COLUMN)
        .and_then(|column| column.as_any().downcast_ref::<FixedSizeListArray>())
        .ok_or_else(|| AppError::Other("LanceDB 结果缺少 embedding 列".to_string()))?;
    if row >= vectors.len() || vectors.is_null(row) {
        return Ok(Vec::new());
    }
    let value = vectors.value(row);
    let floats = value
        .as_any()
        .downcast_ref::<Float32Array>()
        .ok_or_else(|| AppError::Other("embedding 列类型错误".to_string()))?;
    Ok((0..floats.len()).map(|index| floats.value(index)).collect())
}

fn validate_dims(
    dims: usize,
    items: impl Iterator<Item = (i64, usize)>,
    kind: &str,
) -> AppResult<()> {
    for (id, len) in items {
        if len != dims {
            return Err(AppError::Other(format!(
                "{kind} {id} embedding 维度错误：expected {dims}, got {len}"
            )));
        }
    }
    Ok(())
}

fn join_i64(values: &[i64]) -> String {
    values
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn lance_error(error: impl std::fmt::Display) -> AppError {
    AppError::Other(format!("lancedb: {error}"))
}
