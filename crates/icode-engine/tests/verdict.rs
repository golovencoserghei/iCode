//! Tests for the `check_exists` existence oracle.
//!
//! The verdict is decided by MODEL-AGNOSTIC lexical signals (the embedder only adds
//! a lead), so most cases are HERMETIC (no Ollama) and fully representative of the
//! production path. Two LIVE cases (require Ollama, like `search_e2e`) prove the
//! lead behaviour and that a common-verb query stays WEAK even with vectors present.

use std::fs;

use icode_core::model::{MatchKind, Verdict};
use icode_engine::{check_exists, index_path, ExistScope, SqliteCodeStore};

/// Index one Rust source string (at `rel` under a fresh temp root) into a store.
fn store_at(rel: &str, src: &str) -> (tempfile::TempDir, SqliteCodeStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(&path, src).expect("write src");
    let store = SqliteCodeStore::open(dir.path()).expect("open store");
    index_path(dir.path(), &store).expect("index");
    (dir, store)
}

fn store_with(src: &str) -> (tempfile::TempDir, SqliteCodeStore) {
    store_at("lib.rs", src)
}

// ─────────────────────────── EXISTS: a real defined symbol ───────────────────────────

const HAS_CALENDAR_FN: &str = r#"
/// A calendar helper used by the reminder scheduler.
pub fn calendar() -> u32 { 42 }

pub fn unrelated_thing() -> u32 { 1 }
"#;

#[test]
fn exists_when_a_symbol_is_actually_named_the_query() {
    let (_d, store) = store_with(HAS_CALENDAR_FN);

    let v = check_exists(&store, None, "calendar", ExistScope::Any).expect("check_exists");

    assert_eq!(v.verdict, Verdict::Exists, "a defined fn `calendar` → EXISTS: {}", v.reason);
    assert_eq!(v.match_kind, Some(MatchKind::ExactSymbol));
    assert!(v.exact_name_hit, "exact_name_hit must be set");
    assert_eq!(v.best_match.expect("best_match").name, "calendar");
}

// ─────────────── EXISTS: ≥2 query terms land on one compound name ───────────────

const USER_FETCH: &str = r#"
/// Fetch a single user account row from the store by its unique identifier.
pub fn fetch_user_by_id(id: u64) -> String { format!("user {id}") }

pub fn compute_tax(amount: f64) -> f64 { amount * 0.2 }
"#;

#[test]
fn exists_when_two_query_terms_land_on_a_compound_name() {
    let (_d, store) = store_with(USER_FETCH);

    // "fetch" + "user" both segment `fetch_user_by_id` → a real signal, not a
    // one-word coincidence → EXISTS even in a multi-word query, no embedder.
    let v = check_exists(&store, None, "fetch user data", ExistScope::Any).expect("check_exists");

    assert_eq!(v.verdict, Verdict::Exists, "two name-token hits → EXISTS: {}", v.reason);
    assert_eq!(v.match_kind, Some(MatchKind::NameToken));
    assert!(!v.exact_name_hit, "a partial name match is not an exact-name hit");
    assert_eq!(v.best_match.expect("best_match").name, "fetch_user_by_id");
}

// ── WEAK: ≥2 overlap but of GENERIC words, with the distinctive term unmentioned ──

const GET_FILE_OUTLINE: &str = r#"
/// Return the outline (functions and classes) of a file at a path.
pub fn get_file_outline(path: &str) -> String { format!("outline of {path}") }

pub fn unrelated() -> u32 { 0 }
"#;

#[test]
fn generic_two_term_overlap_without_the_distinctive_word_is_not_exists() {
    let (_d, store) = store_with(GET_FILE_OUTLINE);

    // {get, file} both segment `get_file_outline` (overlap 2) but they are generic
    // and the symbol's load-bearing token `outline` is unmentioned → not a confident
    // EXISTS (intent "get a file PATH" ≠ "get a file OUTLINE").
    let v = check_exists(&store, None, "get the file path", ExistScope::Any).expect("check_exists");
    assert_ne!(
        v.verdict,
        Verdict::Exists,
        "two generic terms without the distinctive word → not EXISTS: {}",
        v.reason
    );

    // But a query that DOES name the distinctive term is a clean EXISTS.
    let v2 = check_exists(&store, None, "show the file outline", ExistScope::Any).expect("check_exists");
    assert_eq!(v2.verdict, Verdict::Exists, "distinctive `outline` present → EXISTS: {}", v2.reason);
    assert_eq!(v2.best_match.expect("best").name, "get_file_outline");
}

// ─────────────────── WEAK: the term lives ONLY in a string literal ───────────────────
// The bug from the screenshot: "calendar" is a permission key in a list; a naive
// search reports "there is a calendar". The oracle must call it a MENTION.

const CALENDAR_ONLY_IN_A_LITERAL: &str = r#"
/// Return the permission scope keys granted to a user.
pub fn permission_scopes() -> Vec<&'static str> {
    vec!["profile", "calendar", "reminders"]
}

pub fn something_else() -> u32 { 7 }
"#;

#[test]
fn weak_when_the_term_is_only_a_string_literal() {
    let (_d, store) = store_with(CALENDAR_ONLY_IN_A_LITERAL);

    let v = check_exists(&store, None, "calendar", ExistScope::Any).expect("check_exists");

    assert_eq!(v.verdict, Verdict::Weak, "literal-only 'calendar' → WEAK: {}", v.reason);
    assert_eq!(v.match_kind, Some(MatchKind::BodyOrString));
    assert!(!v.exact_name_hit);
    let snippet = v.best_match.expect("best_match").snippet.expect("literal line cited");
    assert!(snippet.to_lowercase().contains("calendar"), "snippet quotes the literal: {snippet}");
    assert!(v.reason.contains("string literal") || v.reason.contains("bodies"));
}

// ───── WEAK, not EXISTS: one incidental term matching a name in a multi-word query ─────
// Review finding #1: "add" equals a real fn name, but as one term of a multi-word
// query it must NOT be a confident EXISTS. Decided lexically → holds with OR without
// an embedder (see also the LIVE variant below).

const HAS_ADD_FN: &str = r#"
/// Add two integers.
pub fn add(a: u32, b: u32) -> u32 { a + b }

pub fn greet(name: &str) -> String { format!("hi {name}") }
"#;

#[test]
fn incidental_single_token_in_multiword_query_is_weak() {
    let (_d, store) = store_with(HAS_ADD_FN);

    let v = check_exists(&store, None, "please add these totals together", ExistScope::Any)
        .expect("check_exists");
    assert_eq!(v.verdict, Verdict::Weak, "one incidental term → WEAK not EXISTS: {}", v.reason);
    assert_eq!(v.match_kind, Some(MatchKind::ExactSymbol));
    assert!(!v.exact_name_hit, "an incidental match is not a confident hit");
    assert!(
        v.reason.to_lowercase().contains("verify") || v.reason.contains("incidental"),
        "reason flags it for verification: {}",
        v.reason
    );

    // But as a SINGLE deliberately-typed identifier it IS a confident EXISTS.
    let v1 = check_exists(&store, None, "add", ExistScope::Any).expect("check_exists");
    assert_eq!(v1.verdict, Verdict::Exists, "focused single-term → EXISTS: {}", v1.reason);
    assert!(v1.exact_name_hit);
}

// ─────────────────────────────── ABSENT: nothing at all ───────────────────────────────

const NO_CALENDAR_ANYWHERE: &str = r#"
pub fn add(a: u32, b: u32) -> u32 { a + b }
pub fn multiply(a: u32, b: u32) -> u32 { a * b }
"#;

#[test]
fn absent_when_the_term_is_nowhere() {
    let (_d, store) = store_with(NO_CALENDAR_ANYWHERE);

    let v = check_exists(&store, None, "calendar", ExistScope::Any).expect("check_exists");
    assert_eq!(v.verdict, Verdict::Absent, "no calendar anywhere → ABSENT: {}", v.reason);
    assert!(v.best_match.is_none(), "no neighbour without an embedder");
}

// ───────────── test files must NOT satisfy "does this feature exist" ─────────────

#[test]
fn a_symbol_defined_only_in_a_test_file_is_not_exists() {
    // The ONLY `calendar` lives under tests/ → excluded from the symbol space.
    let (_d, store) = store_at("tests/fixtures.rs", HAS_CALENDAR_FN);

    let v = check_exists(&store, None, "calendar", ExistScope::Any).expect("check_exists");
    assert_ne!(v.verdict, Verdict::Exists, "a test-only symbol is not a feature: {}", v.reason);
    assert!(
        v.best_match.as_ref().map(|h| h.name.as_str()) != Some("calendar"),
        "the test-file `calendar` must not be the best_match"
    );
}

// ─────────────────────── LIVE: lead + common-verb WEAK (needs Ollama) ───────────────────────

const LIVE_SAMPLE: &str = r#"
/// Fetch a single user account row from the store by its unique identifier.
pub fn fetch_user_by_id(id: u64) -> String { format!("user {id}") }

/// Compute the sales tax on a monetary amount.
pub fn compute_tax(amount: f64) -> f64 { amount * 0.2 }

/// Add two integers.
pub fn add(a: u32, b: u32) -> u32 { a + b }
"#;

fn live_store() -> (tempfile::TempDir, SqliteCodeStore, icode_embed::OllamaEmbedder) {
    use icode_core::config::EmbedConfig;
    use icode_core::traits::Embedder;
    use icode_embed::OllamaEmbedder;
    use icode_engine::embed_pending;

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("lib.rs"), LIVE_SAMPLE).expect("write src");
    let store = SqliteCodeStore::open(dir.path()).expect("open store");
    index_path(dir.path(), &store).expect("index");

    let cfg = EmbedConfig::default();
    let embedder = OllamaEmbedder::new(&cfg).expect("build OllamaEmbedder");
    embedder.health().expect("Ollama must be running for the live verdict tests");
    embed_pending(&store, &embedder, cfg.batch).expect("embed_pending");
    (dir, store, embedder)
}

/// A query with NO strong name overlap (only the incidental verb "add" hits a name)
/// must stay WEAK EVEN WITH the embedder live — proving the verdict does not lean on
/// vectors (the gap review finding #3 flagged). Intent is "add a user", not arithmetic.
#[test]
fn common_verb_multiword_stays_weak_with_live_embedder() {
    let (_d, store, embedder) = live_store();

    let v = check_exists(&store, Some(&embedder), "add a user account somewhere", ExistScope::Any)
        .expect("check_exists");

    assert_eq!(
        v.verdict,
        Verdict::Weak,
        "one incidental verb match, no ≥2-term overlap → WEAK even with vectors: {} (conf {:.2})",
        v.reason,
        v.confidence
    );
    assert!(!v.exact_name_hit, "must not claim a confident exact hit for a multi-word verb query");
    assert!(v.confidence < 0.6, "WEAK confidence stays modest, got {:.2}", v.confidence);
}

/// A query with NO lexical/name overlap at all (pure meaning): the oracle must NOT
/// invent an EXISTS from vectors — it stays ABSENT but hands back the nearest symbol
/// as a clearly-labelled LEAD.
#[test]
fn pure_semantic_only_is_absent_with_a_lead() {
    let (_d, store, embedder) = live_store();

    let v = check_exists(&store, Some(&embedder), "calculate the levy on a sale", ExistScope::Any)
        .expect("check_exists");

    assert_eq!(v.verdict, Verdict::Absent, "no name/mention match → ABSENT (vectors are a lead): {}", v.reason);
    assert_eq!(v.match_kind, None, "an ABSENT verdict makes no positive match claim");
    let lead = v.best_match.expect("a nearest-by-meaning lead is offered");
    assert_eq!(lead.name, "compute_tax", "the lead is the tax fn, got `{}`", lead.name);
    assert!(v.reason.contains("lead"), "reason labels it a lead to verify: {}", v.reason);
}
