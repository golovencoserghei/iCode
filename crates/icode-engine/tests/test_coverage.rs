//! "Which tests exercise this symbol?" — the question you ask before changing
//! anything, and the one nothing in the toolbox could answer.
//!
//! Grepping the test suite for the symbol's name only finds tests that mention it
//! DIRECTLY. A test that reaches it three calls deep — the common case, and the one
//! that will actually break — is invisible to grep. Reverse reachability over the call
//! graph finds it.
//!
//! Note what makes this work at all in a Rust project: `#[test]` is an ATTRIBUTE, not
//! a naming convention. `deep_path_is_covered` is a test; `helper` is not, and no
//! name/path heuristic can tell them apart when both live in the same file. The parser
//! reads the attribute.

use std::fs;

use icode_engine::{index_path, SqliteCodeStore};

/// `target` is reached by `direct_test` in one hop and by `deep_test` in three.
/// `untested` is reached by nobody.
const SAMPLE: &str = r#"
pub fn target(x: u32) -> u32 {
    x + 1
}

pub fn untested(x: u32) -> u32 {
    x * 2
}

pub fn layer_one(x: u32) -> u32 {
    layer_two(x)
}

pub fn layer_two(x: u32) -> u32 {
    target(x)
}

/// NOT a test — it merely lives next to them and has an innocuous name.
pub fn helper(x: u32) -> u32 {
    target(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_test() {
        target(1);
    }

    #[test]
    fn deep_path_is_covered() {
        layer_one(1);
    }
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
fn a_test_three_calls_away_is_still_found() {
    let (store, _d) = indexed();
    let cov = store.find_tests_covering("target", 5).expect("find_tests_covering");
    let names: Vec<&str> = cov.iter().map(|c| c.test.as_str()).collect();

    assert!(
        names.contains(&"direct_test"),
        "the test that calls `target` directly must be found, got {names:?}"
    );
    // The whole point: grep of the test suite for "target" would NOT find this one.
    assert!(
        names.contains(&"deep_path_is_covered"),
        "a test reaching `target` transitively (test -> layer_one -> layer_two -> target) \
         must be found, got {names:?}"
    );

    // Depth is reported, so the agent knows how indirect the coverage is.
    let direct = cov.iter().find(|c| c.test == "direct_test").unwrap();
    let deep = cov.iter().find(|c| c.test == "deep_path_is_covered").unwrap();
    assert_eq!(direct.depth, 1, "a direct call is depth 1");
    assert!(deep.depth > direct.depth, "the transitive path must report a greater depth");
}

#[test]
fn a_non_test_caller_is_not_reported_as_coverage() {
    let (store, _d) = indexed();
    let cov = store.find_tests_covering("target", 5).expect("find_tests_covering");
    assert!(
        !cov.iter().any(|c| c.test == "helper"),
        "`helper` calls target but is NOT a test — it must not count as coverage"
    );
}

#[test]
fn untested_code_reports_no_coverage_which_is_the_answer() {
    let (store, _d) = indexed();
    let cov = store.find_tests_covering("untested", 5).expect("find_tests_covering");
    assert!(
        cov.is_empty(),
        "nothing reaches `untested` — an empty list IS the finding, got {cov:?}"
    );
}
