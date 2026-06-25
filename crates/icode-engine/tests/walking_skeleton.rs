//! M0.5 walking-skeleton integration test: index a temp dir with one `.rs` file,
//! then verify the store reports the function via stats, lexical search, and
//! precise fetch — exercising parse → store → read across the contract seam.

use std::fs;

use icode_core::model::{CodeQuery, Language, SearchMode, SymbolKind};
use icode_core::traits::CodeReadStore;
use icode_engine::{index_path, SqliteCodeStore};

const SAMPLE: &str = r#"
pub fn greet_user(name: &str) -> String {
    format!("hello {name}")
}

struct Service;

impl Service {
    pub async fn fetch_record(&self, id: u64) -> u64 {
        id
    }
}
"#;

#[test]
fn index_then_search_finds_function() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("sample.rs"), SAMPLE).expect("write sample");

    let store = SqliteCodeStore::open(root).expect("open store");
    let stats = index_path(root, &store).expect("index");

    assert_eq!(stats.files_indexed, 1, "one .rs file indexed");
    assert_eq!(stats.functions, 2, "greet_user + Service::fetch_record");

    // stats() reflects real COUNTs.
    let db = store.stats().expect("stats");
    assert_eq!(db.files, 1);
    assert_eq!(db.functions, 2);
    assert_eq!(db.parse_errors, 0);

    // Lexical search by name (Hybrid → Lexical in the skeleton).
    let hits = store
        .search_code(&CodeQuery {
            text: "greet_user".into(),
            kind: Some(SymbolKind::Function),
            lang: None,
            limit: 10,
            mode: SearchMode::Hybrid,
            with_body: false,
        })
        .expect("search");
    assert!(
        hits.iter().any(|h| h.name == "greet_user"),
        "expected greet_user in hits, got: {:?}",
        hits.iter().map(|h| &h.name).collect::<Vec<_>>()
    );
    // Lean by default: no snippet without with_body.
    assert!(hits.iter().all(|h| h.snippet.is_none()));

    // Precise fetch resolves the impl method with a qualified name + body.
    let func = store
        .get_function("fetch_record", Some(Language::Rust), true)
        .expect("get_function")
        .expect("fetch_record present");
    assert_eq!(func.qualified_name, "Service::fetch_record");
    assert!(func.is_async, "fetch_record is async");
    assert!(func.body.contains("id"), "body populated when requested");
}
