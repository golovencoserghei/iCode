//! `icode` — thin CLI dispatcher. Subcommands delegate to the engine or serve
//! layer; no business logic lives here.
//!
//! M0.5 walking skeleton wires two commands:
//!   `icode index <path>`  — index the tree under <path>, print counters.
//!   `icode serve <path>`  — open the store and serve the MCP protocol over stdio.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use icode_core::config::EmbedConfig;
use icode_core::traits::CodeReadStore;
use icode_engine::SqliteCodeStore;

#[derive(Parser)]
#[command(
    name = "icode",
    version,
    about = "iCode v2 — local code-graph + memory RAG"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Index all `.rs` files under <path> into <path>/.icode/index.db.
    Index {
        /// Project root to index.
        path: PathBuf,
    },
    /// Embed any pending chunks of an already-indexed <path> (catch-up pass).
    Embed {
        /// Project root whose <path>/.icode/index.db is embedded.
        path: PathBuf,
    },
    /// Open the store at <path> and serve the MCP protocol over stdio.
    Serve {
        /// Project root whose <path>/.icode/index.db is served.
        path: PathBuf,
    },
    /// Open the store at <path> and print code-graph statistics.
    Stats {
        /// Project root whose <path>/.icode/index.db is read.
        path: PathBuf,
    },
    /// Read-only health check: index drift vs disk + embedding/orphan invariants.
    /// Exits 0 when healthy, 1 otherwise.
    Doctor {
        /// Project root whose <path>/.icode/index.db is diagnosed against disk.
        path: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Index { path } => run_index(&path),
        Command::Embed { path } => run_embed(&path),
        Command::Serve { path } => run_serve(path),
        Command::Stats { path } => run_stats(&path),
        Command::Doctor { path } => run_doctor(&path),
    }
}

fn run_stats(path: &std::path::Path) -> anyhow::Result<()> {
    let store = SqliteCodeStore::open(path).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let s = store.stats().map_err(|e| anyhow::anyhow!(e.to_string()))?;
    println!("files:        {}", s.files);
    println!("functions:    {}", s.functions);
    println!("classes:      {}", s.classes);
    println!("calls:        {}", s.calls);
    println!("imports:      {}", s.imports);
    println!("routes:       {}", s.routes);
    println!("parse_errors: {}", s.parse_errors);
    Ok(())
}

/// Read-only diagnostics: open the store, diagnose drift vs disk + invariants,
/// print a human-readable report, and exit 0 (healthy) or 1 (drift/invariant
/// broken). Never panics — a real failure (e.g. can't open the db) is an `Err`.
fn run_doctor(path: &std::path::Path) -> anyhow::Result<()> {
    let store = SqliteCodeStore::open(path).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let r = icode_engine::diagnose(&store, path).map_err(|e| anyhow::anyhow!(e.to_string()))?;

    println!("healthy:            {}", if r.healthy { "yes" } else { "no" });
    println!("files indexed:      {}", r.files_indexed);
    println!("missing (on disk, not indexed):  {}", r.missing_count);
    println!("outdated (mtime/size drift):     {}", r.outdated_count);
    println!("stale (indexed, gone from disk): {}", r.stale_count);
    println!("parse errors:       {}", r.parse_errors);
    println!("chunks:             {}", r.chunks);
    println!("embedded:           {}", r.embedded);
    println!("pending embeddings: {}", r.pending_embeddings);
    println!("orphan vectors:     {}", r.orphan_vectors);

    print_examples("missing", &r.missing, r.missing_count);
    print_examples("outdated", &r.outdated, r.outdated_count);
    print_examples("stale", &r.stale, r.stale_count);

    if !r.healthy {
        // Drift / broken invariant is a non-zero exit, but NOT a process error
        // (the diagnosis itself succeeded). Re-run `icode index <path>` to heal.
        std::process::exit(1);
    }
    Ok(())
}

/// Print up to a few example paths for one drift category (nothing if empty).
fn print_examples(label: &str, examples: &[String], total: u64) {
    if examples.is_empty() {
        return;
    }
    println!("  {label} examples ({}/{}):", examples.len(), total);
    for p in examples.iter().take(10) {
        println!("    {p}");
    }
}

fn run_index(path: &std::path::Path) -> anyhow::Result<()> {
    let store = SqliteCodeStore::open(path).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let stats =
        icode_engine::index_path(path, &store).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    println!(
        "indexed {} files: {} functions, {} classes, {} imports, {} calls, {} routes, {} chunks ({} errors)",
        stats.files_indexed,
        stats.functions,
        stats.classes,
        stats.imports,
        stats.calls,
        stats.routes,
        stats.code_chunks,
        stats.errors
    );

    // Best-effort embed pass: the graph is already useful without vectors, so a
    // down/unreachable Ollama must NOT fail `index`. Run `icode embed <path>`
    // later to catch up once the embedder is available.
    embed_pass(&store, /* hard_fail = */ false)?;
    Ok(())
}

/// Stand-alone catch-up embed pass over an already-indexed db. Unlike the pass
/// folded into `index`, a missing embedder here IS a hard error (the user asked
/// to embed explicitly).
fn run_embed(path: &std::path::Path) -> anyhow::Result<()> {
    let store = SqliteCodeStore::open(path).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    embed_pass(&store, /* hard_fail = */ true)
}

/// Build the configured embedder and drain the pending-chunk queue.
///
/// `hard_fail` controls what happens when the embedder is unhealthy or the embed
/// pass errors: `index` (false) reports how many chunks remain pending and keeps
/// the command successful; `embed` (true) surfaces the error.
fn embed_pass(store: &SqliteCodeStore, hard_fail: bool) -> anyhow::Result<()> {
    let cfg = EmbedConfig::default();
    let embedder = match icode_embed::build_embedder(&cfg) {
        Ok(e) => e,
        Err(e) if !hard_fail => {
            report_pending(store, &cfg.model, &e.to_string());
            return Ok(());
        }
        Err(e) => return Err(anyhow::anyhow!(e.to_string())),
    };

    if let Err(e) = embedder.health() {
        if hard_fail {
            return Err(anyhow::anyhow!(e.to_string()));
        }
        report_pending(store, embedder.model_id(), &e.to_string());
        return Ok(());
    }

    match icode_engine::embed_pending(store, embedder.as_ref(), cfg.batch) {
        Ok(stats) => {
            println!(
                "embedded {} chunks ({} batches)",
                stats.embedded, stats.batches
            );
            Ok(())
        }
        Err(e) if !hard_fail => {
            report_pending(store, embedder.model_id(), &e.to_string());
            Ok(())
        }
        Err(e) => Err(anyhow::anyhow!(e.to_string())),
    }
}

/// Print how many chunks still need embedding and why the embedder was skipped.
fn report_pending(store: &SqliteCodeStore, model: &str, reason: &str) {
    use icode_core::traits::CodeWriteStore;
    // Count the remaining queue (cap high; this is a status line, not a hot path).
    let pending = store
        .pending_chunks(model, usize::MAX)
        .map(|p| p.len())
        .unwrap_or(0);
    println!("{pending} chunks pending embedding (ollama unavailable: {reason})");
}

fn run_serve(path: PathBuf) -> anyhow::Result<()> {
    // Build the embedder best-effort BEFORE entering the async runtime (the build
    // + health probe are blocking). A down/unreachable Ollama must NOT fail
    // `serve` — the code-graph tools stay fully useful; only the semantic/hybrid
    // tools degrade. We report which mode we ended up in on stderr (stdout is the
    // MCP stdio transport and must carry only protocol frames).
    let embedder = build_serve_embedder();

    // Build the cross-session memory store from the SAME embedder. The central
    // memory db OWNS an embedder (write-time vectors), so memory is available ONLY
    // when the embedder is. `icode-serve` stays decoupled from `icode-embed`: the
    // bin builds the concrete `SqliteMemoryStore`/`WalStore` and hands it over as
    // `Arc<dyn MemoryStore>`. A build failure degrades to memory-less (never fatal).
    let memory = build_serve_memory(embedder.clone());

    // Serving is async (rmcp/tokio); the rest of the CLI stays sync.
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let store = SqliteCodeStore::open(&path).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        // Pass the project root so the `doctor` MCP tool can reconcile the index
        // against the live source tree (it walks `root` like the indexer does).
        icode_serve::serve_stdio(Arc::new(store), path, embedder, memory).await
    })
}

/// Central memory db path (`~/.icode/icode.db`). `~` is expanded against `$HOME`
/// here so the store's `open` gets an absolute path (its own tilde-expansion is a
/// belt-and-braces fallback). Falls back to the literal `~/...` if `$HOME` is
/// unset (the store will then try to expand it, or fail cleanly).
fn central_db_path() -> String {
    match std::env::var("HOME") {
        Ok(home) => format!("{home}/.icode/icode.db"),
        Err(_) => "~/.icode/icode.db".to_string(),
    }
}

/// Build the cross-session memory store for `serve`, wrapped in the WAL audit
/// decorator. Returns `None` (memory-less mode) when there is no embedder or the
/// central db could not be opened — `serve` never fails here. A stderr note
/// records the resulting mode (stdout is the MCP transport).
fn build_serve_memory(
    embedder: Option<Arc<dyn icode_core::traits::Embedder>>,
) -> Option<Arc<dyn icode_core::traits::MemoryStore>> {
    use icode_engine::{SqliteMemoryStore, WalStore};

    let embedder = match embedder {
        Some(e) => e,
        None => {
            eprintln!("cross-session memory disabled: no embedder (lexical code tools only)");
            return None;
        }
    };

    let db_path = central_db_path();
    let base = match SqliteMemoryStore::open(&db_path, embedder) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cross-session memory disabled: cannot open {db_path}: {e}");
            return None;
        }
    };

    // WAL audit log lives next to the central db (`~/.icode/wal.jsonl`).
    let wal_path = match std::env::var("HOME") {
        Ok(home) => format!("{home}/.icode/wal.jsonl"),
        Err(_) => "~/.icode/wal.jsonl".to_string(),
    };
    let store: Arc<dyn icode_core::traits::MemoryStore> =
        Arc::new(WalStore::new(Arc::new(base), wal_path));
    eprintln!("cross-session memory enabled (db {db_path})");
    Some(store)
}

/// Build the configured embedder for `serve`, probing its health, and report the
/// resulting mode on stderr. Returns `Some(Arc<dyn Embedder>)` when ready, or
/// `None` (lexical-only) on any build/health failure — `serve` never fails here.
fn build_serve_embedder() -> Option<Arc<dyn icode_core::traits::Embedder>> {
    use icode_core::traits::Embedder;
    let cfg = EmbedConfig::default();
    let embedder = match icode_embed::build_embedder(&cfg) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("semantic search disabled: {e} (lexical only)");
            return None;
        }
    };
    if let Err(e) = embedder.health() {
        eprintln!("semantic search disabled: {e} (lexical only)");
        return None;
    }
    eprintln!("semantic search enabled (model {})", embedder.model_id());
    // Box<dyn Embedder> → Arc<dyn Embedder>.
    let arc: Arc<dyn Embedder> = Arc::from(embedder);
    Some(arc)
}
