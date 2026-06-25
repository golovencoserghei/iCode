//! End-to-end LIVE test (requires a running Ollama with the configured embedding
//! model). It is deliberately NOT `#[ignore]`: this is the proof the chunk+embed
//! phase closes the semantic pipeline symbols → chunks → embeddings → vec0.
//!
//! Flow: index a temp Rust project (real `index_path`) → assert chunks were
//! written → build the concrete `OllamaEmbedder` (icode-embed is a dev-dependency
//! ONLY; production icode-engine depends on the `Embedder` trait, never on the
//! embed crate) → `embed_pending` → assert `vec_rows == code_chunks` and the
//! pending queue is empty → KNN a natural-language query and re-hydrate the top
//! rowid via `chunk_hits` into a meaningful symbol.

use std::fs;

use icode_core::config::EmbedConfig;
use icode_core::traits::{CodeReadStore, CodeWriteStore, Embedder, VectorIndex};
use icode_embed::OllamaEmbedder;
use icode_engine::{embed_pending, index_path, SqliteCodeStore};

/// A small but realistic Rust file: a parser-ish function whose docstring/name
/// should be semantically nearest to "parse rust source".
const SAMPLE: &str = r#"
use tree_sitter::Parser;

/// Parse Rust source text into a syntax tree and walk its top-level items.
pub fn parse_rust_source(source: &str) -> usize {
    let mut parser = Parser::new();
    let _ = parser;
    source.lines().count()
}

/// Compute a SHA-256 hex digest of the given bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x?}", bytes)
}

pub struct TreeWalker {
    depth: usize,
}

impl TreeWalker {
    /// Recursively descend the parsed AST, accumulating node counts.
    pub fn walk(&self, node_count: usize) -> usize {
        self.depth + node_count
    }
}
"#;

#[test]
fn embed_pipeline_indexes_embeds_and_knn_retrieves() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("parser.rs"), SAMPLE).expect("write sample");

    // ── index the graph + chunks ──
    let store = SqliteCodeStore::open(root).expect("open store");
    let stats = index_path(root, &store).expect("index");
    assert!(
        stats.code_chunks > 0,
        "indexer wrote code_chunks, got {}",
        stats.code_chunks
    );

    let db_before = store.stats().expect("stats");
    assert_eq!(
        db_before.code_chunks, stats.code_chunks,
        "code_chunks count matches IndexStats"
    );
    assert_eq!(db_before.vec_rows, 0, "no vectors before the embed pass");

    // ── build the concrete embedder (Ollama must be up; this test is not ignored) ──
    let cfg = EmbedConfig::default();
    let embedder = OllamaEmbedder::new(&cfg).expect("build OllamaEmbedder");
    embedder
        .health()
        .expect("Ollama must be running with the embedding model for embed_e2e");

    // ── run the embed pass ──
    let emb = embed_pending(&store, &embedder, cfg.batch).expect("embed_pending");
    assert_eq!(
        emb.embedded as u64, db_before.code_chunks,
        "every chunk embedded: {} embedded vs {} chunks",
        emb.embedded, db_before.code_chunks
    );

    // vec_rows == code_chunks, and the pending queue is now empty (idempotency:
    // a second pass embeds nothing).
    let db_after = store.stats().expect("stats");
    assert_eq!(
        db_after.vec_rows, db_after.code_chunks,
        "vec_rows ({}) must equal code_chunks ({})",
        db_after.vec_rows, db_after.code_chunks
    );
    let pending = store
        .pending_chunks(embedder.model_id(), usize::MAX)
        .expect("pending");
    assert!(
        pending.is_empty(),
        "pending queue drained, {} left",
        pending.len()
    );

    let emb2 = embed_pending(&store, &embedder, cfg.batch).expect("second embed_pending");
    assert_eq!(emb2.embedded, 0, "idempotent: a re-run embeds nothing");

    // ── semantic retrieval: KNN over the query embedding ──
    let qvec = embedder.embed(&["parse rust source"]).expect("embed query");
    assert_eq!(qvec.len(), 1);
    let neighbors = store.vector_index().knn(&qvec[0], 3).expect("knn");
    assert!(!neighbors.is_empty(), "knn returned at least one neighbor");

    // ── re-hydrate the top hit into a CodeHit; it should be a meaningful symbol ──
    let top_rowid = neighbors[0].rowid;
    let hits = store.chunk_hits(&[top_rowid]).expect("chunk_hits");
    assert_eq!(hits.len(), 1, "top rowid re-hydrates to one CodeHit");
    let (_id, hit) = &hits[0];
    assert!(!hit.qualified_name.is_empty(), "hit has a qualified name");
    assert!(!hit.path.is_empty(), "hit has a path");

    // The nearest symbol to "parse rust source" should be the parser function
    // (its name and docstring dominate the chunk text). We assert this softly: it
    // must be one of the indexed symbols, and most likely the parse function.
    let qnames: Vec<String> = store
        .chunk_hits(&neighbors.iter().map(|n| n.rowid).collect::<Vec<_>>())
        .expect("chunk_hits batch")
        .into_iter()
        .map(|(_, h)| h.qualified_name)
        .collect();
    assert!(
        qnames.iter().any(|q| q.contains("parse_rust_source")),
        "the parse function should be among the nearest neighbors to 'parse rust source', got {qnames:?}"
    );
}
