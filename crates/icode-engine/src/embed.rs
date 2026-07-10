//! Embed pass: drain the pending-chunk queue into the vector index.
//!
//! This is the back half of the chunk+embed phase (symbols → chunks → embeddings
//! → vec0). [`chunks_for_file`] writes `code_chunks` rows with NO vectors (the
//! graph hot path must never block on the network); this pass walks the pending
//! queue, brings each chunk's vector up to date, and stamps each chunk's
//! `embed_model`/`embed_dim` so it drains from the queue.
//!
//! Content-addressed cache (the expensive-work saver): re-indexing a file DELETEs
//! its chunks (and their `vec_code` rows) and re-inserts fresh rows with NULL
//! `embed_model` — so a `git checkout` that rewrites files, and crucially the
//! switch back, would otherwise re-embed byte-identical text every time. This pass
//! consults `embed_cache` (`content_hash → vector`) FIRST: a hit rehydrates the
//! vector with ZERO embedder calls; only genuine misses hit the (slow) embedder,
//! and their vectors are written back to the cache. A chunk that reverts to a
//! previously-seen state is therefore free.
//!
//! Decoupling: the embedder is a `&dyn Embedder` (the frozen `icode-core` trait),
//! so `icode-engine` depends only on the contract — never on `icode-embed`. The
//! caller (the `icode` binary, the MCP/web server's on-demand path, or a test)
//! supplies the concrete embedder.
//!
//! [`chunks_for_file`]: crate::chunk::chunks_for_file

use icode_core::error::Result;
use icode_core::traits::{Embedder, VectorIndex};

use crate::store::SqliteCodeStore;

/// Counters from one [`embed_pending`] run.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmbedStats {
    /// Chunks embedded via the embedder (true cache misses) this run.
    pub embedded: usize,
    /// Chunks whose vector was rehydrated from `embed_cache` with NO embedder call
    /// (e.g. a branch switch back to already-seen content).
    pub rehydrated: usize,
    /// Number of embed batches issued (one `Embedder::embed` call each). A run that
    /// is fully served from the cache issues zero.
    pub batches: usize,
}

/// Drain the pending-embedding queue COMPLETELY: repeatedly pull up to `batch`
/// chunks whose embedding is missing or stamped with a different model, fill their
/// vectors (cache hit → rehydrate; miss → embed + cache), write the vectors, and
/// stamp each chunk as embedded (which removes it from the queue).
///
/// Loops until the queue is EMPTY, so this is the BULK path only (`icode index` /
/// `icode embed` / the opt-in `serve` drain). An INTERACTIVE caller must use
/// [`embed_pending_capped`]: a full drain inside a tool call embeds the whole project
/// synchronously and pins the GPU. An embedder failure (e.g. Ollama down) propagates
/// as `Err` — the caller decides whether a partial graph without vectors is fine.
pub fn embed_pending(
    store: &SqliteCodeStore,
    embedder: &dyn Embedder,
    batch: usize,
) -> Result<EmbedStats> {
    embed_pending_capped(store, embedder, batch, usize::MAX)
}

/// Like [`embed_pending`], but touches AT MOST `max_chunks` pending chunks and then
/// returns.
///
/// This is what the interactive / JIT path needs: a semantic query backfills one
/// small slice of the queue and answers, instead of turning a single tool call into
/// a full-project embed. `batch` is the page size handed to the embedder;
/// `max_chunks` is the hard ceiling on the work done in this call. A `batch` of 0 is
/// treated as 1 so the loop always makes progress.
pub fn embed_pending_capped(
    store: &SqliteCodeStore,
    embedder: &dyn Embedder,
    batch: usize,
    max_chunks: usize,
) -> Result<EmbedStats> {
    let batch = batch.max(1);
    let model = embedder.model_id().to_string();
    let dim = embedder.dim();
    let index = store.vector_index();

    let mut stats = EmbedStats::default();

    // Keyset cursor: the max chunk id embedded so far. Each batch resumes at
    // `c.id > after_id` (see `pending_chunks_with_hash`), so the drain is a single
    // forward pass over the primary key — O(N) — not the old scan-from-zero O(N²).
    let mut after_id: i64 = 0;
    // Hard ceiling on chunks touched by THIS call (see `max_chunks`).
    let mut processed: usize = 0;

    loop {
        if processed >= max_chunks {
            break;
        }
        let want = batch.min(max_chunks - processed);
        let pend = store.pending_chunks_with_hash(&model, after_id, want)?;
        if pend.is_empty() {
            break;
        }
        processed += pend.len();
        // Advance the cursor past this batch (rows come back ORDER BY id ASC, so the
        // last one is the max). We stamp every row below, so nothing pending is left
        // behind the cursor.
        after_id = pend.last().map(|(id, _, _)| *id).unwrap_or(after_id);

        // 1) Try the content-addressed cache for the whole batch in one query.
        let hashes: Vec<&str> = pend.iter().map(|(_, h, _)| h.as_str()).collect();
        let cached = store.embed_cache_lookup(&hashes, &model)?;

        // 2) Partition into cache hits (rehydrate, no embedder call) and misses
        //    (must embed). `to_upsert` accumulates BOTH so vec0 is written once.
        let mut to_upsert: Vec<(i64, Vec<f32>)> = Vec::with_capacity(pend.len());
        let mut miss_idx: Vec<usize> = Vec::new();
        for (i, (rowid, hash, _text)) in pend.iter().enumerate() {
            match cached.get(hash) {
                Some(v) => to_upsert.push((*rowid, v.clone())),
                None => miss_idx.push(i),
            }
        }

        // 3) Embed only the misses, then write their vectors back to the cache so a
        //    future re-index of identical text is free.
        if !miss_idx.is_empty() {
            let texts: Vec<&str> = miss_idx.iter().map(|&i| pend[i].2.as_str()).collect();
            let vecs = embedder.embed(&texts)?;
            stats.batches += 1;

            let mut cache_puts: Vec<(String, Vec<f32>)> = Vec::with_capacity(miss_idx.len());
            for (j, &i) in miss_idx.iter().enumerate() {
                let (rowid, hash, _text) = &pend[i];
                let v = vecs[j].clone();
                cache_puts.push((hash.clone(), v.clone()));
                to_upsert.push((*rowid, v));
            }
            store.embed_cache_store(&model, dim, &cache_puts)?;
        }

        // 4) One bulk vec0 upsert for the whole batch (hits + freshly embedded),
        //    then stamp the whole batch embedded in ONE transaction so it leaves the
        //    pending queue (was one autocommit/fsync per row — now one per batch).
        index.upsert_batch(&to_upsert)?;
        let rowids: Vec<i64> = pend.iter().map(|(rowid, _, _)| *rowid).collect();
        store.mark_chunks_embedded(&rowids, &model, dim)?;

        stats.embedded += miss_idx.len();
        stats.rehydrated += pend.len() - miss_idx.len();
    }

    Ok(stats)
}
