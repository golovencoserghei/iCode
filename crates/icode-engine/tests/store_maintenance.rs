//! On-disk housekeeping of the per-project index: the WAL must not stay at its
//! high-water mark after a write burst, and the journal size limit must be set.

use std::fs;

use icode_engine::{index_path, should_vacuum, SqliteCodeStore};

fn write_tree(root: &std::path::Path, files: usize, tag: &str) {
    for i in 0..files {
        let body: String = (0..10)
            .map(|j| format!("pub fn f{i}_{j}_{tag}(x: u32) -> u32 {{ x + {j} }}\n"))
            .collect();
        fs::write(root.join(format!("m{i}.rs")), body).unwrap();
    }
}

fn wal_len(root: &std::path::Path) -> u64 {
    fs::metadata(root.join(".icode/index.db-wal")).map(|m| m.len()).unwrap_or(0)
}

#[test]
fn a_full_pass_that_writes_leaves_an_empty_wal_behind() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write_tree(&root, 200, "a");
    // The store stays open across both passes, as it does in a daemon or a
    // server: closing it would checkpoint on its own and prove nothing.
    let store = SqliteCodeStore::open(&root).unwrap();
    index_path(&root, &store).unwrap();
    assert_eq!(wal_len(&root), 0, "first pass must truncate the WAL it grew");

    write_tree(&root, 200, "b");
    let stats = index_path(&root, &store).unwrap();
    assert_eq!(stats.files_indexed, 200);
    assert_eq!(wal_len(&root), 0, "a re-index of 200 files must truncate the WAL");
}

#[test]
fn checkpoint_truncate_reports_success_on_an_idle_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteCodeStore::open(dir.path()).unwrap();
    assert!(store.checkpoint_truncate());
}

#[test]
fn vacuum_only_after_a_large_purge() {
    assert!(!should_vacuum(0, 0));
    assert!(!should_vacuum(10_000, 0));
    assert!(!should_vacuum(10_000, 499), "under both thresholds");
    assert!(should_vacuum(10_000, 500), "500 files is large in absolute terms");
    assert!(should_vacuum(100, 20), "20% of the index");
    assert!(!should_vacuum(100, 19));
    assert!(should_vacuum(5, 5), "a tree that emptied out");
}

#[test]
fn a_pass_that_purges_most_of_the_tree_shrinks_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write_tree(&root, 300, "a");
    let store = SqliteCodeStore::open(&root).unwrap();
    index_path(&root, &store).unwrap();
    let db = root.join(".icode/index.db");
    let before = fs::metadata(&db).unwrap().len();

    for i in 10..300 {
        fs::remove_file(root.join(format!("m{i}.rs"))).unwrap();
    }
    let stats = index_path(&root, &store).unwrap();
    assert_eq!(stats.files_deleted, 290);
    let after = fs::metadata(&db).unwrap().len();
    assert!(after * 2 < before, "VACUUM must give the space back: {before} -> {after} bytes");
    assert_eq!(wal_len(&root), 0, "and the VACUUM's WAL must be truncated");
}
