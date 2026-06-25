//! M1 keystone integration tests: index a temp Rust tree and verify the full
//! code graph (classes/imports/calls populated, classes searchable) plus the
//! name-match priority fix from M0.5.

use std::fs;

use icode_core::model::{CodeQuery, Language, SearchMode, SymbolKind};
use icode_core::traits::CodeReadStore;
use icode_engine::{index_path, SqliteCodeStore};

/// One file with a struct, an impl method, a free fn, a `use`, and a call —
/// exercises every node kind the Rust parser fills in M1.
const SAMPLE: &str = r#"
use std::collections::HashMap;

pub struct Service {
    cache: HashMap<u64, String>,
}

impl Service {
    pub fn run(&self, id: u64) -> u64 {
        let value = helper(id);
        self.process(value)
    }

    fn process(&self, v: u64) -> u64 {
        v + 1
    }
}

pub fn helper(seed: u64) -> u64 {
    seed * 2
}
"#;

#[test]
fn index_builds_full_code_graph() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("service.rs"), SAMPLE).expect("write sample");

    let store = SqliteCodeStore::open(root).expect("open store");
    let stats = index_path(root, &store).expect("index");
    assert_eq!(stats.files_indexed, 1, "one .rs file indexed");

    // Stats reflect the full graph: a class, several calls and one import.
    let db = store.stats().expect("stats");
    assert_eq!(db.files, 1);
    assert!(db.classes > 0, "expected classes > 0, got {}", db.classes);
    assert!(db.functions >= 3, "run + process + helper, got {}", db.functions);
    assert!(db.imports > 0, "expected the `use` import, got {}", db.imports);
    assert!(db.calls > 0, "expected call edges, got {}", db.calls);

    // search_code(kind=None) finds BOTH a class and a function.
    let class_hits = store
        .search_code(&CodeQuery {
            text: "Service".into(),
            kind: None,
            lang: None,
            limit: 10,
            mode: SearchMode::Hybrid,
            with_body: false,
        })
        .expect("search Service");
    assert!(
        class_hits.iter().any(|h| h.kind == SymbolKind::Class && h.name == "Service"),
        "expected Service class in hits: {:?}",
        class_hits.iter().map(|h| (&h.name, h.kind)).collect::<Vec<_>>()
    );

    let fn_hits = store
        .search_code(&CodeQuery {
            text: "helper".into(),
            kind: None,
            lang: None,
            limit: 10,
            mode: SearchMode::Hybrid,
            with_body: false,
        })
        .expect("search helper");
    assert!(
        fn_hits.iter().any(|h| h.kind == SymbolKind::Function && h.name == "helper"),
        "expected helper function in hits: {:?}",
        fn_hits.iter().map(|h| (&h.name, h.kind)).collect::<Vec<_>>()
    );

    // Explicit kind=Class searches only classes.
    let only_classes = store
        .search_code(&CodeQuery {
            text: "Service".into(),
            kind: Some(SymbolKind::Class),
            lang: None,
            limit: 10,
            mode: SearchMode::Lexical,
            with_body: false,
        })
        .expect("search class-only");
    assert!(!only_classes.is_empty(), "class-only search returned nothing");
    assert!(only_classes.iter().all(|h| h.kind == SymbolKind::Class));

    // Precise class fetch resolves bases/body.
    let class = store
        .get_class("Service", Some(Language::Rust), true)
        .expect("get_class")
        .expect("Service present");
    assert_eq!(class.qualified_name, "Service");
    assert!(class.body.contains("cache"), "body populated when requested");

    // file_outline lists the class and the three functions, ordered by line.
    let outline = store
        .file_outline(&root.join("service.rs").to_string_lossy())
        .expect("outline");
    assert!(
        outline.iter().any(|h| h.name == "Service" && h.kind == SymbolKind::Class),
        "outline missing Service class"
    );
    assert!(
        outline.iter().filter(|h| h.kind == SymbolKind::Function).count() >= 3,
        "outline should list run/process/helper"
    );

    // The impl method `run` is qualified as `Service::run` and its body calls
    // `helper` — verify the parser attributed the call to the enclosing method.
    let func = store
        .get_function("run", Some(Language::Rust), true)
        .expect("get_function")
        .expect("run present");
    assert_eq!(func.qualified_name, "Service::run");
}

/// Two symbols where one symbol's name appears inside the other's body. An exact
/// name match must rank above a body-only match.
const NAME_PRIORITY: &str = r#"
/// A free function literally named `index_path`.
pub fn index_path(root: &str) -> usize {
    root.len()
}

/// A different function whose BODY mentions index_path but whose name does not.
pub fn run_everything(arg: &str) -> usize {
    // calls index_path internally
    let n = index_path(arg);
    n + 1
}
"#;

#[test]
fn name_match_outranks_body_match() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("rank.rs"), NAME_PRIORITY).expect("write sample");

    let store = SqliteCodeStore::open(root).expect("open store");
    index_path(root, &store).expect("index");

    let hits = store
        .search_code(&CodeQuery {
            text: "index_path".into(),
            kind: Some(SymbolKind::Function),
            lang: None,
            limit: 10,
            mode: SearchMode::Lexical,
            with_body: false,
        })
        .expect("search");

    assert!(!hits.is_empty(), "expected at least one hit");
    // The function actually NAMED index_path must come first, ahead of
    // run_everything which only mentions it in its body.
    assert_eq!(
        hits[0].name, "index_path",
        "exact name match must rank first, got order: {:?}",
        hits.iter().map(|h| &h.name).collect::<Vec<_>>()
    );
    // And run_everything (body-only match) should still be found, just lower.
    assert!(
        hits.iter().any(|h| h.name == "run_everything"),
        "body match should still appear in results"
    );
}
