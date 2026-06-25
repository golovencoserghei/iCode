//! A per-project `.icode/index.db` left by an older iCode schema (or a different
//! tool that reused the path) must be discarded and rebuilt, not crash on a
//! schema mismatch. Regression for the stale-db crash found during M1 acceptance.

use icode_core::model::{CodeQuery, SearchMode, SymbolKind};
use icode_core::traits::CodeReadStore;
use icode_engine::SqliteCodeStore;
use rusqlite::Connection;
use std::fs;

#[test]
fn incompatible_db_is_rebuilt_not_crashed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let icode = root.join(".icode");
    fs::create_dir_all(&icode).unwrap();
    let db = icode.join("index.db");

    // Stale/foreign db: a `classes` table WITHOUT `qualified_name`, user_version
    // left at 0 (a different schema generation than ours).
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE classes (id INTEGER PRIMARY KEY, name TEXT);")
            .unwrap();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 0, "precondition: foreign db has user_version 0");
    }

    // Opening must NOT crash — it discards the incompatible db and rebuilds.
    let store = SqliteCodeStore::open(root).expect("open should rebuild an incompatible db");

    // Rebuilt db is empty and on the current schema (the missing `qualified_name`
    // column is back — a class search over the rebuilt FTS must not error).
    let stats = store.stats().expect("stats works on the rebuilt db");
    assert_eq!(stats.files, 0);
    let q = CodeQuery {
        text: "anything".into(),
        kind: Some(SymbolKind::Class),
        lang: None,
        limit: 5,
        mode: SearchMode::Lexical,
        with_body: false,
    };
    assert!(store.search_code(&q).is_ok(), "search over rebuilt schema must work");
}

#[test]
fn current_db_is_reused_across_opens() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // First open creates a v1 db; second open must reuse it (no crash, no wipe).
    let _ = SqliteCodeStore::open(root).unwrap();
    let store = SqliteCodeStore::open(root).expect("reopening a current db must work");
    assert_eq!(store.stats().unwrap().files, 0);
}
