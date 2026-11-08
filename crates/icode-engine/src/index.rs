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
    /// Chunks deleted because their file was no longer in the index — leftovers
    /// from a binary whose `delete_file` did not clear them. Normally 0.
    pub ghost_chunks_pruned: u64,
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
    // Per-user tool caches. These matter because a walk rooted at `$HOME` (which
    // is what `serve` fell back to outside a repo) otherwise pulls in every
    // dependency of every project the user has ever built: an observed 4.0 GB
    // index over 64k files whose paths were `~/.cache/uv/...` and
    // `~/.cache/JetBrains/...`. None of it is the user's code, and all of it
    // dilutes search and inflates the graph.
    ".cache",
    ".local",
    ".cargo",
    ".rustup",
    ".npm",
    ".nvm",
    ".yarn",
    ".pnpm-store",
    ".bun",
    ".deno",
    ".gradle",
    ".m2",
    ".conda",
    ".pyenv",
    ".rbenv",
    ".gem",
    // Framework / infra build caches
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".parcel-cache",
    ".terraform",
    ".direnv",
    // Our own per-project data (index.db, daemon lock/log) — never source.
    ".icode",
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
        .filter_entry(|e| {
            // The walk root itself is never excluded, whatever it is named: only
            // directories BELOW it are matched (a project checked out as `…/build`
            // is still a project).
            !(e.depth() > 0 && is_excluded_dir(e.path()))
                && !(e.depth() > 0 && e.file_type().is_dir() && is_linked_worktree_below(root, e.path()))
        })
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
                        // The parsed content hash IS the fingerprint of these bytes.
                        let fp = format!("sha256:{}", pf.file.content_hash);
                        let _ = store.record_parse_error_once(&pf.file.path, &fp, &e.to_string());
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
                    let fp = error_fingerprint(Path::new(&path));
                    let _ = store.record_parse_error_once(&path, &fp, &error);
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

    // Errors recorded for files that are no longer on disk at all.
    let _ = store.prune_parse_errors_except(&on_disk);

    // Ghost sweep: chunks whose file left the index without taking them along.
    // `delete_file` handles that from now on, so this only ever fires on a database
    // an older binary wrote — but it must fire, because nothing else would: a ghost
    // path is never re-indexed, so `upsert_chunks` never runs for it, and until it
    // goes `search mode=semantic` keeps answering with code that is not on disk.
    let ghosts = store.prune_ghost_chunks()?;
    if ghosts > 0 {
        tracing::info!("index: pruned {ghosts} chunk(s) left behind by files no longer indexed");
        stats.ghost_chunks_pruned = ghosts;
        any_change = true;
    }

    // Receiver-aware call resolution: validate each edge's qualified guess and grade
    // confidence against the COMPLETE functions table. Runs once after the walk (a
    // callee may live in a file processed later than its caller). Skipped on a pure
    // no-op pass — the persisted resolution is already correct, so re-running the
    // full-table UPDATE would only burn a write for an identical result.
    // An index graded by older rules is re-graded once (no re-parse).
    if any_change || store.resolution_outdated()? {
        store.resolve_call_edges(None)?;
        // Graph centrality (free, CPU-only) — a search-ranking prior. Must run AFTER
        // the edges are resolved, since it reads the finished call graph.
        store.compute_symbol_ranks()?;
        // Deleting rows frees pages but never shrinks the file: an index that once
        // held every linked worktree's copy kept its 593 MB after the walk stopped
        // seeing them (88% of rows gone). Reclaim the space when a pass dropped a
        // large share of the tree — never on an ordinary pass, VACUUM rewrites the
        // whole file.
        if should_vacuum(prior.len(), stats.files_deleted) {
            match store.vacuum() {
                Ok(()) => eprintln!(
                    "index: {} of {} files left the index — compacted the database (VACUUM)",
                    stats.files_deleted,
                    prior.len()
                ),
                Err(e) => eprintln!("index: VACUUM skipped ({e}); will retry on a later large purge"),
            }
        }
        // A pass that wrote anything grew the WAL (a VACUUM most of all); hand that
        // space back now rather than leaving it at its high-water mark for the life
        // of the connection.
        store.checkpoint_truncate();
    }

    Ok(stats)
}

/// Bring the part of the index under `under` (the whole tree when `under == root`)
/// back in line with disk using only `stat` — the cheap check for when something
/// else (the watcher daemon) is supposed to have kept the graph current.
///
/// Unlike [`index_path`] it reads no file whose indexed `mtime` and size still match
/// disk, so an unchanged tree costs one walk plus one stat per file. What it does:
///   * a path on disk that is not indexed, or whose mtime/size drifted, goes through
///     [`index_one_file`] (which still hash-skips if only the metadata changed);
///   * an indexed path under `under` that is no longer on disk is deleted;
///   * edges and ranks are re-resolved and the WAL truncated only if a row changed.
///
/// The trade-off is the mtime granularity: an edit that keeps the size and lands
/// in the same second as the indexed mtime is invisible to it. That is the gap the
/// daemon's own events cover; this pass exists for the events the daemon LOST.
pub fn reconcile_by_stat(root: &Path, store: &SqliteCodeStore, under: &Path) -> Result<IndexStats> {
    let mut stats = IndexStats::default();
    let whole_tree = under == root;
    let prior: HashMap<String, (i64, u64)> = if whole_tree {
        store
            .list_files(None, None, usize::MAX)?
            .into_iter()
            .map(|f| (f.path, (f.mtime, f.file_size)))
            .collect()
    } else {
        store
            .files_under(&under.to_string_lossy())?
            .into_iter()
            .map(|(p, m, z)| (p, (m, z)))
            .collect()
    };

    let on_disk: Vec<std::path::PathBuf> = if under.is_dir() {
        walk_source_files(under)
            .into_iter()
            .filter(|p| whole_tree || !is_in_linked_worktree(root, p))
            .collect()
    } else {
        Vec::new()
    };

    let mut any_change = false;
    let mut seen: HashSet<String> = HashSet::with_capacity(on_disk.len());
    for path in &on_disk {
        let key = path.to_string_lossy().to_string();
        let current = std::fs::metadata(path).ok().map(|m| (mtime_secs(&m), m.len()));
        let drifted = match (prior.get(&key), current) {
            (Some(indexed), Some(now)) => *indexed != now,
            (None, Some(_)) => true,
            (_, None) => false, // vanished mid-walk: the deletion sweep below has it
        };
        if current.is_some() {
            seen.insert(key.clone());
        }
        if !drifted {
            stats.files_skipped += 1;
            continue;
        }
        // The whole-graph `resolve_call_edges(None)` below re-grades every edge, so
        // the per-file cross-file re-grade would only repeat it.
        match index_one_file_scoped(path, store, root) {
            Ok(s) if s.files_indexed == 0 => stats.files_skipped += 1,
            Ok(s) => {
                any_change = true;
                stats.files_indexed += 1;
                stats.functions += s.functions;
                stats.classes += s.classes;
            }
            Err(e) => {
                stats.errors += 1;
                let _ = store.record_parse_error_once(&key, &error_fingerprint(path), &e.to_string());
            }
        }
    }
    for path in prior.keys() {
        if !seen.contains(path) {
            store.delete_file(path)?;
            any_change = true;
            stats.files_deleted += 1;
        }
    }
    if any_change || store.resolution_outdated()? {
        store.resolve_call_edges(None)?;
        store.compute_symbol_ranks()?;
        store.checkpoint_truncate();
    }
    Ok(stats)
}

/// Whether a full pass that removed `deleted` of the `prior_files` it started with
/// purged enough to be worth a `VACUUM`: at least 500 files, or at least 20% of the
/// index. Below that the freed pages are simply reused by later writes.
pub fn should_vacuum(prior_files: usize, deleted: u64) -> bool {
    const MIN_FILES: u64 = 500;
    if deleted == 0 {
        return false;
    }
    deleted >= MIN_FILES || deleted.saturating_mul(5) >= prior_files as u64
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
        // Which sub-project this file belongs to. In a single-project repo this is ""
        // and nothing downstream changes; in a monorepo it is the boundary that keeps a
        // call from resolving to a sibling project's identically-named function.
        module: crate::module::module_of(path, root),
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
    let mut touched = HashSet::new();
    let stats = index_one_file_noting(path, store, root, &mut touched)?;
    store.regrade_calls_to(&touched)?;
    Ok(stats)
}

/// [`index_one_file`] without the cross-file re-grade: the names of the functions
/// the file defined before and after are added to `touched`, so a caller indexing a
/// BATCH (the daemon) re-grades the edges to all of them once, after every file of
/// the batch is in place — see [`SqliteCodeStore::regrade_calls_to`]. Nothing is
/// added when the file turned out unchanged.
pub fn index_one_file_noting(
    path: &Path,
    store: &SqliteCodeStore,
    root: &Path,
    touched: &mut HashSet<String>,
) -> Result<IndexStats> {
    let key = path.to_string_lossy();
    let before = store.function_names_in_file(&key)?;
    let stats = index_one_file_scoped(path, store, root)?;
    let after = store.function_names_in_file(&key)?;
    // An oversize file is dropped yet reported as skipped: compare the name sets
    // too, so its vanished definitions still re-grade their callers.
    if stats.files_indexed > 0 || before != after {
        touched.extend(before);
        touched.extend(after);
    }
    Ok(stats)
}

/// Delete one file's rows (a file that vanished from disk), adding the names of the
/// functions it defined to `touched` — their callers elsewhere just lost a
/// definition. The deletion counterpart of [`index_one_file_noting`].
pub fn delete_one_file_noting(store: &SqliteCodeStore, key: &str, touched: &mut HashSet<String>) -> Result<()> {
    touched.extend(store.function_names_in_file(key)?);
    store.delete_file(key)
}

/// Re-index one file and re-grade only ITS outgoing edges (no cross-file pass).
fn index_one_file_scoped(path: &Path, store: &SqliteCodeStore, root: &Path) -> Result<IndexStats> {
    let counts = index_file(path, store, root)?;
    // `index_file` returns zeroed counts when the content hash matched and it skipped
    // the reparse. Report that as SKIPPED, not indexed: the daemon uses this to decide
    // whether anything actually changed, and a false "indexed" makes it redo the
    // whole-graph work (edge resolution, centrality) for nothing.
    if counts.is_empty() {
        return Ok(IndexStats { files_skipped: 1, ..Default::default() });
    }
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

/// Whether the changed `path` is one the indexer walk of `root` skips because of an
/// excluded directory — the daemon's per-event form of the walk's rule.
///
/// Only the components BELOW `root` count. The walk matches directory names
/// relative to its root, so a project that itself lives under `~/.cache/…`,
/// `/srv/build/app` or `…/dist/…` is indexed in full; checking the absolute path
/// here made the daemon drop EVERY event of such a project, and its graph never
/// changed again after the first pass. A path outside `root` is never this tree's
/// source and counts as excluded. `.icode` is in the set, so the daemon never
/// reacts to its own db writes.
pub fn is_excluded_below(root: &Path, path: &Path) -> bool {
    match path.strip_prefix(root) {
        Ok(rel) => is_in_excluded_dir(rel),
        Err(_) => true,
    }
}

/// Directory component names the indexer (and therefore the daemon) skips wholesale
/// — build artefacts, VCS metadata, dependency caches — matched on EVERY component
/// of `path`. Pass a path relative to the walk root (see [`is_excluded_below`]): an
/// absolute path also matches the root's own ancestors.
pub fn is_in_excluded_dir(path: &Path) -> bool {
    path.components().any(|c| match c {
        std::path::Component::Normal(name) => name
            .to_str()
            .map(|n| EXCLUDED_DIRS.contains(&n))
            .unwrap_or(false),
        _ => false,
    })
}

/// Whether `dir`, a directory BELOW the walk root `root`, is a linked git worktree
/// that must not be indexed as part of `root`: its `.git` is a FILE pointing at
/// `…/worktrees/<name>`, or its path relative to `root` runs through
/// `.claude/worktrees` (agent worktrees, which may not even have their `.git`
/// yet). A worktree is a second checkout of the same repo — indexing it under the
/// main tree doubles every symbol and pins the daemon on someone else's branch.
///
/// The root itself is NEVER excluded (`dir == root` → false), even when it is a
/// worktree: indexing a worktree on purpose is legitimate. Submodules (`gitdir:
/// …/modules/<name>`, also inside a worktree: `…/worktrees/<wt>/modules/<name>`)
/// and nested ordinary repos (`.git` directory) are not worktrees and stay in.
///
/// The ONE exclusion rule shared by the indexer walk and the daemon's event filter
/// ([`is_in_linked_worktree`]).
pub fn is_linked_worktree_below(root: &Path, dir: &Path) -> bool {
    let Ok(rel) = dir.strip_prefix(root) else {
        return false;
    };
    if rel.as_os_str().is_empty() {
        return false;
    }
    let names: Vec<_> = rel.components().map(|c| c.as_os_str()).collect();
    names.windows(2).any(|w| w[0] == ".claude" && w[1] == "worktrees") || is_linked_worktree_dir(dir)
}

/// `dir/.git` is a file whose `gitdir:` points straight at `…/worktrees/<name>`.
fn is_linked_worktree_dir(dir: &Path) -> bool {
    let dot_git = dir.join(".git");
    match std::fs::symlink_metadata(&dot_git) {
        Ok(m) if m.is_file() && m.len() < 4096 => {}
        _ => return false,
    }
    let Ok(text) = std::fs::read_to_string(&dot_git) else {
        return false;
    };
    let Some(gitdir) = text.lines().find_map(|l| l.trim().strip_prefix("gitdir:")) else {
        return false;
    };
    let gitdir = Path::new(gitdir.trim());
    gitdir.file_name().is_some()
        && gitdir
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == "worktrees")
}

/// Whether the changed `path` lies inside a linked worktree below `root` — the
/// daemon's form of [`is_linked_worktree_below`], checked on every ancestor
/// directory between the path and the root (the root itself excluded). Works for
/// a path that no longer exists (a deletion event): a vanished directory simply
/// has no `.git` to read, and the `.claude/worktrees` rule is purely lexical.
pub fn is_in_linked_worktree(root: &Path, path: &Path) -> bool {
    path.ancestors()
        .take_while(|a| *a != root && a.starts_with(root))
        .any(|dir| is_linked_worktree_below(root, dir))
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

impl FileCounts {
    /// Nothing was written — `index_file` hit the content-hash skip. Distinguishing
    /// this from a real reindex is what stops the daemon redoing whole-graph work
    /// (edge resolution, centrality) for a file whose bytes never changed.
    fn is_empty(&self) -> bool {
        self.functions == 0
            && self.classes == 0
            && self.imports == 0
            && self.calls == 0
            && self.routes == 0
            && self.code_chunks == 0
    }
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

    // CONTENT-HASH SKIP — the same gate `index_path` applies, which this path was
    // missing entirely.
    //
    // The daemon calls this on every filesystem EVENT, and an event does not mean the
    // bytes changed: an editor's atomic save, a `touch`, a container remounting the
    // tree, a git checkout of an identical blob all fire one. Without this gate each
    // event drove a full tree-sitter reparse plus a rewrite of the file's rows — and
    // rewriting the rows nulls the chunks' vectors, so it also dragged them back into
    // the embed queue. Observed on a real project with a few containers running: the
    // watcher sat at 104% CPU re-indexing 11-19 unchanged files several times a
    // second, indefinitely.
    //
    // A hash of the bytes we already read is nearly free; a reparse is not.
    if !force_reindex() {
        if let Ok(Some(prior)) = store.stat_file(&path.to_string_lossy()) {
            if prior.content_hash == hash {
                // Same bytes. Mirror a clean reindex and refresh only the metadata, so
                // a pure mtime/size drift does not keep coming back.
                let mtime = mtime_secs(&meta);
                if prior.mtime != mtime || prior.file_size != meta.len() {
                    store.touch_file_meta(&path.to_string_lossy(), mtime, meta.len())?;
                }
                return Ok(FileCounts::default());
            }
        }
    }

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
    bytes_hash(source.as_bytes())
}

/// Identify the version of a file that failed to index, for
/// [`SqliteCodeStore::record_parse_error_once`]: `sha256:<hex>` of its raw bytes
/// (the same digest `content_hash` gives valid UTF-8, and still defined for bytes
/// that are not), else `meta:<mtime>:<size>` when the bytes cannot be read, else
/// `missing`. Same bytes ⇒ same fingerprint ⇒ the error is not recorded again.
pub fn error_fingerprint(path: &Path) -> String {
    if let Ok(bytes) = std::fs::read(path) {
        return format!("sha256:{}", bytes_hash(&bytes));
    }
    match std::fs::metadata(path) {
        Ok(meta) => format!("meta:{}:{}", mtime_secs(&meta), meta.len()),
        Err(_) => "missing".to_string(),
    }
}

fn bytes_hash(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
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

#[cfg(test)]
mod exclude_tests {
    use super::{is_excluded_below, is_excluded_dir, is_in_excluded_dir, is_in_linked_worktree, walk_source_files};
    use std::path::Path;

    /// `dir/.git` as a FILE pointing at `gitdir`.
    fn git_file(dir: &Path, gitdir: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(".git"), format!("gitdir: {gitdir}\n")).unwrap();
    }

    fn src(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), "pub fn f() {}\n").unwrap();
    }

    fn walked(root: &Path) -> Vec<String> {
        let mut v: Vec<String> = walk_source_files(root)
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn linked_worktrees_below_the_root_are_skipped_submodules_and_nested_repos_kept() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::create_dir_all(root.join(".git/worktrees/wt")).unwrap();
        src(root, "main.rs");
        // A linked worktree checked out inside the tree.
        git_file(&root.join("wt"), &root.join(".git/worktrees/wt").to_string_lossy());
        src(&root.join("wt"), "dup.rs");
        // An agent worktree under .claude/worktrees (no .git yet).
        src(&root.join(".claude/worktrees/agent-1/src"), "agent.rs");
        // A submodule (relative gitdir into .git/modules) and a nested plain repo.
        git_file(&root.join("vendored"), "../.git/modules/vendored");
        src(&root.join("vendored"), "sub.rs");
        std::fs::create_dir_all(root.join("nested/.git")).unwrap();
        src(&root.join("nested"), "inner.rs");
        // A submodule of a worktree points into …/worktrees/<wt>/modules/<name>.
        git_file(&root.join("lib2"), "/repo/.git/worktrees/wt/modules/lib2");
        src(&root.join("lib2"), "lib2.rs");

        assert_eq!(walked(root), vec!["lib2/lib2.rs", "main.rs", "nested/inner.rs", "vendored/sub.rs"]);

        // The daemon's per-event form agrees, including for a deleted path.
        assert!(is_in_linked_worktree(root, &root.join("wt/dup.rs")));
        assert!(is_in_linked_worktree(root, &root.join(".claude/worktrees/agent-1/src/gone.rs")));
        assert!(!is_in_linked_worktree(root, &root.join("vendored/sub.rs")));
        assert!(!is_in_linked_worktree(root, &root.join("nested/inner.rs")));
        assert!(!is_in_linked_worktree(root, &root.join("main.rs")));
    }

    #[test]
    fn a_worktree_used_as_the_root_is_indexed() {
        let td = tempfile::tempdir().unwrap();
        // The walk root itself is a linked worktree under .claude/worktrees.
        let root = td.path().join("repo/.claude/worktrees/agent-7");
        git_file(&root, "/repo/.git/worktrees/agent-7");
        src(&root.join("src"), "lib.rs");
        assert_eq!(walked(&root), vec!["src/lib.rs"]);
        assert!(!is_in_linked_worktree(&root, &root.join("src/lib.rs")));
    }

    #[test]
    fn user_tool_caches_are_excluded() {
        // The 4.0 GB `$HOME` index was built out of exactly these paths.
        for p in [
            "/home/u/.cache/uv/archive-v0/pkg/mod.py",
            "/home/u/.cache/JetBrains/IntelliJ/index/x.java",
            "/home/u/.local/lib/python3.11/site-packages/x.py",
            "/home/u/.cargo/registry/src/crate-1.0/src/lib.rs",
            "/home/u/.rustup/toolchains/stable/lib/x.rs",
            "/home/u/.npm/_cacache/index-v5/a.js",
            "/home/u/proj/.next/server/pages/index.js",
            "/home/u/proj/.icode/index.db",
        ] {
            assert!(is_in_excluded_dir(Path::new(p)), "should be excluded: {p}");
        }
    }

    #[test]
    fn real_source_is_not_excluded() {
        for p in [
            "/home/u/proj/src/main.rs",
            "/home/u/proj/crates/icode-engine/src/index.rs",
            // A file merely NAMED like a cache dir is still source.
            "/home/u/proj/src/cache.rs",
            "/home/u/proj/src/local_state.rs",
        ] {
            assert!(!is_in_excluded_dir(Path::new(p)), "should be kept: {p}");
        }
    }

    #[test]
    fn excluded_dir_matches_the_directory_itself() {
        assert!(is_excluded_dir(Path::new("/home/u/.cache")));
        assert!(is_excluded_dir(Path::new("/home/u/proj/target")));
        assert!(!is_excluded_dir(Path::new("/home/u/proj/src")));
    }

    #[test]
    fn exclusion_looks_only_below_the_root() {
        // The root's own ancestors may be named anything — a checkout under
        // `~/.cache`, `/srv/build/app` or `…/dist/…` is still a project.
        for root in ["/home/u/.cache/x", "/srv/build/app", "/w/dist/p", "/home/u/.local/src/p"] {
            let root = Path::new(root);
            assert!(!is_excluded_below(root, &root.join("src/a.ts")), "kept: {}", root.display());
            assert!(is_excluded_below(root, &root.join("build/x.ts")), "<root>/build excluded");
            assert!(is_excluded_below(root, &root.join(".icode/index.db")));
            assert!(is_excluded_below(root, &root.join("pkg/node_modules/m/i.js")));
        }
        // Outside the root is never this tree's source.
        assert!(is_excluded_below(Path::new("/srv/app"), Path::new("/srv/other/a.ts")));
    }

    #[test]
    fn a_root_named_like_an_excluded_dir_is_walked() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join(".cache/build");
        src(&root.join("src"), "lib.rs");
        src(&root.join("build"), "gen.rs");
        assert_eq!(walked(&root), vec!["src/lib.rs"]);
    }
}
