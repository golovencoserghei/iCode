//! `icode` — thin CLI dispatcher. Subcommands delegate to the engine or serve
//! layer; no business logic lives here.
//!
//! M0.5 walking skeleton wires two commands:
//!   `icode index <path>`  — index the tree under <path>, print counters.
//!   `icode serve <path>`  — open the store and serve the MCP protocol over stdio.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use icode_engine::SqliteCodeStore;

#[derive(Parser)]
#[command(name = "icode", version, about = "iCode v2 — local code-graph + memory RAG")]
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
    /// Open the store at <path> and serve the MCP protocol over stdio.
    Serve {
        /// Project root whose <path>/.icode/index.db is served.
        path: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Index { path } => run_index(&path),
        Command::Serve { path } => run_serve(path),
    }
}

fn run_index(path: &std::path::Path) -> anyhow::Result<()> {
    let store = SqliteCodeStore::open(path).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let stats = icode_engine::index_path(path, &store).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    println!(
        "indexed {} files, {} functions ({} errors)",
        stats.files_indexed, stats.functions, stats.errors
    );
    Ok(())
}

fn run_serve(path: PathBuf) -> anyhow::Result<()> {
    // Serving is async (rmcp/tokio); the rest of the CLI stays sync.
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let store = SqliteCodeStore::open(&path).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        icode_serve::serve_stdio(Arc::new(store)).await
    })
}
