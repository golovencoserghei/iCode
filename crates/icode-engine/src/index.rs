//! Indexer: walk a directory tree, parse each source file by extension, and
//! persist the full code graph (files + functions + classes + imports + calls +
//! routes) through the `CodeWriteStore`. M1 scope: Rust parser wired; the
//! extension dispatch leaves a slot for `.py` (and others) to plug in later.

use std::path::Path;

use icode_core::error::{Error, Result};
use icode_core::model::{FileRecord, Language};
use icode_core::traits::CodeWriteStore;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::chunk::chunks_for_file;
use crate::parse::{parse_php, parse_python, parse_rust, ParseResult};
use crate::store::SqliteCodeStore;

/// Counters returned by an indexing run.
#[derive(Clone, Copy, Debug, Default)]
pub struct IndexStats {
    pub files_indexed: u64,
    pub functions: u64,
    pub classes: u64,
    pub imports: u64,
    pub calls: u64,
    pub routes: u64,
    /// Embeddable chunks written to `code_chunks` (without vectors — the embed
    /// pass fills those). Counts overflow sub-chunks too, so it can exceed
    /// `functions + classes` for files with very large symbols.
    pub code_chunks: u64,
    pub errors: u64,
}

/// Directory names (path components) skipped wholesale by the walk: build
/// artefacts, VCS metadata, and language dependency caches. Matching is by exact
/// component name, so a nested copy (`a/b/.venv/...`) is excluded too. Critically
/// this keeps Python projects from dragging their whole `.venv`/`site-packages`
/// into the index.
const EXCLUDED_DIRS: &[&str] = &[
    // build artefacts
    "target",
    "dist",
    "build",
    // VCS metadata
    ".git",
    ".svn",
    // JS deps / vendored code
    "node_modules",
    "vendor",
    // Python virtualenvs & caches
    ".venv",
    "venv",
    "site-packages",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    // editor metadata
    ".idea",
];

/// Walk `root` recursively, indexing every supported source file (skipping the
/// build/VCS/dependency directories in [`EXCLUDED_DIRS`]). Each file is parsed and
/// upserted in one store transaction.
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
        // Skip files we have no parser for (dispatch decides support).
        if language_for(path).is_none() {
            continue;
        }

        match index_file(path, store) {
            Ok(counts) => {
                stats.files_indexed += 1;
                stats.functions += counts.functions;
                stats.classes += counts.classes;
                stats.imports += counts.imports;
                stats.calls += counts.calls;
                stats.routes += counts.routes;
                stats.code_chunks += counts.code_chunks;
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

/// Per-file node counts (rolled up into `IndexStats`).
#[derive(Clone, Copy, Default)]
struct FileCounts {
    functions: u64,
    classes: u64,
    imports: u64,
    calls: u64,
    routes: u64,
    code_chunks: u64,
}

fn index_file(path: &Path, store: &SqliteCodeStore) -> Result<FileCounts> {
    let language =
        language_for(path).ok_or_else(|| Error::Invalid("unsupported extension".into()))?;
    let source = std::fs::read_to_string(path).map_err(|e| Error::Io(e.to_string()))?;
    let path_str = path.to_string_lossy().to_string();

    let parsed = parse_for(language, &source, &path_str);

    let meta = std::fs::metadata(path).map_err(|e| Error::Io(e.to_string()))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let file = FileRecord {
        path: path_str,
        language,
        content_hash: content_hash(&source),
        ast_hash: parsed.ast_hash.clone(),
        lines_total: parsed.lines_total,
        mtime,
        file_size: meta.len(),
    };

    store.upsert_file(
        &file,
        &parsed.functions,
        &parsed.classes,
        &parsed.imports,
        &parsed.calls,
        &parsed.routes,
    )?;

    // Chunk+persist the file's symbols (graph-fast path: writes code_chunks with
    // NO vectors, so this never blocks on the network — the embed pass fills
    // vectors asynchronously). Keyed by path, idempotent per file.
    let chunks = chunks_for_file(&parsed.functions, &parsed.classes);
    store.upsert_chunks(&file.path, &chunks)?;

    Ok(FileCounts {
        functions: parsed.functions.len() as u64,
        classes: parsed.classes.len() as u64,
        imports: parsed.imports.len() as u64,
        calls: parsed.calls.len() as u64,
        routes: parsed.routes.len() as u64,
        code_chunks: chunks.len() as u64,
    })
}

/// Map a path's extension to a language we have a parser for. Returns `None` for
/// unsupported files so the walk skips them. `.py` (etc.) plug in here later.
fn language_for(path: &Path) -> Option<Language> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some(Language::Rust),
        Some("py") => Some(Language::Python),
        Some("php") | Some("phtml") => Some(Language::Php),
        _ => None,
    }
}

/// Dispatch to the per-language parser. Only Rust is wired in M1; the match arm
/// is the single extension point other parsers slot into.
fn parse_for(language: Language, source: &str, path: &str) -> ParseResult {
    match language {
        Language::Rust => parse_rust(source, path),
        Language::Python => parse_python(source, path),
        Language::Php => parse_php(source, path),
        // Other languages land with their parsers (M3); until then, no nodes.
        _ => ParseResult {
            lines_total: source.lines().count().max(1) as u32,
            ast_hash: content_hash(source),
            ..Default::default()
        },
    }
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

/// Exclude build/VCS/dependency dirs (and any nested copies) from the walk by
/// matching the path's final component against [`EXCLUDED_DIRS`].
fn is_excluded_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| EXCLUDED_DIRS.contains(&n))
        .unwrap_or(false)
}
