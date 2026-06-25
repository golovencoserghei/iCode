//! `doctor` diagnostics integration tests: prove a freshly-indexed tree reports
//! healthy with zero drift, that on-disk drift (a new file, a deleted file) is
//! detected, and that the embedding/orphan invariants hold.
//!
//! The embed assertions are guarded on a reachable Ollama: when the embedder is
//! up we assert `embedded == chunks`, `pending == 0`, `orphan_vectors == 0`;
//! when it is down we still assert `chunks > 0` and `orphan_vectors == 0` (the
//! orphan invariant never depends on the embedder running).

use std::fs;

use icode_core::config::EmbedConfig;
use icode_core::traits::Embedder;
use icode_embed::OllamaEmbedder;
use icode_engine::{diagnose, embed_pending, index_path, SqliteCodeStore};

const SAMPLE: &str = r#"
/// Add two numbers.
pub fn add(a: u64, b: u64) -> u64 {
    a + b
}

pub struct Counter {
    n: u64,
}

impl Counter {
    pub fn bump(&mut self) {
        self.n += 1;
    }
}
"#;

/// (1) A freshly-indexed temp project diagnoses healthy: no missing/outdated/
/// stale, no orphan vectors, no parse errors.
#[test]
fn fresh_index_is_healthy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("lib.rs"), SAMPLE).expect("write sample");

    let store = SqliteCodeStore::open(root).expect("open store");
    let stats = index_path(root, &store).expect("index");
    assert_eq!(stats.files_indexed, 1, "one .rs file indexed");

    let report = diagnose(&store, root).expect("diagnose");

    assert!(
        report.healthy,
        "fresh index must be healthy, got {report:?}"
    );
    assert_eq!(report.files_indexed, 1);
    assert!(report.missing.is_empty(), "no missing files: {:?}", report.missing);
    assert_eq!(report.missing_count, 0);
    assert!(report.outdated.is_empty(), "no outdated files: {:?}", report.outdated);
    assert_eq!(report.outdated_count, 0);
    assert!(report.stale.is_empty(), "no stale files: {:?}", report.stale);
    assert_eq!(report.stale_count, 0);
    assert_eq!(report.orphan_vectors, 0, "no orphan vectors after a clean index");
    assert_eq!(report.parse_errors, 0, "no parse errors on valid Rust");
}

/// (2a) Adding a NEW source file on disk (not in the index) → `missing` non-empty,
/// `healthy == false`.
#[test]
fn new_file_on_disk_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("lib.rs"), SAMPLE).expect("write sample");

    let store = SqliteCodeStore::open(root).expect("open store");
    index_path(root, &store).expect("index");

    // Fresh index is healthy.
    assert!(diagnose(&store, root).expect("diagnose").healthy);

    // Drop a new .rs file the index has never seen.
    fs::write(root.join("extra.rs"), "pub fn extra() -> u64 { 7 }\n").expect("write extra");

    let report = diagnose(&store, root).expect("diagnose after add");
    assert!(!report.healthy, "an un-indexed file makes the index unhealthy");
    assert!(
        report.missing.iter().any(|p| p.ends_with("extra.rs")),
        "extra.rs should be reported missing, got {:?}",
        report.missing
    );
    assert_eq!(report.missing_count, 1, "exactly one missing file");
}

/// (2b) Deleting an indexed file from disk → `stale` non-empty, `healthy == false`.
#[test]
fn deleted_file_is_stale() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("lib.rs"), SAMPLE).expect("write sample");
    fs::write(root.join("gone.rs"), "pub fn gone() {}\n").expect("write gone");

    let store = SqliteCodeStore::open(root).expect("open store");
    index_path(root, &store).expect("index");
    assert!(diagnose(&store, root).expect("diagnose").healthy);

    // Remove a file that IS in the index.
    fs::remove_file(root.join("gone.rs")).expect("remove gone.rs");

    let report = diagnose(&store, root).expect("diagnose after delete");
    assert!(!report.healthy, "an indexed-but-deleted file makes the index unhealthy");
    assert!(
        report.stale.iter().any(|p| p.ends_with("gone.rs")),
        "gone.rs should be reported stale, got {:?}",
        report.stale
    );
    assert_eq!(report.stale_count, 1, "exactly one stale file");
}

/// (3) Orphan invariant + embedding counters. After indexing, `chunks > 0` and
/// `orphan_vectors == 0` ALWAYS (the delete-mirror trigger guarantees it). When a
/// live Ollama is reachable, run the embed pass and additionally assert
/// `embedded == chunks` and `pending_embeddings == 0`.
#[test]
fn orphan_invariant_holds_and_embedding_counters_track() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("lib.rs"), SAMPLE).expect("write sample");

    let store = SqliteCodeStore::open(root).expect("open store");
    index_path(root, &store).expect("index");

    let before = diagnose(&store, root).expect("diagnose pre-embed");
    assert!(before.chunks > 0, "indexer wrote chunks, got {}", before.chunks);
    assert_eq!(before.orphan_vectors, 0, "no orphan vectors ever (delete-mirror trigger)");
    // Before any embedding, every chunk is pending and nothing is embedded.
    assert_eq!(before.embedded, 0, "no vectors before the embed pass");
    assert_eq!(
        before.pending_embeddings, before.chunks,
        "all chunks pending before embedding"
    );

    // Best-effort live embed pass: only assert the embedded invariants if Ollama
    // is actually reachable (otherwise the orphan/chunk invariants above suffice).
    let cfg = EmbedConfig::default();
    let embedder = match OllamaEmbedder::new(&cfg) {
        Ok(e) if e.health().is_ok() => e,
        _ => {
            eprintln!("doctor: Ollama unavailable — skipping the live embed assertions");
            return;
        }
    };

    embed_pending(&store, &embedder, cfg.batch).expect("embed_pending");

    let after = diagnose(&store, root).expect("diagnose post-embed");
    assert_eq!(
        after.embedded, after.chunks,
        "every chunk embedded: embedded={} chunks={}",
        after.embedded, after.chunks
    );
    assert_eq!(after.pending_embeddings, 0, "no pending embeddings after a full embed pass");
    assert_eq!(after.orphan_vectors, 0, "still no orphan vectors after embedding");
    assert!(after.healthy, "fully indexed + embedded tree is healthy, got {after:?}");
}
