//! `doctor` — read-only diagnostics for a per-project index.
//!
//! Answers one question: "does this index still match the source tree on disk,
//! and do its internal invariants hold?". It ONLY runs SELECTs (and a disk walk)
//! — it never writes, re-indexes, or repairs. The CLI / MCP surfaces it; the
//! indexer (`icode index`) is what actually fixes any drift it reports.
//!
//! Drift is computed against the SAME walk the indexer uses
//! ([`crate::index::walk_source_files`]): same excluded dirs, same supported
//! extensions, same path string form — so a file the indexer would skip is never
//! mis-reported as missing/stale.

use std::collections::HashMap;
use std::path::Path;

use icode_core::error::Result;
use icode_core::traits::CodeReadStore;

use crate::index::walk_source_files;
use crate::store::SqliteCodeStore;

/// How many example paths to keep per drift list. The counters stay exact
/// (`missing_count` etc. count everything); the `Vec`s are capped so a wholly
/// un-indexed tree doesn't produce a multi-thousand-line report.
const MAX_EXAMPLES: usize = 50;

/// Read-only health report for a per-project index. Counters are always exact;
/// the `missing`/`outdated`/`stale` vectors are capped at [`MAX_EXAMPLES`]
/// examples each (use the `*_count` fields for the true totals).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct DiagnosticReport {
    /// True iff there is NO drift (missing/outdated/stale all empty) and the
    /// hard invariants hold (`orphan_vectors == 0` && `parse_errors == 0`).
    /// `pending_embeddings` deliberately does NOT affect this — an un-drained
    /// embed queue is a normal background state, not a fault.
    pub healthy: bool,
    /// Files currently in the `files` table.
    pub files_indexed: u64,

    /// On disk but NOT in the index (up to [`MAX_EXAMPLES`] examples).
    pub missing: Vec<String>,
    /// Exact count of missing files (may exceed `missing.len()`).
    pub missing_count: u64,
    /// Indexed but on-disk mtime/size diverged (up to [`MAX_EXAMPLES`] examples).
    pub outdated: Vec<String>,
    /// Exact count of outdated files.
    pub outdated_count: u64,
    /// In the index but gone from disk (up to [`MAX_EXAMPLES`] examples).
    pub stale: Vec<String>,
    /// Exact count of stale files.
    pub stale_count: u64,

    /// Rows in `parse_errors` (a file that failed to parse on the last index).
    pub parse_errors: u64,
    /// Rows in `code_chunks` (embeddable units).
    pub chunks: u64,
    /// Rows in `vec_code` (chunks that actually have a vector).
    pub embedded: u64,
    /// Chunks with no vector yet — the embed backlog (does NOT affect `healthy`).
    pub pending_embeddings: u64,
    /// `vec_code` rowids with no backing chunk. MUST be 0 (the delete-mirror
    /// trigger guarantees it); non-zero is a broken invariant.
    pub orphan_vectors: u64,
}

/// Run a full read-only diagnosis of `store` against the source tree at `root`.
///
/// Walks the disk via [`walk_source_files`] (the indexer's own walk), reconciles
/// it against the `files` table, and reads the embedding/orphan invariants. Pure
/// SELECTs + a directory walk; nothing is written.
pub fn diagnose(store: &SqliteCodeStore, root: &Path) -> Result<DiagnosticReport> {
    // ── disk side: (path-string, mtime, size) for every file the indexer covers.
    let disk: HashMap<String, (i64, u64)> = walk_source_files(root)
        .into_iter()
        .filter_map(|p| {
            let meta = std::fs::metadata(&p).ok()?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            // Match the exact string form the indexer stored (`to_string_lossy`).
            Some((p.to_string_lossy().to_string(), (mtime, meta.len())))
        })
        .collect();

    // ── index side: every indexed file with its recorded mtime/size.
    // `usize::MAX` so the cap never truncates the reconciliation (this is a
    // diagnostic, not a hot path).
    let indexed = store.list_files(None, None, usize::MAX)?;
    let indexed_paths: HashMap<&str, (i64, u64)> = indexed
        .iter()
        .map(|f| (f.path.as_str(), (f.mtime, f.file_size)))
        .collect();

    // missing: on disk, not in the index.
    let mut missing: Vec<String> = Vec::new();
    let mut missing_count = 0u64;
    // outdated: in both, but mtime OR size diverged.
    let mut outdated: Vec<String> = Vec::new();
    let mut outdated_count = 0u64;
    for (path, (disk_mtime, disk_size)) in &disk {
        match indexed_paths.get(path.as_str()) {
            None => {
                missing_count += 1;
                if missing.len() < MAX_EXAMPLES {
                    missing.push(path.clone());
                }
            }
            Some((idx_mtime, idx_size)) => {
                if idx_mtime != disk_mtime || idx_size != disk_size {
                    outdated_count += 1;
                    if outdated.len() < MAX_EXAMPLES {
                        outdated.push(path.clone());
                    }
                }
            }
        }
    }

    // stale: in the index, gone from disk.
    let mut stale: Vec<String> = Vec::new();
    let mut stale_count = 0u64;
    for f in &indexed {
        if !disk.contains_key(&f.path) {
            stale_count += 1;
            if stale.len() < MAX_EXAMPLES {
                stale.push(f.path.clone());
            }
        }
    }

    // Stable, deterministic example ordering.
    missing.sort();
    outdated.sort();
    stale.sort();

    // ── invariants & counters from the store (read-only).
    let stats = store.stats()?;
    let pending_embeddings = store.pending_embeddings_count()?;
    let orphan_vectors = store.orphan_vectors_count()?;

    let healthy = missing_count == 0
        && outdated_count == 0
        && stale_count == 0
        && orphan_vectors == 0
        && stats.parse_errors == 0;

    Ok(DiagnosticReport {
        healthy,
        files_indexed: stats.files,
        missing,
        missing_count,
        outdated,
        outdated_count,
        stale,
        stale_count,
        parse_errors: stats.parse_errors,
        chunks: stats.code_chunks,
        embedded: stats.vec_rows,
        pending_embeddings,
        orphan_vectors,
    })
}
