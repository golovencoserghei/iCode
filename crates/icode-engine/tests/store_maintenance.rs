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

fn parse_error_rows(root: &std::path::Path) -> i64 {
    let conn = rusqlite::Connection::open(root.join(".icode/index.db")).unwrap();
    conn.query_row("SELECT COUNT(*) FROM parse_errors", [], |r| r.get(0)).unwrap()
}

#[test]
fn a_failing_file_is_recorded_once_per_version() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let bad = root.join("bad.rs");
    fs::write(&bad, b"fn f() {}\n\xff\xfe not utf-8\n").unwrap();
    let store = SqliteCodeStore::open(&root).unwrap();
    let key = bad.to_string_lossy().to_string();

    // The daemon path: same bytes failing on every event.
    let fp = icode_engine::error_fingerprint(&bad);
    assert!(fp.starts_with("sha256:"), "raw bytes are hashable even when not UTF-8: {fp}");
    assert!(store.record_parse_error_once(&key, &fp, "invalid utf-8").unwrap());
    for _ in 0..5 {
        assert!(!store.record_parse_error_once(&key, &fp, "invalid utf-8").unwrap(), "a repeat is not news");
    }
    assert_eq!(parse_error_rows(&root), 1);

    // Full passes over the unchanged file do not pile up rows either.
    index_path(&root, &store).unwrap();
    index_path(&root, &store).unwrap();
    assert_eq!(parse_error_rows(&root), 1);

    // New bytes, still broken: recorded again, replacing the old row.
    fs::write(&bad, b"fn g() {}\n\xff still not utf-8\n").unwrap();
    let fp2 = icode_engine::error_fingerprint(&bad);
    assert_ne!(fp, fp2);
    assert!(store.record_parse_error_once(&key, &fp2, "invalid utf-8").unwrap());
    assert_eq!(parse_error_rows(&root), 1);
}

#[test]
fn an_index_from_before_the_fingerprint_column_gains_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    drop(SqliteCodeStore::open(&root).unwrap());
    {
        // Rebuild the table in its old shape, with an old-style row in it.
        let conn = rusqlite::Connection::open(root.join(".icode/index.db")).unwrap();
        conn.execute_batch(
            "DROP TABLE parse_errors; \
             CREATE TABLE parse_errors (id INTEGER PRIMARY KEY, path TEXT NOT NULL, error TEXT NOT NULL); \
             INSERT INTO parse_errors (path, error) VALUES ('/x.rs', 'old');",
        )
        .unwrap();
    }
    let store = SqliteCodeStore::open(&root).unwrap();
    assert!(store.record_parse_error_once("/x.rs", "sha256:abc", "new").unwrap());
    assert_eq!(parse_error_rows(&root), 1, "the old duplicate rows for the path are replaced");
}

#[test]
fn reconcile_by_stat_repairs_missed_creates_edits_and_deletes() {
    use icode_core::traits::CodeReadStore;
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write_tree(&root, 20, "a");
    let store = SqliteCodeStore::open(&root).unwrap();
    index_path(&root, &store).unwrap();

    let s = icode_engine::reconcile_by_stat(&root, &store, &root).unwrap();
    assert_eq!((s.files_indexed, s.files_deleted, s.files_skipped), (0, 0, 20), "unchanged tree");

    fs::write(root.join("new.rs"), "pub fn brand_new() {}\n").unwrap();
    fs::write(root.join("m0.rs"), "pub fn edited_and_longer_than_before_by_far() {}\n".repeat(30)).unwrap();
    fs::remove_file(root.join("m1.rs")).unwrap();
    let s = icode_engine::reconcile_by_stat(&root, &store, &root).unwrap();
    assert_eq!((s.files_indexed, s.files_deleted), (2, 1));
    let key = |n: &str| root.join(n).to_string_lossy().to_string();
    assert!(store.stat_file(&key("new.rs")).unwrap().is_some());
    assert!(store.stat_file(&key("m1.rs")).unwrap().is_none());

    // A subtree: a directory moved in, then moved out again.
    let outside = tempfile::tempdir().unwrap();
    let pkg = outside.path().join("pkg");
    fs::create_dir_all(pkg.join("inner")).unwrap();
    fs::write(pkg.join("x.rs"), "pub fn x() {}\n").unwrap();
    fs::write(pkg.join("inner/y.rs"), "pub fn y() {}\n").unwrap();
    fs::rename(&pkg, root.join("pkg")).unwrap();
    let s = icode_engine::reconcile_by_stat(&root, &store, &root.join("pkg")).unwrap();
    assert_eq!(s.files_indexed, 2);
    fs::rename(root.join("pkg"), outside.path().join("back")).unwrap();
    let s = icode_engine::reconcile_by_stat(&root, &store, &root.join("pkg")).unwrap();
    assert_eq!(s.files_deleted, 2);
    assert_eq!(store.files_under(&root.to_string_lossy()).unwrap().len(), 20, "the rest is intact");
}

#[test]
fn concurrent_first_opens_of_an_old_index_all_succeed() {
    for round in 0..5 {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        drop(SqliteCodeStore::open(&root).unwrap());
        {
            let conn = rusqlite::Connection::open(root.join(".icode/index.db")).unwrap();
            conn.execute_batch(
                "DROP TABLE parse_errors; \
                 CREATE TABLE parse_errors (id INTEGER PRIMARY KEY, path TEXT NOT NULL, error TEXT NOT NULL);",
            )
            .unwrap();
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (root, barrier) = (root.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    SqliteCodeStore::open(&root).map(|_| ()).map_err(|e| e.to_string())
                })
            })
            .collect();
        let errors: Vec<String> =
            handles.into_iter().filter_map(|h| h.join().unwrap().err()).collect();
        assert!(errors.is_empty(), "round {round}: {errors:?}");
    }
}

#[test]
fn parse_errors_clear_once_the_file_indexes_or_disappears() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::write(root.join("a.rs"), b"fn a() {}\n\xff\n").unwrap();
    fs::write(root.join("b.rs"), b"fn b() {}\n\xff\n").unwrap();
    let store = SqliteCodeStore::open(&root).unwrap();
    index_path(&root, &store).unwrap();
    assert_eq!(parse_error_rows(&root), 2);

    fs::write(root.join("a.rs"), "fn a() {}\n").unwrap(); // fixed
    fs::remove_file(root.join("b.rs")).unwrap(); // gone, and never was in `files`
    index_path(&root, &store).unwrap();
    assert_eq!(parse_error_rows(&root), 0);
}
