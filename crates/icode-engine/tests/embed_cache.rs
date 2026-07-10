//! Deterministic (NO Ollama) proof of the content-addressed embed cache.
//!
//! The cache (`embed_cache`: `content_hash → vector`) is what makes re-indexing
//! free of redundant embedding — crucially the `git checkout` ping-pong the daemon
//! used to churn through. This test stands in a counting fake `Embedder` for the
//! real one and asserts the embedder is called ONLY for genuinely-new chunk text:
//!
//!   * first embed of file A — every chunk is a miss (embedder called),
//!   * switch the file to B, re-embed — B's chunks are misses (embedder called),
//!   * switch BACK to A, re-embed — A's chunks are byte-identical to step 1, so
//!     they REHYDRATE from the cache with ZERO embedder calls.
//!
//! Re-indexing here uses the real `index_path` (same write path a branch switch
//! drives via the daemon): it DELETEs each file's chunks (dropping their vectors)
//! and re-inserts fresh, pending rows — exactly the state the cache must rescue.

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use icode_core::error::Result;
use icode_core::traits::{CodeReadStore, Embedder};
use icode_engine::{embed_pending, embed_pending_capped, index_path, SqliteCodeStore};

/// Output dim — must match the `vec_code` vec0 column width (`schema::VEC_DIM`),
/// else `upsert_batch` rejects the vector with `DimMismatch`.
const DIM: usize = 1024;

const SAMPLE_A: &str = r#"
/// Read the entire contents of a file from disk into a String.
pub fn read_file(path: &str) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

/// Compute a SHA-256 hex digest of the given bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x?}", bytes)
}
"#;

const SAMPLE_B: &str = r#"
/// Send an HTTP GET request to a URL and return the response body text.
pub fn http_get(url: &str) -> String {
    format!("GET {url}")
}

/// Parse a JSON document string into a node count.
pub fn parse_json(doc: &str) -> usize {
    doc.len()
}
"#;

/// A fake embedder that counts how many texts it was asked to embed and returns a
/// deterministic non-zero vector per text. It NEVER touches the network, so this
/// test is hermetic — and the call counter is the whole point: it must not move
/// when the cache should have served the request.
struct CountingEmbedder {
    calls: AtomicUsize,
}

impl CountingEmbedder {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Embedder for CountingEmbedder {
    fn model_id(&self) -> &str {
        "fake-counting-model"
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

/// Deterministic, content-derived, non-zero vector of length `DIM`.
fn fake_vec(text: &str) -> Vec<f32> {
    let bytes = text.as_bytes();
    (0..DIM)
        .map(|i| {
            let b = if bytes.is_empty() {
                1.0
            } else {
                bytes[i % bytes.len()] as f32
            };
            (i as f32) * 0.001 + b + 0.1
        })
        .collect()
}

#[test]
fn cache_rehydrates_on_reindex_without_re_embedding() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let file = root.join("io.rs");

    let store = SqliteCodeStore::open(root).expect("open store");
    let emb = CountingEmbedder::new();

    // ── step 1: index A and embed every chunk (cold cache → all misses) ──
    fs::write(&file, SAMPLE_A).expect("write A");
    index_path(root, &store).expect("index A");
    let n_a = store.stats().expect("stats").code_chunks as usize;
    assert!(n_a > 0, "file A produced chunks");

    let s1 = embed_pending(&store, &emb, 32).expect("embed A");
    assert_eq!(s1.embedded, n_a, "every A chunk embedded on a cold cache");
    assert_eq!(s1.rehydrated, 0, "nothing to rehydrate yet");
    assert_eq!(emb.calls(), n_a, "embedder called once per A chunk");
    let st = store.stats().expect("stats");
    assert_eq!(st.vec_rows, st.code_chunks, "vectors complete after A");

    // ── step 2: switch the file to B and re-index+embed (B is new → misses) ──
    fs::write(&file, SAMPLE_B).expect("write B");
    index_path(root, &store).expect("index B");
    let calls_before_b = emb.calls();
    let s2 = embed_pending(&store, &emb, 32).expect("embed B");
    assert!(s2.embedded > 0, "B's new chunks were embedded");
    assert!(
        emb.calls() > calls_before_b,
        "embedder was called for genuinely-new B content"
    );

    // ── step 3: switch BACK to A — byte-identical to step 1, so it must come
    //            entirely from the cache with ZERO embedder calls ──
    fs::write(&file, SAMPLE_A).expect("write A again");
    index_path(root, &store).expect("re-index A");
    let calls_before_revert = emb.calls();

    let s3 = embed_pending(&store, &emb, 32).expect("embed reverted A");
    assert_eq!(
        emb.calls(),
        calls_before_revert,
        "revert to A made NO embedder calls — fully served from the cache"
    );
    assert_eq!(s3.embedded, 0, "no chunk re-embedded on revert");
    assert_eq!(s3.rehydrated, n_a, "all A chunks rehydrated from the cache");

    // The vector index is whole again (graph and vectors back in lock-step).
    let st = store.stats().expect("stats");
    assert_eq!(
        st.vec_rows, st.code_chunks,
        "vectors restored from cache after the revert ({} vec vs {} chunks)",
        st.vec_rows, st.code_chunks
    );

    // A second pass is a no-op: the queue is drained, still no embedder calls.
    let calls_steady = emb.calls();
    let s4 = embed_pending(&store, &emb, 32).expect("idempotent pass");
    assert_eq!(s4.embedded, 0);
    assert_eq!(s4.rehydrated, 0);
    assert_eq!(emb.calls(), calls_steady, "idempotent: nothing pending");
}

/// An INTERACTIVE (JIT) call must never turn into a full-project embed. `embed_pending`
/// loops until the queue is empty; `embed_pending_capped` stops at `max_chunks`, so a
/// single `recall` / semantic query backfills a slice and answers instead of pinning
/// the GPU for the whole index.
#[test]
fn capped_embed_never_drains_the_whole_queue() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("io.rs"), SAMPLE_A).expect("write");
    let store = SqliteCodeStore::open(dir.path()).expect("open");
    index_path(dir.path(), &store).expect("index");
    let n = store.stats().expect("stats").code_chunks as usize;
    assert!(n >= 2, "sample yields at least two chunks, got {n}");

    let emb = CountingEmbedder::new();
    // Cap at ONE chunk: the call must touch exactly one, not all `n`.
    let s = embed_pending_capped(&store, &emb, 32, 1).expect("capped embed");
    assert_eq!(s.embedded + s.rehydrated, 1, "exactly one chunk processed");
    assert_eq!(emb.calls(), 1, "embedder called for exactly one chunk");

    let st = store.stats().expect("stats");
    assert!(
        st.vec_rows < st.code_chunks,
        "queue must NOT be drained by a capped call ({} vec vs {} chunks)",
        st.vec_rows,
        st.code_chunks
    );

    // The bulk path still drains the remainder.
    embed_pending(&store, &emb, 32).expect("bulk drain");
    let st = store.stats().expect("stats");
    assert_eq!(st.vec_rows, st.code_chunks, "bulk drain completes the index");
}

/// The cross-project / worktree payoff: with a SHARED central embed cache, the same
/// code at the same repo-relative path — checked out under a DIFFERENT root, exactly
/// as a git worktree or a second clone is — re-embeds NOTHING. It relies on two
/// pieces working together: (1) `with_embed_cache_db` routes the cache to one shared
/// db, and (2) the chunk header path is rendered relative to the project root, so a
/// chunk's `content_hash` is independent of where the repo lives on disk.
#[test]
fn shared_central_cache_reused_across_projects() {
    let central_dir = tempfile::tempdir().expect("central tmp");
    let central = central_dir.path().join("central.db");
    let central = central.to_str().unwrap();

    let emb = CountingEmbedder::new();

    // ── Project A (its own root): index + embed into the SHARED cache (cold) ──
    let dir_a = tempfile::tempdir().expect("A tmp");
    fs::write(dir_a.path().join("io.rs"), SAMPLE_A).expect("write A");
    let store_a = SqliteCodeStore::open(dir_a.path())
        .expect("open A")
        .with_embed_cache_db(central)
        .expect("A shared cache");
    index_path(dir_a.path(), &store_a).expect("index A");
    let n = store_a.stats().expect("stats").code_chunks as usize;
    assert!(n > 0, "A produced chunks");
    let sa = embed_pending(&store_a, &emb, 32).expect("embed A");
    assert_eq!(sa.embedded, n, "cold shared cache: A embeds every chunk");
    assert_eq!(emb.calls(), n, "embedder called once per A chunk");

    // ── Project B: a DIFFERENT root (worktree/clone), SAME code at the SAME
    //    repo-relative path, SAME shared cache ──
    let dir_b = tempfile::tempdir().expect("B tmp");
    fs::write(dir_b.path().join("io.rs"), SAMPLE_A).expect("identical code in B");
    let store_b = SqliteCodeStore::open(dir_b.path())
        .expect("open B")
        .with_embed_cache_db(central)
        .expect("B shared cache");
    index_path(dir_b.path(), &store_b).expect("index B");
    let before = emb.calls();
    let sb = embed_pending(&store_b, &emb, 32).expect("embed B");

    // The payoff: B re-embeds NOTHING — every chunk is served from the shared cache
    // A filled, because the chunk text is checkout-location-independent.
    assert_eq!(emb.calls(), before, "B made ZERO embedder calls — served from the shared cache");
    assert_eq!(sb.embedded, 0, "no chunk embedded in B");
    assert_eq!(sb.rehydrated, n, "all of B's chunks rehydrated from the shared cache");
    let st = store_b.stats().expect("stats");
    assert_eq!(st.vec_rows, st.code_chunks, "B's vectors complete without embedding");
}
