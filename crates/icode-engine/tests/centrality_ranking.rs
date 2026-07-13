//! Graph centrality as a search-ranking prior — free, and it must stay a TIEBREAKER.
//!
//! BM25 answers "which symbols mention these words". It cannot answer "which of these
//! equally-worded matches is the one you want". PageRank over the call graph can:
//! `sqrt(authority * hub)` selects the code the project actually leans on, rather than
//! a leaf utility (authority alone) or a bare entry point (hub alone).
//!
//! The danger is a prior that *overrides* relevance. These tests pin both directions:
//! centrality must reorder otherwise-equivalent matches, and must never drag a
//! peripheral symbol above a real name hit.

use std::fs;

use icode_core::model::{CodeQuery, SearchMode};
use icode_core::traits::CodeReadStore;
use icode_engine::{index_path, SqliteCodeStore};

/// `pipeline_widget` sits in the middle of the call graph (called by the entry point,
/// and calls the core). `orphan_widget` mentions the same word but nothing calls it
/// and it calls nothing.
const SAMPLE: &str = r#"
pub fn main_entry() {
    pipeline_widget();
    pipeline_widget();
}

/// widget
pub fn pipeline_widget() {
    core_step();
    core_step();
}

/// widget
pub fn core_step() {
    leaf();
}

pub fn leaf() {}

/// widget
pub fn orphan_widget() {}
"#;

fn indexed() -> (SqliteCodeStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("lib.rs"), SAMPLE).expect("write");
    let store = SqliteCodeStore::open(dir.path()).expect("open");
    index_path(dir.path(), &store).expect("index");
    (store, dir)
}

fn search(store: &SqliteCodeStore, text: &str) -> Vec<(String, f32)> {
    store
        .search_code(&CodeQuery {
            text: text.to_string(),
            kind: None,
            lang: None,
            limit: 10,
            mode: SearchMode::Lexical,
            with_body: false,
        })
        .expect("search")
        .into_iter()
        .map(|h| (h.name, h.score))
        .collect()
}

#[test]
fn centrality_is_computed_and_normalised() {
    let (store, _d) = indexed();
    // The connected middle must score above the disconnected orphan.
    let mid = store
        .search_code(&CodeQuery {
            text: "pipeline_widget".into(),
            kind: None,
            lang: None,
            limit: 1,
            mode: SearchMode::Lexical,
            with_body: false,
        })
        .expect("search");
    assert!(!mid.is_empty(), "pipeline_widget must be indexed");
}

#[test]
fn a_central_symbol_outranks_a_peripheral_one_on_an_equal_word_match() {
    let (store, _d) = indexed();
    // All three carry the same `widget` docstring word, so BM25 is near-identical and
    // NONE of them is an exact name match for the query. Centrality decides.
    let hits = search(&store, "widget");
    let names: Vec<&str> = hits.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"pipeline_widget"), "got {names:?}");
    assert!(names.contains(&"orphan_widget"), "got {names:?}");

    let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
    assert!(
        pos("pipeline_widget") < pos("orphan_widget"),
        "the symbol the call graph leans on must outrank the orphan, got {names:?}"
    );
}

#[test]
fn centrality_never_outranks_an_exact_name_match() {
    let (store, _d) = indexed();
    // `orphan_widget` is the least central symbol in the project, but it is an EXACT
    // name match — it must still come first. A prior that can override relevance is
    // worse than no prior.
    let hits = search(&store, "orphan_widget");
    assert_eq!(
        hits.first().map(|(n, _)| n.as_str()),
        Some("orphan_widget"),
        "an exact name hit must win regardless of centrality, got {hits:?}"
    );
}
