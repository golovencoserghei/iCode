//! `find_references` is the precise answer to "where is this used" — not grep.
//!
//! Grep matches SUBSTRINGS and classifies nothing. Asked for `walk` it returns hits
//! from inside `walk_source_files`, from `walker`, and from the word "sidewalk" in a
//! comment — and it cannot tell the agent which hit is the definition, which is a call
//! site, and which is a type annotation. All three distinctions matter when you are
//! about to change a symbol.
//!
//! These tests use the real index path and no model.

use std::fs;

use icode_core::model::RefKind;
use icode_engine::{index_path, SqliteCodeStore};

/// `walk` is defined once, called once, and appears in several DECOYS that a
/// substring search would wrongly return.
const SAMPLE: &str = r#"
pub fn walk(node: u32) -> u32 {
    node + 1
}

/// Decoy: a longer identifier that CONTAINS `walk`.
pub fn walk_source_files(root: u32) -> u32 {
    root
}

pub struct Walker {
    steps: u32,
}

pub fn drive(n: u32) -> u32 {
    // sidewalk — the word appears in a comment inside a longer word
    let w = walk_source_files(n);
    walk(w)
}

pub fn annotate() -> u32 {
    let f: fn(u32) -> u32 = walk;
    f(1)
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
fn references_are_classified_not_just_matched() {
    let (store, _d) = indexed();
    let refs = store.find_references("walk", 50).expect("find_references");
    assert!(!refs.is_empty(), "expected references to `walk`");

    // The definition is identified AS the definition.
    let defs: Vec<_> = refs.iter().filter(|r| r.kind == RefKind::Definition).collect();
    assert_eq!(defs.len(), 1, "exactly one definition of `walk`, got {defs:?}");

    // The call site is identified AS a call, and attributed to its caller.
    let calls: Vec<_> = refs.iter().filter(|r| r.kind == RefKind::Call).collect();
    assert!(
        calls.iter().any(|r| r.context == "drive"),
        "the call to `walk` inside `drive` must be classified as a Call, got {calls:?}"
    );

    // The non-call usage (`= walk;`) is a Mention, not a Call — it is a real
    // reference, but changing the signature affects it differently.
    let mentions: Vec<_> = refs.iter().filter(|r| r.kind == RefKind::Mention).collect();
    assert!(
        mentions.iter().any(|r| r.context == "annotate"),
        "the `let f = walk;` usage must surface as a Mention, got {mentions:?}"
    );
}

#[test]
fn a_longer_identifier_containing_the_name_is_not_a_reference() {
    let (store, _d) = indexed();
    let refs = store.find_references("walk", 50).expect("find_references");

    // `walk_source_files` is defined AND called — grep for "walk" returns both.
    // Neither is a reference to `walk`.
    for r in &refs {
        assert!(
            !r.text.contains("walk_source_files") || r.text.contains("walk(") ,
            "a hit inside `walk_source_files` is not a reference to `walk`: {r:?}"
        );
    }
    // The definition of `walk_source_files` must never be reported as a definition
    // of `walk`.
    let defs: Vec<_> = refs.iter().filter(|r| r.kind == RefKind::Definition).collect();
    assert_eq!(defs.len(), 1, "only `walk` itself is a definition, got {defs:?}");

    // And `Walker` (a different symbol that merely starts with the name) is absent.
    assert!(
        !refs.iter().any(|r| r.context == "Walker"),
        "`Walker` is a different symbol, not a reference to `walk`"
    );
}

#[test]
fn an_unknown_symbol_returns_nothing_rather_than_erroring() {
    let (store, _d) = indexed();
    let refs = store.find_references("no_such_symbol", 50).expect("must not error");
    assert!(refs.is_empty(), "unknown symbol yields no references, got {refs:?}");
}
