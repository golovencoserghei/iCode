//! End-to-end LIVE test for the central memory store (M4 base). Requires a
//! running Ollama with the configured embedding model — like `embed_e2e`, it is
//! deliberately NOT `#[ignore]`: it proves the full memory pipeline
//! (add → embed → vec0 + fts → RRF search → list/get/delete/resolve) and the
//! orphan-free invariant of the mem_rowid bridge.
//!
//! `icode-embed` is a DEV-dependency only; production `icode-engine` depends on
//! the `Embedder` trait from `icode-core`, never on a concrete embedder crate.

use std::sync::Arc;

use icode_core::config::EmbedConfig;
use icode_core::ids::MemoryId;
use icode_core::model::{AddOutcome, Category, NewMemory};
use icode_core::traits::{Embedder, ReadableMemoryStore, WritableMemoryStore};
use icode_embed::OllamaEmbedder;
use icode_engine::SqliteMemoryStore;
use rusqlite::Connection;

/// Build the live Ollama embedder (the test asserts it is reachable up front).
fn embedder() -> Arc<dyn Embedder> {
    let cfg = EmbedConfig::default();
    let e = OllamaEmbedder::new(&cfg).expect("build OllamaEmbedder");
    e.health()
        .expect("Ollama must be running with the embedding model for memory_store");
    Arc::new(e)
}

fn add(store: &SqliteMemoryStore, project: &str, content: &str, cat: Category) -> MemoryId {
    match store
        .add(NewMemory {
            project: project.into(),
            content: content.into(),
            category: cat,
            tags: vec![],
            importance: 0.0,
            session_id: None,
        })
        .expect("add")
    {
        AddOutcome::Added { id } => id,
        AddOutcome::Duplicate { existing, .. } => {
            panic!("expected Added, got Duplicate({existing}) — dedup is M4.2")
        }
    }
}

/// Direct-SQL invariant check: vec_memory, fts_memory, mem_rowid, and memories all
/// hold the SAME row count (no orphan vector/fts/bridge row after a delete).
fn assert_orphan_free(db_path: &str) {
    let conn = Connection::open(db_path).expect("reopen db for invariant check");
    let count = |t: &str| -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
            .expect("count")
    };
    let m = count("memories");
    let v = count("vec_memory");
    let f = count("fts_memory");
    let b = count("mem_rowid");
    assert_eq!(m, v, "memories ({m}) == vec_memory ({v})");
    assert_eq!(m, f, "memories ({m}) == fts_memory ({f})");
    assert_eq!(m, b, "memories ({m}) == mem_rowid ({b})");
}

#[test]
fn memory_store_full_lifecycle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("icode.db");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let store = SqliteMemoryStore::open(&db_path_str, embedder()).expect("open store");

    // ── add 3 memories to "demo" + 1 to "other" ──
    let auth_id = add(
        &store,
        "demo",
        "fixed auth race condition with a mutex in the login middleware",
        Category::Bug,
    );
    let _pg_id = add(
        &store,
        "demo",
        "chose postgres over sqlite for the production database",
        Category::Decision,
    );
    let todo_id = add(&store, "demo", "todo: add rate limiting to the API", Category::Todo);
    let _other_id = add(&store, "other", "refactored the email templating module", Category::Progress);

    // ── search: the auth race memory must top "race condition bug" ──
    let hits = store
        .search("demo", "race condition bug", 5, None, false)
        .expect("search");
    assert!(!hits.is_empty(), "search returned hits");
    assert_eq!(
        hits[0].record.id, auth_id,
        "top hit for 'race condition bug' must be the auth race memory, got {:?}: {}",
        hits[0].record.id, hits[0].record.content
    );
    // Annotated: a just-created memory is Fresh, 0 days old.
    assert_eq!(hits[0].age_days, 0, "freshly added → 0 days");

    // ── list("demo") → exactly the 3 demo memories ──
    let demo_list = store.list("demo", None, 100, false).expect("list demo");
    assert_eq!(demo_list.len(), 3, "demo has 3 memories, got {}", demo_list.len());

    // ── get(id) round-trips ──
    let got = store.get(&auth_id).expect("get").expect("auth memory exists");
    assert_eq!(got.id, auth_id);
    assert_eq!(got.project, "demo");
    assert_eq!(got.category, Category::Bug);
    assert!(got.content.contains("race condition"));

    // ── search_all finds the postgres decision across projects ──
    let all_hits = store.search_all("database choice", 5, false).expect("search_all");
    assert!(
        all_hits.iter().any(|h| h.record.content.contains("postgres")),
        "search_all('database choice') should surface the postgres decision, got {:?}",
        all_hits.iter().map(|h| &h.record.content).collect::<Vec<_>>()
    );

    // ── list_projects → demo (3) + other (1), reserved excluded ──
    let mut projects = store.list_projects().expect("list_projects");
    projects.sort();
    assert_eq!(
        projects,
        vec![("demo".to_string(), 3u64), ("other".to_string(), 1u64)],
        "list_projects returns demo+other with counts"
    );

    // ── resolve(todo) → search without include_resolved must NOT return it ──
    store.resolve(&todo_id, "shipped the rate limiter").expect("resolve");
    let active_hits = store
        .search("demo", "rate limiting api", 5, None, false)
        .expect("search after resolve");
    assert!(
        !active_hits.iter().any(|h| h.record.id == todo_id),
        "resolved todo must be hidden from a non-include_resolved search"
    );
    // include_resolved=true brings it back.
    let resolved_hits = store
        .search("demo", "rate limiting api", 5, None, true)
        .expect("search include_resolved");
    assert!(
        resolved_hits.iter().any(|h| h.record.id == todo_id),
        "include_resolved=true must surface the resolved todo"
    );

    // ── delete(auth) → gone, and the bridge stays orphan-free ──
    store.delete(&auth_id).expect("delete");
    assert!(store.get(&auth_id).expect("get after delete").is_none(), "deleted memory is gone");
    assert_eq!(
        store.list("demo", None, 100, true).expect("list after delete").len(),
        2,
        "demo down to 2 after deleting the auth memory"
    );

    // The store keeps a WAL connection open; checkpoint so the invariant reader on
    // a fresh connection sees committed rows.
    drop(store);
    assert_orphan_free(&db_path_str);
}
