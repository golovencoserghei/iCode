//! M1 read-surface integration test: index a temp Rust tree exercising the full
//! group-A graph/analysis surface of `CodeReadStore`, then assert the by-name
//! call graph (callers/callees/chain), complexity ranking, repo map, dead-code
//! detection, implementation lookup, and body grep all return sensible results.
//!
//! Everything here is the APPROXIMATE name-based surface (typed receiver
//! resolution is M3b); the fixture is chosen so the name space is unambiguous.

use std::fs;

use icode_core::traits::CodeReadStore;
use icode_engine::{index_path, SqliteCodeStore};

/// A clean call chain `a → b → c`, a trait `Drawable` with a sub-trait
/// `Widget: Drawable` (so `bases` is populated and `find_implementations`
/// resolves), and an uncalled `dead` function. `entry` calls `a` so the chain is
/// reachable; `dead` is referenced by nothing.
const SAMPLE: &str = r#"
pub trait Drawable {
    fn draw(&self);
}

pub trait Widget: Drawable {
    fn widget_marker(&self);
}

pub fn a() -> u64 {
    b()
}

pub fn b() -> u64 {
    c()
}

pub fn c() -> u64 {
    UNIQUE_GREP_TOKEN_42
}

pub fn dead() -> u64 {
    0
}

pub fn entry() -> u64 {
    a()
}
"#;

#[test]
fn read_surface_graph_and_analysis() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("graph.rs"), SAMPLE).expect("write sample");

    let store = SqliteCodeStore::open(root).expect("open store");
    let stats = index_path(root, &store).expect("index");
    assert_eq!(stats.files_indexed, 1, "one .rs file indexed");

    // ── call graph: callers/callees of `b` are both non-empty ──
    let callers_b = store.get_callers("b", 50).expect("get_callers b");
    assert!(
        callers_b.iter().any(|c| c.caller.ends_with("a") && c.callee == "b"),
        "expected a→b among callers of b, got: {:?}",
        callers_b.iter().map(|c| (&c.caller, &c.callee)).collect::<Vec<_>>()
    );

    let callees_b = store.get_callees("b", 50).expect("get_callees b");
    assert!(
        callees_b.iter().any(|c| c.callee == "c"),
        "expected b→c among callees of b, got: {:?}",
        callees_b.iter().map(|c| (&c.caller, &c.callee)).collect::<Vec<_>>()
    );

    // ── call_chain a → c is exactly [a, b, c] in order ──
    let chain = store.call_chain("a", "c", 10).expect("call_chain a→c");
    assert_eq!(chain, vec!["a", "b", "c"], "call chain a→b→c expected, got {:?}", chain);

    // No path the wrong way (c does not call a) ⇒ empty.
    let no_chain = store.call_chain("c", "a", 10).expect("call_chain c→a");
    assert!(no_chain.is_empty(), "c→a should be unreachable, got {:?}", no_chain);

    // ── complexity ranking is non-empty ──
    let complex = store.find_complex_functions(None, 10).expect("complex");
    assert!(!complex.is_empty(), "expected at least one complex function");

    // ── repo_map.stats.functions > 0 ──
    let map = store.repo_map(10).expect("repo_map");
    assert!(map.stats.functions > 0, "repo_map functions > 0, got {}", map.stats.functions);
    // `entry` calls `a` and `main`-less project ⇒ no entry point unless routes;
    // but the languages aggregate must list rust.
    assert!(
        map.languages.iter().any(|(l, _)| l == "rust"),
        "languages should include rust: {:?}",
        map.languages
    );

    // ── dead-code finds `dead` (uncalled), and not the called `a`/`b`/`c` ──
    let dead = store.find_dead_code(None, 50).expect("find_dead_code");
    let dead_names: Vec<&str> = dead.iter().map(|h| h.name.as_str()).collect();
    assert!(dead_names.contains(&"dead"), "dead() should be flagged, got {:?}", dead_names);
    assert!(!dead_names.contains(&"b"), "b() is called, must not be dead: {:?}", dead_names);

    // ── find_implementations(Drawable) finds the Widget sub-trait ──
    let impls = store.find_implementations("Drawable").expect("find_implementations");
    assert!(
        impls.iter().any(|q| q == "Widget"),
        "Widget: Drawable should be an implementation, got {:?}",
        impls
    );

    // ── grep_code over a body substring returns a hit ──
    let hits = store.grep_code("UNIQUE_GREP_TOKEN_42", None, 50).expect("grep_code");
    assert!(!hits.is_empty(), "grep for body token should hit");
    assert!(
        hits.iter().any(|h| h.text.contains("UNIQUE_GREP_TOKEN_42")),
        "grep hit text should contain the token: {:?}",
        hits.iter().map(|h| &h.text).collect::<Vec<_>>()
    );

    // An invalid regex is a clean Invalid error, not a panic.
    assert!(store.grep_code("(", None, 10).is_err(), "unbalanced regex must Err");

    // ── symbol_context ties it together for `b` ──
    let ctx = store.symbol_context("b", None).expect("symbol_context b");
    assert!(ctx.definition.is_some(), "b should resolve a definition");
    assert!(!ctx.callers.is_empty(), "b has callers");
    assert!(!ctx.callees.is_empty(), "b has callees");

    // ── read_file returns the requested 1-based inclusive window ──
    let path = root.join("graph.rs").to_string_lossy().to_string();
    let whole = store.read_file(&path, None, None).expect("read whole");
    assert!(whole.contains("pub fn a()"), "whole file read should contain fn a");
    let window = store.read_file(&path, Some(1), Some(2)).expect("read window");
    assert_eq!(window.lines().count(), 2, "two-line window: {window:?}");
}
