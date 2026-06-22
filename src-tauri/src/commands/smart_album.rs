use tauri::State;

use crate::db::Pool;
use crate::repo::images_repo::{self, ImagePage, ImageQueryParams};
use crate::repo::smart_albums_repo::{self, SmartAlbum, SmartAlbumInput};

#[tauri::command]
pub fn smart_album_save(
    pool: State<'_, Pool>,
    input: SmartAlbumInput,
) -> Result<SmartAlbum, String> {
    if input.name.trim().is_empty() {
        return Err("智能相册名称不能为空".to_string());
    }
    serde_json::from_str::<serde_json::Value>(&input.filter_json)
        .map_err(|error| format!("筛选条件不是有效 JSON：{error}"))?;
    let conn = pool.get().map_err(|e| e.to_string())?;
    smart_albums_repo::save(&conn, &input).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn smart_album_list(pool: State<'_, Pool>) -> Result<Vec<SmartAlbum>, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    smart_albums_repo::list(&conn).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn smart_album_delete(pool: State<'_, Pool>, id: i64) -> Result<bool, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    smart_albums_repo::delete(&conn, id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn smart_album_query(
    pool: State<'_, Pool>,
    id: i64,
    offset: Option<i64>,
    limit: Option<i64>,
) -> Result<ImagePage, String> {
    let conn = pool.get().map_err(|e| e.to_string())?;
    let album = smart_albums_repo::get(&conn, id).map_err(|e| e.to_string())?;
    let mut params = serde_json::from_str::<ImageQueryParams>(&album.filter_json)
        .map_err(|error| format!("智能相册筛选条件无法解析：{error}"))?;
    params.offset = offset.or(params.offset);
    params.limit = limit.or(params.limit);
    images_repo::query(&conn, &params).map_err(|e| e.to_string())
}
