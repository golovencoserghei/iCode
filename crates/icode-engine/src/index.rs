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
use crate::parse::{
    parse_go, parse_html, parse_java, parse_javascript, parse_php, parse_python, parse_rust, parse_tsx,
    parse_typescript, ParseResult,
};
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

/// Walk `root` recursively and yield every *supported* source file path (the same
/// excludes + extension dispatch the indexer uses). This is the single source of
/// truth for "which files belong in the index" — both [`index_path`] and the
/// read-only `doctor` diagnostics walk via this helper so a file the indexer would
/// skip is never reported as drift.
///
/// Returns absolute/`root`-relative paths exactly as `WalkDir` yields them, so a
/// caller comparing against the `files` table must compare on the same string form
/// the indexer stored (the indexer stores `path.to_string_lossy()` of this path).
pub fn walk_source_files(root: &Path) -> Vec<std::path::PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_excluded_dir(e.path()))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| language_for(p).is_some())
        .collect()
}

/// Walk `root` recursively, indexing every supported source file (skipping the
/// build/VCS/dependency directories in [`EXCLUDED_DIRS`]). Each file is parsed and
/// upserted in one store transaction.
pub fn index_path(root: &Path, store: &SqliteCodeStore) -> Result<IndexStats> {
    let mut stats = IndexStats::default();

    for path in walk_source_files(root) {
        let path = path.as_path();

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

    // Receiver-aware call resolution: now that EVERY file's definitions are in the
    // functions table, validate each edge's qualified guess and grade confidence.
    // Must run once, after the whole walk (a callee may be defined in a file
    // indexed later than its caller).
    store.resolve_call_edges(None)?;

    Ok(stats)
}

/// Index a SINGLE source file into the store: parse → `upsert_file` (replaces the
/// file's rows, cascading away the old graph) → chunk → `upsert_chunks`. This is
/// the exact per-file path [`index_path`] runs in its walk, lifted out so the live
/// daemon ([`crate::daemon`]) can re-index one changed file without re-walking the
/// whole tree.
///
/// CRITICAL — path form: the row keys (`files.path`, `code_chunks.path`, …) are
/// `path.to_string_lossy()` of `path` *exactly as given*. [`index_path`] feeds the
/// paths `walk_source_files(root)` yields (so for an absolute `root` they are
/// absolute, for a relative `root` relative). A caller (the daemon) MUST pass a
/// `path` in that same form, or the keys won't match the indexed rows and
/// delete/upsert silently miss. The daemon canonicalises `root` and rebuilds the
/// changed path under it so this holds.
///
/// Returns the per-file [`IndexStats`] for one file (`files_indexed = 1` on
/// success). An unsupported extension or a read/parse failure is an `Err` — the
/// daemon turns that into a `record_parse_error` and keeps running.
pub fn index_one_file(path: &Path, store: &SqliteCodeStore) -> Result<IndexStats> {
    let counts = index_file(path, store)?;
    // Re-grade this file's outgoing edges against the (already-complete) functions
    // table. Scoped to the one path for speed — the rest of the project is already
    // resolved; only edges whose target def CHANGED in this file could drift, which
    // a subsequent touch of that file heals.
    store.resolve_call_edges(Some(&path.to_string_lossy()))?;
    Ok(IndexStats {
        files_indexed: 1,
        functions: counts.functions,
        classes: counts.classes,
        imports: counts.imports,
        calls: counts.calls,
        routes: counts.routes,
        code_chunks: counts.code_chunks,
        errors: 0,
    })
}

/// True when `path`'s extension maps to a parser we support (the daemon uses this
/// to skip non-source files before touching the store). Mirrors the extension
/// dispatch [`index_path`] walks through [`walk_source_files`].
pub fn is_supported_source(path: &Path) -> bool {
    language_for(path).is_some()
}

/// Directory component names the indexer (and therefore the daemon) skips wholesale
/// — build artefacts, VCS metadata, dependency caches. Exposed so the daemon's
/// watch loop can drop events under any excluded directory using the *same* set the
/// indexer walks with (a divergence would make the daemon index files `index_path`
/// never sees). Includes `.icode` so the daemon never reacts to its own db writes.
pub fn is_in_excluded_dir(path: &Path) -> bool {
    path.components().any(|c| match c {
        std::path::Component::Normal(name) => name
            .to_str()
            .map(|n| EXCLUDED_DIRS.contains(&n) || n == ".icode")
            .unwrap_or(false),
        _ => false,
    })
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

    let parsed = parse_for(language, &source, &path_str, path);

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
        // JavaScript family (incl. JSX, ESM/CJS module variants).
        Some("js") | Some("jsx") | Some("mjs") | Some("cjs") => Some(Language::JavaScript),
        // TypeScript family (`.tsx` uses the JSX-aware grammar — see parse_for).
        Some("ts") | Some("tsx") => Some(Language::TypeScript),
        Some("go") => Some(Language::Go),
        Some("java") => Some(Language::Java),
        Some("html") | Some("htm") => Some(Language::Html),
        _ => None,
    }
}

/// Dispatch to the per-language parser. `os_path` carries the extension so the
/// TypeScript arm can pick the TSX grammar for `.tsx` files.
fn parse_for(language: Language, source: &str, path: &str, os_path: &Path) -> ParseResult {
    match language {
        Language::Rust => parse_rust(source, path),
        Language::Python => parse_python(source, path),
        Language::Php => parse_php(source, path),
        Language::JavaScript => parse_javascript(source, path),
        Language::TypeScript => {
            if os_path.extension().and_then(|e| e.to_str()) == Some("tsx") {
                parse_tsx(source, path)
            } else {
                parse_typescript(source, path)
            }
        }
        Language::Go => parse_go(source, path),
        Language::Java => parse_java(source, path),
        Language::Html => parse_html(source, path),
        // Other languages land with their parsers later; until then, no nodes.
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
