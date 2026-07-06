//! Wave 2 — INCREMENTAL full-index correctness.
//!
//! `index_path` must produce the same GRAPH as a clean reindex while doing only the
//! work that changed. These tests pin the four properties that matter:
//!   (a) re-indexing an unchanged tree changes NOTHING — no graph churn AND no new
//!       pending embeddings (unchanged files keep their vectors, never re-embed);
//!   (b) editing ONE file re-indexes only that file (others keep their vectors);
//!   (c) deleting a file from disk removes its symbols on the next pass;
//!   (d) the parallel parse is DETERMINISTIC — a fresh index of the same tree is
//!       reproducible run to run (same symbols, same positions, same counts).
//!
//! Embedding here is hermetic: a counting fake `Embedder` (NO Ollama) stands in for
//! the real one, so the "did anything re-embed?" assertions are exact and fast.

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use icode_core::error::Result;
use icode_core::traits::{CodeReadStore, Embedder};
use icode_engine::{embed_pending, index_path, SqliteCodeStore};

/// Must match the `vec_code` vec0 column width (`schema::VEC_DIM`).
const DIM: usize = 1024;

/// A hermetic embedder that counts how many texts it embedded. The counter is the
/// whole point: it must NOT move when nothing genuinely changed.
struct CountingEmbedder {
    calls: AtomicUsize,
}
impl CountingEmbedder {
    fn new() -> Self {
        Self { calls: AtomicUsize::new(0) }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
impl Embedder for CountingEmbedder {
    fn model_id(&self) -> &str {
        "fake-incremental-model"
    }
    fn dim(&self) -> usize {
        DIM
    }
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|t| fake_vec(t)).collect())
    }
    fn health(&self) -> Result<()> {
        Ok(())
    }
}
fn fake_vec(text: &str) -> Vec<f32> {
    let bytes = text.as_bytes();
    (0..DIM)
        .map(|i| {
            let b = if bytes.is_empty() { 1.0 } else { bytes[i % bytes.len()] as f32 };
            (i as f32) * 0.001 + b + 0.1
        })
        .collect()
}

/// A comparable snapshot of the code graph: (files, functions, classes, calls,
/// imports, routes, chunks) plus every symbol as (kind, qualified_name, path,
/// line_start, line_end), sorted. Two equal snapshots ⇒ the same graph.
type Snapshot = ((u64, u64, u64, u64, u64, u64, u64), Vec<(String, String, String, u32, u32)>);

fn snapshot(store: &SqliteCodeStore) -> Snapshot {
    let files = store.list_files(None, None, usize::MAX).expect("list_files");
    let mut symbols: Vec<(String, String, String, u32, u32)> = Vec::new();
    for f in &files {
        for h in store.file_outline(&f.path).expect("file_outline") {
            symbols.push((
                format!("{:?}", h.kind),
                h.qualified_name,
                h.path,
                h.line_start,
                h.line_end,
            ));
        }
    }
    symbols.sort();
    let st = store.stats().expect("stats");
    (
        (st.files, st.functions, st.classes, st.calls, st.imports, st.routes, st.code_chunks),
        symbols,
    )
}

// ──────────────────────────── (a) unchanged tree is a no-op ────────────────────

#[test]
fn reindex_unchanged_tree_makes_no_change_and_no_pending() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("a.rs"), "pub fn alpha() -> u32 { 1 }\n").expect("write a");
    fs::write(root.join("b.rs"), "pub fn beta() -> u32 { 2 }\n").expect("write b");

    let store = SqliteCodeStore::open(root).expect("open");
    let s1 = index_path(root, &store).expect("index 1");
    assert_eq!(s1.files_indexed, 2, "both files indexed on the first pass");
    assert_eq!(s1.files_skipped, 0);
    assert_eq!(s1.files_deleted, 0);

    // Embed everything, then confirm the queue is drained.
    let emb = CountingEmbedder::new();
    embed_pending(&store, &emb, 16).expect("embed");
    assert!(emb.calls() > 0, "cold cache: embedder called at least once");
    assert_eq!(store.pending_embeddings_count().expect("pending"), 0, "queue drained");

    let before = snapshot(&store);
    let st_before = store.stats().expect("stats");
    assert_eq!(st_before.vec_rows, st_before.code_chunks, "all chunks embedded");

    // ── re-index the UNCHANGED tree ──────────────────────────────────────────
    let s2 = index_path(root, &store).expect("index 2 (unchanged)");
    assert_eq!(s2.files_indexed, 0, "nothing re-parsed on an unchanged pass");
    assert_eq!(s2.files_skipped, 2, "both files skipped by content-hash");
    assert_eq!(s2.files_deleted, 0);

    // The crux: unchanged files did NOT bounce back into the embed queue.
    assert_eq!(
        store.pending_embeddings_count().expect("pending after no-op"),
        0,
        "unchanged files must NOT null their vectors / re-enter the queue"
    );

    // A subsequent embed pass calls the embedder ZERO times (nothing pending).
    let calls = emb.calls();
    let es = embed_pending(&store, &emb, 16).expect("embed no-op");
    assert_eq!(emb.calls(), calls, "no re-embedding after an unchanged reindex");
    assert_eq!(es.embedded, 0);
    assert_eq!(es.rehydrated, 0);

    // The graph is byte-for-byte identical.
    assert_eq!(snapshot(&store), before, "unchanged reindex must not alter the graph");
}

// ──────────────────────────── (b) one edit re-indexes only that file ───────────

#[test]
fn editing_one_file_reindexes_only_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("a.rs"), "pub fn alpha() -> u32 { 1 }\n").expect("write a");
    fs::write(root.join("b.rs"), "pub fn beta() -> u32 { 2 }\n").expect("write b");

    let store = SqliteCodeStore::open(root).expect("open");
    index_path(root, &store).expect("index 1");
    let emb = CountingEmbedder::new();
    embed_pending(&store, &emb, 16).expect("embed 1");
    assert_eq!(store.pending_embeddings_count().expect("pending"), 0);

    // ── edit only b.rs ───────────────────────────────────────────────────────
    fs::write(root.join("b.rs"), "pub fn beta_changed() -> u32 { 22 }\n").expect("edit b");
    let s = index_path(root, &store).expect("index 2");
    assert_eq!(s.files_indexed, 1, "only the changed file re-parsed");
    assert_eq!(s.files_skipped, 1, "the unchanged file was skipped");
    assert_eq!(s.files_deleted, 0);

    // a.rs's symbol survives; b.rs's old symbol is replaced by the new one.
    assert!(store.get_function("alpha", None, false).unwrap().is_some(), "alpha survives");
    assert!(store.get_function("beta", None, false).unwrap().is_none(), "old beta replaced");
    assert!(
        store.get_function("beta_changed", None, false).unwrap().is_some(),
        "new symbol present"
    );

    // a.rs's vector was NOT dropped: only b.rs's chunk is pending now.
    let st = store.stats().expect("stats");
    assert!(st.vec_rows > 0, "a.rs's vector survived the reindex of b.rs");
    assert!(
        st.vec_rows < st.code_chunks,
        "b.rs's new chunk is pending (vec_rows {} < chunks {})",
        st.vec_rows,
        st.code_chunks
    );
    assert_eq!(
        store.pending_embeddings_count().unwrap(),
        st.code_chunks - st.vec_rows,
        "exactly the changed file's chunks are pending"
    );

    // Embedding the delta restores lock-step.
    embed_pending(&store, &emb, 16).expect("embed delta");
    let st = store.stats().expect("stats");
    assert_eq!(st.vec_rows, st.code_chunks, "vectors whole again after embedding the delta");
}

// ──────────────────────────── (c) deletion removes symbols ─────────────────────

#[test]
fn deleting_a_file_removes_its_symbols() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("a.rs"), "pub fn alpha() -> u32 { 1 }\n").expect("write a");
    fs::write(root.join("b.rs"), "pub fn beta() -> u32 { 2 }\n").expect("write b");

    let store = SqliteCodeStore::open(root).expect("open");
    index_path(root, &store).expect("index 1");
    assert_eq!(store.stats().unwrap().files, 2);

    // ── remove b.rs from disk, then re-index ─────────────────────────────────
    fs::remove_file(root.join("b.rs")).expect("rm b");
    let s = index_path(root, &store).expect("index 2");
    assert_eq!(s.files_deleted, 1, "the vanished file was dropped");
    assert_eq!(s.files_indexed, 0, "the surviving file was not re-parsed");
    assert_eq!(s.files_skipped, 1, "the surviving file was skipped");

    // b.rs's symbol is gone; a.rs's remains; the index matches a clean reindex.
    assert!(store.get_function("beta", None, false).unwrap().is_none(), "beta removed");
    assert!(store.get_function("alpha", None, false).unwrap().is_some(), "alpha remains");
    let st = store.stats().expect("stats");
    assert_eq!(st.files, 1, "only a.rs left in the index");
    assert_eq!(st.functions, 1);
}

// ──────────────────────────── (d) parallel parse is deterministic ──────────────

#[test]
fn parallel_index_is_deterministic() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    // Several files with cross-file calls so both parsing AND edge resolution have
    // something to (re)produce identically across runs.
    for i in 0..8 {
        let src = format!(
            "pub fn f{i}(x: u32) -> u32 {{ helper_{i}(x) }}\n\
             pub fn helper_{i}(x: u32) -> u32 {{ x + {i} }}\n\
             pub struct S{i} {{ n: u32 }}\n\
             impl S{i} {{ pub fn go(&self) -> u32 {{ self.n }} }}\n"
        );
        fs::write(root.join(format!("m{i}.rs")), src).expect("write module");
    }

    // Fresh index three times (wiping the db between runs); every run must yield the
    // same graph regardless of how rayon scheduled the parallel parse.
    let mut snaps: Vec<Snapshot> = Vec::new();
    for _ in 0..3 {
        let _ = fs::remove_dir_all(root.join(".icode"));
        let store = SqliteCodeStore::open(root).expect("open");
        index_path(root, &store).expect("index");
        snaps.push(snapshot(&store));
    }

    assert_eq!(snaps[0], snaps[1], "run 1 == run 2 (deterministic)");
    assert_eq!(snaps[1], snaps[2], "run 2 == run 3 (deterministic)");
    // Sanity: the tree actually produced a non-trivial graph.
    assert_eq!(snaps[0].0 .0, 8, "8 files indexed");
    assert!(snaps[0].0 .1 >= 24, "at least 24 functions (f/helper/go per module)");
}
