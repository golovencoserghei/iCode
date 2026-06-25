//! `icode` — thin CLI dispatcher. Subcommands delegate to the engine or serve
//! layer; no business logic lives here.
//!
//! Commands:
//!   `icode index <path>`   — index the tree under <path>, print counters.
//!   `icode embed <path>`   — embed any pending chunks (catch-up pass).
//!   `icode serve <path>`   — open the store and serve the MCP protocol over stdio.
//!   `icode stats <path>`   — print code-graph statistics.
//!   `icode doctor <path>`  — read-only index health check (exit 1 on drift).
//!   `icode setup [path]`   — friendly onboarding: probe Ollama, print MCP config.
//!   `icode mcp-config [p]` — print only the Claude Code MCP registration JSON.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use icode_core::config::EmbedConfig;
use icode_core::traits::CodeReadStore;
use icode_core::traits::Embedder;
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
    /// Live daemon: watch a project and keep its index up to date as files change.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
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
    /// Friendly onboarding: probe Ollama (pull the embedding model if missing),
    /// print the Claude Code MCP registration snippet, and list the next steps.
    /// Never panics — a down Ollama just degrades to lexical-only and keeps going.
    Setup {
        /// Optional project root to bake into the MCP snippet (else a placeholder).
        project_path: Option<PathBuf>,
    },
    /// Print ONLY the Claude Code MCP registration JSON for this binary to stdout
    /// (so `icode mcp-config /my/proj > .mcp.json` works). No other output.
    McpConfig {
        /// Optional project root to bake into the snippet (else a placeholder).
        project_path: Option<PathBuf>,
    },
    /// Claude Code lifecycle hooks. Claude Code runs these on session events and
    /// reads their stdout JSON to inject memory context. They are FAST, print
    /// valid `hookSpecificOutput` JSON, and NEVER fail (a down Ollama degrades to
    /// a minimal/empty additionalContext, exit 0).
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },
}

#[derive(Subcommand)]
enum HookAction {
    /// SessionStart hook: prime the agent with the developer profile + recent
    /// project memory. Degrades to a behavioural trigger when memory is empty or
    /// the embedder is down. Prints `hookSpecificOutput` JSON; always exits 0.
    SessionStart {
        /// Project name (else the basename of `--cwd`, else the process cwd).
        #[arg(long)]
        project: Option<String>,
        /// Working directory whose basename is the project (when `--project` is absent).
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// PreCompact hook: re-inject the L0 "always-on" rules (e.g. "answer in
    /// Russian") so they survive context compaction. Reads L0 notes WITHOUT an
    /// embedder (lexical list, no Ollama needed). Prints `hookSpecificOutput`
    /// JSON; always exits 0 (empty additionalContext when there are no L0 rules).
    Precompact {
        /// Project name (else the basename of `--cwd`, else the process cwd).
        #[arg(long)]
        project: Option<String>,
        /// Working directory whose basename is the project (when `--project` is absent).
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Print a ready-to-paste `~/.claude/settings.json` hooks snippet wiring the
    /// SessionStart / PreCompact events to `icode hook …`, plus where to put it.
    Config,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Run the watcher in the foreground: initial sync, then live-reindex on every
    /// change until Ctrl-C. One daemon per project (PID-locked). status/stop over
    /// IPC are future work — for now stop the foreground process directly.
    Run {
        /// Project root to watch and keep indexed.
        path: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Index { path } => run_index(&path),
        Command::Embed { path } => run_embed(&path),
        Command::Serve { path } => run_serve(path),
        Command::Daemon { action } => match action {
            DaemonAction::Run { path } => run_daemon_cmd(&path),
        },
        Command::Stats { path } => run_stats(&path),
        Command::Doctor { path } => run_doctor(&path),
        Command::Setup { project_path } => run_setup(project_path.as_deref()),
        Command::McpConfig { project_path } => run_mcp_config(project_path.as_deref()),
        Command::Hook { action } => match action {
            HookAction::SessionStart { project, cwd } => {
                run_hook_session_start(project.as_deref(), cwd.as_deref());
                Ok(())
            }
            HookAction::Precompact { project, cwd } => {
                run_hook_precompact(project.as_deref(), cwd.as_deref());
                Ok(())
            }
            HookAction::Config => {
                run_hook_config();
                Ok(())
            }
        },
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

// ──────────────────────────── setup / mcp-config ────────────────────────────

/// Absolute path to THIS binary, for the MCP `command` field. Falls back to the
/// bare name `icode` (resolved via PATH) when the exe path can't be determined.
fn icode_exe_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "icode".to_string())
}

/// Render the Claude Code MCP registration snippet for this binary. `project`
/// is the path baked into `args` (a placeholder when the user gave none). Built
/// by hand (no serde_json dep in the bin) but kept strictly valid: the only
/// interpolated values are filesystem paths, which we JSON-escape.
fn mcp_config_json(project: &str) -> String {
    let exe = json_escape(&icode_exe_path());
    let proj = json_escape(project);
    format!(
        "{{\n  \"mcpServers\": {{\n    \"icode\": {{\n      \"command\": \"{exe}\",\n      \"args\": [\"serve\", \"{proj}\"]\n    }}\n  }}\n}}"
    )
}

/// Minimal JSON string escaping (backslash, quote, and the control chars that
/// can appear in a path). Enough to keep the hand-built snippet valid.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

/// The project path to bake into the snippet: the absolute form of the user's
/// argument when given (so the registration is portable), else a placeholder.
fn snippet_project(project_path: Option<&std::path::Path>) -> String {
    match project_path {
        Some(p) => std::fs::canonicalize(p)
            .ok()
            .and_then(|c| c.to_str().map(str::to_string))
            .unwrap_or_else(|| p.display().to_string()),
        None => "/path/to/project".to_string(),
    }
}

/// `icode mcp-config [project]` — print ONLY the JSON registration snippet to
/// stdout (pipe-friendly: `icode mcp-config /my/proj > .mcp.json`).
fn run_mcp_config(project_path: Option<&std::path::Path>) -> anyhow::Result<()> {
    println!("{}", mcp_config_json(&snippet_project(project_path)));
    Ok(())
}

/// `icode setup [project]` — friendly onboarding. Probes Ollama (pulling the
/// embedding model if it's missing), prints the MCP registration snippet, and
/// lists the next steps. Never panics and never returns `Err`: a down/incomplete
/// Ollama degrades to lexical-only and the rest of the guidance still prints.
fn run_setup(project_path: Option<&std::path::Path>) -> anyhow::Result<()> {
    println!("iCode setup — local code-graph + memory RAG\n");

    // Step 1: Ollama / embedding model.
    println!("1. Checking Ollama (embedding backend)");
    let cfg = EmbedConfig::default();
    check_ollama(&cfg);

    // Step 2: MCP registration snippet.
    let project = snippet_project(project_path);
    println!("\n2. Connect iCode to Claude Code (MCP server)");
    println!("   Add this to `.mcp.json` in your project root (or your");
    println!("   `~/.claude` settings) so Claude Code launches the iCode server:\n");
    for line in mcp_config_json(&project).lines() {
        println!("   {line}");
    }
    if project_path.is_none() {
        println!("\n   (replace `/path/to/project` with the project you want indexed,");
        println!("    or re-run `icode setup <project_path>` to bake it in.)");
    }

    // Step 3: next steps.
    let hint = project_path.map(|_| project.as_str()).unwrap_or("<project>");
    println!("\n3. Next steps");
    println!("   icode index  {hint}    # build the code-graph + embeddings");
    println!("   icode doctor {hint}    # verify the index is healthy");
    println!("   then connect the MCP server above and use `recall` from Claude Code");

    Ok(())
}

/// Probe Ollama for `setup`: build the embedder, run its health check, and on a
/// missing model attempt `ollama pull <model>` before re-probing. Prints a clear
/// status for every branch; on hard unavailability it prints how to start Ollama
/// and notes the lexical-only fallback. Never returns an error.
fn check_ollama(cfg: &EmbedConfig) {
    let embedder = match icode_embed::build_embedder(cfg) {
        Ok(e) => e,
        Err(e) => {
            println!("   ! could not build embedder: {e}");
            println!("   → semantic search and cross-session memory will be disabled");
            println!("     (lexical code tools still work).");
            return;
        }
    };

    // First probe. Success = Ollama up AND the model is present.
    if embedder.health().is_ok() {
        println!(
            "   ✓ Ollama reachable, model {} ready",
            embedder.model_id()
        );
        return;
    }

    // Health failed. Distinguish "Ollama down" from "model missing" by checking
    // reachability cheaply: if a pull connects, Ollama is up and we just lacked
    // the model. We try the pull unconditionally — it's the right action for the
    // common "fresh machine, model not pulled yet" case, and a clean no-op when
    // the model is already there.
    println!(
        "   model {} not ready — attempting `ollama pull {}`",
        embedder.model_id(),
        embedder.model_id()
    );
    if try_pull_model(embedder.model_id()) {
        // Re-probe after a successful pull.
        if embedder.health().is_ok() {
            println!(
                "   ✓ Ollama reachable, model {} ready",
                embedder.model_id()
            );
            return;
        }
        println!("   ! pull finished but health still failing for {}", embedder.model_id());
    }

    // Either the pull failed to run (ollama binary missing / daemon down) or it
    // ran but health still fails. Give the actionable hint and degrade.
    println!("   ! Ollama not available — start it with: ollama serve");
    println!("     (then re-run `icode setup`; semantic/memory are disabled until then,");
    println!("      lexical code tools work regardless.)");
}

/// Run `ollama pull <model>`, streaming its output to the terminal and waiting
/// for completion. Returns `true` only when the command ran AND exited 0.
/// Any failure to even launch `ollama` (not installed / daemon down) is caught
/// and reported as `false` — never panics.
fn try_pull_model(model: &str) -> bool {
    use std::process::Command;
    match Command::new("ollama").arg("pull").arg(model).status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            println!("   ! `ollama pull {model}` exited with {status}");
            false
        }
        Err(e) => {
            println!("   ! could not run `ollama pull {model}`: {e}");
            false
        }
    }
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

/// `icode daemon run <path>` — foreground live-indexing daemon. Opens the store,
/// builds the embedder best-effort (a down Ollama is non-fatal: the graph stays
/// live and vectors just aren't refreshed — a note goes to stderr, mirroring
/// `serve`), then watches the tree until Ctrl-C. The single-writer PID-lock inside
/// `run_daemon` rejects a second daemon on the same project with a clear error.
fn run_daemon_cmd(path: &std::path::Path) -> anyhow::Result<()> {
    let store = SqliteCodeStore::open(path).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let embedder = build_daemon_embedder();
    icode_engine::run_daemon(path, store, embedder).map_err(|e| anyhow::anyhow!(e.to_string()))
}

/// Build the embedder for the daemon, probing health and reporting the mode on
/// stderr. Returns `Some(Box<dyn Embedder>)` when ready, else `None` (graph-only,
/// no vector refresh). Never fails — same best-effort contract as `serve`.
fn build_daemon_embedder() -> Option<Box<dyn Embedder>> {
    let cfg = EmbedConfig::default();
    let embedder = match icode_embed::build_embedder(&cfg) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("daemon: semantic refresh disabled: {e} (graph kept live)");
            return None;
        }
    };
    if let Err(e) = embedder.health() {
        eprintln!("daemon: semantic refresh disabled: {e} (graph kept live)");
        return None;
    }
    eprintln!("daemon: semantic refresh enabled (model {})", embedder.model_id());
    Some(embedder)
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

// ──────────────────────────── Claude Code lifecycle hooks ────────────────────────────
//
// Claude Code runs an external command on lifecycle events (SessionStart,
// PreCompact, …), reads its stdout, and — when the stdout is a JSON object of the
// shape `{"hookSpecificOutput": {"hookEventName": "<event>", "additionalContext":
// "<text>"}}` — injects `additionalContext` into the model's context for that
// turn. Our `icode hook …` subcommands print exactly that.
//
// Hard contract for EVERY hook: be fast, print a VALID JSON object (or nothing),
// and NEVER fail the process. A down Ollama, a missing central db, a poisoned
// lock — all degrade to a minimal/empty `additionalContext` and exit 0. A hook
// that errored or hung would stall Claude Code on every event.

/// How many recent project memories `session-start` surfaces.
const HOOK_SESSION_PROJECT_N: usize = 6;
/// How many developer-profile notes `session-start` surfaces.
const HOOK_SESSION_PROFILE_N: usize = 5;
/// Per-line content cap in the injected context (keeps the prompt budget small).
const HOOK_LINE_MAX: usize = 200;

/// Resolve the project name for a hook: explicit `--project`, else the basename of
/// `--cwd`, else the basename of the process cwd, else `"general"`. Never empty.
fn hook_project(project: Option<&str>, cwd: Option<&std::path::Path>) -> String {
    if let Some(p) = project {
        let p = p.trim();
        if !p.is_empty() {
            return p.to_string();
        }
    }
    let dir = cwd
        .map(|p| p.to_path_buf())
        .or_else(|| std::env::current_dir().ok());
    dir.as_deref()
        .and_then(|d| d.file_name())
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "general".to_string())
}

/// Full JSON string escaping for arbitrary memory content (which can carry quotes,
/// newlines, and other control chars). Unlike `json_escape` (paths only), this
/// also `\u`-escapes every control char below 0x20, so the hand-built hook JSON
/// stays valid no matter what a memory holds.
fn json_escape_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Render the `hookSpecificOutput` envelope Claude Code reads. `event` is the
/// `hookEventName` (e.g. "SessionStart" / "PreCompact"); `context` is the text to
/// inject (already plain text — escaped here). Built by hand (no serde_json dep in
/// the bin) but kept strictly valid via [`json_escape_str`].
fn hook_output_json(event: &str, context: &str) -> String {
    format!(
        "{{\"hookSpecificOutput\":{{\"hookEventName\":\"{}\",\"additionalContext\":\"{}\"}}}}",
        json_escape_str(event),
        json_escape_str(context),
    )
}

/// One-line summary of a memory for the injected context: `[category] content`,
/// whitespace-collapsed and truncated to `HOOK_LINE_MAX` chars.
fn hook_memory_line(rec: &icode_core::model::MemoryRecord) -> String {
    let category = format!("{:?}", rec.category).to_lowercase();
    let content = collapse_ws(&rec.content);
    let content = truncate_chars(&content, HOOK_LINE_MAX);
    format!("[{category}] {content}")
}

/// Collapse runs of whitespace (incl. newlines) to single spaces and trim — so a
/// multi-line memory becomes one tidy context line.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Char-boundary-safe truncation with an ellipsis (never splits a UTF-8 char).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// `icode hook session-start` — prime the agent with the developer profile +
/// recent project memory. Opens the central memory store read-only (no embedder
/// needed: `session_start` only `list`s + `record_access`es), runs the engine's
/// `session_start`, and prints a `SessionStart` envelope. On any failure (no db,
/// lock poisoned, …) it falls back to a light behavioural trigger. Always exit 0.
fn run_hook_session_start(project: Option<&str>, cwd: Option<&std::path::Path>) {
    let project = hook_project(project, cwd);
    let context = build_session_start_context(&project);
    println!("{}", hook_output_json("SessionStart", &context));
}

/// Behavioural fallback when no memory is available (no db / empty / open failed).
/// A light trigger telling the agent the MCP memory tools exist for this project.
fn hook_session_fallback(project: &str) -> String {
    format!(
        "This project (`{project}`) has iCode cross-session memory. \
         Call `session_start`/`recall`/`search_memory` via the iCode MCP server when you need \
         past context, decisions, or the developer profile."
    )
}

/// Build the SessionStart `additionalContext` text for `project`. Best-effort:
/// returns the behavioural fallback string on any error or empty store.
fn build_session_start_context(project: &str) -> String {
    use icode_engine::SqliteMemoryStore;

    let db_path = central_db_path();
    let store = match SqliteMemoryStore::open_readonly(&db_path) {
        Ok(s) => s,
        Err(_) => return hook_session_fallback(project),
    };

    let session = match icode_engine::session_start(
        &store,
        project,
        HOOK_SESSION_PROJECT_N,
        HOOK_SESSION_PROFILE_N,
    ) {
        Ok(s) => s,
        Err(_) => return hook_session_fallback(project),
    };

    if session.developer_profile.is_empty() && session.project_context.is_empty() {
        return hook_session_fallback(project);
    }

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("# iCode memory — session start for `{project}`"));

    if !session.developer_profile.is_empty() {
        lines.push(String::new());
        lines.push("## Developer profile (cross-project, always honour these):".to_string());
        // L0/important notes first so the critical always-on rules lead.
        let cfg = icode_core::config::RankingConfig::default();
        let mut profile = session.developer_profile.clone();
        profile.sort_by(|a, b| {
            let al = icode_engine::is_l0(a, &cfg);
            let bl = icode_engine::is_l0(b, &cfg);
            bl.cmp(&al).then(
                b.importance
                    .partial_cmp(&a.importance)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });
        for rec in &profile {
            lines.push(format!("- {}", hook_memory_line(rec)));
        }
    }

    if !session.project_context.is_empty() {
        lines.push(String::new());
        lines.push("## Recent project memory:".to_string());
        for rec in &session.project_context {
            lines.push(format!("- {}", hook_memory_line(rec)));
        }
    }

    lines.join("\n")
}

/// `icode hook precompact` — re-inject the L0 "always-on" rules before/after
/// context compaction so critical standing rules (e.g. "answer in Russian") are
/// not lost. Reads L0 notes WITHOUT an embedder (a lexical `list`, no Ollama),
/// from the developer profile AND the project. Prints a `PreCompact` envelope with
/// an empty `additionalContext` when there are no L0 rules. Always exit 0.
fn run_hook_precompact(project: Option<&str>, cwd: Option<&std::path::Path>) {
    let project = hook_project(project, cwd);
    let context = build_precompact_context(&project);
    println!("{}", hook_output_json("PreCompact", &context));
}

/// Build the PreCompact `additionalContext`: the L0 rules from the developer
/// profile + the project, listed for re-injection. Empty string when there are
/// none (or the store can't open). Uses `list` only — NO embedding backend.
fn build_precompact_context(project: &str) -> String {
    use icode_core::ids::DEVELOPER_PROJECT;
    use icode_core::traits::ReadableMemoryStore;
    use icode_engine::SqliteMemoryStore;

    let db_path = central_db_path();
    // Read-only: no embedder, so this works even with Ollama down.
    let store = match SqliteMemoryStore::open_readonly(&db_path) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };

    let cfg = icode_core::config::RankingConfig::default();
    // Pull a generous slice of recent rows from each side; the L0 filter is applied
    // in-process via `is_l0` (importance >= 5, or >= l0_min with an l0/always-on/
    // critical tag). `list` reads SQLite directly — no vectors, no Ollama.
    let mut l0: Vec<icode_core::model::MemoryRecord> = Vec::new();
    if let Ok(profile) = store.list(DEVELOPER_PROJECT, None, 100, false) {
        l0.extend(profile.into_iter().filter(|r| icode_engine::is_l0(r, &cfg)));
    }
    if let Ok(proj) = store.list(project, None, 100, false) {
        l0.extend(proj.into_iter().filter(|r| icode_engine::is_l0(r, &cfg)));
    }

    if l0.is_empty() {
        return String::new();
    }

    // Highest importance first (the most critical rule leads).
    l0.sort_by(|a, b| {
        b.importance
            .partial_cmp(&a.importance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut lines: Vec<String> = Vec::new();
    lines.push(
        "# iCode L0 rules — these ALWAYS-ON rules must survive context compaction:".to_string(),
    );
    for rec in &l0 {
        lines.push(format!("- {}", hook_memory_line(rec)));
    }
    lines.join("\n")
}

/// `icode hook config` — print a ready-to-paste `~/.claude/settings.json` hooks
/// snippet wiring SessionStart / PreCompact to `icode hook …`, plus where to put
/// it. The JSON is valid on its own (pipe-friendly); the guidance goes to stderr
/// so `icode hook config > snippet.json` stays clean.
fn run_hook_config() {
    eprintln!("# Add the `hooks` block below to ~/.claude/settings.json (merge into an");
    eprintln!("# existing \"hooks\" object if you already have one). Claude Code will then");
    eprintln!("# run `icode hook …` on SessionStart and PreCompact to inject memory and");
    eprintln!("# re-assert L0 rules across compaction. (stdout below is the JSON snippet.)");
    eprintln!();
    println!("{}", hook_settings_json());
}

/// The `~/.claude/settings.json` hooks snippet. Hand-built but valid JSON: the
/// SessionStart and PreCompact events each run this binary's hook subcommand,
/// passing the live `--cwd` via Claude Code's `$CLAUDE_PROJECT_DIR` so the project
/// is resolved from the working directory.
fn hook_settings_json() -> String {
    let exe = json_escape(&icode_exe_path());
    // `$CLAUDE_PROJECT_DIR` is expanded by Claude Code's shell when it runs the
    // hook, so the project is the live working directory's basename.
    let cmd_session = format!("{exe} hook session-start --cwd \\\"$CLAUDE_PROJECT_DIR\\\"");
    let cmd_precompact = format!("{exe} hook precompact --cwd \\\"$CLAUDE_PROJECT_DIR\\\"");
    format!(
        "{{\n  \"hooks\": {{\n    \"SessionStart\": [\n      {{\n        \"hooks\": [\n          {{ \"type\": \"command\", \"command\": \"{cmd_session}\" }}\n        ]\n      }}\n    ],\n    \"PreCompact\": [\n      {{\n        \"hooks\": [\n          {{ \"type\": \"command\", \"command\": \"{cmd_precompact}\" }}\n        ]\n      }}\n    ]\n  }}\n}}"
    )
}
