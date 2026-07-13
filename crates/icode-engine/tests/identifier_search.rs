//! Lexical search must SEE inside compound identifiers — no embedding model.
//!
//! SQLite's default FTS5 tokenizer splits `snake_case` for free but treats a
//! `camelCase` / `PascalCase` identifier as one opaque token, so `Handler` did not
//! match `HttpRequestHandler` and `embed` did not match `embedPending`. That blind
//! spot covers most of JS/TS/Go/Java/C# and every Rust type name — and it is exactly
//! what a vector index was compensating for, at the cost of a GPU.
//!
//! These tests drive the REAL index path (`index_path` → FTS5 → `search_code`) with
//! NO embedder, and assert the compound identifiers are now reachable by their parts.

use std::fs;

use icode_core::model::{CodeQuery, SearchMode};
use icode_core::traits::CodeReadStore;
use icode_engine::{index_path, SqliteCodeStore};

const SAMPLE: &str = r#"
/// Handle an inbound HTTP request.
pub struct HttpRequestHandler {
    inner: u32,
}

pub fn embedPending(store: &str) -> usize {
    store.len()
}

pub fn parseJSONBody(raw: &str) -> usize {
    raw.len()
}

pub fn embed_pending_snake(store: &str) -> usize {
    store.len()
}
"#;

/// Index the sample and return a lexical-only store (no embedder anywhere).
fn indexed() -> (SqliteCodeStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("lib.rs"), SAMPLE).expect("write");
    let store = SqliteCodeStore::open(dir.path()).expect("open");
    index_path(dir.path(), &store).expect("index");
    (store, dir)
}

fn search(store: &SqliteCodeStore, text: &str) -> Vec<String> {
    store
        .search_code(&CodeQuery {
            text: text.to_string(),
            kind: None,
            lang: None,
            limit: 20,
            mode: SearchMode::Lexical, // lexical ONLY — proves no vectors are involved
            with_body: false,
        })
        .expect("search")
        .into_iter()
        .map(|h| h.qualified_name)
        .collect()
}

#[test]
fn a_word_inside_a_pascal_case_identifier_is_findable() {
    let (store, _d) = indexed();
    // The regression: before the identifier-split column, ALL of these returned zero.
    for term in ["Handler", "Request", "Http"] {
        let hits = search(&store, term);
        assert!(
            hits.iter().any(|q| q.contains("HttpRequestHandler")),
            "`{term}` must find HttpRequestHandler, got {hits:?}"
        );
    }
}

#[test]
fn a_word_inside_a_camel_case_identifier_is_findable() {
    let (store, _d) = indexed();
    let hits = search(&store, "embed");
    assert!(
        hits.iter().any(|q| q.contains("embedPending")),
        "`embed` must find embedPending, got {hits:?}"
    );

    // An acronym run inside a camelCase name is its own word.
    let hits = search(&store, "JSON");
    assert!(
        hits.iter().any(|q| q.contains("parseJSONBody")),
        "`JSON` must find parseJSONBody, got {hits:?}"
    );
}

#[test]
fn exact_compound_name_still_matches_exactly() {
    let (store, _d) = indexed();
    // The original token is kept alongside its pieces, so precision does not regress.
    let hits = search(&store, "HttpRequestHandler");
    assert!(
        hits.iter().any(|q| q.contains("HttpRequestHandler")),
        "exact compound query must still match, got {hits:?}"
    );
}

#[test]
fn snake_case_still_works() {
    let (store, _d) = indexed();
    let hits = search(&store, "snake");
    assert!(
        hits.iter().any(|q| q.contains("embed_pending_snake")),
        "snake_case must keep working, got {hits:?}"
    );
}
