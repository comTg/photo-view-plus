//! T3 验收：repo + path normalize 联动。
//! 命令层（roots_add 等）依赖 Tauri State，不在集成测试覆盖，留给手工 / e2e。

use photo_view_plus_lib::*;
use repo::roots_repo::{self, NewRoot, RootPatch};
use tempfile::TempDir;

fn fresh() -> (db::Pool, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::open(&dir.path().join("app.sqlite")).unwrap();
    (pool, dir)
}

fn local(path: &str, label: Option<&str>) -> NewRoot {
    NewRoot {
        path: path.to_string(),
        label: label.map(str::to_string),
        root_type: "local".to_string(),
    }
}

#[test]
fn insert_and_get() {
    let (pool, _d) = fresh();
    let c = pool.get().unwrap();
    let r = roots_repo::insert(&c, &local("/tmp/a", Some("A")), 1000).unwrap();
    assert!(r.id > 0);
    assert_eq!(r.path, "/tmp/a");
    assert_eq!(r.label.as_deref(), Some("A"));
    assert_eq!(r.root_type, "local");
    assert!(r.enabled);
    assert_eq!(r.created_at, 1000);

    let again = roots_repo::get(&c, r.id).unwrap();
    assert_eq!(again.map(|x| x.id), Some(r.id));
}

#[test]
fn list_ordered_by_created_at() {
    let (pool, _d) = fresh();
    let c = pool.get().unwrap();
    roots_repo::insert(&c, &local("/tmp/b", None), 200).unwrap();
    roots_repo::insert(&c, &local("/tmp/a", None), 100).unwrap();
    roots_repo::insert(&c, &local("/tmp/c", None), 300).unwrap();

    let list = roots_repo::list(&c).unwrap();
    assert_eq!(list.len(), 3);
    let paths: Vec<_> = list.iter().map(|r| r.path.as_str()).collect();
    // 按 created_at ASC：100 → 200 → 300，不是按插入顺序
    assert_eq!(paths, vec!["/tmp/a", "/tmp/b", "/tmp/c"]);
}

#[test]
fn find_by_path() {
    let (pool, _d) = fresh();
    let c = pool.get().unwrap();
    roots_repo::insert(&c, &local("/tmp/x", None), 0).unwrap();
    assert!(roots_repo::find_by_path(&c, "/tmp/x").unwrap().is_some());
    assert!(roots_repo::find_by_path(&c, "/tmp/y").unwrap().is_none());
}

#[test]
fn duplicate_path_rejected_at_db_level() {
    let (pool, _d) = fresh();
    let c = pool.get().unwrap();
    roots_repo::insert(&c, &local("/tmp/dup", None), 0).unwrap();
    let err = roots_repo::insert(&c, &local("/tmp/dup", None), 0);
    assert!(err.is_err(), "UNIQUE(path) must reject duplicate");
}

#[test]
fn update_partial_fields() {
    let (pool, _d) = fresh();
    let c = pool.get().unwrap();
    let r = roots_repo::insert(&c, &local("/tmp/u", Some("old")), 0).unwrap();

    let updated = roots_repo::update(
        &c,
        r.id,
        &RootPatch {
            label: Some("new".to_string()),
            enabled: Some(false),
            settings_json: None,
        },
    )
    .unwrap()
    .expect("row should exist");
    assert_eq!(updated.label.as_deref(), Some("new"));
    assert!(!updated.enabled);
    assert!(updated.settings_json.is_none());
}

#[test]
fn update_empty_patch_is_noop() {
    let (pool, _d) = fresh();
    let c = pool.get().unwrap();
    let r = roots_repo::insert(&c, &local("/tmp/np", Some("k")), 0).unwrap();
    let same = roots_repo::update(&c, r.id, &RootPatch::default()).unwrap();
    assert_eq!(same.map(|x| x.label), Some(Some("k".to_string())));
}

#[test]
fn remove_works() {
    let (pool, _d) = fresh();
    let c = pool.get().unwrap();
    let r = roots_repo::insert(&c, &local("/tmp/r", None), 0).unwrap();
    assert!(roots_repo::remove(&c, r.id).unwrap());
    assert!(roots_repo::get(&c, r.id).unwrap().is_none());
    // 二次删除返回 false
    assert!(!roots_repo::remove(&c, r.id).unwrap());
}

#[test]
fn touch_last_scan_updates_timestamp() {
    let (pool, _d) = fresh();
    let c = pool.get().unwrap();
    let r = roots_repo::insert(&c, &local("/tmp/t", None), 0).unwrap();
    assert!(r.last_scan_at.is_none());
    roots_repo::touch_last_scan(&c, r.id, 12345).unwrap();
    let after = roots_repo::get(&c, r.id).unwrap().unwrap();
    assert_eq!(after.last_scan_at, Some(12345));
}
