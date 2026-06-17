use photo_view_plus_lib::repo::lancedb_repo::{EmbeddingRecord, LanceDbRepo, CLIP_DIMS};

#[tokio::test]
async fn lancedb_embedding_roundtrip_and_search() {
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
