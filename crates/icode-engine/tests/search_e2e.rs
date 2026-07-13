//! End-to-end LIVE test of the semantic search SERVICE (requires a running Ollama
//! with the configured embedding model — like `embed_e2e`, deliberately NOT
//! `#[ignore]`). It proves the `search` module's three entry points work against
//! real vectors:
//!
//!   * `semantic_search("read a file from disk")` surfaces the file-reading fn,
//!   * `hybrid_search(...)` (RRF of dense+lexical) keeps it near the top,
//!   * `find_similar(<symbol>)` returns a non-empty, sensible list that NEVER
//!     contains the query symbol itself.
//!
//! Decoupling note: `icode-embed` is a DEV-dependency only. Production
//! `icode-engine` (incl. the `search` module) depends solely on the `Embedder`
//! trait from `icode-core` — never on the concrete embed crate. This test, being
//! a dev artefact, is allowed to construct the concrete `OllamaEmbedder`.

use std::fs;

use icode_core::config::EmbedConfig;
use icode_core::traits::Embedder;
use icode_embed::OllamaEmbedder;
use icode_engine::{embed_pending, find_similar, hybrid_search, index_path, semantic_search, SqliteCodeStore};

/// A small Rust file with a few clearly-distinct behaviours so the dense ranker
/// has real semantic signal to separate (read-a-file vs hash-bytes vs http-get).
const SAMPLE: &str = r#"
use std::fs;

/// Read the entire contents of a file from disk into a String.
pub fn read_file(path: &str) -> std::io::Result<String> {
    fs::read_to_string(path)
}

/// Open a file on disk and load all of its bytes into memory.
pub fn load_file_bytes(path: &str) -> std::io::Result<Vec<u8>> {
    fs::read(path)
}

/// Compute a SHA-256 hex digest of the given bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x?}", bytes)
}

/// Send an HTTP GET request to a URL and return the response body as text.
pub fn http_get(url: &str) -> String {
    format!("GET {url}")
}

/// Parse a JSON document string into a generic value tree.
pub fn parse_json(doc: &str) -> usize {
    doc.len()
}
"#;

/// Build the store, index `SAMPLE`, and embed all chunks. Returns the store and a
/// live embedder. Panics with a clear message if Ollama is not reachable.
fn setup() -> (tempfile::TempDir, SqliteCodeStore, OllamaEmbedder) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("io.rs"), SAMPLE).expect("write sample");

    let store = SqliteCodeStore::open(root).expect("open store");
    let stats = index_path(root, &store).expect("index");
    assert!(stats.code_chunks > 0, "indexer wrote code_chunks");

    let cfg = EmbedConfig::default();
    let embedder = OllamaEmbedder::new(&cfg).expect("build OllamaEmbedder");
    embedder
        .health()
        .expect("Ollama must be running with the embedding model for search_e2e");

    let emb = embed_pending(&store, &embedder, cfg.batch).expect("embed_pending");
    assert!(emb.embedded > 0, "embedded at least one chunk");

    (dir, store, embedder)
}

#[test]
fn semantic_search_finds_read_file() {
    let (_dir, store, embedder) = setup();

    let hits = semantic_search(&store, &embedder, "read a file from disk", 5).expect("semantic");
    assert!(!hits.is_empty(), "semantic search returned hits");

    // The file-reading functions should dominate the top of the ranking over the
    // hashing / http / json functions. We assert read_file is in the top 2.
    let top2: Vec<&str> = hits.iter().take(2).map(|h| h.name.as_str()).collect();
    assert!(
        top2.contains(&"read_file") || top2.contains(&"load_file_bytes"),
        "a file-reading fn should be in the top-2 for 'read a file from disk', got {:?}",
        hits.iter().map(|h| (&h.name, h.score)).collect::<Vec<_>>()
    );

    // Scores are cosine similarities and must be sorted descending.
    for w in hits.windows(2) {
        assert!(w[0].score >= w[1].score, "semantic hits sorted by score desc");
    }
}

#[test]
fn hybrid_search_finds_read_file() {
    let (_dir, store, embedder) = setup();

    let hits = hybrid_search(&store, &embedder, "read a file from disk", 5).expect("hybrid");
    assert!(!hits.is_empty(), "hybrid search returned hits");

    let top3: Vec<&str> = hits.iter().take(3).map(|h| h.name.as_str()).collect();
    assert!(
        top3.contains(&"read_file") || top3.contains(&"load_file_bytes"),
        "a file-reading fn should be in the top-3 for hybrid 'read a file from disk', got {:?}",
        hits.iter().map(|h| (&h.name, h.score)).collect::<Vec<_>>()
    );

    // RRF scores are positive and descending.
    for w in hits.windows(2) {
        assert!(w[0].score >= w[1].score, "hybrid hits sorted by RRF score desc");
    }
    assert!(hits[0].score > 0.0, "RRF score is positive");
}

#[test]
fn find_similar_excludes_self_and_returns_neighbours() {
    let (_dir, store, _embedder) = setup();

    // read_file and load_file_bytes are near-synonyms; the nearest neighbour of
    // read_file should be load_file_bytes, and read_file must NOT appear.
    //
    // NOTE: `find_similar` no longer takes an embedder — it is MinHash over token
    // shingles, so this runs with no model at all (the `setup()` embedder is still
    // built for the semantic/hybrid cases in this file, just not used here).
    let hits = find_similar(&store, "read_file", 5).expect("find_similar");
    assert!(!hits.is_empty(), "find_similar returned a non-empty list");
    assert!(
        hits.iter().all(|h| h.name != "read_file" && h.qualified_name != "read_file"),
        "find_similar must exclude the query symbol itself, got {:?}",
        hits.iter().map(|h| &h.qualified_name).collect::<Vec<_>>()
    );
    // The nearest neighbour should be the other file-loading function.
    assert_eq!(
        hits[0].name, "load_file_bytes",
        "nearest neighbour of read_file should be load_file_bytes, got {:?}",
        hits.iter().map(|h| (&h.name, h.score)).collect::<Vec<_>>()
    );
}
