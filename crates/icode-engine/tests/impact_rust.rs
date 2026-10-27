//! `graph(op="impact")` on a Rust tree.
//!
//! Reverse dependencies used to be resolved by one rule: does the import string
//! look like a suffix of the file's path? That answers for Python/JS/Go, and for
//! Rust it answers NEVER — `use icode_engine::stale_report` shares no token with
//! `crates/icode-engine/src/verify/mod.rs`. Every Rust file, however widely
//! imported, reported an empty impact set, which is worse than no feature: it
//! reads as "nothing depends on this, safe to change".
//!
//! These tests pin both halves of the fix — that real importers are found, and
//! that a same-named module in another crate is NOT mistaken for one.

use std::fs;
use std::path::Path;

use icode_core::traits::CodeReadStore;
use icode_engine::{index_path, SqliteCodeStore};

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, body).expect("write");
}

/// A two-crate workspace: `beta` uses `alpha`, and inside `alpha` the crate root
/// uses its own `store` module. Both spellings of a Rust import appear.
fn fixture(root: &Path) {
    write(&root.join("crates/alpha/Cargo.toml"), "[package]\nname = \"alpha\"\n");
    write(
        &root.join("crates/alpha/src/lib.rs"),
        "pub mod store;\npub use store::Widget;\n\npub fn alpha_entry() -> u32 { 1 }\n",
    );
    write(
        &root.join("crates/alpha/src/store/mod.rs"),
        "pub struct Widget {\n    pub n: u32,\n}\n\npub fn make_widget() -> Widget {\n    Widget { n: 0 }\n}\n",
    );
    write(
        &root.join("crates/alpha/src/unrelated.rs"),
        "pub fn lonely_fn() -> u32 {\n    7\n}\n",
    );

    write(&root.join("crates/beta/Cargo.toml"), "[package]\nname = \"beta\"\n");
    write(
        &root.join("crates/beta/src/main.rs"),
        "use alpha::store::make_widget;\n\nfn main() {\n    let _ = make_widget();\n}\n",
    );
    // A module named `store` in the OTHER crate — the homonym that must not be
    // dragged in by `alpha::store::…`.
    write(
        &root.join("crates/beta/src/store.rs"),
        "pub fn beta_store_helper() -> u32 {\n    3\n}\n",
    );
}

fn abs(root: &Path, rel: &str) -> String {
    root.join(rel).to_string_lossy().to_string()
}

#[test]
fn a_rust_file_reports_the_files_that_import_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);

    let store = SqliteCodeStore::open(root).expect("open store");
    index_path(root, &store).expect("index");

    let impacted = store
        .impact_analysis(&abs(root, "crates/alpha/src/store/mod.rs"), 2)
        .expect("impact");

    assert!(
        impacted.contains(&abs(root, "crates/alpha/src/lib.rs")),
        "`pub use store::Widget` makes lib.rs an importer: {impacted:?}"
    );
    assert!(
        impacted.contains(&abs(root, "crates/beta/src/main.rs")),
        "`use alpha::store::make_widget` makes main.rs an importer: {impacted:?}"
    );
}

/// The crate-qualified path `alpha::store::…` must not attach to `beta`'s own
/// `store.rs` just because the segment matches. A fabricated reverse edge is how
/// the name-keyed graph misled people before the module boundary landed.
#[test]
fn a_same_named_module_in_another_crate_is_not_an_importer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);

    let store = SqliteCodeStore::open(root).expect("open store");
    index_path(root, &store).expect("index");

    let impacted = store
        .impact_analysis(&abs(root, "crates/beta/src/store.rs"), 2)
        .expect("impact");

    assert!(
        !impacted.contains(&abs(root, "crates/beta/src/main.rs")),
        "beta/src/store.rs is not what `alpha::store::…` names: {impacted:?}"
    );
}

/// A file nobody imports still reports nothing — the fix widens the resolver, it
/// does not make it say yes to everything.
#[test]
fn an_unimported_file_has_no_impact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fixture(root);

    let store = SqliteCodeStore::open(root).expect("open store");
    index_path(root, &store).expect("index");

    let impacted = store
        .impact_analysis(&abs(root, "crates/alpha/src/unrelated.rs"), 2)
        .expect("impact");

    assert!(impacted.is_empty(), "nothing imports unrelated.rs: {impacted:?}");
}
