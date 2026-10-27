//! What `audit(op="dead")` is allowed to accuse.
//!
//! The audit answers "you can delete this", so a false positive is not noise —
//! it points at code that must not go. Three kinds of live code have no incoming
//! CALL edge and used to be reported anyway:
//!
//!   * test functions (the harness calls them, nothing in the graph does),
//!   * trait methods reached by dispatch (`Display::fmt` behind every `{}`),
//!   * functions passed as VALUES (`.map_err(log_err)`).
//!
//! On this repository's own index those three were 93% of the answer. These tests
//! pin each exclusion, and pin that genuinely dead code is still reported — an
//! audit that accuses nobody is just as useless.

use std::fs;

use icode_core::traits::CodeReadStore;
use icode_engine::{index_path, SqliteCodeStore};

const SAMPLE: &str = r#"
use std::fmt;

pub struct Thing {
    pub n: u32,
}

impl fmt::Display for Thing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Thing({})", self.n)
    }
}

/// Never called, never mentioned, not a test, not a trait method. This is what
/// the audit exists to find.
fn truly_dead_helper(x: u32) -> u32 {
    x + 1
}

/// Handed to `map` as a value — the parser records no call for it.
fn doubler(x: u32) -> u32 {
    x * 2
}

pub fn run(values: Vec<u32>) -> Vec<u32> {
    values.into_iter().map(doubler).collect()
}

pub fn main() {
    let t = Thing { n: 1 };
    println!("{}", t);
    let _ = run(vec![1, 2, 3]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_nobody_calls() {
        assert_eq!(run(vec![2]), vec![4]);
    }
}
"#;

fn names(hits: &[icode_core::model::CodeHit]) -> Vec<String> {
    hits.iter().map(|h| h.name.clone()).collect()
}

#[test]
fn dead_code_names_the_dead_and_spares_the_living() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("lib.rs"), SAMPLE).expect("write sample");

    let store = SqliteCodeStore::open(root).expect("open store");
    index_path(root, &store).expect("index");

    let dead = names(&store.find_dead_code(None, 50).expect("dead"));

    assert!(
        dead.contains(&"truly_dead_helper".to_string()),
        "the one genuinely dead function must still be reported: {dead:?}"
    );
    assert!(
        !dead.contains(&"a_test_nobody_calls".to_string()),
        "a test is called by the harness, not by the graph: {dead:?}"
    );
    assert!(
        !dead.contains(&"fmt".to_string()),
        "`Display::fmt` is reached through dispatch: {dead:?}"
    );
    assert!(
        !dead.contains(&"doubler".to_string()),
        "a function passed as a value is used, not dead: {dead:?}"
    );
}

/// `unreachable` walks the call graph from the entry points, so a test — which no
/// entry point reaches — is unreachable by construction and reporting it says
/// nothing. Same for a dispatched trait method.
#[test]
fn unreachable_does_not_list_tests_or_dispatched_methods() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("lib.rs"), SAMPLE).expect("write sample");

    let store = SqliteCodeStore::open(root).expect("open store");
    index_path(root, &store).expect("index");

    let unreachable = names(&store.find_unreachable(None, 50).expect("unreachable"));

    assert!(
        !unreachable.contains(&"a_test_nobody_calls".to_string()),
        "tests are not what 'unreachable' is asking about: {unreachable:?}"
    );
    assert!(
        !unreachable.contains(&"fmt".to_string()),
        "`Display::fmt` is reached through dispatch: {unreachable:?}"
    );
}
