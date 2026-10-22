use photo_view_plus_lib::repo::lancedb_repo::{
    EmbeddingRecord, FaceEmbeddingRecord, LanceDbRepo, VectorTable, FACE_DIMS, LEGACY_CLIP_MODEL,
};

const MODEL: &str = "chinese-clip-vit-b-16";

fn unit(dims: usize, axis: usize) -> Vec<f32> {
    let mut vector = vec![0.0; dims];
    vector[axis] = 1.0;
    vector
}

fn record(image_id: i64, model: &str, embedding: Vec<f32>) -> EmbeddingRecord {
    EmbeddingRecord {
        image_id,
        model: model.to_string(),
        embedding,
        created_at: 1,
    }
}

#[tokio::test]
async fn lancedb_embedding_roundtrip_and_search() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = LanceDbRepo::new(dir.path().join("vectors"));
    let table = VectorTable::image_clip(MODEL, 512);
    let vector = unit(512, 0);

    repo.upsert_embeddings(&table, &[record(42, MODEL, vector.clone())])
        .await
        .expect("upsert");

    assert_eq!(repo.count(&table).await.expect("count"), 1);
    assert_eq!(
        repo.embedding_for_image(&table, 42)
            .await
            .expect("embedding")
            .expect("some")[0],
        1.0
    );
    assert!(repo
        .embedding_for_image(&table, 7)
        .await
        .expect("missing")
        .is_none());

    let hits = repo.top_k(&table, &vector, 10).await.expect("top_k");
    assert_eq!(hits[0].image_id, 42);
    assert!(
        (hits[0].score - 1.0).abs() < 1e-5,
        "余弦相似度：同一向量为 1"
    );

    repo.delete_embeddings(&table, &[42]).await.expect("delete");
    assert_eq!(repo.count(&table).await.expect("count"), 0);
}

#[test]
fn vector_tables_are_named_by_model_and_dims() {
    assert_eq!(
        VectorTable::image_clip(LEGACY_CLIP_MODEL, 512).name(),
        "image_embeddings_clip",
        "旧模型沿用旧表，便于回退"
    );
    assert_eq!(
        VectorTable::image_clip(MODEL, 512).name(),
        "image_clip_emb_chinese_clip_vit_b_16_512"
    );
    assert_eq!(
        VectorTable::image_text("bge-small-zh-v1.5", 512).name(),
        "image_text_emb_bge_small_zh_v1_5_512"
    );
    let base = VectorTable::image_text("bge-base-zh-v1.5", 768);
    assert_eq!(base.dims(), 768);
    assert_ne!(base, VectorTable::image_text("bge-base-zh-v1.5", 512));
}

#[tokio::test]
async fn tables_of_different_models_and_dims_are_independent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = LanceDbRepo::new(dir.path().join("vectors"));
    let legacy = VectorTable::image_clip(LEGACY_CLIP_MODEL, 512);
    let chinese = VectorTable::image_clip(MODEL, 512);
    let text = VectorTable::image_text("bge-base-zh-v1.5", 768);

    repo.upsert_embeddings(&legacy, &[record(1, LEGACY_CLIP_MODEL, unit(512, 0))])
        .await
        .expect("legacy");
    repo.upsert_embeddings(&chinese, &[record(1, MODEL, unit(512, 1))])
        .await
        .expect("chinese");
    repo.upsert_embeddings(&text, &[record(1, "bge-base-zh-v1.5", unit(768, 700))])
        .await
        .expect("768 维文本向量");

    assert_eq!(
        repo.embedding_for_image(&legacy, 1)
            .await
            .expect("legacy")
            .expect("some")[0],
        1.0,
        "换模型不覆盖旧表"
    );
    assert_eq!(
        repo.embedding_for_image(&chinese, 1)
            .await
            .expect("chinese")
            .expect("some")[1],
        1.0
    );
    assert_eq!(
        repo.top_k(&text, &unit(768, 700), 5).await.expect("top_k")[0].image_id,
        1
    );

    // 维度不对：写入和查询都报错，不会把向量写进别的表。
    assert!(repo
        .upsert_embeddings(&chinese, &[record(2, MODEL, unit(768, 0))])
        .await
        .is_err());
    assert!(repo.top_k(&chinese, &unit(768, 0), 5).await.is_err());
}

#[tokio::test]
async fn face_embeddings_use_their_own_dims() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = LanceDbRepo::new(dir.path().join("vectors"));
    repo.upsert_face_embeddings(&[FaceEmbeddingRecord {
        face_id: 9,
        embedding: unit(FACE_DIMS, 3),
        created_at: 1,
    }])
    .await
    .expect("face upsert");
    let faces = repo.list_face_embeddings().await.expect("faces");
    assert_eq!(faces.len(), 1);
    assert_eq!(faces[0].0, 9);
    assert_eq!(faces[0].1.len(), FACE_DIMS);
    assert!(repo
        .upsert_face_embeddings(&[FaceEmbeddingRecord {
            face_id: 10,
            embedding: unit(FACE_DIMS + 1, 0),
            created_at: 1,
        }])
        .await
        .is_err());
}
