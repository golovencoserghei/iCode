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

/// A file that leaves the tree takes its chunks with it.
///
/// This is the regression for the defect that made semantic search answer from
/// deleted code: `delete_file` used to clear only the `files` row, and
/// `code_chunks` hangs off `path`, not off `files.id`, so the FK cascade never
/// reached it. The chunks (and their vectors) outlived the file forever, because
/// a path that is never re-indexed never goes through `upsert_chunks`.
#[test]
fn a_deleted_file_leaves_no_chunks_behind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("alpha.rs"), SAMPLE).expect("write alpha");
    fs::write(root.join("beta.rs"), SAMPLE).expect("write beta");

    let store = SqliteCodeStore::open(root).expect("open store");
    index_path(root, &store).expect("index");
    let before = diagnose(&store, root).expect("diagnose");
    assert_eq!(before.files_indexed, 2);
    assert_eq!(before.ghost_chunks, 0, "nothing is a ghost yet");

    fs::remove_file(root.join("beta.rs")).expect("remove beta");
    index_path(root, &store).expect("re-index");

    let after = diagnose(&store, root).expect("diagnose after delete");
    assert_eq!(after.files_indexed, 1, "the deleted file left the index");
    assert_eq!(
        after.ghost_chunks, 0,
        "its chunks left with it — otherwise semantic search still returns them"
    );
    assert!(after.chunks < before.chunks, "the chunk count actually dropped");
    assert!(after.healthy, "healthy again after the delete: {after:?}");
}

/// An index written by an older binary carries ghosts nothing would ever clean:
/// their path is not re-indexed, so `upsert_chunks` never runs for it. `doctor`
/// must report them (and NOT call the tree healthy), and an index pass must clear
/// them.
#[test]
fn ghosts_from_an_older_index_are_reported_then_pruned() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("alpha.rs"), SAMPLE).expect("write alpha");

    let store = SqliteCodeStore::open(root).expect("open store");
    index_path(root, &store).expect("index");

    // Forge exactly what the old `delete_file` left: a chunk whose file is gone.
    store
        .insert_ghost_chunk_for_test("/gone/from/disk/ancient.rs")
        .expect("forge ghost");

    let sick = diagnose(&store, root).expect("diagnose with ghost");
    assert_eq!(sick.ghost_chunks, 1, "the ghost is counted");
    assert!(
        !sick.healthy,
        "a tree that can answer with deleted code is not healthy: {sick:?}"
    );

    let stats = index_path(root, &store).expect("re-index");
    assert_eq!(stats.ghost_chunks_pruned, 1, "the index pass swept it");

    let healed = diagnose(&store, root).expect("diagnose after prune");
    assert_eq!(healed.ghost_chunks, 0);
    assert!(healed.healthy, "healthy once the ghost is gone: {healed:?}");
}
