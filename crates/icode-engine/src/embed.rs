//! Embed pass: drain the pending-chunk queue into the vector index.
//!
//! This is the back half of the chunk+embed phase (symbols → chunks → embeddings
//! → vec0). [`chunks_for_file`] writes `code_chunks` rows with NO vectors (the
//! graph hot path must never block on the network); this pass walks the pending
//! queue, embeds each batch through the `Embedder` trait, writes the vectors into
//! the `vec0` index, and stamps each chunk's `embed_model`/`embed_dim` so it
//! drains from the queue.
//!
//! Decoupling: the embedder is a `&dyn Embedder` (the frozen `icode-core` trait),
//! so `icode-engine` depends only on the contract — never on `icode-embed`. The
//! caller (the `icode` binary, or a test) supplies the concrete embedder.
//!
//! [`chunks_for_file`]: crate::chunk::chunks_for_file

use icode_core::error::Result;
use icode_core::traits::{CodeWriteStore, Embedder, VectorIndex};

use crate::store::SqliteCodeStore;

/// Counters from one [`embed_pending`] run.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmbedStats {
    /// Chunks embedded and written to the vector index this run.
    pub embedded: usize,
    /// Number of embed batches issued (one `Embedder::embed` call each).
    pub batches: usize,
}

/// Drain the pending-embedding queue: repeatedly pull up to `batch` chunks whose
/// embedding is missing or stamped with a different model, embed them, write the
/// vectors, and stamp each chunk as embedded (which removes it from the queue).
///
/// Loops until the queue is empty. A `batch` of 0 is treated as 1 so the loop
/// always makes progress. An embedder failure (e.g. Ollama down) propagates as
/// `Err` — the caller decides whether a partial graph without vectors is fine.
pub fn embed_pending(
    store: &SqliteCodeStore,
    embedder: &dyn Embedder,
    batch: usize,
) -> Result<EmbedStats> {
    let batch = batch.max(1);
    let model = embedder.model_id().to_string();
    let dim = embedder.dim();
    let index = store.vector_index();

    let mut stats = EmbedStats::default();

    loop {
        let pend = store.pending_chunks(&model, batch)?;
        if pend.is_empty() {
            break;
        }

        // Cheap pointer slice over the owned chunk texts for the batch embed call.
        let texts: Vec<&str> = pend.iter().map(|(_, t)| t.as_str()).collect();
        let vecs = embedder.embed(&texts)?;
        stats.batches += 1;

        // Pair each chunk's rowid with its fresh vector for a single bulk upsert.
        let items: Vec<(i64, Vec<f32>)> = pend.iter().map(|(rowid, _)| *rowid).zip(vecs).collect();
        index.upsert_batch(&items)?;

        // Stamp each chunk as embedded so it leaves the pending queue; without
        // this the same rows would be returned forever (an infinite loop).
        for (rowid, _) in &pend {
            store.mark_chunk_embedded(*rowid, &model, dim)?;
        }

        stats.embedded += pend.len();
    }

    Ok(stats)
}
