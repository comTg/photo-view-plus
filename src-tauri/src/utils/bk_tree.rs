//! 64-bit dHash 的 BK-tree 索引。
//!
//! `bk-tree` crate 已经把树本身写好了，我们只需要：
//! 1. 给它一个 hamming 度量（`Metric` trait 实现）
//! 2. 把 `(image_id, dhash)` 一并塞进去，查询时返回 image_id
//!
//! 复杂度：典型 O(log N) 查询；100 万 phash 内存 ~24MB（u64 + i64 + 节点指针），可接受。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use bk_tree::{BKTree, Metric};

/// 节点：把 image_id 和 dhash 绑在一起，让查询直接拿 id。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DhashEntry {
    pub image_id: i64,
    pub dhash: u64,
}

/// Hamming distance over 64-bit dHash。
#[derive(Default, Debug, Clone, Copy)]
pub struct Hamming;

impl Metric<DhashEntry> for Hamming {
    fn distance(&self, a: &DhashEntry, b: &DhashEntry) -> u32 {
        (a.dhash ^ b.dhash).count_ones()
    }

    fn threshold_distance(&self, a: &DhashEntry, b: &DhashEntry, _: u32) -> Option<u32> {
        Some(self.distance(a, b))
    }
}

/// 并发安全的 BK-tree state。Tauri 把它放进 `app.manage`，命令层用 `State<DhashIndex>`。
pub struct DhashIndex {
    tree: Mutex<BKTree<DhashEntry, Hamming>>,
    size: AtomicUsize,
}

impl Default for DhashIndex {
    fn default() -> Self {
        Self {
            tree: Mutex::new(BKTree::new(Hamming)),
            size: AtomicUsize::new(0),
        }
    }
}

impl DhashIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从一组 (image_id, dhash) 批量建树。启动时调用一次。
    pub fn rebuild_from(&self, entries: impl IntoIterator<Item = (i64, u64)>) {
        let mut guard = self.lock_tree();
        *guard = BKTree::new(Hamming);
        let mut count = 0usize;
        for (image_id, dhash) in entries {
            guard.add(DhashEntry { image_id, dhash });
            count += 1;
        }
        self.size.store(count, Ordering::Relaxed);
    }

    /// 增量插入。phash_service 完成单张图后调用。
    pub fn insert(&self, image_id: i64, dhash: u64) {
        self.lock_tree().add(DhashEntry { image_id, dhash });
        self.size.fetch_add(1, Ordering::Relaxed);
    }

    /// 查找 hamming distance ≤ max_dist 的全部条目。返回 `(image_id, distance)`。
    /// 调用方需自行过滤掉 query 对应的 image_id。
    pub fn find_within(&self, query: u64, max_dist: u32) -> Vec<(i64, u32)> {
        let needle = DhashEntry {
            image_id: 0,
            dhash: query,
        };
        let guard = self.lock_tree();
        guard
            .find(&needle, max_dist)
            .map(|(dist, entry)| (entry.image_id, dist))
            .collect()
    }

    /// 树中节点数（含重复 dhash）。诊断 / 日志用。
    pub fn len(&self) -> usize {
        self.size.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock_tree(&self) -> MutexGuard<'_, BKTree<DhashEntry, Hamming>> {
        self.tree.lock().expect("DhashIndex mutex poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hamming_counts_bit_diff() {
        let a = DhashEntry {
            image_id: 1,
            dhash: 0b0000,
        };
        let b = DhashEntry {
            image_id: 2,
            dhash: 0b1010,
        };
        assert_eq!(Hamming.distance(&a, &b), 2);
    }

    #[test]
    fn find_within_returns_close_matches() {
        let index = DhashIndex::new();
        index.rebuild_from([
            (1, 0b0000_0000_0000_0000),
            (2, 0b0000_0000_0000_0011), // distance 2
            (3, 0b1111_1111_1111_1111), // distance 16
        ]);

        let matches = index.find_within(0b0000_0000_0000_0000, 3);
        let ids: Vec<i64> = matches.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(!ids.contains(&3));
    }

    #[test]
    fn incremental_insert_visible_in_query() {
        let index = DhashIndex::new();
        index.insert(1, 0);
        assert_eq!(index.find_within(0, 0).len(), 1);
        index.insert(2, 1);
        assert_eq!(index.find_within(0, 1).len(), 2);
    }

    #[test]
    fn len_reports_node_count() {
        let index = DhashIndex::new();
        assert!(index.is_empty());
        index.insert(1, 0);
        index.insert(2, 100);
        assert_eq!(index.len(), 2);
    }
}
