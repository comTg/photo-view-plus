use std::sync::Arc;

use serde::Deserialize;
use tauri::State;

use crate::config::AppPaths;
use crate::db::Pool;
use crate::queue::Scheduler;
use crate::repo::face_repo::{self, Face, FaceCluster};
use crate::repo::images_repo::ImagePage;
use crate::repo::lancedb_repo::LanceDbRepo;
use crate::services::ai_supervisor::AiSupervisor;
use crate::services::{face_service, settings_service};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FaceRunArgs {
    pub image_ids: Option<Vec<i64>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameClusterArgs {
    pub cluster_id: i64,
    pub label: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeClustersArgs {
    pub from_cluster_id: i64,
    pub to_cluster_id: i64,
}

#[tauri::command]
pub fn face_run(
    pool: State<'_, Pool>,
    scheduler: State<'_, Scheduler>,
    supervisor: State<'_, Arc<AiSupervisor>>,
    paths: State<'_, AppPaths>,
    args: Option<FaceRunArgs>,
) -> Result<usize, String> {
    let settings = settings_service::read(&paths).map_err(|e| e.to_string())?;
    if !settings.face_enabled {
        return Err("人脸识别默认关闭，请先在设置 > 隐私中启用人脸识别".to_string());
    }
    let vectors = LanceDbRepo::new(paths.vectors_dir.clone());
    face_service::enqueue_face_detection(
        &pool,
        &scheduler,
        supervisor.inner().clone(),
        vectors,
        paths.thumbs_dir.clone(),
        args.and_then(|args| args.image_ids),
    )
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn faces_cluster(
    pool: State<'_, Pool>,
    paths: State<'_, AppPaths>,
) -> Result<usize, String> {
    let vectors = LanceDbRepo::new(paths.vectors_dir.clone());
    face_service::rebuild_clusters(&pool, &vectors)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn face_status(pool: State<'_, Pool>) -> Result<face_service::FaceStatus, String> {
    face_service::status(&pool).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn faces_list_clusters(pool: State<'_, Pool>) -> Result<Vec<FaceCluster>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    face_repo::list_clusters(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn faces_for_image(pool: State<'_, Pool>, image_id: i64) -> Result<Vec<Face>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    face_repo::faces_for_image(&conn, image_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn faces_images_for_cluster(
    pool: State<'_, Pool>,
    cluster_id: i64,
    offset: Option<i64>,
    limit: Option<i64>,
) -> Result<ImagePage, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    face_repo::images_for_cluster(&conn, cluster_id, offset.unwrap_or(0), limit.unwrap_or(200))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn face_cluster_rename(pool: State<'_, Pool>, args: RenameClusterArgs) -> Result<(), String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    face_repo::rename_cluster(&conn, args.cluster_id, args.label).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn face_clusters_merge(pool: State<'_, Pool>, args: MergeClustersArgs) -> Result<(), String> {
    let mut conn = pool.get().map_err(|e| e.to_string())?;
    face_repo::merge_clusters(&mut conn, args.from_cluster_id, args.to_cluster_id)
        .map_err(|e| e.to_string())
}
