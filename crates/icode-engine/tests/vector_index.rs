//! M2 vector layer: the sqlite-vec `Vec0Index` and the `code_chunks` ↔ vector
//! plumbing on `SqliteCodeStore`.
//!
//! Asserts KNOWN cosine distances (not just a round-trip) the way the M0
//! `vec_spike` does, plus the chunk lifecycle: upsert_chunks → pending_chunks →
//! (write a vector + stamp the model) → pending drains → chunk_hits re-hydrates a
//! CodeHit.

use icode_core::model::{CodeChunk, SymbolKind};
use icode_core::traits::{CodeReadStore, CodeWriteStore, VectorIndex};
use icode_engine::SqliteCodeStore;

const DIM: usize = 1024;

/// A unit basis vector e_i in R^DIM (a 1.0 at index `i`, zeros elsewhere).
fn basis(i: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[i] = 1.0;
    v
}

/// normalize([1,1,0,…,0]) — at 45° to both e_0 and e_1.
fn diag_01() -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    let s = 1.0f32 / 2.0f32.sqrt();
    v[0] = s;
    v[1] = s;
    v
}

#[test]
fn vec0_knn_known_cosine_distances() {
    let store = SqliteCodeStore::open_in_memory().expect("open in-memory store");
    let idx = store.vector_index();
    assert_eq!(idx.dim(), DIM);

    let a = basis(0); // rowid 1
    let b = basis(1); // rowid 2 (orthogonal to a)
    let c = diag_01(); // rowid 3 (45° from a)

    idx.upsert_batch(&[(1, a.clone()), (2, b.clone()), (3, c.clone())])
        .expect("batch upsert");
    assert_eq!(idx.count().unwrap(), 3, "three vectors stored");

    // KNN with query == a: nearest is a (dist ~0), then c (~0.2929), then b (~1.0).
    let hits = idx.knn(&a, 3).expect("knn");
    assert_eq!(hits.len(), 3);

    assert_eq!(hits[0].rowid, 1, "nearest must be rowid 1 (identical to query)");
    assert!(hits[0].distance.abs() < 1e-4, "cos dist to identical ~0, got {}", hits[0].distance);

    assert_eq!(hits[1].rowid, 3, "second must be rowid 3 (45deg)");
    assert!(
        (hits[1].distance - 0.29289).abs() < 1e-3,
        "cos 45deg ~0.2929, got {}",
        hits[1].distance
    );

    assert_eq!(hits[2].rowid, 2, "third must be rowid 2 (orthogonal)");
    assert!((hits[2].distance - 1.0).abs() < 1e-3, "cos orthogonal ~1.0, got {}", hits[2].distance);

    // delete one → count drops; clear → empty.
    idx.delete(2).expect("delete");
    assert_eq!(idx.count().unwrap(), 2, "count after delete");
    idx.clear().expect("clear");
    assert_eq!(idx.count().unwrap(), 0, "count after clear");
}

#[test]
fn dim_mismatch_is_rejected() {
    let store = SqliteCodeStore::open_in_memory().unwrap();
    let idx = store.vector_index();
    // Wrong-length vector → DimMismatch (not a silent truncation).
    let err = idx.upsert(1, &[1.0, 0.0, 0.0]).unwrap_err();
    assert!(matches!(err, icode_core::error::Error::DimMismatch { index: DIM, got: 3 }));
    // Wrong-length query likewise.
    assert!(idx.knn(&[1.0, 0.0], 1).is_err());
}

#[test]
fn chunk_lifecycle_upsert_pending_embed_hits() {
    let store = SqliteCodeStore::open_in_memory().unwrap();
    let model = "qwen3-embedding:0.6b";

    let chunks = vec![
        CodeChunk {
            symbol_kind: SymbolKind::Function,
            symbol_id: Some(42),
            qualified_name: Some("app::svc::run".into()),
            path: "src/svc.rs".into(),
            line_start: 10,
            line_end: 20,
            chunk_text: "fn run() { /* … */ }".into(),
            content_hash: "h1".into(),
        },
        CodeChunk {
            symbol_kind: SymbolKind::Class,
            symbol_id: Some(43),
            qualified_name: Some("app::svc::Service".into()),
            path: "src/svc.rs".into(),
            line_start: 30,
            line_end: 50,
            chunk_text: "struct Service;".into(),
            content_hash: "h2".into(),
        },
    ];

    // upsert_chunks returns rowids in input order.
    let rowids = store.upsert_chunks("src/svc.rs", &chunks).expect("upsert_chunks");
    assert_eq!(rowids.len(), 2);
    assert!(rowids[0] < rowids[1], "rowids assigned in input order");

    // Both chunks lack a vector → both pending.
    let pending = store.pending_chunks(model, 100).expect("pending_chunks");
    assert_eq!(pending.len(), 2, "both chunks pending before embedding");
    assert_eq!(pending[0].0, rowids[0]);
    assert_eq!(pending[0].1, "fn run() { /* … */ }");

    // Emulate the indexer embedding the FIRST chunk: write its vector AND stamp
    // the model (pending_chunks keys on BOTH the vec row AND embed_model).
    let idx = store.vector_index();
    idx.upsert(rowids[0], &basis(0)).expect("write vector for chunk 0");
    store
        .mark_chunk_embedded(rowids[0], model, DIM)
        .expect("stamp embed model");

    let pending = store.pending_chunks(model, 100).expect("pending after one embed");
    assert_eq!(pending.len(), 1, "only the un-embedded chunk remains pending");
    assert_eq!(pending[0].0, rowids[1]);

    // chunk_hits re-hydrates a lean CodeHit.
    let hits = store.chunk_hits(&[rowids[0]]).expect("chunk_hits");
    assert_eq!(hits.len(), 1);
    let (rid, hit) = &hits[0];
    assert_eq!(*rid, rowids[0]);
    assert_eq!(hit.kind, SymbolKind::Function);
    assert_eq!(hit.qualified_name, "app::svc::run");
    assert_eq!(hit.name, "run", "name is the last segment of the qualified name");
    assert_eq!(hit.path, "src/svc.rs");
    assert_eq!(hit.line_start, 10);
    assert_eq!(hit.line_end, 20);
    assert_eq!(hit.score, 0.0, "score is a placeholder until the caller folds in distance");
    assert!(hit.snippet.is_none(), "lean hit: no snippet");
    assert!(!hit.stale);

    // stats() reflects the new rows.
    let stats = store.stats().unwrap();
    assert_eq!(stats.code_chunks, 2);
    assert_eq!(stats.vec_rows, 1);
}

#[test]
fn re_upsert_chunks_replaces_and_cascades_vectors() {
    let store = SqliteCodeStore::open_in_memory().unwrap();
    let chunk = CodeChunk {
        symbol_kind: SymbolKind::Function,
        symbol_id: None,
        qualified_name: Some("f".into()),
        path: "a.rs".into(),
        line_start: 1,
        line_end: 2,
        chunk_text: "fn f() {}".into(),
        content_hash: "h".into(),
    };
    let r1 = store.upsert_chunks("a.rs", std::slice::from_ref(&chunk)).unwrap();
    store.vector_index().upsert(r1[0], &basis(0)).unwrap();
    assert_eq!(store.vector_index().count().unwrap(), 1);

    // Re-indexing the same path deletes the old chunk; the AFTER DELETE trigger
    // must drop its orphaned vector (vec0 has no FK cascade).
    let _ = store.upsert_chunks("a.rs", std::slice::from_ref(&chunk)).unwrap();
    assert_eq!(
        store.vector_index().count().unwrap(),
        0,
        "old vector dropped by the code_chunks_ad trigger on re-upsert"
    );
}
