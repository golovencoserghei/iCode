//! Indexer: walk a directory tree, parse each `.rs` file, persist files +
//! functions through the `CodeWriteStore`. M0.5 scope: Rust only, no embeddings.

use std::path::Path;

use icode_core::error::{Error, Result};
use icode_core::model::{FileRecord, Language};
use icode_core::traits::CodeWriteStore;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::parse::parse_rust;
use crate::store::SqliteCodeStore;

/// Counters returned by an indexing run.
#[derive(Clone, Copy, Debug, Default)]
pub struct IndexStats {
    pub files_indexed: u64,
    pub functions: u64,
    pub errors: u64,
}

/// Walk `root` recursively, indexing every `.rs` file (skipping `target` and
/// `.git`). Each file is parsed and upserted in one store transaction.
pub fn index_path(root: &Path, store: &SqliteCodeStore) -> Result<IndexStats> {
    let mut stats = IndexStats::default();

    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_excluded_dir(e.path()))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }

        match index_file(path, store) {
            Ok(n_funcs) => {
                stats.files_indexed += 1;
                stats.functions += n_funcs;
            }
            Err(e) => {
                stats.errors += 1;
                // Best-effort error record; don't abort the whole run.
                let _ = store.record_parse_error(&path.to_string_lossy(), &e.to_string());
            }
        }
    }

    Ok(stats)
}

fn index_file(path: &Path, store: &SqliteCodeStore) -> Result<u64> {
    let source = std::fs::read_to_string(path).map_err(|e| Error::Io(e.to_string()))?;
    let path_str = path.to_string_lossy().to_string();

    let parsed = parse_rust(&source, &path_str);

    let meta = std::fs::metadata(path).map_err(|e| Error::Io(e.to_string()))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let file = FileRecord {
        path: path_str,
        language: Language::Rust,
        content_hash: content_hash(&source),
        ast_hash: parsed.ast_hash.clone(),
        lines_total: parsed.lines_total,
        mtime,
        file_size: meta.len(),
    };

    let n = parsed.functions.len() as u64;
    store.upsert_file(&file, &parsed.functions, &[], &[], &[], &[])?;
    Ok(n)
}

fn content_hash(source: &str) -> String {
    let mut h = Sha256::new();
    h.update(source.as_bytes());
    let digest = h.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Exclude `target` and `.git` (and any nested copies) from the walk.
fn is_excluded_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == "target" || n == ".git")
        .unwrap_or(false)
}
