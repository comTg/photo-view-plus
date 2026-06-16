//! MVP2 去重端到端集成测试。
//!
//! 用例：
//! 1. migration 0002 幂等
//! 2. exact 分组：10 对相同 blake3 → 恰好 10 组
//! 3. visual 分组：合成 30 组 dhash fixture，F1 ≥ 0.92
//!
//! fixture 是代码生成的（不依赖外部图），保证 CI 上可重现。

use std::collections::HashMap;

use photo_view_plus_lib::repo::images_repo::{self, NewImageMetadata};
use photo_view_plus_lib::repo::roots_repo::{self, NewRoot};
use photo_view_plus_lib::repo::{duplicates_repo, images_repo as ir};
use photo_view_plus_lib::services::dedup_service::{
    self, DedupCoordinator, VISUAL_THRESHOLD_DEFAULT,
};
use photo_view_plus_lib::utils::bk_tree::DhashIndex;
use photo_view_plus_lib::*;

fn fresh() -> (db::Pool, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::open(&dir.path().join("app.sqlite")).unwrap();
    (pool, dir)
}

fn make_root(conn: &rusqlite::Connection, path: &str) -> i64 {
    roots_repo::insert(
        conn,
        &NewRoot {
            path: path.to_string(),
            label: None,
            root_type: "local".to_string(),
        },
        1,
    )
    .unwrap()
    .id
}

fn add_image(conn: &rusqlite::Connection, root_id: i64, name: &str) -> i64 {
    let outcome = images_repo::upsert_scanned_image(
        conn,
        &NewImageMetadata {
            root_id,
            rel_path: name.to_string(),
            filename: name.to_string(),
            extension: "jpg".to_string(),
            size_bytes: 1,
            mtime: 1,
            width: None,
            height: None,
            orientation: None,
            taken_at: None,
            gps_lat: None,
            gps_lng: None,
            camera_make: None,
            camera_model: None,
        },
        1,
    )
    .unwrap();
    outcome.image_id()
}

#[test]
fn test_001_migration_0002_adds_hash_columns() {
    let (pool, _dir) = fresh();
    let conn = pool.get().unwrap();
    // schema_version 应该至少为 2
    let v: u32 = conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert!(v >= 2, "expected schema_version >= 2, got {v}");

    for table in ["duplicate_groups", "duplicate_items"] {
        assert!(
            db::table_exists(&conn, table).unwrap(),
            "table {table} should exist after 0002"
        );
    }

    // 写入新列应该工作
    let root = make_root(&conn, "/tmp/m0002");
    let id = add_image(&conn, root, "a.jpg");
    ir::set_blake3(&conn, id, "deadbeef").unwrap();
    ir::set_dhash(&conn, id, 0xCAFE_BABE_DEAD_BEEF).unwrap();
    let rec = ir::get_detail(&conn, id).unwrap().unwrap();
    assert_eq!(rec.blake3.as_deref(), Some("deadbeef"));
    assert_eq!(rec.dhash.map(|v| v as u64), Some(0xCAFE_BABE_DEAD_BEEF));
}

#[test]
fn test_002_migration_idempotent_on_existing_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.sqlite");
    let pool1 = db::open(&path).unwrap();
    let v1: u32 = pool1
        .get()
        .unwrap()
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    drop(pool1);

    let pool2 = db::open(&path).unwrap();
    let v2: u32 = pool2
        .get()
        .unwrap()
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v1, v2);
    assert!(v2 >= 2);
}

#[test]
fn test_003_exact_grouping_finds_all_pairs() {
    let (pool, _dir) = fresh();
    let conn = pool.get().unwrap();
    let root = make_root(&conn, "/tmp/exact");

    // 10 组完全重复：每组 2 张，blake3 相同
    let mut expected_pairs = Vec::new();
    for g in 0..10 {
        let h = format!("{:064x}", g);
        let a = add_image(&conn, root, &format!("g{g}_a.jpg"));
        let b = add_image(&conn, root, &format!("g{g}_b.jpg"));
        ir::set_blake3(&conn, a, &h).unwrap();
        ir::set_blake3(&conn, b, &h).unwrap();
        expected_pairs.push((a, b));
    }

    // 5 张孤立图（blake3 唯一）—— 不应进任何 group
    for i in 0..5 {
        let id = add_image(&conn, root, &format!("solo{i}.jpg"));
        ir::set_blake3(&conn, id, &format!("{:064x}", 100 + i)).unwrap();
    }

    let coord = DedupCoordinator::new();
    let mut events = 0;
    let n = dedup_service::run_exact_grouping_with(&conn, &coord, |_, m| {
        assert_eq!(m, "exact");
        events += 1;
    })
    .unwrap();

    assert_eq!(n, 10, "should find 10 exact groups");
    assert_eq!(events, 10);

    let page = duplicates_repo::list_groups(
        &conn,
        &duplicates_repo::GroupQueryParams {
            method: Some("exact".to_string()),
            status: Some("open".to_string()),
            offset: None,
            limit: None,
        },
    )
    .unwrap();
    assert_eq!(page.total, 10);
    for g in &page.items {
        assert_eq!(g.item_count, 2);
    }
}

/// 生成 30 组 dhash fixture：每组 base + 0-2 bit 翻转的变体；组间汉明距离远（≥ 32）。
fn synth_visual_fixture() -> Vec<Vec<u64>> {
    let mut rng_state: u64 = 0x1234_5678_9abc_def0;
    let next = |state: &mut u64| -> u64 {
        // xorshift64
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    };
    let mut bases: Vec<u64> = Vec::new();
    let mut groups = Vec::new();
    // 组内 hamming ≤ 2、阈值 = 2，所以组间 ≥ 10 就远超阈值（5 倍 margin）
    const MIN_INTER_GROUP_DIST: u32 = 10;
    for _ in 0..30 {
        let mut base = next(&mut rng_state);
        let mut tries = 0;
        while bases
            .iter()
            .any(|b: &u64| (*b ^ base).count_ones() < MIN_INTER_GROUP_DIST)
        {
            base = next(&mut rng_state);
            tries += 1;
            assert!(
                tries < 10_000,
                "couldn't find base with min dist {MIN_INTER_GROUP_DIST}"
            );
        }
        bases.push(base);

        let variants = 2 + (next(&mut rng_state) % 3); // 2-4 张
        let mut group = vec![base];
        for _ in 1..variants {
            // 翻 0-2 个 bit
            let mut v = base;
            let flips = next(&mut rng_state) % 3; // 0..=2
            for _ in 0..flips {
                let bit = next(&mut rng_state) % 64;
                v ^= 1u64 << bit;
            }
            group.push(v);
        }
        groups.push(group);
    }
    groups
}

#[test]
fn test_004_visual_grouping_f1_meets_threshold() {
    let (pool, _dir) = fresh();
    let conn = pool.get().unwrap();
    let root = make_root(&conn, "/tmp/visual");

    let fixture = synth_visual_fixture();
    let mut image_to_group: HashMap<i64, usize> = HashMap::new();
    let mut all_ids = Vec::new();
    let index = DhashIndex::new();

    for (gi, hashes) in fixture.iter().enumerate() {
        for (vi, hash) in hashes.iter().enumerate() {
            let id = add_image(&conn, root, &format!("g{gi}_v{vi}.jpg"));
            ir::set_dhash(&conn, id, *hash).unwrap();
            image_to_group.insert(id, gi);
            all_ids.push(id);
            index.insert(id, *hash);
        }
    }

    let coord = DedupCoordinator::new();
    let n = dedup_service::run_visual_grouping_with(
        &conn,
        &index,
        VISUAL_THRESHOLD_DEFAULT,
        &coord,
        |_, _| {},
    )
    .unwrap();
    assert!(n > 0, "should produce some visual groups");

    // 读出算法分组：image_id → algorithm_group_id
    let algo_groups_page = duplicates_repo::list_groups(
        &conn,
        &duplicates_repo::GroupQueryParams {
            method: Some("phash".to_string()),
            status: Some("open".to_string()),
            offset: None,
            limit: Some(500),
        },
    )
    .unwrap();
    let mut algo_group_of: HashMap<i64, i64> = HashMap::new();
    for g in &algo_groups_page.items {
        for item in duplicates_repo::items_for_group(&conn, g.id).unwrap() {
            algo_group_of.insert(item.image_id, g.id);
        }
    }

    // 对所有图对计算 TP/FP/FN
    let mut tp = 0usize;
    let mut fp = 0usize;
    let mut fnn = 0usize;
    for i in 0..all_ids.len() {
        for j in (i + 1)..all_ids.len() {
            let a = all_ids[i];
            let b = all_ids[j];
            let truth_same = image_to_group[&a] == image_to_group[&b];
            let algo_same = match (algo_group_of.get(&a), algo_group_of.get(&b)) {
                (Some(ga), Some(gb)) => ga == gb,
                _ => false,
            };
            match (truth_same, algo_same) {
                (true, true) => tp += 1,
                (false, true) => fp += 1,
                (true, false) => fnn += 1,
                (false, false) => {}
            }
        }
    }
    let precision = if tp + fp == 0 {
        1.0
    } else {
        tp as f64 / (tp + fp) as f64
    };
    let recall = if tp + fnn == 0 {
        1.0
    } else {
        tp as f64 / (tp + fnn) as f64
    };
    let f1 = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };

    eprintln!(
        "visual F1: precision={precision:.3} recall={recall:.3} F1={f1:.3} (tp={tp} fp={fp} fn={fnn})"
    );
    assert!(
        f1 >= 0.92,
        "F1 below threshold: precision={precision:.3} recall={recall:.3} F1={f1:.3}"
    );
}

#[test]
fn test_005_rerun_exact_replaces_open_groups() {
    let (pool, _dir) = fresh();
    let conn = pool.get().unwrap();
    let root = make_root(&conn, "/tmp/rerun");
    let a = add_image(&conn, root, "a.jpg");
    let b = add_image(&conn, root, "b.jpg");
    ir::set_blake3(&conn, a, "hash1").unwrap();
    ir::set_blake3(&conn, b, "hash1").unwrap();

    let coord = DedupCoordinator::new();
    let n1 = dedup_service::run_exact_grouping_with(&conn, &coord, |_, _| {}).unwrap();
    assert_eq!(n1, 1);
    let n2 = dedup_service::run_exact_grouping_with(&conn, &coord, |_, _| {}).unwrap();
    assert_eq!(n2, 1, "rerun should drop and rebuild");

    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM duplicate_groups WHERE method='exact'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(total, 1, "should not accumulate duplicate exact groups");
}
