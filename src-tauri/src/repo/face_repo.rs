use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::repo::images_repo::{self, ImagePage};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FaceBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Face {
    pub id: i64,
    pub image_id: i64,
    pub cluster_id: Option<i64>,
    pub bbox: FaceBox,
    pub confidence: f64,
    pub embedding_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FaceCluster {
    pub id: i64,
    pub label: Option<String>,
    pub sample_image_id: Option<i64>,
    pub face_count: i64,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct NewFace {
    pub bbox: FaceBox,
    pub confidence: f64,
    pub embedding_ref: Option<String>,
}

pub fn replace_faces_for_image(
    conn: &mut Connection,
    image_id: i64,
    faces: &[NewFace],
) -> AppResult<Vec<Face>> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM faces WHERE image_id = ?1", [image_id])?;
    let mut written = Vec::new();
    for face in faces {
        let bbox_json = serde_json::to_string(&face.bbox)
            .map_err(|error| crate::error::AppError::Other(error.to_string()))?;
        tx.execute(
            "INSERT INTO faces(image_id, bbox_json, confidence, embedding_ref)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                image_id,
                bbox_json,
                face.confidence.clamp(0.0, 1.0),
                face.embedding_ref
            ],
        )?;
        written.push(Face {
            id: tx.last_insert_rowid(),
            image_id,
            cluster_id: None,
            bbox: face.bbox.clone(),
            confidence: face.confidence.clamp(0.0, 1.0),
            embedding_ref: face.embedding_ref.clone(),
        });
    }
    tx.commit()?;
    Ok(written)
}

pub fn faces_for_image(conn: &Connection, image_id: i64) -> AppResult<Vec<Face>> {
    let mut stmt = conn.prepare(
        "SELECT id, image_id, cluster_id, bbox_json, confidence, embedding_ref
         FROM faces
         WHERE image_id = ?1
         ORDER BY confidence DESC, id ASC",
    )?;
    let rows = stmt.query_map([image_id], row_to_face)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn list_clusters(conn: &Connection) -> AppResult<Vec<FaceCluster>> {
    let mut stmt = conn.prepare(
        "SELECT id, label, sample_image_id, face_count, created_at
         FROM face_clusters
         WHERE face_count > 0
         ORDER BY label IS NULL, face_count DESC, id ASC",
    )?;
    let rows = stmt.query_map([], row_to_cluster)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn rename_cluster(conn: &Connection, cluster_id: i64, label: Option<String>) -> AppResult<()> {
    conn.execute(
        "UPDATE face_clusters SET label = ?1 WHERE id = ?2",
        params![label.map(|label| label.trim().to_string()), cluster_id],
    )?;
    Ok(())
}

pub fn set_embedding_ref(conn: &Connection, face_id: i64, embedding_ref: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE faces SET embedding_ref = ?1 WHERE id = ?2",
        params![embedding_ref, face_id],
    )?;
    Ok(())
}

pub fn merge_clusters(
    conn: &mut Connection,
    from_cluster_id: i64,
    to_cluster_id: i64,
) -> AppResult<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE faces SET cluster_id = ?1 WHERE cluster_id = ?2",
        params![to_cluster_id, from_cluster_id],
    )?;
    tx.execute("DELETE FROM face_clusters WHERE id = ?1", [from_cluster_id])?;
    refresh_cluster_counts_in_tx(&tx)?;
    tx.commit()?;
    Ok(())
}

pub fn images_for_cluster(
    conn: &Connection,
    cluster_id: i64,
    offset: i64,
    limit: i64,
) -> AppResult<ImagePage> {
    let limit = limit.clamp(1, 500);
    let offset = offset.max(0);
    let total: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT f.image_id)
         FROM faces f
         JOIN images i ON i.id = f.image_id
         WHERE f.cluster_id = ?1 AND i.deleted_at IS NULL",
        [cluster_id],
        |row| row.get(0),
    )?;
    let mut stmt = conn.prepare(
        "SELECT DISTINCT f.image_id
         FROM faces f
         JOIN images i ON i.id = f.image_id
         WHERE f.cluster_id = ?1 AND i.deleted_at IS NULL
         ORDER BY COALESCE(i.taken_at, i.mtime) DESC, i.id DESC
         LIMIT ?2 OFFSET ?3",
    )?;
    let rows = stmt.query_map(params![cluster_id, limit, offset], |row| row.get(0))?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row?);
    }
    let items = images_repo::get_details_by_ids(conn, &ids)?;
    Ok(ImagePage {
        items,
        total,
        offset,
        limit,
    })
}

pub fn rebuild_clusters(conn: &mut Connection, embeddings: &[(i64, Vec<f32>)]) -> AppResult<usize> {
    let faces = all_faces(conn)?;
    let prior_labels = prior_labels(conn)?;
    let groups = cluster_components(&faces, embeddings, 0.82);
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM face_clusters", [])?;
    tx.execute("UPDATE faces SET cluster_id = NULL", [])?;

    let now = now_unix();
    let mut written = 0usize;
    for group in groups {
        let sample_face = group.first().and_then(|face_id| faces.get(face_id));
        let Some(sample_face) = sample_face else {
            continue;
        };
        let label = group
            .iter()
            .filter_map(|face_id| faces.get(face_id).and_then(|face| face.cluster_id))
            .filter_map(|cluster_id| prior_labels.get(&cluster_id).cloned())
            .next();
        tx.execute(
            "INSERT INTO face_clusters(label, sample_image_id, face_count, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![label, sample_face.image_id, group.len() as i64, now],
        )?;
        let cluster_id = tx.last_insert_rowid();
        for face_id in group {
            tx.execute(
                "UPDATE faces SET cluster_id = ?1 WHERE id = ?2",
                params![cluster_id, face_id],
            )?;
        }
        written += 1;
    }
    tx.commit()?;
    Ok(written)
}

fn row_to_face(row: &rusqlite::Row<'_>) -> rusqlite::Result<Face> {
    let bbox_json: String = row.get(3)?;
    let bbox = serde_json::from_str(&bbox_json).unwrap_or(FaceBox {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
    });
    Ok(Face {
        id: row.get(0)?,
        image_id: row.get(1)?,
        cluster_id: row.get(2)?,
        bbox,
        confidence: row.get(4)?,
        embedding_ref: row.get(5)?,
    })
}

fn row_to_cluster(row: &rusqlite::Row<'_>) -> rusqlite::Result<FaceCluster> {
    Ok(FaceCluster {
        id: row.get(0)?,
        label: row.get(1)?,
        sample_image_id: row.get(2)?,
        face_count: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn all_faces(conn: &Connection) -> AppResult<HashMap<i64, Face>> {
    let mut stmt = conn.prepare(
        "SELECT id, image_id, cluster_id, bbox_json, confidence, embedding_ref
         FROM faces
         ORDER BY id ASC",
    )?;
    let rows = stmt.query_map([], row_to_face)?;
    let mut out = HashMap::new();
    for row in rows {
        let face = row?;
        out.insert(face.id, face);
    }
    Ok(out)
}

fn prior_labels(conn: &Connection) -> AppResult<HashMap<i64, String>> {
    let mut stmt = conn.prepare("SELECT id, label FROM face_clusters WHERE label IS NOT NULL")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, label) = row?;
        out.insert(id, label);
    }
    Ok(out)
}

fn cluster_components(
    faces: &HashMap<i64, Face>,
    embeddings: &[(i64, Vec<f32>)],
    threshold: f32,
) -> Vec<Vec<i64>> {
    let mut index_by_face = HashMap::new();
    for (index, (face_id, _)) in embeddings.iter().enumerate() {
        index_by_face.insert(*face_id, index);
    }
    let mut parent = (0..embeddings.len()).collect::<Vec<_>>();
    for i in 0..embeddings.len() {
        for j in (i + 1)..embeddings.len() {
            if cosine(&embeddings[i].1, &embeddings[j].1) >= threshold {
                union(&mut parent, i, j);
            }
        }
    }

    let mut by_root: HashMap<usize, Vec<i64>> = HashMap::new();
    for (index, (face_id, _)) in embeddings.iter().enumerate() {
        by_root
            .entry(find(&mut parent, index))
            .or_default()
            .push(*face_id);
    }
    for face_id in faces.keys() {
        if !index_by_face.contains_key(face_id) {
            by_root
                .entry(embeddings.len() + *face_id as usize)
                .or_default()
                .push(*face_id);
        }
    }
    by_root
        .into_values()
        .filter(|group| !group.is_empty())
        .collect()
}

fn refresh_cluster_counts_in_tx(tx: &rusqlite::Transaction<'_>) -> AppResult<()> {
    let ids = {
        let mut stmt = tx.prepare("SELECT id FROM face_clusters")?;
        let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        ids
    };
    for id in ids {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM faces WHERE cluster_id = ?1",
            [id],
            |row| row.get(0),
        )?;
        let sample_image_id: Option<i64> = tx
            .query_row(
                "SELECT image_id FROM faces WHERE cluster_id = ?1 ORDER BY confidence DESC LIMIT 1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        tx.execute(
            "UPDATE face_clusters SET face_count = ?1, sample_image_id = ?2 WHERE id = ?3",
            params![count, sample_image_id, id],
        )?;
    }
    Ok(())
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut a_norm = 0.0;
    let mut b_norm = 0.0;
    for (left, right) in a.iter().zip(b.iter()) {
        dot += left * right;
        a_norm += left * left;
        b_norm += right * right;
    }
    if a_norm <= f32::EPSILON || b_norm <= f32::EPSILON {
        0.0
    } else {
        dot / (a_norm.sqrt() * b_norm.sqrt())
    }
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let root_a = find(parent, a);
    let root_b = find(parent, b);
    if root_a != root_b {
        parent[root_b] = root_a;
    }
}

fn find(parent: &mut [usize], value: usize) -> usize {
    if parent[value] != value {
        let root = find(parent, parent[value]);
        parent[value] = root;
    }
    parent[value]
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
