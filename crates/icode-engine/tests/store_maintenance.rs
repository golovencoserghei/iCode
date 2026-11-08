//! On-disk housekeeping of the per-project index: the WAL must not stay at its
//! high-water mark after a write burst, and the journal size limit must be set.

use std::fs;

use icode_engine::{index_path, SqliteCodeStore};

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
