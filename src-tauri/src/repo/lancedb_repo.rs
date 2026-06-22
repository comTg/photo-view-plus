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
use lancedb::query::{ExecutableQuery, QueryBase};
use serde::Serialize;

use crate::error::{AppError, AppResult};

const TABLE_NAME: &str = "image_embeddings_clip";
const FACE_TABLE_NAME: &str = "face_embeddings";
const VECTOR_COLUMN: &str = "embedding";
pub const CLIP_DIMS: i32 = 512;

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

    pub async fn upsert_embeddings(&self, records: &[EmbeddingRecord]) -> AppResult<()> {
        if records.is_empty() {
            return Ok(());
        }
        validate_records(records)?;
        let table = self.open_or_create_table().await?;
        let ids: Vec<i64> = records.iter().map(|record| record.image_id).collect();
        let predicate = format!("image_id IN ({})", join_i64(&ids));
        let _ = table.delete(predicate.as_str()).await;
        let batch = records_to_batch(records)?;
        let schema = batch.schema();
        let data = RecordBatchIterator::new(vec![Ok(batch)].into_iter(), schema);
        let reader: Box<dyn RecordBatchReader + Send> = Box::new(data);
        table.add(reader).execute().await.map_err(lance_error)?;
        Ok(())
    }

    pub async fn embedding_for_image(&self, image_id: i64) -> AppResult<Option<Vec<f32>>> {
        let table = match self.open_table().await {
            Ok(table) => table,
            Err(_) => return Ok(None),
        };
        let stream = table
            .query()
            .only_if(format!("image_id = {image_id}"))
            .limit(1)
            .execute()
            .await
            .map_err(lance_error)?;
        let batches = stream.try_collect::<Vec<_>>().await.map_err(lance_error)?;
        Ok(batches
            .first()
            .and_then(|batch| vector_from_batch(batch, 0).ok()))
    }

    pub async fn top_k(&self, query: &[f32], limit: usize) -> AppResult<Vec<VectorSearchHit>> {
        if query.len() != CLIP_DIMS as usize {
            return Err(AppError::Other(format!(
                "CLIP embedding 维度错误：expected {}, got {}",
                CLIP_DIMS,
                query.len()
            )));
        }
        let table = match self.open_table().await {
            Ok(table) => table,
            Err(_) => return Ok(Vec::new()),
        };
        let stream = table
            .query()
            .nearest_to(query)
            .map_err(lance_error)?
            .limit(limit)
            .execute()
            .await
            .map_err(lance_error)?;
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
                    .unwrap_or(0.0);
                out.push(VectorSearchHit {
                    image_id: image_ids.value(row),
                    score: 1.0 / (1.0 + distance.max(0.0)),
                });
            }
        }
        Ok(out)
    }

    pub async fn count(&self) -> AppResult<usize> {
        let table = match self.open_table().await {
            Ok(table) => table,
            Err(_) => return Ok(0),
        };
        table.count_rows(None).await.map_err(lance_error)
    }

    pub async fn upsert_face_embeddings(&self, records: &[FaceEmbeddingRecord]) -> AppResult<()> {
        if records.is_empty() {
            return Ok(());
        }
        validate_face_records(records)?;
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

    async fn open_table(&self) -> AppResult<lancedb::Table> {
        let db = self.connection().await?;
        db.open_table(TABLE_NAME)
            .execute()
            .await
            .map_err(lance_error)
    }

    async fn open_or_create_table(&self) -> AppResult<lancedb::Table> {
        let db = self.connection().await?;
        if let Ok(table) = db.open_table(TABLE_NAME).execute().await {
            return Ok(table);
        }
        db.create_empty_table(TABLE_NAME, embedding_schema())
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

fn embedding_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("image_id", DataType::Int64, false),
        Field::new("model", DataType::Utf8, false),
        Field::new(
            VECTOR_COLUMN,
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                CLIP_DIMS,
            ),
            false,
        ),
        Field::new("created_at", DataType::Int64, false),
    ]))
}

fn face_embedding_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("face_id", DataType::Int64, false),
        Field::new(
            VECTOR_COLUMN,
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                CLIP_DIMS,
            ),
            false,
        ),
        Field::new("created_at", DataType::Int64, false),
    ]))
}

fn records_to_batch(records: &[EmbeddingRecord]) -> AppResult<RecordBatch> {
    let schema = embedding_schema();
    let image_ids = Int64Array::from_iter_values(records.iter().map(|record| record.image_id));
    let models = StringArray::from_iter_values(records.iter().map(|record| record.model.as_str()));
    let embeddings = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        records.iter().map(|record| {
            Some(
                record
                    .embedding
                    .iter()
                    .copied()
                    .map(Some)
                    .collect::<Vec<_>>(),
            )
        }),
        CLIP_DIMS,
    );
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
    let embeddings = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        records.iter().map(|record| {
            Some(
                record
                    .embedding
                    .iter()
                    .copied()
                    .map(Some)
                    .collect::<Vec<_>>(),
            )
        }),
        CLIP_DIMS,
    );
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

fn validate_records(records: &[EmbeddingRecord]) -> AppResult<()> {
    for record in records {
        if record.embedding.len() != CLIP_DIMS as usize {
            return Err(AppError::Other(format!(
                "image {} embedding 维度错误：expected {}, got {}",
                record.image_id,
                CLIP_DIMS,
                record.embedding.len()
            )));
        }
    }
    Ok(())
}

fn validate_face_records(records: &[FaceEmbeddingRecord]) -> AppResult<()> {
    for record in records {
        if record.embedding.len() != CLIP_DIMS as usize {
            return Err(AppError::Other(format!(
                "face {} embedding 维度错误：expected {}, got {}",
                record.face_id,
                CLIP_DIMS,
                record.embedding.len()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lancedb_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = LanceDbRepo::new(dir.path().join("vectors"));
        let mut vector = vec![0.0; CLIP_DIMS as usize];
        vector[0] = 1.0;
        repo.upsert_embeddings(&[EmbeddingRecord {
            image_id: 42,
            model: "clip-vit-b-32".to_string(),
            embedding: vector.clone(),
            created_at: 1,
        }])
        .await
        .expect("upsert");
        assert_eq!(repo.count().await.expect("count"), 1);
        assert_eq!(
            repo.embedding_for_image(42)
                .await
                .expect("embedding")
                .expect("some")[0],
            1.0
        );
        let hits = repo.top_k(&vector, 10).await.expect("top_k");
        assert_eq!(hits[0].image_id, 42);
    }
}
