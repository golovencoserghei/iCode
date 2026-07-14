//! `icode` — thin CLI dispatcher. Subcommands delegate to the engine or serve
//! layer; no business logic lives here.
//!
//! Commands:
//!   `icode index <path>`   — index the tree under <path>, print counters.
//!   `icode embed <path>`   — embed any pending chunks (catch-up pass).
//!   `icode serve [path]`   — serve the MCP protocol over stdio (path defaults to the
//!                            launch dir's working-tree root; syncs the index first).
//!   `icode web <path>`     — open the store and serve the local web dashboard (127.0.0.1).
//!   `icode stats <path>`   — print code-graph statistics.
//!   `icode doctor <path>`  — read-only index health check (exit 1 on drift).
//!   `icode setup [path]`   — friendly onboarding: probe Ollama, print MCP config.
//!   `icode mcp-config [p]` — print only the Claude Code MCP registration JSON.

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
    /// Serve the MCP protocol over stdio for a project. With NO <path>, resolves the
    /// working-tree root of the launch dir (`$CLAUDE_PROJECT_DIR` or cwd) — so one
    /// MCP registration serves whichever checkout OR git worktree Claude Code opens.
    /// Runs an incremental index sync on startup so the code graph is current, and
    /// drains embeddings in the background.
    Serve {
        /// Project root to serve (`<path>/.icode/index.db`). Optional: omit to
        /// resolve the enclosing working-tree root from the launch directory.
        path: Option<PathBuf>,
    },
    /// Open the store at <path> and serve the local web dashboard on 127.0.0.1.
    Web {
        /// Project root whose <path>/.icode/index.db is served.
        path: PathBuf,
        /// TCP port to bind on 127.0.0.1 (loopback only). Default 7420.
        #[arg(long, default_value_t = 7420)]
        port: u16,
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
    /// Grounded existence check: does a feature/symbol matching <query> ACTUALLY
    /// exist in the index? Prints a VERDICT (EXISTS/WEAK/ABSENT) + evidence, so a
    /// term that only appears in a string literal is not mistaken for a feature.
    CheckExists {
        /// Project root whose <path>/.icode/index.db is queried.
        path: PathBuf,
        /// The feature/behaviour to check for, in plain words.
        query: String,
        /// Symbol space to check: function|class|route|any (default any).
        #[arg(long)]
        kind: Option<String>,
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
        /// Working directory the project is derived from when `--project` is absent
        /// (enclosing git-repo-root basename; a throwaway dir → `general`).
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
        /// Working directory the project is derived from when `--project` is absent
        /// (enclosing git-repo-root basename; a throwaway dir → `general`).
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Stop hook: safety net for sessions the agent never closes with
    /// `session_end`. Reads the Stop payload from stdin, digests the transcript
    /// and UPSERTS one auto-summary memory per session (tag `sid:<8>`); an
    /// explicit `session_end` in the transcript deletes the draft instead.
    /// Prints nothing; always exits 0.
    Stop {
        /// Project name (else the basename of `--cwd`, else the stdin `cwd`).
        #[arg(long)]
        project: Option<String>,
        /// Working directory the project is derived from when `--project` is absent
        /// (enclosing git-repo-root basename; a throwaway dir → `general`).
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Print a ready-to-paste `~/.claude/settings.json` hooks snippet wiring the
    /// SessionStart / PreCompact / Stop events to `icode hook …`, plus where to
    /// put it.
    Config,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Run the watcher in the FOREGROUND: initial sync, then live-reindex on every
    /// change until Ctrl-C. One daemon per project (flock'd). Use `start` for the
    /// detached form.
    Run {
        /// Project root to watch and keep indexed.
        path: PathBuf,
    },
    /// Start the watcher DETACHED (its own session, survives the terminal closing).
    /// Logs to `<path>/.icode/daemon.log`. Idempotent: if one is already running for
    /// this project, says so and does nothing.
    Start {
        /// Project root to watch and keep indexed.
        path: PathBuf,
    },
    /// Is a daemon watching this project? Prints its PID, or that none is running.
    Status {
        /// Project root to check.
        path: PathBuf,
    },
    /// Stop the daemon watching this project (SIGTERM).
    Stop {
        /// Project root whose daemon to stop.
        path: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Index { path } => run_index(&path),
        Command::Embed { path } => run_embed(&path),
        Command::Serve { path } => run_serve(path),
        Command::Web { path, port } => run_web(path, port),
        Command::Daemon { action } => match action {
            DaemonAction::Run { path } => run_daemon_cmd(&path),
            DaemonAction::Start { path } => run_daemon_start(&path),
            DaemonAction::Status { path } => run_daemon_status(&path),
            DaemonAction::Stop { path } => run_daemon_stop(&path),
        },
        Command::Stats { path } => run_stats(&path),
        Command::Doctor { path } => run_doctor(&path),
        Command::CheckExists { path, query, kind } => {
            run_check_exists(&path, &query, kind.as_deref())
        }
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
            HookAction::Stop { project, cwd } => {
                run_hook_stop(project.as_deref(), cwd.as_deref());
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

/// `icode check-exists <path> "<query>" [--kind …]` — grounded existence oracle.
/// Opens the store, embeds pending chunks best-effort (so the semantic signal is
/// live; a down Ollama just drops it), runs `check_exists`, and prints a
/// human-readable verdict. Read-only.
fn run_check_exists(
    path: &std::path::Path,
    query: &str,
    kind: Option<&str>,
) -> anyhow::Result<()> {
    use icode_core::model::{MatchKind, SymbolKind, Verdict};
    use icode_engine::ExistScope;

    let scope = match kind {
        None | Some("") | Some("any") => ExistScope::Any,
        Some("function") => ExistScope::Function,
        Some("class") => ExistScope::Class,
        Some("route") => ExistScope::Route,
        Some(other) => {
            anyhow::bail!("invalid kind '{other}' (expected function|class|route|any)")
        }
    };

    let store = SqliteCodeStore::open(path).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let embedder = build_serve_embedder();
    if let Some(emb) = embedder.as_deref() {
        if let Err(e) = icode_engine::embed_pending(&store, emb, EmbedConfig::default().batch) {
            eprintln!("icode: embed pass skipped ({e}); semantic signal disabled");
        }
    }

    let v = icode_engine::check_exists(&store, None, query, scope)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    let kind_label = |k: SymbolKind| match k {
        SymbolKind::Function => "fn",
        SymbolKind::Class => "class",
        SymbolKind::FileWindow => "file",
    };
    let mk_label = |m: MatchKind| match m {
        MatchKind::ExactSymbol => "exact_symbol",
        MatchKind::NameToken => "name_token",
        MatchKind::Semantic => "semantic",
        MatchKind::BodyOrString => "body_or_string",
    };
    let verdict = match v.verdict {
        Verdict::Exists => "EXISTS",
        Verdict::Weak => "WEAK",
        Verdict::Absent => "ABSENT",
    };

    println!("verdict:    {verdict}  (confidence {:.2})", v.confidence);
    println!("reason:     {}", v.reason);
    if let Some(b) = &v.best_match {
        let mk = v.match_kind.map(|m| format!("  match_kind={}", mk_label(m))).unwrap_or_default();
        println!(
            "best_match: [{}] {} ({}:{}){mk}",
            kind_label(b.kind),
            b.qualified_name,
            b.path,
            b.line_start
        );
    }
    if !v.evidence.is_empty() {
        println!("evidence:");
        for e in &v.evidence {
            print!(
                "  - [{}] {} ({}:{}) [{}]",
                kind_label(e.hit.kind),
                e.hit.qualified_name,
                e.hit.path,
                e.hit.line_start,
                mk_label(e.match_kind)
            );
            if let Some(s) = &e.hit.snippet {
                print!("  «{s}»");
            }
            println!();
        }
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
    let store = open_store_with_shared_cache(path)?;
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

    // Indexing is now FREE by default — pure CPU, no model, no GPU.
    //
    // The graph, the identifier-aware lexical search and MinHash similarity all come
    // out of the parse; vectors only power `search mode=semantic|hybrid`, which is no
    // longer the default. Embedding every chunk here meant a plain `icode index` woke
    // a local model and chewed the GPU for a capability most calls never use. Opt in
    // with `ICODE_EMBED=1` (or run `icode embed <path>` later).
    let want_embed = std::env::var("ICODE_EMBED")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    if want_embed {
        keep_model_resident_for_bulk();
        // Best-effort: a down/unreachable Ollama must NOT fail `index`.
        embed_pass(&store, /* hard_fail = */ false)?;
    } else {
        println!(
            "vectors skipped (free mode). Graph + lexical + similarity are ready. \
             For `search mode=semantic`: ICODE_EMBED=1 icode index <path>, or icode embed <path>."
        );
    }
    Ok(())
}

/// Stand-alone catch-up embed pass over an already-indexed db. Unlike the pass
/// folded into `index`, a missing embedder here IS a hard error (the user asked
/// to embed explicitly).
fn run_embed(path: &std::path::Path) -> anyhow::Result<()> {
    keep_model_resident_for_bulk();
    let store = open_store_with_shared_cache(path)?;
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

/// Render the Claude Code MCP registration snippet for this binary. Emits `serve`
/// with NO baked path: the server resolves the enclosing working-tree root from its
/// launch cwd at runtime, so ONE registration serves whichever checkout OR git
/// worktree Claude Code opens (no per-worktree config, no stale absolute path).
/// Built by hand (no serde_json dep in the bin); the only interpolated value is the
/// executable path, which we JSON-escape.
fn mcp_config_json() -> String {
    let exe = json_escape(&icode_exe_path());
    format!(
        "{{\n  \"mcpServers\": {{\n    \"icode\": {{\n      \"command\": \"{exe}\",\n      \"args\": [\"serve\"]\n    }}\n  }}\n}}"
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

/// `icode mcp-config` — print ONLY the JSON registration snippet to stdout
/// (pipe-friendly: `icode mcp-config > .mcp.json`). The snippet is path-free and
/// resolves the served project from the launch dir, so it needs no project argument
/// and works unchanged across every checkout and worktree.
fn run_mcp_config(_project_path: Option<&std::path::Path>) -> anyhow::Result<()> {
    println!("{}", mcp_config_json());
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

    // Step 2: MCP registration snippet (path-free — resolves the served project
    // from the launch dir at runtime, so it works across checkouts and worktrees).
    println!("\n2. Connect iCode to Claude Code (MCP server)");
    println!("   Add this to `.mcp.json` in your project root (or your");
    println!("   `~/.claude` settings) so Claude Code launches the iCode server:\n");
    for line in mcp_config_json().lines() {
        println!("   {line}");
    }
    println!("\n   The server serves whichever repo/worktree you open — no path to");
    println!("   edit, and it syncs the code graph on startup so it's always current.");

    // Step 3: next steps. `serve` self-syncs, so indexing is an OPTIONAL pre-warm.
    let hint = snippet_project(project_path);
    let hint = if project_path.is_some() { hint.as_str() } else { "<project>" };
    println!("\n3. Next steps");
    println!("   (optional) icode index  {hint}    # pre-build the graph + embeddings");
    println!("   (optional) icode doctor {hint}    # verify the index is healthy");
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

fn run_serve(path: Option<PathBuf>) -> anyhow::Result<()> {
    // Resolve WHICH project to serve. With no explicit path, anchor at the launch
    // dir's working-tree root — so a git worktree (or any subdir launch) serves its
    // OWN `.icode/index.db`, and one path-free `.mcp.json` works across worktrees.
    let root = resolve_serve_root(path);
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    eprintln!("icode serve: project root = {}", root.display());

    // Several editor windows each hold a long-lived server, so keep one server's
    // footprint small: 16 MiB of SQLite page cache instead of the 64 MiB default
    // tuned for the bulk indexer. Explicit `ICODE_CACHE_KIB` still wins.
    lighten_serve_memory();

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
        let store = Arc::new(open_store_with_shared_cache(&root)?);

        // Incremental graph sync so the served working tree is CURRENT before the
        // first tool call (a fresh worktree indexes its own files here). Best-effort:
        // hash-skips unchanged files, writes only to stderr (stdout is the MCP
        // transport), and any failure degrades to serving the existing graph rather
        // than aborting serve. It is local + parallel (no network), so it does not
        // stall the handshake for a typical tree.
        match icode_engine::index_path(&root, store.as_ref()) {
            Ok(s) => eprintln!(
                "icode serve: graph synced ({} indexed, {} skipped, {} errors)",
                s.files_indexed, s.files_skipped, s.errors
            ),
            Err(e) => eprintln!("icode serve: graph sync skipped ({e}); serving existing index"),
        }

        // The startup sync is a SNAPSHOT: it makes the graph true at 0 s and steadily
        // less true from then on, as the session edits files. A stale graph does not
        // merely omit things — it asserts that code still exists, still calls what it
        // used to, and is still reachable. An agent believes it.
        //
        // So the watcher is started here rather than left as a chore the user has to
        // remember: one detached daemon per project, flock'd so several editor windows
        // cannot race, idle when nothing changes. `ICODE_NO_DAEMON=1` opts out.
        let want_daemon = std::env::var("ICODE_NO_DAEMON")
            .map(|v| !matches!(v.trim(), "1" | "true" | "yes" | "on"))
            .unwrap_or(true);
        if want_daemon {
            match daemon_pid(&root) {
                Some(pid) => eprintln!("icode serve: watcher already running (pid {pid})"),
                None => match spawn_daemon(&root) {
                    Ok(()) => eprintln!("icode serve: watcher started — the graph stays live"),
                    // Never fatal: a served snapshot is still useful.
                    Err(e) => eprintln!("icode serve: could not start the watcher ({e})"),
                },
            }
        }

        // Bulk embedding on startup is OPT-IN: `serve` must stay light. A developer
        // keeps several editor windows open and EACH spawns its own server — auto-
        // draining from every one of them hammers the local model and pins it (~800 MB)
        // in RAM. Default OFF: the graph is lexical + structural immediately, and
        // semantic fills lazily via query-time JIT or an explicit `icode embed`.
        // Set `ICODE_SERVE_EMBED=1` to restore the background drain. When it does run,
        // `embed_pending` locks the store per batch (the network `embed` call is
        // outside the lock), so it never starves the serving reads.
        let drain_enabled = std::env::var("ICODE_SERVE_EMBED")
            .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        if !drain_enabled {
            eprintln!(
                "icode serve: background embedding off (default); semantic fills via \
                 JIT / `icode embed`. Set ICODE_SERVE_EMBED=1 to drain on startup."
            );
        } else if let Some(emb) = embedder.clone() {
            let store_bg = store.clone();
            let batch = EmbedConfig::default().batch;
            tokio::task::spawn_blocking(move || {
                match icode_engine::embed_pending(store_bg.as_ref(), emb.as_ref(), batch) {
                    Ok(s) => eprintln!(
                        "icode serve: embeddings drained ({} embedded, {} batches)",
                        s.embedded, s.batches
                    ),
                    Err(e) => eprintln!("icode serve: embedding drain skipped ({e})"),
                }
            });
        }

        // Pass the project root so the `doctor` MCP tool can reconcile the index
        // against the live source tree (it walks `root` like the indexer does).
        icode_serve::serve_stdio(store, root, embedder, memory).await
    })
}

/// `icode web <path> [--port N]` — open the store and serve the local web
/// dashboard on `127.0.0.1:<port>` (default 7420). Mirrors `serve`: builds the
/// embedder + cross-session memory best-effort BEFORE the async runtime (a down
/// Ollama degrades code search to lexical and disables the memory views, never
/// fatal). Binding is loopback-only — the locality invariant is enforced in
/// `icode-serve::web::serve`, not configurable here. The startup URL is printed
/// to stderr by the serve layer.
fn run_web(path: PathBuf, port: u16) -> anyhow::Result<()> {
    lighten_serve_memory();
    let embedder = build_serve_embedder();
    let memory = build_serve_memory(embedder.clone());

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let store = open_store_with_shared_cache(&path)?;
        icode_serve::serve_web(Arc::new(store), embedder, memory, port).await
    })
}

/// `icode daemon run <path>` — foreground live-indexing daemon. Opens the store and
/// watches the tree until Ctrl-C, keeping the GRAPH in lock-step with the source.
/// It does NOT embed: vectors are refreshed on demand by the semantic tools (or
/// `icode embed`), so a `git checkout` storm never churns the embedder. The
/// single-writer PID-lock inside `run_daemon` rejects a second daemon on the same
/// project with a clear error.
/// The PID of the daemon watching `root`, if one is alive.
///
/// The lock file carries the holder's PID; the flock itself is what guarantees
/// single-writer, so a stale PID in the file after a crash is harmless — we probe
/// liveness with `kill(pid, 0)` rather than trusting the file.
fn daemon_pid(root: &std::path::Path) -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(root.join(".icode/daemon.lock"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    // Signal 0 = existence/permission probe, sends nothing.
    if unsafe { libc::kill(pid, 0) } == 0 {
        Some(pid)
    } else {
        None
    }
}

/// `icode daemon start <path>` — spawn the watcher DETACHED and return immediately.
///
/// Without this the daemon was foreground-only, so in practice nobody ran one and the
/// graph went stale between indexes — which is worse than it sounds, because a stale
/// graph does not merely omit things, it ASSERTS things that are no longer true.
///
/// Detachment is `setsid` in the child: a new session with no controlling terminal, so
/// closing the shell does not SIGHUP it. Output goes to `<path>/.icode/daemon.log`.
/// Idempotent — the flock in the child decides who wins, so a double `start` is safe.
fn run_daemon_start(path: &std::path::Path) -> anyhow::Result<()> {
    let root = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(pid) = daemon_pid(&root) {
        println!("daemon already running for {} (pid {pid})", root.display());
        return Ok(());
    }
    spawn_daemon(&root)?;

    // The child writes its PID into the lock file only once it has taken the flock, so
    // reading it immediately races and reports "not running" for a daemon that is in
    // fact starting. Wait briefly for it to appear rather than lie.
    for _ in 0..50 {
        if let Some(pid) = daemon_pid(&root) {
            println!(
                "daemon started for {} (pid {pid}) — logging to {}",
                root.display(),
                root.join(".icode/daemon.log").display()
            );
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    println!(
        "daemon spawned for {} but it has not taken the lock yet — check {}",
        root.display(),
        root.join(".icode/daemon.log").display()
    );
    Ok(())
}

/// Spawn `icode daemon run <root>` in its own session, logging to `.icode/daemon.log`.
/// Best-effort and non-blocking: the caller does not wait on the child.
fn spawn_daemon(root: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let dir = root.join(".icode");
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))?;
    let log_err = log.try_clone()?;

    let mut cmd = std::process::Command::new(icode_exe_path());
    cmd.arg("daemon")
        .arg("run")
        .arg(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));
    unsafe {
        // SAFETY: `setsid` is async-signal-safe and this runs in the forked child
        // before exec. It detaches the child from our controlling terminal so the
        // watcher outlives the shell that started it.
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

/// `icode daemon status <path>`.
fn run_daemon_status(path: &std::path::Path) -> anyhow::Result<()> {
    let root = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match daemon_pid(&root) {
        Some(pid) => println!("daemon RUNNING for {} (pid {pid})", root.display()),
        None => println!("no daemon for {} — the graph is only as fresh as the last index", root.display()),
    }
    Ok(())
}

/// `icode daemon stop <path>` — SIGTERM the watcher. The flock is released by the
/// kernel when it exits, so no lock file needs cleaning up.
fn run_daemon_stop(path: &std::path::Path) -> anyhow::Result<()> {
    let root = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match daemon_pid(&root) {
        Some(pid) => {
            if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
                println!("daemon stopped for {} (pid {pid})", root.display());
            } else {
                println!("could not signal pid {pid}: {}", std::io::Error::last_os_error());
            }
        }
        None => println!("no daemon running for {}", root.display()),
    }
    Ok(())
}

fn run_daemon_cmd(path: &std::path::Path) -> anyhow::Result<()> {
    let store = SqliteCodeStore::open(path).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    icode_engine::run_daemon(path, store).map_err(|e| anyhow::anyhow!(e.to_string()))
}

/// Central memory db path (`~/.icode/icode.db`). `~` is expanded against `$HOME`
/// here so the store's `open` gets an absolute path (its own tilde-expansion is a
/// belt-and-braces fallback). Falls back to the literal `~/...` if `$HOME` is
/// unset (the store will then try to expand it, or fail cleanly).
fn central_db_path() -> String {
    // Test/ops escape hatch: point every memory consumer (serve, hooks) at an
    // alternate central db without touching the real one.
    if let Ok(p) = std::env::var("ICODE_DB_PATH") {
        if !p.trim().is_empty() {
            return p;
        }
    }
    match std::env::var("HOME") {
        Ok(home) => format!("{home}/.icode/icode.db"),
        Err(_) => "~/.icode/icode.db".to_string(),
    }
}

/// Open a code store for an EMBEDDING path (index / embed / serve / web) with the
/// SHARED central embed cache attached: an identical chunk is embedded ONCE per
/// machine and reused across every project, git worktree, and re-clone. Read-only
/// paths (stats/doctor/check_exists) and the graph-only daemon skip it and open the
/// store plainly.
fn open_store_with_shared_cache(path: &std::path::Path) -> anyhow::Result<SqliteCodeStore> {
    SqliteCodeStore::open(path)
        .and_then(|s| s.with_embed_cache_db(&central_db_path()))
        .map_err(|e| anyhow::anyhow!(e.to_string()))
}

/// Trim a LONG-LIVED server's footprint. A developer keeps several editor windows
/// open and each spawns its own `serve`/`web` process, so the 64 MiB SQLite page
/// cache tuned for the bulk indexer multiplies across them. Drop it to 16 MiB unless
/// the user set `ICODE_CACHE_KIB` explicitly. Must run BEFORE the store is opened —
/// the pragma is applied at connection time.
fn lighten_serve_memory() {
    if std::env::var_os("ICODE_CACHE_KIB").is_none() {
        std::env::set_var("ICODE_CACHE_KIB", "16384");
    }
}

/// Bulk embed paths (`icode index` / `icode embed`) keep the model resident BETWEEN
/// batches; otherwise the light interactive default (`keep_alive=0s`) would unload
/// and reload it once per batch. Interactive paths (serve/web) keep the light
/// default so an occasional embed never pins the model in RAM. Explicit env wins.
fn keep_model_resident_for_bulk() {
    if std::env::var_os("ICODE_OLLAMA_KEEP_ALIVE").is_none() {
        std::env::set_var("ICODE_OLLAMA_KEEP_ALIVE", "5m");
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

/// Basename of a path, `None` if empty/unnameable.
fn basename_str(d: &std::path::Path) -> Option<String> {
    d.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Given the `.git` FILE of a linked worktree, resolve the MAIN repository's
/// working-tree root, so every worktree of a repo maps to the SAME project name
/// (no per-worktree memory bucket). A linked worktree's `.git` is a file
/// `gitdir: <repo>/.git/worktrees/<name>`; the shared git dir is that dir's
/// `commondir` pointer (usually `../..` → `<repo>/.git`), whose parent is the main
/// working tree. Returns `None` when this is not a resolvable worktree pointer
/// (e.g. a submodule, whose `.git` also is a file but has no `worktrees/` segment)
/// — the caller then falls back to the worktree dir's own basename.
fn worktree_main_root(dot_git_file: &std::path::Path, worktree_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string(dot_git_file).ok()?;
    let gitdir = text.lines().find_map(|l| l.trim().strip_prefix("gitdir:"))?.trim();
    let gitdir_abs = if std::path::Path::new(gitdir).is_absolute() {
        std::path::PathBuf::from(gitdir)
    } else {
        worktree_dir.join(gitdir)
    };
    // Only remap TRUE worktrees (…/.git/worktrees/<name>); leave submodules etc.
    if !gitdir_abs.components().any(|c| c.as_os_str() == "worktrees") {
        return None;
    }
    // Shared git dir: the `commondir` pointer if present, else strip `worktrees/<name>`.
    let common = match std::fs::read_to_string(gitdir_abs.join("commondir")) {
        Ok(rel) => {
            let rel = rel.trim();
            if std::path::Path::new(rel).is_absolute() {
                std::path::PathBuf::from(rel)
            } else {
                gitdir_abs.join(rel)
            }
        }
        Err(_) => gitdir_abs.parent()?.parent()?.to_path_buf(),
    };
    // Canonicalize to fold any `..` from the relative `commondir`; the shared dir is
    // `<repo>/.git`, so its parent is the main working-tree root.
    let common = std::fs::canonicalize(&common).unwrap_or(common);
    if common.file_name().map(|n| n == ".git").unwrap_or(false) {
        common.parent().map(|p| p.to_path_buf())
    } else {
        Some(common)
    }
}

/// Basename of the enclosing GIT REPO root for `dir`: walk up until a `.git` entry
/// is found. A `.git` DIRECTORY marks a normal checkout (this dir is the root). A
/// `.git` FILE marks a linked worktree (or submodule): for a worktree we remap to
/// the MAIN repo root via [`worktree_main_root`] so all worktrees share one project
/// name; anything else falls back to this dir's own basename. `None` if `dir` is
/// not inside a git repo.
///
/// This is the strong, subdirectory- and casing-stable "which project" signal
/// (`…/iCode/crates/foo` → `iCode`, `Onyx`/`onyx` can't split), and it treats a
/// worktree as the SAME project as its main checkout instead of a fresh bucket.
fn git_root_basename(dir: &std::path::Path) -> Option<String> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let dot_git = d.join(".git");
        match std::fs::metadata(&dot_git) {
            Ok(m) if m.is_dir() => return basename_str(d),
            Ok(m) if m.is_file() => {
                return worktree_main_root(&dot_git, d)
                    .and_then(|root| basename_str(&root))
                    .or_else(|| basename_str(d));
            }
            _ => {}
        }
        cur = d.parent();
    }
    None
}

/// True if `dir` is a throwaway / container location that must NOT mint its own
/// memory bucket: the home directory itself, or a temp/scratch dir. Running a hook
/// from such a cwd (with no `--project` and no enclosing repo) is what produced the
/// junk projects `worker` (== `$HOME`), `tmp`, and `scratchpad` that polluted
/// cross-project listings — those now collapse to `general` instead. Keyed on the
/// basename (and an exact `$HOME` match), NOT a path prefix, so a real project that
/// merely lives under a system temp root (e.g. a test's tempdir) is unaffected.
fn is_throwaway_dir(dir: &std::path::Path) -> bool {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() && dir == std::path::Path::new(&home) {
            return true;
        }
    }
    matches!(
        dir.file_name().and_then(|n| n.to_str()),
        Some("tmp") | Some("temp") | Some("scratchpad") | Some(".cache")
    )
}

/// Resolve the project name for a hook. Precedence:
///   1. explicit `--project` (trimmed, if non-empty);
///   2. the basename of the enclosing GIT REPO root of the cwd — the canonical,
///      subdirectory- and casing-stable "which project" signal;
///   3. the cwd basename, UNLESS the cwd is a throwaway/container location
///      (`$HOME`, a `tmp`/`scratchpad`/… dir), which collapses to `general`;
///   4. `general`.
/// Never empty. Preferring the git root (2) and rejecting throwaway dirs (3) is what
/// keeps one project in one bucket instead of fragmenting into `crates`, `Onyx` vs
/// `onyx`, `worker`, `tmp`, or `scratchpad`.
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
    let Some(dir) = dir else {
        return "general".to_string();
    };
    if let Some(root) = git_root_basename(&dir) {
        return root;
    }
    if is_throwaway_dir(&dir) {
        return "general".to_string();
    }
    dir.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "general".to_string())
}

/// The working-tree root for `dir`: the nearest ancestor (incl. `dir`) that holds a
/// `.git` entry — a normal repo root OR a linked worktree's OWN root. Unlike
/// [`git_root_basename`] (which remaps a worktree to its MAIN checkout so cross-tree
/// MEMORY stays in one bucket), this keeps the worktree's own root: the per-tree
/// CODE index must reflect THIS working tree's files/branch, so a worktree indexes
/// into its own `.icode/index.db`. `None` when `dir` is not inside a git repo.
fn find_repo_top(dir: &std::path::Path) -> Option<PathBuf> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    None
}

/// Resolve the project root for `serve`. Precedence:
///   1. an explicit `<path>` argument;
///   2. else the working-tree root of the launch dir — `$CLAUDE_PROJECT_DIR` if
///      Claude Code set it, otherwise the process cwd — via [`find_repo_top`], so a
///      launch from any SUBDIRECTORY (or a git worktree) anchors the single
///      `.icode/index.db` at the tree root, never in a subdir;
///   3. else that launch dir as-is (not inside a repo).
/// This is what lets ONE path-free `.mcp.json` serve whichever checkout or worktree
/// Claude Code opens; the memory side resolves to the shared canonical name, so the
/// two together Just Work across worktrees.
fn resolve_serve_root(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    let launch = std::env::var("CLAUDE_PROJECT_DIR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    find_repo_top(&launch).unwrap_or(launch)
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
         past context, decisions, or the developer profile. \
         Use `project=\"{project}\"` for every icode call — it is the canonical name for this \
         repo; do not derive a different one from the working directory."
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

    // Pin the canonical project name so memory stays in ONE bucket: the agent picks
    // the `project` argument for every icode call, and deriving it ad-hoc from the
    // path is exactly what fragments a repo across `crates`, `Onyx`/`onyx`, etc.
    lines.push(String::new());
    lines.push(format!(
        "> Use `project=\"{project}\"` for EVERY icode memory call this session \
         (session_start / recall / add_memory / …). It is the canonical name for \
         this repo — do not derive a different one from the working directory."
    ));

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

/// `icode hook stop` — the `session_end` safety net. Claude Code fires Stop at
/// the end of EVERY assistant turn, piping `{session_id, transcript_path, cwd}`
/// on stdin. The hook digests the transcript and keeps ONE auto-summary memory
/// per session (upsert by the `sid:<8>` tag), so a crashed or abandoned session
/// still leaves its knowledge behind; when the transcript shows an explicit
/// `session_end` call, the draft is deleted instead. Prints nothing and always
/// exits 0 — any failure (no stdin, unreadable transcript, Ollama down, …) is a
/// silent no-op, never a stall.
fn run_hook_stop(project: Option<&str>, cwd: Option<&std::path::Path>) {
    let _ = try_hook_stop(project, cwd);
}

/// The fallible body of `hook stop`; `None` short-circuits to the silent no-op.
fn try_hook_stop(project: Option<&str>, cwd: Option<&std::path::Path>) -> Option<()> {
    use icode_core::traits::Embedder;
    use std::io::Read;

    // Bounded stdin read: the Stop payload is a small JSON object.
    let mut raw = String::new();
    std::io::stdin()
        .take(1 << 20)
        .read_to_string(&mut raw)
        .ok()?;
    let input = icode_engine::parse_stop_input(&raw)?;

    // Project: explicit --project / --cwd win; else the stdin `cwd` basename.
    let cwd = cwd
        .map(std::path::Path::to_path_buf)
        .or_else(|| input.cwd.as_ref().map(PathBuf::from));
    let project = hook_project(project, cwd.as_deref());

    let transcript = std::fs::read_to_string(&input.transcript_path).ok()?;
    let digest = icode_engine::digest_transcript(&transcript);

    if digest.session_end_called {
        // The agent closed the session properly — drop the draft. `delete`
        // never embeds, so the readonly (NoEmbedder) store suffices.
        let store = icode_engine::SqliteMemoryStore::open_readonly(&central_db_path()).ok()?;
        icode_engine::remove_auto_summary(&store, &project, &input.session_id).ok()?;
        return Some(());
    }

    let content = icode_engine::auto_summary_content(&digest)?;

    // add/update embed the content, so this path needs a live embedder — built
    // quietly (no stderr chatter on every turn), degrading to a no-op when down.
    let embedder = icode_embed::build_embedder(&EmbedConfig::default()).ok()?;
    embedder.health().ok()?;
    let embedder: Arc<dyn Embedder> = Arc::from(embedder);
    let base = icode_engine::SqliteMemoryStore::open(&central_db_path(), embedder).ok()?;

    // Same WAL audit decorator as `serve`, so auto-summaries are journaled too.
    let wal_path = match std::env::var("HOME") {
        Ok(home) => format!("{home}/.icode/wal.jsonl"),
        Err(_) => "~/.icode/wal.jsonl".to_string(),
    };
    let store = icode_engine::WalStore::new(Arc::new(base), wal_path);
    icode_engine::upsert_auto_summary(&store, &project, &input.session_id, &content).ok()
}

/// `icode hook config` — print a ready-to-paste `~/.claude/settings.json` hooks
/// snippet wiring SessionStart / PreCompact to `icode hook …`, plus where to put
/// it. The JSON is valid on its own (pipe-friendly); the guidance goes to stderr
/// so `icode hook config > snippet.json` stays clean.
fn run_hook_config() {
    eprintln!("# Add the `hooks` block below to ~/.claude/settings.json (merge into an");
    eprintln!("# existing \"hooks\" object if you already have one). Claude Code will then");
    eprintln!("# run `icode hook …` on SessionStart / PreCompact / Stop to inject memory,");
    eprintln!("# re-assert L0 rules across compaction, and auto-save a session summary");
    eprintln!("# when the agent forgets `session_end`. (stdout below is the JSON snippet.)");
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
    let cmd_stop = format!("{exe} hook stop --cwd \\\"$CLAUDE_PROJECT_DIR\\\"");
    format!(
        "{{\n  \"hooks\": {{\n    \"SessionStart\": [\n      {{\n        \"hooks\": [\n          {{ \"type\": \"command\", \"command\": \"{cmd_session}\" }}\n        ]\n      }}\n    ],\n    \"PreCompact\": [\n      {{\n        \"hooks\": [\n          {{ \"type\": \"command\", \"command\": \"{cmd_precompact}\" }}\n        ]\n      }}\n    ],\n    \"Stop\": [\n      {{\n        \"hooks\": [\n          {{ \"type\": \"command\", \"command\": \"{cmd_stop}\" }}\n        ]\n      }}\n    ]\n  }}\n}}"
    )
}

#[cfg(test)]
mod derivation_tests {
    use super::{find_repo_top, git_root_basename, hook_project, is_throwaway_dir, resolve_serve_root};
    use std::path::Path;

    /// A tempdir whose `root/` holds a `.git` dir and a nested `a/b` subtree.
    fn repo_with_subdir() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let td = tempfile::tempdir().expect("tempdir");
        let root = td.path().join("root");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        (td, root, deep)
    }

    #[test]
    fn git_root_basename_is_stable_across_subdirs() {
        let (_td, root, deep) = repo_with_subdir();
        assert_eq!(git_root_basename(&deep).as_deref(), Some("root"));
        assert_eq!(git_root_basename(&root).as_deref(), Some("root"));
    }

    #[test]
    fn git_root_basename_none_outside_repo() {
        let td = tempfile::tempdir().expect("tempdir");
        let plain = td.path().join("nogit").join("x");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(git_root_basename(&plain), None);
    }

    #[test]
    fn throwaway_dirs_by_basename() {
        assert!(is_throwaway_dir(Path::new("/whatever/scratchpad")));
        assert!(is_throwaway_dir(Path::new("/whatever/tmp")));
        assert!(is_throwaway_dir(Path::new("/whatever/temp")));
        assert!(!is_throwaway_dir(Path::new("/whatever/myproj")));
    }

    #[test]
    fn hook_project_prefers_git_root_then_general_for_throwaway() {
        let (_td, _root, deep) = repo_with_subdir();
        // Explicit --project always wins.
        assert_eq!(hook_project(Some("Explicit"), Some(&deep)), "Explicit");
        // A subdir of a repo resolves to the repo-root basename (canonical).
        assert_eq!(hook_project(None, Some(&deep)), "root");

        // A non-repo throwaway dir collapses to "general" (no junk bucket).
        let td = tempfile::tempdir().expect("tempdir");
        let scratch = td.path().join("scratchpad");
        std::fs::create_dir_all(&scratch).unwrap();
        assert_eq!(hook_project(None, Some(&scratch)), "general");

        // A non-repo, non-throwaway dir still resolves to its basename.
        let proj = td.path().join("myproj");
        std::fs::create_dir_all(&proj).unwrap();
        assert_eq!(hook_project(None, Some(&proj)), "myproj");
    }

    #[test]
    fn git_root_basename_maps_worktree_to_main_repo() {
        let td = tempfile::tempdir().expect("tempdir");
        // Main checkout with the worktree admin dir under its real `.git`.
        let main = td.path().join("main");
        let wt_admin = main.join(".git").join("worktrees").join("feat");
        std::fs::create_dir_all(&wt_admin).unwrap();
        std::fs::write(wt_admin.join("commondir"), "../..\n").unwrap();
        // Linked worktree: its `.git` is a FILE pointing at the admin dir.
        let wt = td.path().join("feat-wt");
        let deep = wt.join("crates").join("x");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_admin.display())).unwrap();

        // From anywhere inside the worktree, the project is the MAIN repo name —
        // NOT the worktree dir's basename ("feat-wt"). One repo, one memory bucket.
        assert_eq!(git_root_basename(&deep).as_deref(), Some("main"));
        assert_eq!(hook_project(None, Some(&deep)), "main");
    }

    #[test]
    fn find_repo_top_is_worktree_own_root_not_main() {
        let td = tempfile::tempdir().expect("tempdir");
        // Main checkout: `.git` dir; a subdir resolves UP to the repo root.
        let main = td.path().join("main");
        std::fs::create_dir_all(main.join(".git")).unwrap();
        let sub = main.join("crates").join("x");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(find_repo_top(&sub).as_deref(), Some(main.as_path()));

        // Worktree: its OWN root (NOT remapped to main) — the code index is per-tree.
        let wt_admin = main.join(".git").join("worktrees").join("feat");
        std::fs::create_dir_all(&wt_admin).unwrap();
        let wt = td.path().join("feat-wt");
        let wsub = wt.join("crates");
        std::fs::create_dir_all(&wsub).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_admin.display())).unwrap();
        assert_eq!(find_repo_top(&wsub).as_deref(), Some(wt.as_path()));

        // Outside any repo → None.
        let bare = tempfile::tempdir().expect("tempdir");
        assert_eq!(find_repo_top(bare.path()), None);
    }

    #[test]
    fn resolve_serve_root_prefers_explicit_path() {
        let p = std::path::PathBuf::from("/explicit/project/root");
        assert_eq!(resolve_serve_root(Some(p.clone())), p);
    }
}
