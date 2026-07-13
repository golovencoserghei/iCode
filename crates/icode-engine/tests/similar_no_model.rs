//! "Find similar code" must work with NO embedder — and must find a RENAMED clone.
//!
//! `find_similar` used to embed the symbol and run a vector KNN, which made it the
//! last code tool that required a GPU-backed model. It is now MinHash over token
//! shingles (lexical + structural views). This test drives the real index path and
//! never constructs an `Embedder` at all: if any embedding crept back in, it could
//! not compile, let alone pass.

use std::fs;

use icode_engine::{find_similar, index_path, SqliteCodeStore};

/// `load_user` and `fetch_account` are the same code with EVERY local name changed —
/// the case a dense vector handles poorly and a structural signature nails.
/// `render_chart` is unrelated and must not outrank the clone.
const SAMPLE: &str = r#"
pub fn load_user(db: &Db, id: u64) -> Option<User> {
    let row = db.query("SELECT * FROM users WHERE id = ?", id)?;
    let user = User::from_row(row)?;
    cache.insert(id, user.clone());
    Some(user)
}

pub fn fetch_account(store: &Db, key: u64) -> Option<User> {
    let record = store.query("SELECT * FROM users WHERE id = ?", key)?;
    let account = User::from_row(record)?;
    cache.insert(key, account.clone());
    Some(account)
}

pub fn render_chart(points: &[f64], width: usize) -> String {
    let mut svg = String::new();
    for (i, p) in points.iter().enumerate() {
        svg.push_str(&format!("<circle cx='{}' cy='{}'/>", i * width, p));
    }
    svg
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
fn a_renamed_clone_is_the_top_neighbour_without_any_embedder() {
    let (store, _d) = indexed();

    // NOTE the signature: no `&dyn Embedder` argument exists any more.
    let hits = find_similar(&store, "load_user", 5).expect("find_similar");

    assert!(!hits.is_empty(), "expected neighbours, got none");
    assert_eq!(
        hits[0].name, "fetch_account",
        "the renamed clone must rank first, got {:?}",
        hits.iter().map(|h| &h.name).collect::<Vec<_>>()
    );
    // Unrelated code must not beat the clone.
    if let Some(chart) = hits.iter().find(|h| h.name == "render_chart") {
        assert!(
            chart.score < hits[0].score,
            "unrelated code ({}) must not outrank the clone ({})",
            chart.score,
            hits[0].score
        );
    }
}

#[test]
fn the_symbol_itself_is_never_its_own_neighbour() {
    let (store, _d) = indexed();
    let hits = find_similar(&store, "load_user", 5).expect("find_similar");
    assert!(
        !hits.iter().any(|h| h.name == "load_user"),
        "a symbol must not be returned as similar to itself"
    );
}

#[test]
fn an_unknown_symbol_yields_no_neighbours_rather_than_an_error() {
    let (store, _d) = indexed();
    let hits = find_similar(&store, "no_such_symbol", 5).expect("must not error");
    assert!(hits.is_empty(), "unknown symbol should return an empty list");
}
