//! A METHOD call must never be credited to a same-named FREE function.
//!
//! The call graph stored `receiver: Option<String>` and threw away the SYNTAX of the
//! call. So a project that happened to define `fn collect` collected every
//! `.collect()` in the repo as one of its callers. Measured on this codebase before
//! the fix: `join` reported 137 callers (4 real), `collect` 99 (13 real), `lines` 17
//! (2 real). Every consumer inherited the lie — `get_callers`, impact analysis, and
//! graph centrality alike.
//!
//! `receiver` cannot recover the distinction on its own: `icode_engine::index_path()`
//! (a real path call to a free function) and `store.add()` (a method call) both leave
//! a bare lowercase word behind. The parser, however, always knew — tree-sitter hands
//! it a `field_expression` for `a.b()` and a `scoped_identifier` for `a::b()`. That
//! bit is now recorded as `Call::is_method`.

use std::fs;

use icode_core::traits::CodeReadStore;
use icode_engine::{index_path, SqliteCodeStore};

/// `collect` is defined as a free function AND used as the ubiquitous iterator
/// method. Only `calls_the_free_collect` actually calls the free one.
const SAMPLE: &str = r#"
fn collect(items: &[u32]) -> Vec<u32> {
    items.to_vec()
}

fn calls_the_free_collect(items: &[u32]) -> Vec<u32> {
    collect(items)
}

fn iterates_a(items: &[u32]) -> Vec<u32> {
    items.iter().map(|x| x + 1).collect()
}

fn iterates_b(items: &[u32]) -> Vec<u32> {
    items.iter().filter(|x| **x > 0).cloned().collect()
}

fn iterates_c(items: &[u32]) -> Vec<u32> {
    items.iter().rev().cloned().collect()
}
"#;

fn indexed() -> (SqliteCodeStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("lib.rs"), SAMPLE).expect("write");
    let store = SqliteCodeStore::open(dir.path()).expect("open");
    index_path(dir.path(), &store).expect("index");
    (store, dir)
}

#[test]
fn iterator_method_calls_are_not_callers_of_a_free_fn_with_the_same_name() {
    let (store, _d) = indexed();
    let callers = store.get_callers("collect", 50).expect("get_callers");

    let names: Vec<&str> = callers.iter().map(|c| c.caller.as_str()).collect();

    // The ONE real caller — a free call `collect(items)`.
    assert!(
        names.contains(&"calls_the_free_collect"),
        "the real free-function call must survive, got {names:?}"
    );

    // The three `.collect()` method calls must NOT be credited to it.
    for bogus in ["iterates_a", "iterates_b", "iterates_c"] {
        assert!(
            !names.contains(&bogus),
            "`{bogus}` only calls `.collect()` as a METHOD — it must not appear as a \
             caller of the free `fn collect`. Got {names:?}"
        );
    }
    assert_eq!(callers.len(), 1, "exactly one real caller, got {names:?}");
}

#[test]
fn the_parser_records_call_syntax() {
    let (store, _d) = indexed();
    // Every edge is present in the graph — the filter lives in the QUERY, not the
    // ingest, so nothing is silently lost and other consumers can still see it all.
    let all = store.get_callees("iterates_a", 50).expect("get_callees");
    let collect_edge = all
        .iter()
        .find(|c| c.callee == "collect")
        .expect("the .collect() edge is still recorded");
    assert!(
        collect_edge.is_method,
        "`.collect()` must be recorded as METHOD syntax"
    );

    let free = store.get_callees("calls_the_free_collect", 50).expect("get_callees");
    let free_edge = free
        .iter()
        .find(|c| c.callee == "collect")
        .expect("the free collect(items) edge");
    assert!(
        !free_edge.is_method,
        "`collect(items)` is a FREE call, not method syntax"
    );
}
