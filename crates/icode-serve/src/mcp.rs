//! rmcp MCP server (stdio). Exposes the full group-A read surface of the
//! code-graph as MCP tools (search / fetch / call-graph / analysis / files).
//!
//! Rule (frozen): `#[tool]` functions contain NO logic — they deserialize args,
//! call into `icode-engine` (the heavy sync call wrapped in `spawn_blocking`),
//! and project the result to JSON. rmcp API drift stays isolated here.

use std::sync::Arc;

use icode_core::ids::{MemoryId, DEVELOPER_PROJECT};
use icode_core::model::{Category, CodeQuery, Language, NewMemory, SearchMode, SymbolKind};
use icode_core::traits::{CodeReadStore, Embedder, MemoryStore};
use icode_engine::SqliteCodeStore;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};

/// Error string returned by the semantic tools when no embedder was wired up
/// (Ollama unreachable at startup). The server stays useful in lexical-only mode.
const NO_EMBEDDER_ERR: &str =
    "semantic search unavailable: no embedder (is ollama running?)";

/// Error string returned by EVERY memory tool when the cross-session memory store
/// could not be built (no embedder → no central memory db). The code-graph tools
/// stay fully usable; only memory degrades.
const NO_MEMORY_ERR: &str = "memory unavailable: no embedder (is ollama running?)";

/// Default project memories `session_start` returns when the caller omits the arg.
const DEFAULT_N_PROJECT_MEMORIES: usize = 8;
/// Default profile notes `session_start` returns when the caller omits the arg.
const DEFAULT_N_PROFILE_NOTES: usize = 5;
/// Batch size for the on-demand embed pass run before a semantic query.
const JIT_EMBED_BATCH: usize = 32;

#[derive(Clone)]
pub struct CodeMcpServer {
    store: Arc<SqliteCodeStore>,
    /// Project root this store was indexed from. Carried so the read-only
    /// `doctor` tool can reconcile the index against the live source tree on
    /// disk (it walks `root` exactly as the indexer does). Empty/`.` is harmless
    /// for stores that never call doctor (e.g. in-memory test fixtures).
    root: std::path::PathBuf,
    /// Optional embedder for the semantic / hybrid tools. `None` when the binary
    /// could not reach Ollama at startup: semantic tools then return a clear
    /// error and `find_existing` / `get_symbol_context` degrade to lexical-only.
    embedder: Option<Arc<dyn Embedder>>,
    /// Optional cross-session memory store. `None` when no embedder was available
    /// at startup (the central memory db OWNS an embedder for write-time vectors),
    /// in which case every memory tool returns [`NO_MEMORY_ERR`]. The bin builds
    /// the concrete `SqliteMemoryStore`/`WalStore` and passes it as `Arc<dyn
    /// MemoryStore>` — `icode-serve` stays decoupled from `icode-embed`.
    memory: Option<Arc<dyn MemoryStore>>,
    // Consumed by the `#[tool_handler]` macro expansion (routes tool calls);
    // dead-code analysis can't see that path, so the read is suppressed here.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

// ──────────────────────────── arg structs ────────────────────────────

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct RepoMapArgs {
    /// How many top entries to include per section (modules/complex/hotspots). Default 30.
    pub top: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SymbolContextArgs {
    /// Symbol name (bare function/class name) to gather context for.
    pub name: String,
    /// Optional file path to disambiguate a symbol that exists in several files.
    pub file_hint: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SearchFunctionArgs {
    /// Lexical query matched against function name / qualified name.
    pub query: String,
    /// Max hits to return (default 10).
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SearchClassArgs {
    /// Lexical query matched against class name / qualified name.
    pub query: String,
    /// Max hits to return (default 10).
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct FindExistingArgs {
    /// Free-form description of the behaviour you want to (re)implement.
    pub query: String,
    /// Restrict to one symbol kind: "function" or "class" (default: both).
    pub kind: Option<String>,
    /// Max hits to return (default 10).
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SemanticSearchArgs {
    /// Natural-language description of the behaviour/code you want to find.
    pub query: String,
    /// Max hits to return (default 10).
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct FindSimilarArgs {
    /// Qualified name of an existing symbol to find semantic neighbours of
    /// (e.g. "TreeWalker::walk" or "parse_rust_source").
    pub qualified_name: String,
    /// Max hits to return (default 10).
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetFunctionArgs {
    /// Bare function name to fetch.
    pub name: String,
    /// Optional language filter: "php"|"python"|"rust"|"go"|"java"|"javascript"|"typescript"|"html".
    pub language: Option<String>,
    /// Include the full function body (default false).
    pub with_body: Option<bool>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetClassArgs {
    /// Bare class name to fetch.
    pub name: String,
    /// Optional language filter: "php"|"python"|"rust"|"go"|"java"|"javascript"|"typescript"|"html".
    pub language: Option<String>,
    /// Include the full class body (default false).
    pub with_body: Option<bool>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct FileOutlineArgs {
    /// File path whose functions and classes should be listed (ordered by line).
    pub path: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct CallersArgs {
    /// Symbol name whose direct callers to list.
    pub name: String,
    /// Max rows to return (default 50).
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct CalleesArgs {
    /// Symbol name whose direct callees to list.
    pub name: String,
    /// Max rows to return (default 50).
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct CallChainArgs {
    /// Source symbol name.
    pub from: String,
    /// Target symbol name.
    pub to: String,
    /// Max edges to traverse in the call graph (default 12).
    pub max_depth: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DependenciesArgs {
    /// File path whose (transitive) module dependencies to resolve.
    pub path: String,
    /// Transitive hops to follow (default 3).
    pub depth: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ImpactArgs {
    /// File path whose reverse dependencies (importers) to resolve.
    pub path: String,
    /// Transitive hops to follow (default 3).
    pub depth: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ImplementationsArgs {
    /// Base class / interface name whose implementors to list.
    pub name: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct AnalysisArgs {
    /// Optional language filter (e.g. "rust", "php", "python").
    pub language: Option<String>,
    /// Max hits to return (default 20).
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct FindRoutesArgs {
    /// Filter by HTTP method (exact, case-insensitive).
    pub method: Option<String>,
    /// Filter by route path substring.
    pub path: Option<String>,
    /// Filter by handler class/method substring.
    pub handler: Option<String>,
    /// Max rows to return (default 50).
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GrepCodeArgs {
    /// Regex pattern matched against stored symbol bodies.
    pub pattern: String,
    /// Optional language filter (e.g. "rust", "php").
    pub language: Option<String>,
    /// Max hits to return (default 50).
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListFilesArgs {
    /// Optional path substring filter.
    pub pattern: Option<String>,
    /// Optional language filter (e.g. "rust", "php").
    pub language: Option<String>,
    /// Max rows to return (default 50).
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct StatFileArgs {
    /// File path to stat (returns the indexed FileRecord or null).
    pub path: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ReadFileArgs {
    /// File path to read from disk.
    pub path: String,
    /// First line (1-based inclusive); default = start of file.
    pub start: Option<u32>,
    /// Last line (1-based inclusive); default = end of file.
    pub end: Option<u32>,
}

// ──────────────────────────── memory arg structs ────────────────────────────

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SessionStartArgs {
    /// Project to resume (its recent context is loaded). Required.
    pub project: String,
    /// How many recent project memories to load (default 8).
    pub n_project_memories: Option<usize>,
    /// How many developer-profile notes to load (default 5).
    pub n_profile_notes: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SessionEndArgs {
    /// Project being wrapped up.
    pub project: String,
    /// One-paragraph narrative of what was accomplished (saved as a Progress memory).
    pub summary: String,
    /// Finished items (each saved as Progress; each auto-resolves the bug/todo it closes).
    pub completed: Option<Vec<String>>,
    /// Decisions made (each saved as a Decision memory).
    pub decisions: Option<Vec<String>>,
    /// Open next-steps (each saved as a Todo memory).
    pub todos: Option<Vec<String>>,
    /// Optional session id to stamp a session touch (last_session_at).
    pub session_id: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct AddMemoryArgs {
    /// Project the memory belongs to.
    pub project: String,
    /// The self-contained fact to remember.
    pub content: String,
    /// Category: decision|progress|context|bug|todo|code|general (default general).
    pub category: Option<String>,
    /// Free-form tags for later filtering.
    pub tags: Option<Vec<String>>,
    /// Importance 0..5 (L0 floor at 5); default 0.
    pub importance: Option<f32>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SearchMemoryArgs {
    /// Project to search within.
    pub project: String,
    /// Natural-language query (dense + lexical RRF fusion).
    pub query: String,
    /// Max hits to return (default 5).
    pub n_results: Option<usize>,
    /// Optional category filter (decision|progress|context|bug|todo|code|general).
    pub category: Option<String>,
    /// Include resolved memories too (default false).
    pub include_resolved: Option<bool>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SearchAllArgs {
    /// Natural-language query across ALL projects (reserved projects excluded).
    pub query: String,
    /// Max hits to return (default 5).
    pub n_results: Option<usize>,
    /// Optional category filter (decision|progress|context|bug|todo|code|general).
    pub category: Option<String>,
    /// Include resolved memories too (default false).
    pub include_resolved: Option<bool>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListMemoriesArgs {
    /// Project whose memories to list (newest first).
    pub project: String,
    /// Optional category filter (decision|progress|context|bug|todo|code|general).
    pub category: Option<String>,
    /// Max rows to return (default 20).
    pub limit: Option<usize>,
    /// Include resolved memories too (default false).
    pub include_resolved: Option<bool>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct UpdateMemoryArgs {
    /// Id of the memory to update (`mem_<ulid>__<project>`).
    pub memory_id: String,
    /// New content (omit to leave unchanged; a real change re-embeds).
    pub content: Option<String>,
    /// Replacement tags (omit to leave unchanged).
    pub tags: Option<Vec<String>>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DeleteMemoryArgs {
    /// Id of the memory to delete (`mem_<ulid>__<project>`).
    pub memory_id: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ResolveMemoryArgs {
    /// Id of the bug/todo to mark resolved (`mem_<ulid>__<project>`).
    pub memory_id: String,
    /// Why it is resolved (recorded with the resolution).
    pub reason: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct AddDeveloperNoteArgs {
    /// The cross-project preference / rule about how this developer works.
    pub content: String,
    /// Category (default general).
    pub category: Option<String>,
    /// Free-form tags.
    pub tags: Option<Vec<String>>,
    /// Importance 0..5 (default 0).
    pub importance: Option<f32>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GetDeveloperProfileArgs {
    /// Optional query — when given, semantically searches the profile; else lists it.
    pub query: Option<String>,
    /// Max notes to return (default 5).
    pub n_results: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct RecallArgs {
    /// Project whose cross-session memory to consult (the code section is the
    /// served project regardless).
    pub project: String,
    /// Task / natural-language query to recall code AND memory for.
    pub query: String,
    /// Max hits per section (default 8).
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct CodeToMemoryArgs {
    /// Project whose memory to search.
    pub project: String,
    /// Symbol name (function/class) to find related memory for.
    pub symbol: String,
    /// Max hits to return (default 8).
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct WhyThisExistsArgs {
    /// Project whose decision memory to search.
    pub project: String,
    /// Symbol name (function/class) to find the rationale for.
    pub symbol: String,
    /// Max hits to return (default 8).
    pub limit: Option<usize>,
}

// ──────────────────────────── tools ────────────────────────────

#[tool_router]
impl CodeMcpServer {
    /// Construct the server. `embedder` is `Some` when semantic/hybrid search is
    /// available, `None` for lexical-only (Ollama down at startup). `memory` is
    /// `Some` when the cross-session memory store was wired up (it needs an
    /// embedder), `None` to make every memory tool report [`NO_MEMORY_ERR`].
    pub fn new(
        store: Arc<SqliteCodeStore>,
        root: std::path::PathBuf,
        embedder: Option<Arc<dyn Embedder>>,
        memory: Option<Arc<dyn MemoryStore>>,
    ) -> Self {
        Self {
            store,
            root,
            embedder,
            memory,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Return code-graph statistics (file/function/class/call/import/route counts) as JSON.")]
    async fn get_stats(&self) -> String {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || store.stats()).await;
        match result {
            Ok(Ok(stats)) => to_json(&stats),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Read-only index health check: drift vs the source tree on disk (missing/outdated/stale files) plus embedding/orphan invariants. Returns a DiagnosticReport as JSON. Re-run `icode index <path>` to heal any reported drift.")]
    async fn doctor(&self) -> String {
        let store = self.store.clone();
        let root = self.root.clone();
        let result = tokio::task::spawn_blocking(move || icode_engine::diagnose(&store, &root)).await;
        match result {
            Ok(Ok(report)) => to_json(&report),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Architecture overview in one call: stats, languages, modules, complex functions, call hotspots, entry points.")]
    async fn get_repo_map(&self, Parameters(args): Parameters<RepoMapArgs>) -> String {
        let store = self.store.clone();
        let top = args.top.unwrap_or(30);
        let result = tokio::task::spawn_blocking(move || store.repo_map(top)).await;
        match result {
            Ok(Ok(map)) => to_json(&map),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Everything about a symbol in one call: definition, callers, callees, imports, routes, implementations, and (when an embedder is available) semantically similar symbols.")]
    async fn get_symbol_context(&self, Parameters(args): Parameters<SymbolContextArgs>) -> String {
        let store = self.store.clone();
        let embedder = self.embedder.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut ctx = store.symbol_context(&args.name, args.file_hint.as_deref())?;
            // Enrich `similar_symbols` semantically when an embedder is wired up.
            // Best-effort: a vector/embed failure must NOT sink the whole context
            // call, so it leaves `similar_symbols` empty rather than erroring.
            // With no embedder the field stays empty (the store left it empty).
            if let Some(emb) = embedder.as_deref() {
                if let Some(def) = ctx.definition.as_ref() {
                    let qn = match def {
                        icode_core::model::FunctionOrClass::Function(f) => f.qualified_name.clone(),
                        icode_core::model::FunctionOrClass::Class(c) => c.qualified_name.clone(),
                    };
                    jit_embed(&store, emb);
                    if let Ok(similar) = icode_engine::find_similar(&store, emb, &qn, 5) {
                        ctx.similar_symbols = similar;
                    }
                }
            }
            Ok::<_, icode_core::error::Error>(ctx)
        })
        .await;
        match result {
            Ok(Ok(ctx)) => to_json(&ctx),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Lexical search over function names. Returns a JSON array of CodeHit.")]
    async fn search_function(&self, Parameters(args): Parameters<SearchFunctionArgs>) -> String {
        let store = self.store.clone();
        let query = CodeQuery {
            text: args.query,
            kind: Some(SymbolKind::Function),
            lang: None,
            limit: args.limit.unwrap_or(10),
            mode: SearchMode::Hybrid,
            with_body: false,
        };
        let result = tokio::task::spawn_blocking(move || store.search_code(&query)).await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Lexical search over class names. Returns a JSON array of CodeHit.")]
    async fn search_class(&self, Parameters(args): Parameters<SearchClassArgs>) -> String {
        let store = self.store.clone();
        let query = CodeQuery {
            text: args.query,
            kind: Some(SymbolKind::Class),
            lang: None,
            limit: args.limit.unwrap_or(10),
            mode: SearchMode::Hybrid,
            with_body: false,
        };
        let result = tokio::task::spawn_blocking(move || store.search_code(&query)).await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Find existing functions/classes that already do what you describe (avoid duplicate work). Semantic+lexical RRF fusion when an embedder is available, else lexical-only. NOTE: passing a `kind` filter (function|class) forces the lexical-only path EVEN with a live embedder — semantic similarity is NOT applied in that case; omit `kind` (or pass 'all') to get semantic matching. JSON array of CodeHit.")]
    async fn find_existing(&self, Parameters(args): Parameters<FindExistingArgs>) -> String {
        let kind = match args.kind.as_deref() {
            None | Some("") | Some("all") => Ok(None),
            Some("function") => Ok(Some(SymbolKind::Function)),
            Some("class") => Ok(Some(SymbolKind::Class)),
            Some(other) => Err(format!("invalid kind '{other}' (expected function|class|all)")),
        };
        let kind = match kind {
            Ok(k) => k,
            Err(e) => return err_json(&e),
        };
        let store = self.store.clone();
        let embedder = self.embedder.clone();
        let limit = args.limit.unwrap_or(10);
        let query = args.query;
        let result = tokio::task::spawn_blocking(move || {
            // Embedder present → semantic+lexical RRF (catches duplicates that use
            // different words). Note: hybrid fuses BOTH symbol kinds; the `kind`
            // filter is honoured only on the lexical-only fallback path, where the
            // store applies it. (Kind-filtering a fused result would drop signal;
            // documented limitation — pass kind only when you need lexical-only.)
            match embedder.as_deref() {
                Some(emb) if kind.is_none() => {
                    jit_embed(&store, emb);
                    icode_engine::hybrid_search(&store, emb, &query, limit)
                }
                _ => store.search_code(&CodeQuery {
                    text: query,
                    kind,
                    lang: None,
                    limit,
                    mode: SearchMode::Hybrid,
                    with_body: false,
                }),
            }
        })
        .await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Semantic (vector KNN) search over the code base by natural-language meaning. Requires an embedder (Ollama); JSON array of CodeHit or an error if unavailable.")]
    async fn semantic_search_code(&self, Parameters(args): Parameters<SemanticSearchArgs>) -> String {
        let emb = match self.embedder.clone() {
            Some(e) => e,
            None => return err_json(NO_EMBEDDER_ERR),
        };
        let store = self.store.clone();
        let limit = args.limit.unwrap_or(10);
        let query = args.query;
        let result = tokio::task::spawn_blocking(move || {
            jit_embed(&store, emb.as_ref());
            icode_engine::semantic_search(&store, emb.as_ref(), &query, limit)
        })
        .await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Find symbols semantically similar to an existing symbol (by qualified name). Requires an embedder (Ollama); the symbol itself is excluded. JSON array of CodeHit or an error if unavailable.")]
    async fn find_similar(&self, Parameters(args): Parameters<FindSimilarArgs>) -> String {
        let emb = match self.embedder.clone() {
            Some(e) => e,
            None => return err_json(NO_EMBEDDER_ERR),
        };
        let store = self.store.clone();
        let limit = args.limit.unwrap_or(10);
        let qn = args.qualified_name;
        let result = tokio::task::spawn_blocking(move || {
            jit_embed(&store, emb.as_ref());
            icode_engine::find_similar(&store, emb.as_ref(), &qn, limit)
        })
        .await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Fetch one function definition by name (optionally narrowed by language). JSON FunctionDef or null.")]
    async fn get_function(&self, Parameters(args): Parameters<GetFunctionArgs>) -> String {
        let lang = match parse_lang_arg(args.language.as_deref()) {
            Ok(l) => l,
            Err(e) => return err_json(&e),
        };
        let store = self.store.clone();
        let with_body = args.with_body.unwrap_or(false);
        let result =
            tokio::task::spawn_blocking(move || store.get_function(&args.name, lang, with_body)).await;
        match result {
            Ok(Ok(def)) => to_json(&def),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Fetch one class definition by name (optionally narrowed by language). JSON ClassDef or null.")]
    async fn get_class(&self, Parameters(args): Parameters<GetClassArgs>) -> String {
        let lang = match parse_lang_arg(args.language.as_deref()) {
            Ok(l) => l,
            Err(e) => return err_json(&e),
        };
        let store = self.store.clone();
        let with_body = args.with_body.unwrap_or(false);
        let result =
            tokio::task::spawn_blocking(move || store.get_class(&args.name, lang, with_body)).await;
        match result {
            Ok(Ok(def)) => to_json(&def),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "List a file's functions and classes (ordered by line) as a JSON array of CodeHit.")]
    async fn get_file_outline(&self, Parameters(args): Parameters<FileOutlineArgs>) -> String {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || store.file_outline(&args.path)).await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Direct callers of a symbol. Receiver-aware: pass a QUALIFIED name (e.g. `ServiceA::handle`) for a precise, class-scoped answer; a bare name (`handle`) keeps broad recall. JSON array of Call — each edge carries `resolved_callee` (the disambiguated target) and `confidence` (0.3..0.9, higher = surer).")]
    async fn get_callers(&self, Parameters(args): Parameters<CallersArgs>) -> String {
        let store = self.store.clone();
        let limit = args.limit.unwrap_or(50);
        let result = tokio::task::spawn_blocking(move || store.get_callers(&args.name, limit)).await;
        match result {
            Ok(Ok(calls)) => to_json(&calls),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Direct callees of a symbol (by-name call graph). JSON array of Call — each edge carries `resolved_callee` (receiver-aware target, e.g. `ServiceA::handle`) and `confidence` (0.3..0.9).")]
    async fn get_callees(&self, Parameters(args): Parameters<CalleesArgs>) -> String {
        let store = self.store.clone();
        let limit = args.limit.unwrap_or(50);
        let result = tokio::task::spawn_blocking(move || store.get_callees(&args.name, limit)).await;
        match result {
            Ok(Ok(calls)) => to_json(&calls),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Shortest call path between two symbols (BFS over the call graph). JSON array of symbol names.")]
    async fn get_call_chain(&self, Parameters(args): Parameters<CallChainArgs>) -> String {
        let store = self.store.clone();
        let max_depth = args.max_depth.unwrap_or(12);
        let result =
            tokio::task::spawn_blocking(move || store.call_chain(&args.from, &args.to, max_depth)).await;
        match result {
            Ok(Ok(chain)) => to_json(&chain),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Modules a file depends on, followed transitively. JSON array of module strings.")]
    async fn find_dependencies(&self, Parameters(args): Parameters<DependenciesArgs>) -> String {
        let store = self.store.clone();
        let depth = args.depth.unwrap_or(3);
        let result = tokio::task::spawn_blocking(move || store.find_dependencies(&args.path, depth)).await;
        match result {
            Ok(Ok(deps)) => to_json(&deps),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Reverse dependencies: files that import the given file (impact set), followed transitively. JSON array of paths.")]
    async fn impact_analysis(&self, Parameters(args): Parameters<ImpactArgs>) -> String {
        let store = self.store.clone();
        let depth = args.depth.unwrap_or(3);
        let result = tokio::task::spawn_blocking(move || store.impact_analysis(&args.path, depth)).await;
        match result {
            Ok(Ok(paths)) => to_json(&paths),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Classes that implement / extend the given base class or interface. JSON array of qualified names.")]
    async fn find_implementations(&self, Parameters(args): Parameters<ImplementationsArgs>) -> String {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || store.find_implementations(&args.name)).await;
        match result {
            Ok(Ok(impls)) => to_json(&impls),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "APPROXIMATE candidate dead-code — functions whose NAME never appears as a call CALLEE (by-name in-degree = 0). This is NOT proof they are never called: dynamic/reflective calls, callers outside the indexed project, trait/virtual dispatch, and same-name collisions are all invisible. Entry points (main + route handlers) are excluded. Treat hits as candidates to review, not confirmed dead code; do not delete on this basis alone. JSON array of CodeHit.")]
    async fn find_dead_code(&self, Parameters(args): Parameters<AnalysisArgs>) -> String {
        let lang = match parse_lang_arg(args.language.as_deref()) {
            Ok(l) => l,
            Err(e) => return err_json(&e),
        };
        let store = self.store.clone();
        let limit = args.limit.unwrap_or(20);
        let result = tokio::task::spawn_blocking(move || store.find_dead_code(lang, limit)).await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "APPROXIMATE candidate dead-code detector — NOT proof. Walks the call graph by BFS from entry points (main + route handlers) over BARE call-NAME edges, catching dead clusters (a->b->c where a is itself dead), not just zero-in-degree functions. Caveats: same-named functions collide; trait/virtual/dynamic/reflective dispatch is invisible; a LIBRARY crate with no main and no routes has no entry points, so almost everything is reported unreachable (mass false positives). Treat every hit as a candidate to review; never delete code on this basis alone. JSON array of CodeHit.")]
    async fn find_unreachable(&self, Parameters(args): Parameters<AnalysisArgs>) -> String {
        let lang = match parse_lang_arg(args.language.as_deref()) {
            Ok(l) => l,
            Err(e) => return err_json(&e),
        };
        let store = self.store.clone();
        let limit = args.limit.unwrap_or(20);
        let result = tokio::task::spawn_blocking(move || store.find_unreachable(lang, limit)).await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Functions ranked by a complexity proxy (span + fan_out*5 + callers*2). JSON array of ComplexFunction.")]
    async fn find_complex_functions(&self, Parameters(args): Parameters<AnalysisArgs>) -> String {
        let lang = match parse_lang_arg(args.language.as_deref()) {
            Ok(l) => l,
            Err(e) => return err_json(&e),
        };
        let store = self.store.clone();
        let limit = args.limit.unwrap_or(20);
        let result = tokio::task::spawn_blocking(move || store.find_complex_functions(lang, limit)).await;
        match result {
            Ok(Ok(fns)) => to_json(&fns),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Find HTTP routes by method / path / handler filters. JSON array of Route.")]
    async fn find_routes(&self, Parameters(args): Parameters<FindRoutesArgs>) -> String {
        let store = self.store.clone();
        let limit = args.limit.unwrap_or(50);
        let result = tokio::task::spawn_blocking(move || {
            store.find_routes(
                args.method.as_deref(),
                args.path.as_deref(),
                args.handler.as_deref(),
                limit,
            )
        })
        .await;
        match result {
            Ok(Ok(routes)) => to_json(&routes),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Regex search over stored symbol bodies. JSON array of GrepHit (path/line/text).")]
    async fn grep_code(&self, Parameters(args): Parameters<GrepCodeArgs>) -> String {
        let lang = match parse_lang_arg(args.language.as_deref()) {
            Ok(l) => l,
            Err(e) => return err_json(&e),
        };
        let store = self.store.clone();
        let limit = args.limit.unwrap_or(50);
        let result =
            tokio::task::spawn_blocking(move || store.grep_code(&args.pattern, lang, limit)).await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "List indexed files (optional path/language filters). JSON array of FileRecord.")]
    async fn list_files(&self, Parameters(args): Parameters<ListFilesArgs>) -> String {
        let lang = match parse_lang_arg(args.language.as_deref()) {
            Ok(l) => l,
            Err(e) => return err_json(&e),
        };
        let store = self.store.clone();
        let limit = args.limit.unwrap_or(50);
        let result = tokio::task::spawn_blocking(move || {
            store.list_files(args.pattern.as_deref(), lang, limit)
        })
        .await;
        match result {
            Ok(Ok(files)) => to_json(&files),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Stat one indexed file (path/language/hashes/lines/size). JSON FileRecord or null.")]
    async fn stat_file(&self, Parameters(args): Parameters<StatFileArgs>) -> String {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || store.stat_file(&args.path)).await;
        match result {
            Ok(Ok(rec)) => to_json(&rec),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Read a line range [start,end] of a file from disk (1-based inclusive; capped). Returns the text.")]
    async fn read_file(&self, Parameters(args): Parameters<ReadFileArgs>) -> String {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || {
            store.read_file(&args.path, args.start, args.end)
        })
        .await;
        match result {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    // ──────────────────────────── memory tools ────────────────────────────

    #[tool(description = "Start a session: load the developer profile (cross-project preferences) AND recent project context in one call. Returns {developer_profile, project_context}. Call this FIRST.")]
    async fn session_start(&self, Parameters(args): Parameters<SessionStartArgs>) -> String {
        let mem = match self.memory.clone() {
            Some(m) => m,
            None => return err_json(NO_MEMORY_ERR),
        };
        let n_project = args.n_project_memories.unwrap_or(DEFAULT_N_PROJECT_MEMORIES);
        let n_profile = args.n_profile_notes.unwrap_or(DEFAULT_N_PROFILE_NOTES);
        let project = args.project;
        let result = tokio::task::spawn_blocking(move || {
            icode_engine::session_start(mem.as_ref(), &project, n_project, n_profile)
        })
        .await;
        match result {
            Ok(Ok(s)) => serde_json::json!({
                "developer_profile": s.developer_profile,
                "project_context": s.project_context,
            })
            .to_string(),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "End a session: save summary (Progress), decisions (Decision), completed (Progress), todos (Todo); auto-resolve the bug/todo memories each completed item closes. Returns {saved, auto_resolved}.")]
    async fn session_end(&self, Parameters(args): Parameters<SessionEndArgs>) -> String {
        let mem = match self.memory.clone() {
            Some(m) => m,
            None => return err_json(NO_MEMORY_ERR),
        };
        let SessionEndArgs {
            project,
            summary,
            completed,
            decisions,
            todos,
            session_id,
        } = args;
        let completed = completed.unwrap_or_default();
        let decisions = decisions.unwrap_or_default();
        let todos = todos.unwrap_or_default();
        let result = tokio::task::spawn_blocking(move || {
            icode_engine::session_end(
                mem.as_ref(),
                &project,
                &summary,
                &completed,
                &decisions,
                &todos,
                session_id.as_deref(),
            )
        })
        .await;
        match result {
            Ok(Ok(s)) => serde_json::json!({
                "saved": s.saved,
                "auto_resolved": s.auto_resolved,
            })
            .to_string(),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Save one memory. category: decision|progress|context|bug|todo|code|general (default general). A near-duplicate is reported as {outcome:duplicate} (not an error).")]
    async fn add_memory(&self, Parameters(args): Parameters<AddMemoryArgs>) -> String {
        let mem = match self.memory.clone() {
            Some(m) => m,
            None => return err_json(NO_MEMORY_ERR),
        };
        let category = args
            .category
            .as_deref()
            .map(parse_category_arg)
            .unwrap_or(Category::General);
        let new = NewMemory {
            project: args.project,
            content: args.content,
            category,
            tags: args.tags.unwrap_or_default(),
            importance: args.importance.unwrap_or(0.0),
            session_id: None,
        };
        let result = tokio::task::spawn_blocking(move || mem.add(new)).await;
        match result {
            Ok(Ok(outcome)) => to_json(&outcome),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Semantic + lexical (RRF) search of one project's memories. JSON array of MemoryHit.")]
    async fn search_memory(&self, Parameters(args): Parameters<SearchMemoryArgs>) -> String {
        let mem = match self.memory.clone() {
            Some(m) => m,
            None => return err_json(NO_MEMORY_ERR),
        };
        let category = args.category.as_deref().map(parse_category_arg);
        let n = args.n_results.unwrap_or(5);
        let include_resolved = args.include_resolved.unwrap_or(false);
        let project = args.project;
        let query = args.query;
        let result = tokio::task::spawn_blocking(move || {
            mem.search(&project, &query, n, category, include_resolved)
        })
        .await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Cross-project semantic memory search (reserved projects excluded). JSON array of MemoryHit.")]
    async fn search_all(&self, Parameters(args): Parameters<SearchAllArgs>) -> String {
        let mem = match self.memory.clone() {
            Some(m) => m,
            None => return err_json(NO_MEMORY_ERR),
        };
        // search_all has no per-call category filter on the trait; an explicit
        // category is honoured by post-filtering the fused hits here.
        let category = args.category.as_deref().map(parse_category_arg);
        let n = args.n_results.unwrap_or(5);
        let include_resolved = args.include_resolved.unwrap_or(false);
        let query = args.query;
        let result = tokio::task::spawn_blocking(move || {
            mem.search_all(&query, n, include_resolved).map(|hits| match category {
                Some(c) => hits.into_iter().filter(|h| h.record.category == c).collect(),
                None => hits,
            })
        })
        .await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "List a project's memories (newest first; optional category filter). JSON array of MemoryRecord.")]
    async fn list_memories(&self, Parameters(args): Parameters<ListMemoriesArgs>) -> String {
        let mem = match self.memory.clone() {
            Some(m) => m,
            None => return err_json(NO_MEMORY_ERR),
        };
        let category = args.category.as_deref().map(parse_category_arg);
        let limit = args.limit.unwrap_or(20);
        let include_resolved = args.include_resolved.unwrap_or(false);
        let project = args.project;
        let result = tokio::task::spawn_blocking(move || {
            mem.list(&project, category, limit, include_resolved)
        })
        .await;
        match result {
            Ok(Ok(recs)) => to_json(&recs),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "List known projects with their memory counts (reserved projects excluded). JSON array of [name, count].")]
    async fn list_projects(&self) -> String {
        let mem = match self.memory.clone() {
            Some(m) => m,
            None => return err_json(NO_MEMORY_ERR),
        };
        let result = tokio::task::spawn_blocking(move || mem.list_projects()).await;
        match result {
            Ok(Ok(rows)) => to_json(&rows),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Update a memory's content and/or tags by id. Re-embeds only when the content actually changes. Returns {ok:true} or an error.")]
    async fn update_memory(&self, Parameters(args): Parameters<UpdateMemoryArgs>) -> String {
        let mem = match self.memory.clone() {
            Some(m) => m,
            None => return err_json(NO_MEMORY_ERR),
        };
        let id = MemoryId(args.memory_id);
        let content = args.content;
        let tags = args.tags;
        let result = tokio::task::spawn_blocking(move || {
            mem.update(&id, content.as_deref(), tags.as_deref())
        })
        .await;
        match result {
            Ok(Ok(())) => serde_json::json!({ "ok": true }).to_string(),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Delete a memory by id (removes its vector + lexical row too). Returns {ok:true} or an error.")]
    async fn delete_memory(&self, Parameters(args): Parameters<DeleteMemoryArgs>) -> String {
        let mem = match self.memory.clone() {
            Some(m) => m,
            None => return err_json(NO_MEMORY_ERR),
        };
        let id = MemoryId(args.memory_id);
        let result = tokio::task::spawn_blocking(move || mem.delete(&id)).await;
        match result {
            Ok(Ok(())) => serde_json::json!({ "ok": true }).to_string(),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Mark a bug/todo memory resolved by id, recording the reason. Returns {ok:true} or an error.")]
    async fn resolve_memory(&self, Parameters(args): Parameters<ResolveMemoryArgs>) -> String {
        let mem = match self.memory.clone() {
            Some(m) => m,
            None => return err_json(NO_MEMORY_ERR),
        };
        let id = MemoryId(args.memory_id);
        let reason = args.reason;
        let result = tokio::task::spawn_blocking(move || mem.resolve(&id, &reason)).await;
        match result {
            Ok(Ok(())) => serde_json::json!({ "ok": true }).to_string(),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Save a cross-project developer note (preference / rule about how this developer works). Stored under the reserved profile project. Returns Added/Duplicate JSON.")]
    async fn add_developer_note(&self, Parameters(args): Parameters<AddDeveloperNoteArgs>) -> String {
        let mem = match self.memory.clone() {
            Some(m) => m,
            None => return err_json(NO_MEMORY_ERR),
        };
        let category = args
            .category
            .as_deref()
            .map(parse_category_arg)
            .unwrap_or(Category::General);
        let new = NewMemory {
            project: DEVELOPER_PROJECT.to_string(),
            content: args.content,
            category,
            tags: args.tags.unwrap_or_default(),
            importance: args.importance.unwrap_or(0.0),
            session_id: None,
        };
        let result = tokio::task::spawn_blocking(move || mem.add(new)).await;
        match result {
            Ok(Ok(outcome)) => to_json(&outcome),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Read the developer profile: with a query, semantically search it; without, list it newest-first. JSON array of MemoryRecord (list) or MemoryHit (search).")]
    async fn get_developer_profile(&self, Parameters(args): Parameters<GetDeveloperProfileArgs>) -> String {
        let mem = match self.memory.clone() {
            Some(m) => m,
            None => return err_json(NO_MEMORY_ERR),
        };
        let n = args.n_results.unwrap_or(5);
        let query = args.query.filter(|q| !q.trim().is_empty());
        let result = tokio::task::spawn_blocking(move || match query {
            Some(q) => mem
                .search(DEVELOPER_PROJECT, &q, n, None, false)
                .map(|hits| serde_json::to_value(hits).unwrap_or(serde_json::Value::Null)),
            None => mem
                .list(DEVELOPER_PROJECT, None, n, false)
                .map(|recs| serde_json::to_value(recs).unwrap_or(serde_json::Value::Null)),
        })
        .await;
        match result {
            Ok(Ok(v)) => v.to_string(),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    // ──────────────────────────── recall (code + memory synergy) ────────────────────────────

    #[tool(description = "Flagship recall: one query → relevant CODE and relevant MEMORY in SEPARATE, independently-ranked sections (plus a facts section, empty until the knowledge graph lands). JSON {relevant_code, relevant_memory, facts}. Degrades gracefully: with no embedder the code section is lexical-only; with no memory store relevant_memory is empty (the code section still answers).")]
    async fn recall(&self, Parameters(args): Parameters<RecallArgs>) -> String {
        let store = self.store.clone();
        // Clone the Arcs BEFORE spawn_blocking; `.as_deref()` inside the closure
        // turns each `Option<Arc<dyn ..>>` into the `Option<&dyn ..>` the engine
        // wants, without lifetime trouble (the Arcs outlive the borrow).
        let embedder = self.embedder.clone();
        let memory = self.memory.clone();
        let limit = args.limit.unwrap_or(8);
        let project = args.project;
        let query = args.query;
        let result = tokio::task::spawn_blocking(move || {
            // Refresh vectors on demand so the code section reflects current source
            // (no-op/cheap when nothing changed; cache-accelerated after a checkout).
            if let Some(emb) = embedder.as_deref() {
                jit_embed(&store, emb);
            }
            icode_engine::recall(
                &store,
                embedder.as_deref(),
                memory.as_deref(),
                &project,
                &query,
                limit,
            )
        })
        .await;
        match result {
            Ok(Ok(recall)) => to_json(&recall),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Memory semantically related to a code symbol ('what do we remember about this symbol?'). JSON array of MemoryHit, or an error if the memory store is unavailable.")]
    async fn code_to_memory(&self, Parameters(args): Parameters<CodeToMemoryArgs>) -> String {
        let mem = match self.memory.clone() {
            Some(m) => m,
            None => return err_json(NO_MEMORY_ERR),
        };
        let limit = args.limit.unwrap_or(8);
        let project = args.project;
        let symbol = args.symbol;
        let result = tokio::task::spawn_blocking(move || {
            icode_engine::code_to_memory(mem.as_ref(), &project, &symbol, limit)
        })
        .await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Why a symbol exists: DECISION-category memory recording the rationale behind it. JSON array of MemoryHit, or an error if the memory store is unavailable.")]
    async fn why_this_exists(&self, Parameters(args): Parameters<WhyThisExistsArgs>) -> String {
        let mem = match self.memory.clone() {
            Some(m) => m,
            None => return err_json(NO_MEMORY_ERR),
        };
        let limit = args.limit.unwrap_or(8);
        let project = args.project;
        let symbol = args.symbol;
        let result = tokio::task::spawn_blocking(move || {
            icode_engine::why_this_exists(mem.as_ref(), &project, &symbol, limit)
        })
        .await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }
}

#[tool_handler]
impl ServerHandler for CodeMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some("iCode v2 code-graph MCP server (group A read tools)".into());
        info
    }
}

/// Serve the MCP protocol over stdio until the client disconnects. `embedder` is
/// `Some` to enable the semantic/hybrid code tools, `None` for lexical-only.
/// `memory` is `Some` to enable the cross-session memory tools, `None` to make
/// them return [`NO_MEMORY_ERR`]. The bin builds `memory` and passes it as an
/// `Arc<dyn MemoryStore>`, so `icode-serve` never depends on `icode-embed`.
pub async fn serve_stdio(
    store: Arc<SqliteCodeStore>,
    root: std::path::PathBuf,
    embedder: Option<Arc<dyn Embedder>>,
    memory: Option<Arc<dyn MemoryStore>>,
) -> anyhow::Result<()> {
    let service = CodeMcpServer::new(store, root, embedder, memory)
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;
    service.waiting().await?;
    Ok(())
}

/// Parse a `category` argument string into a `Category`. The mapping mirrors
/// `Category`'s lowercase serde names; an unknown/empty value falls back to
/// `General` (a lenient default — a memory is still saved rather than rejected
/// over a typo'd category). Callers with an `Option<&str>` use this via
/// `.map(parse_category_arg)` (filter) or with `.unwrap_or(General)` (add).
fn parse_category_arg(s: &str) -> Category {
    match s.trim().to_lowercase().as_str() {
        "decision" => Category::Decision,
        "progress" => Category::Progress,
        "context" => Category::Context,
        "bug" => Category::Bug,
        "todo" => Category::Todo,
        "code" => Category::Code,
        _ => Category::General,
    }
}

/// Parse an optional `language` argument string into `Option<Language>`.
/// `None`/empty means "any language"; an unrecognised string is an error so the
/// agent gets a clear signal rather than a silent no-filter fallback.
fn parse_lang_arg(s: Option<&str>) -> std::result::Result<Option<Language>, String> {
    match s.map(str::trim) {
        None | Some("") => Ok(None),
        Some(v) => match v.to_lowercase().as_str() {
            "php" => Ok(Some(Language::Php)),
            "python" | "py" => Ok(Some(Language::Python)),
            "javascript" | "js" => Ok(Some(Language::JavaScript)),
            "typescript" | "ts" => Ok(Some(Language::TypeScript)),
            "go" | "golang" => Ok(Some(Language::Go)),
            "java" => Ok(Some(Language::Java)),
            "rust" | "rs" => Ok(Some(Language::Rust)),
            "html" => Ok(Some(Language::Html)),
            "text" => Ok(Some(Language::Text)),
            other => Err(format!(
                "unknown language '{other}' (expected php|python|javascript|typescript|go|java|rust|html|text)"
            )),
        },
    }
}

/// Bring the vector index up to date ON DEMAND, just before a semantic query.
///
/// This is the "smart embedding" seam: the daemon keeps the graph live but never
/// embeds, so vectors are filled here — only when semantic search is actually used.
/// `embed_pending` is cache-accelerated (`content_hash → vector`), so a query after
/// a `git checkout` (or a switch back) re-embeds only genuinely-new code and
/// rehydrates everything else for free. In steady state the pending queue is empty
/// and this is a couple of cheap SQL counts.
///
/// Best-effort: an embedder hiccup (Ollama momentarily down) must NOT sink the
/// search — it is logged to stderr and the query proceeds against whatever vectors
/// already exist. Runs inside the tool's `spawn_blocking` closure (sync, off the
/// async reactor).
fn jit_embed(store: &SqliteCodeStore, embedder: &dyn Embedder) {
    if let Err(e) = icode_engine::embed_pending(store, embedder, JIT_EMBED_BATCH) {
        eprintln!("icode: on-demand embed skipped ({e})");
    }
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| err_json(&e.to_string()))
}

fn err_json(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No-Ollama (lexical-only) behaviour: with `embedder: None` the semantic
    /// tools must return the documented unavailable-error JSON, and the degrading
    /// tools (`find_existing`, `get_symbol_context`) must still succeed lexically
    /// with an empty `similar_symbols`. Deterministic — no network involved.
    #[tokio::test]
    async fn semantic_tools_degrade_without_embedder() {
        let store = Arc::new(SqliteCodeStore::open_in_memory().expect("store"));
        let server = CodeMcpServer::new(store, std::path::PathBuf::from("."), None, None);

        let s = server
            .semantic_search_code(Parameters(SemanticSearchArgs {
                query: "anything".into(),
                limit: None,
            }))
            .await;
        assert_eq!(s, err_json(NO_EMBEDDER_ERR), "semantic_search_code reports unavailable");

        let fs = server
            .find_similar(Parameters(FindSimilarArgs {
                qualified_name: "whatever".into(),
                limit: None,
            }))
            .await;
        assert_eq!(fs, err_json(NO_EMBEDDER_ERR), "find_similar reports unavailable");

        // find_existing degrades to lexical-only and must NOT error (empty db → []).
        let fe = server
            .find_existing(Parameters(FindExistingArgs {
                query: "read a file".into(),
                kind: None,
                limit: Some(5),
            }))
            .await;
        assert_eq!(fe, "[]", "find_existing degrades to lexical (empty db → [])");

        // get_symbol_context on an unknown symbol returns a context with empty
        // similar_symbols (no embedder to enrich it).
        let ctx = server
            .get_symbol_context(Parameters(SymbolContextArgs {
                name: "unknown_symbol".into(),
                file_hint: None,
            }))
            .await;
        let v: serde_json::Value = serde_json::from_str(&ctx).expect("ctx json");
        assert!(
            v.get("similar_symbols")
                .and_then(|s| s.as_array())
                .map(|a| a.is_empty())
                .unwrap_or(false),
            "similar_symbols is empty without an embedder, got {ctx}"
        );
    }

    /// With `memory: None` every memory tool must return the documented
    /// unavailable-error JSON (and NOT panic / hang). Deterministic — no store,
    /// no Ollama. Covers a representative read, write, and the session lifecycle.
    #[tokio::test]
    async fn memory_tools_report_unavailable_without_memory() {
        let store = Arc::new(SqliteCodeStore::open_in_memory().expect("store"));
        let server = CodeMcpServer::new(store, std::path::PathBuf::from("."), None, None);
        let want = err_json(NO_MEMORY_ERR);

        let add = server
            .add_memory(Parameters(AddMemoryArgs {
                project: "demo".into(),
                content: "x".into(),
                category: None,
                tags: None,
                importance: None,
            }))
            .await;
        assert_eq!(add, want, "add_memory unavailable");

        let search = server
            .search_memory(Parameters(SearchMemoryArgs {
                project: "demo".into(),
                query: "x".into(),
                n_results: None,
                category: None,
                include_resolved: None,
            }))
            .await;
        assert_eq!(search, want, "search_memory unavailable");

        let ss = server
            .session_start(Parameters(SessionStartArgs {
                project: "demo".into(),
                n_project_memories: None,
                n_profile_notes: None,
            }))
            .await;
        assert_eq!(ss, want, "session_start unavailable");

        let lp = server.list_projects().await;
        assert_eq!(lp, want, "list_projects unavailable");

        let gp = server
            .get_developer_profile(Parameters(GetDeveloperProfileArgs {
                query: None,
                n_results: None,
            }))
            .await;
        assert_eq!(gp, want, "get_developer_profile unavailable");

        // The two memory-only recall helpers also report unavailable.
        let c2m = server
            .code_to_memory(Parameters(CodeToMemoryArgs {
                project: "demo".into(),
                symbol: "read_file".into(),
                limit: None,
            }))
            .await;
        assert_eq!(c2m, want, "code_to_memory unavailable");

        let why = server
            .why_this_exists(Parameters(WhyThisExistsArgs {
                project: "demo".into(),
                symbol: "read_file".into(),
                limit: None,
            }))
            .await;
        assert_eq!(why, want, "why_this_exists unavailable");
    }

    /// `recall` degrades but still answers when neither embedder nor memory is
    /// wired: the code section is lexical (empty db → []), the memory section is
    /// empty, and `facts` is empty — all three KEYS are present. Deterministic.
    #[tokio::test]
    async fn recall_degrades_without_embedder_or_memory() {
        let store = Arc::new(SqliteCodeStore::open_in_memory().expect("store"));
        let server = CodeMcpServer::new(store, std::path::PathBuf::from("."), None, None);

        let out = server
            .recall(Parameters(RecallArgs {
                project: "demo".into(),
                query: "read a file from disk".into(),
                limit: Some(5),
            }))
            .await;
        let v: serde_json::Value = serde_json::from_str(&out).expect("recall json");
        for key in ["relevant_code", "relevant_memory", "facts"] {
            let arr = v.get(key).and_then(|x| x.as_array());
            assert!(
                arr.map(|a| a.is_empty()).unwrap_or(false),
                "recall section '{key}' is present and empty in fully-degraded mode, got {out}"
            );
        }
    }

    /// `parse_category_arg` maps the wire strings and falls back to General.
    #[test]
    fn category_arg_parsing() {
        assert_eq!(parse_category_arg("decision"), Category::Decision);
        assert_eq!(parse_category_arg("BUG"), Category::Bug);
        assert_eq!(parse_category_arg(" todo "), Category::Todo);
        assert_eq!(parse_category_arg("nonsense"), Category::General);
        assert_eq!(parse_category_arg(""), Category::General);
    }
}
