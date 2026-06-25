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
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Index { path } => run_index(&path),
        Command::Embed { path } => run_embed(&path),
        Command::Serve { path } => run_serve(path),
        Command::Stats { path } => run_stats(&path),
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
    // Serving is async (rmcp/tokio); the rest of the CLI stays sync.
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let store = SqliteCodeStore::open(&path).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        icode_serve::serve_stdio(Arc::new(store)).await
    })
}
