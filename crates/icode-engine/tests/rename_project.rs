//! Project rename: memory must follow a renamed directory.
//!
//! Offline on purpose — a deterministic fake embedder stands in for Ollama, so
//! this runs in CI. It checks the three places a project name lives (the column,
//! the id suffix, the rowid bridge) and that search still finds the moved record.

use std::sync::Arc;

use icode_core::error::Result;
use icode_core::ids::MemoryId;
use icode_core::model::{AddOutcome, Category, NewMemory};
use icode_core::traits::{Embedder, ReadableMemoryStore, WritableMemoryStore};
use icode_engine::SqliteMemoryStore;
use rusqlite::Connection;

/// 1024-dim vector derived from the text: distinct texts get distinct directions.
struct FakeEmbedder;

impl Embedder for FakeEmbedder {
    fn model_id(&self) -> &str {
        "fake"
    }
    fn dim(&self) -> usize {
        1024
    }
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; 1024];
                for (i, b) in t.bytes().enumerate() {
                    v[(b as usize * 7 + i) % 1024] += 1.0;
                }
                v
            })
            .collect())
    }
    fn health(&self) -> Result<()> {
        Ok(())
    }
}

fn add(store: &SqliteMemoryStore, project: &str, content: &str) -> MemoryId {
    match store
        .add(NewMemory {
            project: project.into(),
            content: content.into(),
            category: Category::Context,
            tags: vec![],
            importance: 0.0,
            session_id: None,
        })
        .expect("add")
    {
        AddOutcome::Added { id } => id,
        AddOutcome::Duplicate { .. } => panic!("unexpected duplicate"),
    }
}

fn open(dir: &tempfile::TempDir) -> (SqliteMemoryStore, String) {
    let db = dir.path().join("icode.db").to_str().unwrap().to_string();
    (SqliteMemoryStore::open(&db, Arc::new(FakeEmbedder)).unwrap(), db)
}

#[test]
fn rename_moves_records_ids_bridge_and_registry() {
    let dir = tempfile::tempdir().unwrap();
    let (store, db) = open(&dir);
    let id = add(&store, "oldname", "chose sqlite for the local cache layer");
    add(&store, "unrelated", "something about another project entirely");
    store.touch_session("oldname", "s1").ok();

    // dry run changes nothing
    let r = store.rename_project("oldname", "newname", None, false, false).unwrap();
    assert_eq!(r.memories, 1);
    assert_eq!(store.list("oldname", None, 10, true).unwrap().len(), 1);

    let r = store
        .rename_project("oldname", "newname", Some("/work/newname"), false, true)
        .unwrap();
    assert_eq!(r.memories, 1);

    assert!(store.list("oldname", None, 10, true).unwrap().is_empty());
    let moved = store.list("newname", None, 10, true).unwrap();
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].id.project(), Some("newname"));
    assert_ne!(moved[0].id, id);
    assert!(store.get(&id).unwrap().is_none(), "old id is gone");

    // search (fts + vector through the bridge) still reaches the moved record
    let hits = store.search("newname", "sqlite local cache", 5, None, false).unwrap();
    assert!(hits.iter().any(|h| h.record.id == moved[0].id), "found via bridge");

    // other projects untouched; bridge and tables stay in lock-step
    assert_eq!(store.list("unrelated", None, 10, true).unwrap().len(), 1);
    let conn = Connection::open(&db).unwrap();
    let n = |t: &str| -> i64 { conn.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0)).unwrap() };
    assert_eq!(n("memories"), n("mem_rowid"));
    assert_eq!(n("memories"), n("vec_memory"));
    assert_eq!(n("memories"), n("fts_memory"));
    let (name, root): (String, Option<String>) = conn
        .query_row("SELECT name, root_path FROM projects WHERE name = 'newname'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((name.as_str(), root.as_deref()), ("newname", Some("/work/newname")));
    let old_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM projects WHERE name = 'oldname'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(old_rows, 0);
}

#[test]
fn rename_refuses_existing_target_unless_merge() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _) = open(&dir);
    add(&store, "a", "alpha fact about the build pipeline");
    add(&store, "b", "beta fact about deployment targets");

    assert!(store.rename_project("a", "b", None, false, true).is_err());
    assert_eq!(store.list("a", None, 10, true).unwrap().len(), 1, "refusal wrote nothing");

    store.rename_project("a", "b", None, true, true).unwrap();
    assert_eq!(store.list("b", None, 10, true).unwrap().len(), 2);
}

#[test]
fn rename_rejects_bad_names_and_unknown_projects() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _) = open(&dir);
    add(&store, "a", "some fact");
    for bad in ["", "has space", "a/b", "__reserved", "a"] {
        assert!(store.rename_project("a", bad, None, false, true).is_err(), "{bad:?}");
    }
    assert!(store.rename_project("ghost", "x", None, false, true).is_err());
}
