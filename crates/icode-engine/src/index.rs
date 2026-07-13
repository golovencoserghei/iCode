//! Indexer: walk a directory tree, parse each source file by extension, and
//! persist the full code graph (files + functions + classes + imports + calls +
//! routes) through the `CodeWriteStore`. M1 scope: Rust parser wired; the
//! extension dispatch leaves a slot for `.py` (and others) to plug in later.

use std::collections::{HashMap, HashSet};
use std::fs::Metadata;
use std::path::Path;

use icode_core::error::{Error, Result};
use icode_core::model::{Call, ClassDef, CodeChunk, FileRecord, FunctionDef, Import, Language, Route};
use icode_core::traits::{CodeReadStore, CodeWriteStore};
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::chunk::chunks_for_file;
use crate::parse::{
    parse_go, parse_html, parse_java, parse_javascript, parse_php, parse_python, parse_rust, parse_tsx,
    parse_typescript, ParseResult,
};
use crate::store::SqliteCodeStore;

/// Default upper bound (bytes) on a source file the indexer will parse. Above this
/// the file is skipped (and dropped from the index if previously present): a
/// multi-MB minified/generated blob would otherwise be read whole and driven
/// through tree-sitter, blowing up time and memory for ~zero graph value. Override
/// with `ICODE_MAX_FILE_BYTES` (0 / unparseable → this default).
pub const DEFAULT_MAX_FILE_BYTES: u64 = 1_500_000;

/// Files parsed per rayon batch before the batch's results are flushed to the store.
/// Bounds peak memory (at most this many parsed files — bodies included — are held
/// at once) while still saturating cores on the CPU-bound tree-sitter parse.
const PARSE_BATCH: usize = 256;

/// The configured per-file size cap (see [`DEFAULT_MAX_FILE_BYTES`]).
fn max_file_bytes() -> u64 {
    std::env::var("ICODE_MAX_FILE_BYTES")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_FILE_BYTES)
}

/// Whether `ICODE_FORCE_REINDEX` asks for a full reparse (bypassing the
/// content-hash skip). Any of `1`/`true`/`yes` (case-insensitive) enables it.
fn force_reindex() -> bool {
    std::env::var("ICODE_FORCE_REINDEX")
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

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
    /// Files left untouched because their content hashed identically to the index
    /// (incremental full-index, Wave 2): NOT re-parsed, NOT re-chunked, their
    /// vectors NOT nulled. Includes metadata-only "touch" refreshes.
    pub files_skipped: u64,
    /// Files skipped for exceeding the size cap (`ICODE_MAX_FILE_BYTES`).
    pub files_skipped_oversize: u64,
    /// Files removed from the index this run because they vanished from disk (or
    /// grew past the size cap and became un-indexable).
    pub files_deleted: u64,
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

/// Walk `root` recursively and bring the index into lock-step with disk — an
/// INCREMENTAL full-index (Wave 2). Its result is byte-for-byte the same GRAPH a
/// clean reindex would build, but it does only the work that actually changed:
///
///  * **skip** — a file whose current bytes hash to the stored `content_hash` is
///    left entirely alone: not re-parsed, not re-`upsert`ed, its chunks/vectors NOT
///    nulled (so unchanged files never bounce back into the embed queue). A pure
///    metadata drift (same bytes, new mtime/size) refreshes just the `files` row.
///  * **reindex** — a new or content-changed file is parsed and upserted as before;
///  * **delete** — a file in the index but gone from disk (or now past the size cap)
///    is dropped, so a full pass equals a clean reindex.
///
/// Parsing is done in PARALLEL via rayon (CPU-bound, pure) in path-sorted batches;
/// the store writes stay sequential behind the store's single `Mutex`, applied in
/// sorted-path order so rowids are assigned deterministically regardless of thread
/// scheduling. `ICODE_FORCE_REINDEX=1` bypasses the hash skip (full reparse).
pub fn index_path(root: &Path, store: &SqliteCodeStore) -> Result<IndexStats> {
    let mut stats = IndexStats::default();
    let max_bytes = max_file_bytes();
    let force = force_reindex();

    // Deterministic order: writes below land in sorted-path order regardless of the
    // parallel parse, so function/chunk rowids come out reproducible.
    let mut files = walk_source_files(root);
    files.sort();

    // Prior index snapshot: path → (content_hash, mtime, size), in one query.
    let prior: HashMap<String, FileMeta> = store
        .list_files(None, None, usize::MAX)?
        .into_iter()
        .map(|f| {
            (
                f.path.clone(),
                FileMeta { content_hash: f.content_hash, mtime: f.mtime, file_size: f.file_size },
            )
        })
        .collect();

    // Every path present on disk (indexer key form) — for the deletion sweep.
    let on_disk: HashSet<String> =
        files.iter().map(|p| p.to_string_lossy().to_string()).collect();

    let mut any_change = false;
    let mut oversize_to_drop: Vec<String> = Vec::new();

    for batch in files.chunks(PARSE_BATCH) {
        // Parse the batch in PARALLEL (pure CPU work; no store access). `map` +
        // ordered `collect` preserves the sorted order for the sequential write.
        let outcomes: Vec<Outcome> = batch
            .par_iter()
            .map(|p| classify(p, prior.get(p.to_string_lossy().as_ref()), max_bytes, force, root))
            .collect();

        // Sequential store writes behind the single Mutex, in sorted-path order.
        for outcome in outcomes {
            match outcome {
                Outcome::Unchanged => stats.files_skipped += 1,
                Outcome::Touch { path, mtime, file_size } => {
                    stats.files_skipped += 1;
                    store.touch_file_meta(&path, mtime, file_size)?;
                }
                Outcome::Reindex(pf) => match write_parsed(store, &pf) {
                    Ok(counts) => {
                        any_change = true;
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
                        let _ = store.record_parse_error(&pf.file.path, &e.to_string());
                    }
                },
                Outcome::Oversize { path } => {
                    stats.files_skipped_oversize += 1;
                    tracing::warn!(
                        "index: skipping {} — larger than {} bytes (ICODE_MAX_FILE_BYTES)",
                        path,
                        max_bytes
                    );
                    // If it was indexed before (shrank below the cap once), it must
                    // now leave the index to stay consistent with a clean reindex.
                    if prior.contains_key(&path) {
                        oversize_to_drop.push(path);
                    }
                }
                Outcome::Failed { path, error } => {
                    stats.errors += 1;
                    let _ = store.record_parse_error(&path, &error);
                }
            }
        }
    }

    // Deletion sweep: indexed files gone from disk, plus now-oversize ones.
    for path in prior.keys() {
        if !on_disk.contains(path) {
            store.delete_file(path)?;
            any_change = true;
            stats.files_deleted += 1;
        }
    }
    for path in &oversize_to_drop {
        store.delete_file(path)?;
        any_change = true;
        stats.files_deleted += 1;
    }

    // Receiver-aware call resolution: validate each edge's qualified guess and grade
    // confidence against the COMPLETE functions table. Runs once after the walk (a
    // callee may live in a file processed later than its caller). Skipped on a pure
    // no-op pass — the persisted resolution is already correct, so re-running the
    // full-table UPDATE would only burn a write for an identical result.
    if any_change {
        store.resolve_call_edges(None)?;
        // Graph centrality (free, CPU-only) — a search-ranking prior. Must run AFTER
        // the edges are resolved, since it reads the finished call graph.
        store.compute_symbol_ranks()?;
    }

    Ok(stats)
}

/// The prior-index facts the incremental pass compares a file against.
struct FileMeta {
    content_hash: String,
    mtime: i64,
    file_size: u64,
}

/// A fully parsed file, ready for the (sequential) store write — all the CPU-bound
/// work (read → parse → chunk) already done on a rayon worker.
struct ParsedFile {
    file: FileRecord,
    functions: Vec<FunctionDef>,
    classes: Vec<ClassDef>,
    imports: Vec<Import>,
    calls: Vec<Call>,
    routes: Vec<Route>,
    chunks: Vec<CodeChunk>,
}

/// Per-file verdict from [`classify`] (computed in parallel; applied sequentially).
enum Outcome {
    /// Content hashed identically to the index — do nothing at all.
    Unchanged,
    /// Same content, drifted mtime/size — refresh just the `files` row metadata.
    Touch { path: String, mtime: i64, file_size: u64 },
    /// New or changed content — parsed and ready to write. Boxed to keep the enum
    /// small (the parsed payload is large; most outcomes are not `Reindex`).
    Reindex(Box<ParsedFile>),
    /// Over the size cap — skip (and drop from the index if it was there).
    Oversize { path: String },
    /// Read/parse failure — record it, keep going.
    Failed { path: String, error: String },
}

/// Classify one walked file against its prior index entry. Reads + hashes the file
/// and parses ONLY when the content actually changed — pure and store-free, so it
/// runs on a rayon worker. The heavy work (tree-sitter parse + chunk assembly)
/// happens here, off the store's serialization point.
fn classify(
    path: &Path,
    prior: Option<&FileMeta>,
    max_bytes: u64,
    force: bool,
    root: &Path,
) -> Outcome {
    let path_str = path.to_string_lossy().to_string();
    // Defensive: the walk already filtered to supported extensions.
    let language = match language_for(path) {
        Some(l) => l,
        None => return Outcome::Unchanged,
    };
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return Outcome::Failed { path: path_str, error: e.to_string() },
    };
    if meta.len() > max_bytes {
        return Outcome::Oversize { path: path_str };
    }
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => return Outcome::Failed { path: path_str, error: e.to_string() },
    };
    let hash = content_hash(&source);
    if !force {
        if let Some(p) = prior {
            if p.content_hash == hash {
                // Unchanged bytes. Mirror a clean reindex's fresh mtime/size without
                // any parse/chunk/vector churn when only the metadata drifted.
                let mtime = mtime_secs(&meta);
                if p.mtime != mtime || p.file_size != meta.len() {
                    return Outcome::Touch { path: path_str, mtime, file_size: meta.len() };
                }
                return Outcome::Unchanged;
            }
        }
    }
    Outcome::Reindex(Box::new(build_parsed(path, language, &source, &meta, hash, root)))
}

/// Write one already-parsed file into the store: replace its graph rows and its
/// chunks (the sequential half of the pipeline).
fn write_parsed(store: &SqliteCodeStore, pf: &ParsedFile) -> Result<FileCounts> {
    store.upsert_file(
        &pf.file,
        &pf.functions,
        &pf.classes,
        &pf.imports,
        &pf.calls,
        &pf.routes,
    )?;
    store.upsert_chunks(&pf.file.path, &pf.chunks)?;
    Ok(FileCounts {
        functions: pf.functions.len() as u64,
        classes: pf.classes.len() as u64,
        imports: pf.imports.len() as u64,
        calls: pf.calls.len() as u64,
        routes: pf.routes.len() as u64,
        code_chunks: pf.chunks.len() as u64,
    })
}

/// Build a [`ParsedFile`] from already-read source (pure CPU work; safe on a rayon
/// worker). `content_hash` is precomputed by the caller so an unchanged file hashes
/// exactly once.
fn build_parsed(
    path: &Path,
    language: Language,
    source: &str,
    meta: &Metadata,
    content_hash: String,
    root: &Path,
) -> ParsedFile {
    let path_str = path.to_string_lossy().to_string();
    let parsed = parse_for(language, source, &path_str, path);
    let file = FileRecord {
        path: path_str,
        language,
        content_hash,
        ast_hash: parsed.ast_hash.clone(),
        lines_total: parsed.lines_total,
        mtime: mtime_secs(meta),
        file_size: meta.len(),
    };
    let chunks = chunks_for_file(&parsed.functions, &parsed.classes, root);
    ParsedFile {
        file,
        functions: parsed.functions,
        classes: parsed.classes,
        imports: parsed.imports,
        calls: parsed.calls,
        routes: parsed.routes,
        chunks,
    }
}

/// File mtime as whole seconds since the epoch (0 when unavailable). Matches the
/// form `doctor` reconciles against.
fn mtime_secs(meta: &Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
pub fn index_one_file(path: &Path, store: &SqliteCodeStore, root: &Path) -> Result<IndexStats> {
    let counts = index_file(path, store, root)?;
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
        ..Default::default()
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

/// Parse ONE file and write its graph — the point path the daemon re-runs on a
/// single changed file. Reads → (size-gates) → parses → `upsert_file` +
/// `upsert_chunks`. This always reparses (no hash skip): the daemon only calls it
/// for a file it already knows changed, so a skip check would be dead weight.
fn index_file(path: &Path, store: &SqliteCodeStore, root: &Path) -> Result<FileCounts> {
    let language =
        language_for(path).ok_or_else(|| Error::Invalid("unsupported extension".into()))?;
    let meta = std::fs::metadata(path).map_err(|e| Error::Io(e.to_string()))?;

    // Over the size cap: skip parsing, and make sure it isn't lingering in the index
    // (a file that grew past the cap must leave, matching `index_path`).
    if meta.len() > max_file_bytes() {
        tracing::warn!(
            "index: skipping {} — larger than {} bytes (ICODE_MAX_FILE_BYTES)",
            path.display(),
            max_file_bytes()
        );
        store.delete_file(&path.to_string_lossy())?;
        return Ok(FileCounts::default());
    }

    let source = std::fs::read_to_string(path).map_err(|e| Error::Io(e.to_string()))?;
    let hash = content_hash(&source);
    // Chunk+persist the file's symbols (graph-fast path: writes code_chunks with NO
    // vectors, so this never blocks on the network — the embed pass fills vectors
    // asynchronously). Keyed by path, idempotent per file.
    let pf = build_parsed(path, language, &source, &meta, hash, root);
    write_parsed(store, &pf)
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
