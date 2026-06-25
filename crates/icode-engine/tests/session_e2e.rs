//! End-to-end LIVE test for the session orchestration (`session_start` /
//! `session_end`). Like `memory_store.rs`, it needs a running Ollama with the
//! configured embedding model and is deliberately NOT `#[ignore]`: it proves the
//! full lifecycle — context priming on start, wrap-up persistence on end, and the
//! auto-resolve of a close bug/todo by a completed item.
//!
//! `icode-embed` is a DEV-dependency only; the session functions themselves take
//! a `&dyn MemoryStore` and never touch a concrete embedder crate.

use std::sync::Arc;

use icode_core::config::EmbedConfig;
use icode_core::ids::DEVELOPER_PROJECT;
use icode_core::model::{AddOutcome, Category, MemoryStatus, NewMemory};
use icode_core::traits::{Embedder, MemoryStore, ReadableMemoryStore, WritableMemoryStore};
use icode_embed::OllamaEmbedder;
use icode_engine::{session_end, session_start, SqliteMemoryStore};

/// Build the live Ollama embedder (asserts it is reachable up front).
fn embedder() -> Arc<dyn Embedder> {
    let cfg = EmbedConfig::default();
    let e = OllamaEmbedder::new(&cfg).expect("build OllamaEmbedder");
    e.health()
        .expect("Ollama must be running with the embedding model for session_e2e");
    Arc::new(e)
}

/// A fresh, isolated central memory db in a temp dir.
fn store() -> (tempfile::TempDir, SqliteMemoryStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("icode.db");
    let store = SqliteMemoryStore::open(path.to_str().unwrap(), embedder()).expect("open store");
    (dir, store)
}

fn add(store: &SqliteMemoryStore, project: &str, content: &str, cat: Category) -> icode_core::ids::MemoryId {
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
        AddOutcome::Duplicate { existing, .. } => existing,
    }
}

/// `session_start` returns the developer profile AND recent project context, and
/// warms up the surfaced ids (access_count > 0 afterwards).
#[test]
fn session_start_returns_profile_and_context() {
    let (_dir, store) = store();
    let project = "alpha";

    // Seed a profile note and a couple of project memories.
    let _ = add(&store, DEVELOPER_PROJECT, "always answer in Russian", Category::General);
    let ctx_id = add(&store, project, "API uses cursor-based pagination", Category::Context);
    let _ = add(&store, project, "chose JWT over sessions for auth", Category::Decision);

    let s = session_start(&store as &dyn MemoryStore, project, 8, 5).expect("session_start");

    assert!(
        !s.developer_profile.is_empty(),
        "developer_profile must be non-empty (seeded one note)"
    );
    assert!(
        s.developer_profile
            .iter()
            .any(|r| r.content.contains("Russian")),
        "profile contains the seeded note"
    );
    assert!(
        s.project_context.len() >= 2,
        "project_context returns the seeded project memories, got {}",
        s.project_context.len()
    );
    assert!(
        s.project_context.iter().all(|r| r.project == project),
        "project_context is scoped to the project"
    );

    // Warm-up: the context id was record_access'd, so its count is now > 0.
    let warmed = store.get(&ctx_id).expect("get").expect("exists");
    assert!(
        warmed.access_count >= 1,
        "session_start warms up surfaced ids (access_count = {})",
        warmed.access_count
    );
}

/// `session_end` saves the wrap-up (summary/decisions/completed/todos) and
/// auto-resolves an OPEN bug that a completed item is clearly about.
#[test]
fn session_end_saves_and_auto_resolves_open_bug() {
    let (_dir, store) = store();
    let project = "beta";

    // An open bug the session will fix.
    let bug_id = add(
        &store,
        project,
        "race condition in the auth middleware causes intermittent 401s under load",
        Category::Bug,
    );
    // An unrelated open todo that must STAY open (not closed by the completed item).
    let todo_id = add(
        &store,
        project,
        "add a dark-mode toggle to the settings page",
        Category::Todo,
    );

    let completed = vec![
        "Fixed the auth middleware race condition that caused intermittent 401s by adding a lock"
            .to_string(),
    ];
    let decisions = vec!["Chose a Mutex over a channel for the auth lock".to_string()];
    let todos = vec!["Still need to add rate limiting to the login endpoint".to_string()];

    let out = session_end(
        &store as &dyn MemoryStore,
        project,
        "Closed out the auth race condition and recorded the locking decision.",
        &completed,
        &decisions,
        &todos,
        Some("sess-1"),
    )
    .expect("session_end");

    // saved = summary(1) + decisions(1) + completed(1) + todos(1) = 4 (no dups here).
    assert_eq!(out.saved, 4, "all four wrap-up memories were saved");
    assert!(
        out.auto_resolved >= 1,
        "the completed item auto-resolved the matching open bug, got {}",
        out.auto_resolved
    );

    // The bug is now resolved …
    let bug = store.get(&bug_id).expect("get bug").expect("bug exists");
    assert_eq!(
        bug.status,
        MemoryStatus::Resolved,
        "the open auth bug was auto-resolved"
    );
    assert!(
        bug.resolve_reason
            .as_deref()
            .unwrap_or("")
            .contains("completed work"),
        "resolve reason records the auto-resolution"
    );

    // … but the unrelated dark-mode todo stays open.
    let todo = store.get(&todo_id).expect("get todo").expect("todo exists");
    assert_eq!(
        todo.status,
        MemoryStatus::Active,
        "an unrelated todo is NOT closed by the completed item"
    );

    // The narrative + todo landed and are findable.
    let recent = store
        .list(project, Some(Category::Todo), 10, false)
        .expect("list todos");
    assert!(
        recent.iter().any(|r| r.content.contains("rate limiting")),
        "the session todo was persisted"
    );
}
