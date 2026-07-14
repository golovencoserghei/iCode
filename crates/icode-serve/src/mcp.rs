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

// ──────────────── arg structs for the consolidated (12-tool) surface ────────────────
//
// Keep these descriptions SHORT: every one of them is serialized into the model's
// context on every single request.

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// What to look for: plain words or an identifier.
    pub query: String,
    /// function | class | any (default any).
    pub kind: Option<String>,
    /// lexical (default, no model) | semantic | hybrid.
    pub mode: Option<String>,
    /// Max hits (default 10).
    pub limit: Option<usize>,
    /// Include symbol bodies (default false; costs context).
    pub with_body: Option<bool>,
    /// Sub-project to search within (a monorepo member, e.g. "internal-agent"). Omit to
    /// search the whole tree. Without this a monorepo drowns the answer in siblings.
    pub scope: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SymbolArgs {
    /// Symbol name (bare or qualified).
    pub name: String,
    /// context (default) | def | callers | callees | references | tests | implementations | similar.
    pub op: Option<String>,
    /// Disambiguate a symbol defined in several files.
    pub file_hint: Option<String>,
    /// Language filter for op=def.
    pub language: Option<String>,
    /// Include the body (op=def).
    pub with_body: Option<bool>,
    /// Max rows (default 50).
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct GraphArgs {
    /// chain | impact | deps.
    pub op: String,
    /// Symbol (chain: the source) or file path (impact/deps).
    pub target: String,
    /// chain: the destination symbol.
    pub to: Option<String>,
    /// Hops to traverse.
    pub depth: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct MapArgs {
    /// Entries per section (default 30).
    pub top: Option<usize>,
    /// Sub-project to map (a monorepo member). Omit to list the members.
    pub scope: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct FileArgs {
    /// outline | list | stat | read.
    pub op: String,
    /// File path (op=list: an optional glob-ish filter).
    pub path: Option<String>,
    /// op=read: first line (1-based).
    pub start: Option<u32>,
    /// op=read: last line (inclusive).
    pub end: Option<u32>,
    /// Language filter (op=list).
    pub language: Option<String>,
    /// Max rows.
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct AuditArgs {
    /// dead | unreachable | complex | routes | health.
    pub op: String,
    /// Language filter.
    pub language: Option<String>,
    /// Max rows.
    pub limit: Option<usize>,
    /// op=routes: HTTP method filter.
    pub method: Option<String>,
    /// op=routes: path filter.
    pub path: Option<String>,
    /// op=routes: handler filter.
    pub handler: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct MemoryArgs {
    /// add | search | list | update | delete | resolve | search_all | projects | for_symbol | why.
    pub op: String,
    /// Project the memory belongs to.
    pub project: Option<String>,
    /// Memory text (op=add/update).
    pub content: Option<String>,
    /// Query (op=search/search_all).
    pub query: Option<String>,
    /// Memory id (op=update/delete/resolve).
    pub memory_id: Option<String>,
    /// Symbol (op=for_symbol/why).
    pub symbol: Option<String>,
    /// decision|progress|context|bug|todo|code|general.
    pub category: Option<String>,
    /// Free-form tags.
    pub tags: Option<Vec<String>>,
    /// Importance 0..5 (5 = always-on).
    pub importance: Option<f32>,
    /// Why it is resolved (op=resolve).
    pub reason: Option<String>,
    /// Max rows.
    pub limit: Option<usize>,
    /// Include resolved memories.
    pub include_resolved: Option<bool>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SessionArgs {
    /// start | end.
    pub op: String,
    /// Project name.
    pub project: String,
    /// op=end: one-paragraph summary.
    pub summary: Option<String>,
    /// op=end: finished items.
    pub completed: Option<Vec<String>>,
    /// op=end: decisions made.
    pub decisions: Option<Vec<String>>,
    /// op=end: open next steps.
    pub todos: Option<Vec<String>>,
    /// op=end: session id.
    pub session_id: Option<String>,
    /// op=start: how many project memories to load.
    pub limit: Option<usize>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DeveloperArgs {
    /// note | profile.
    pub op: String,
    /// op=note: the cross-project rule/preference.
    pub content: Option<String>,
    /// op=profile: optional search query.
    pub query: Option<String>,
    /// Category.
    pub category: Option<String>,
    /// Tags.
    pub tags: Option<Vec<String>>,
    /// Importance 0..5.
    pub importance: Option<f32>,
    /// Store even if it looks project-specific.
    pub force: Option<bool>,
    /// Max rows.
    pub limit: Option<usize>,
}

// ──────────────────────────── arg structs (internal handlers) ────────────────────────────

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
pub struct CheckExistsArgs {
    /// The feature/behaviour to check for, in plain words (e.g. "a calendar for
    /// reminders", "rate limiting on login").
    pub query: String,
    /// Symbol space to interrogate: "function" | "class" | "route" | "any"
    /// (default "any").
    pub kind: Option<String>,
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
    /// Store the note even if it looks project/machine-specific (names a filesystem
    /// path). Default false — such a note is rejected because the profile is
    /// injected into EVERY project; use add_memory(project=…) for a project fact.
    pub force: Option<bool>,
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

    async fn get_symbol_context(&self, Parameters(args): Parameters<SymbolContextArgs>) -> String {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut ctx = store.symbol_context(&args.name, args.file_hint.as_deref())?;
            // `similar_symbols` is filled by MinHash — no embedder needed, so the
            // field is populated even with Ollama down. Best-effort: a failure leaves
            // it empty rather than sinking the whole context call.
            if let Some(def) = ctx.definition.as_ref() {
                let qn = match def {
                    icode_core::model::FunctionOrClass::Function(f) => f.qualified_name.clone(),
                    icode_core::model::FunctionOrClass::Class(c) => c.qualified_name.clone(),
                };
                if let Ok(similar) = icode_engine::find_similar(&store, &qn, 5) {
                    ctx.similar_symbols = similar;
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

    async fn check_exists_impl(&self, Parameters(args): Parameters<CheckExistsArgs>) -> String {
        let scope = match args.kind.as_deref() {
            None | Some("") | Some("any") => Ok(icode_engine::ExistScope::Any),
            Some("function") => Ok(icode_engine::ExistScope::Function),
            Some("class") => Ok(icode_engine::ExistScope::Class),
            Some("route") => Ok(icode_engine::ExistScope::Route),
            Some(other) => {
                Err(format!("invalid kind '{other}' (expected function|class|route|any)"))
            }
        };
        let scope = match scope {
            Ok(s) => s,
            Err(e) => return err_json(&e),
        };
        let store = self.store.clone();
        let embedder = self.embedder.clone();
        let query = args.query;
        let result = tokio::task::spawn_blocking(move || {
            // Embed pending chunks first so the semantic signal is live (mirrors the
            // other semantic tools); a down/absent embedder simply drops that signal
            // and the verdict forms from the exact-name + body-or-string signals.
            if let Some(emb) = embedder.as_deref() {
                jit_embed(&store, emb);
            }
            // The embedder is DELIBERATELY not passed.
            //
            // check_exists is an oracle over the INDEX; the semantic "lead" it used the
            // embedder for is a nicety that cost the answer. With vectors absent (index
            // is free by default now) the semantic arm has nothing to match, so it
            // offered whatever was nearest in another sub-project — the reported case
            // answered a question about `internal-agent` with `admin_ui` from
            // `data-gateway`, while the literal `page_metrics` sat in the index. It also
            // dragged the query through the embedding backend, which on a real project
            // took minutes. Lexical evidence is what makes this oracle honest; a guess
            // from a model is what makes it dangerous.
            icode_engine::check_exists(&store, None, &query, scope)
        })
        .await;
        match result {
            Ok(Ok(v)) => to_json(&v),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

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

    async fn find_tests_covering(&self, Parameters(args): Parameters<CallersArgs>) -> String {
        let store = self.store.clone();
        let depth = args.limit.unwrap_or(5);
        let name = args.name;
        let result =
            tokio::task::spawn_blocking(move || store.find_tests_covering(&name, depth)).await;
        match result {
            Ok(Ok(t)) => to_json(&t),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    /// Lexical search over BOTH symbol kinds, with a module scope.
    ///
    /// `search` used to route kind=any into `find_existing`, which fuses dense+lexical
    /// with RRF — and RRF re-ranks by POSITION, which throws away `search_code`'s
    /// exact-name boost. The visible symptom: querying `page_metrics` returned the
    /// symbol literally named `page_metrics` in SECOND place, behind an unrelated
    /// `_fetch_members`. For an existence question that is the most expensive error
    /// there is, because the honest answer was sitting in the index.
    async fn search_lexical(&self, Parameters(args): Parameters<SearchArgs>) -> String {
        let store = self.store.clone();
        let limit = args.limit.unwrap_or(10);
        let with_body = args.with_body.unwrap_or(false);
        let scope = args.scope.clone();
        let kind = match args.kind.as_deref() {
            Some("function") => Some(SymbolKind::Function),
            Some("class") => Some(SymbolKind::Class),
            _ => None,
        };
        let query = args.query;
        let result = tokio::task::spawn_blocking(move || {
            store.search_code_scoped(
                &CodeQuery {
                    text: query,
                    kind,
                    lang: None,
                    limit,
                    mode: SearchMode::Lexical,
                    with_body,
                },
                scope.as_deref(),
            )
        })
        .await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    async fn find_references(&self, Parameters(args): Parameters<CallersArgs>) -> String {
        let store = self.store.clone();
        let limit = args.limit.unwrap_or(100);
        let name = args.name;
        let result =
            tokio::task::spawn_blocking(move || store.find_references(&name, limit)).await;
        match result {
            Ok(Ok(refs)) => to_json(&refs),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    /// No embedder gate any more: similarity is MinHash over token shingles, so this
    /// works with Ollama down — and it answers the question actually being asked
    /// ("code shaped like this") rather than "code about the same topic".
    async fn find_similar(&self, Parameters(args): Parameters<FindSimilarArgs>) -> String {
        let store = self.store.clone();
        let limit = args.limit.unwrap_or(10);
        let qn = args.qualified_name;
        let result =
            tokio::task::spawn_blocking(move || icode_engine::find_similar(&store, &qn, limit))
                .await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

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

    async fn get_file_outline(&self, Parameters(args): Parameters<FileOutlineArgs>) -> String {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || store.file_outline(&args.path)).await;
        match result {
            Ok(Ok(hits)) => to_json(&hits),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

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

    async fn find_implementations(&self, Parameters(args): Parameters<ImplementationsArgs>) -> String {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || store.find_implementations(&args.name)).await;
        match result {
            Ok(Ok(impls)) => to_json(&impls),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

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

    async fn stat_file(&self, Parameters(args): Parameters<StatFileArgs>) -> String {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || store.stat_file(&args.path)).await;
        match result {
            Ok(Ok(rec)) => to_json(&rec),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

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

    async fn add_developer_note(&self, Parameters(args): Parameters<AddDeveloperNoteArgs>) -> String {
        let mem = match self.memory.clone() {
            Some(m) => m,
            None => return err_json(NO_MEMORY_ERR),
        };
        // Cross-project leak gate: a note that names a filesystem path is almost
        // always a project/machine fact misfiled into the always-injected profile.
        // Reject with an actionable message unless the caller forces it.
        if !args.force.unwrap_or(false) {
            if let Some(marker) = looks_project_specific(&args.content) {
                return err_json(&format!(
                    "refused: this note contains a project/machine-specific path ('{marker}') and \
                     would leak into EVERY project via the always-injected developer profile. Save \
                     it with add_memory(project=…) instead. Pass force=true to store it in the \
                     profile anyway."
                ));
            }
        }
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

    async fn recall_impl(&self, Parameters(args): Parameters<RecallArgs>) -> String {
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

    /// Resolve a caller-supplied path to the ABSOLUTE form the index is keyed on.
    ///
    /// The graph stores absolute paths (the indexer walks an absolute root), but an
    /// agent naturally writes a repo-relative path (`crates/foo/src/lib.rs`) — which
    /// matched nothing and returned an empty list, the worst kind of failure: a
    /// silent one. A relative path is joined to the project root; an absolute path is
    /// passed through untouched.
    fn abs_path(&self, path: &str) -> String {
        let p = std::path::Path::new(path);
        if p.is_absolute() {
            return path.to_string();
        }
        let joined = self.root.join(p);
        std::fs::canonicalize(&joined)
            .unwrap_or(joined)
            .to_string_lossy()
            .to_string()
    }

    // ──────────────────────── consolidated tool surface (12) ────────────────────────
    //
    // The 42 fine-grained handlers above are now PRIVATE. Their schemas were loaded
    // into the model's context on EVERY request — measured at ~6.7k tokens of pure
    // overhead before a single question was asked, and MCP tool results stay in
    // context for the rest of the session. Twelve `op`-dispatching tools expose the
    // same capability for a fraction of that fixed cost.

    #[tool(description = "Search code by words or identifier. Identifier-aware: `Handler` finds `HttpRequestHandler`. kind: function|class|any. mode: lexical (default, free, exact-name-first)|semantic|hybrid. scope: limit to one sub-project in a monorepo.")]
    async fn search(&self, Parameters(a): Parameters<SearchArgs>) -> String {
        match a.mode.as_deref().unwrap_or("lexical") {
            "semantic" => {
                self.semantic_search_code(Parameters(SemanticSearchArgs {
                    query: a.query,
                    limit: a.limit,
                }))
                .await
            }
            "hybrid" => {
                self.find_existing(Parameters(FindExistingArgs {
                    query: a.query,
                    kind: a.kind,
                    limit: a.limit,
                }))
                .await
            }
            // Lexical is the default and the free path: identifier-aware, and it keeps
            // the exact-name boost that hybrid's RRF fusion discards.
            _ => self.search_lexical(Parameters(a)).await,
        }
    }

    #[tool(description = "Everything about a symbol. op: context (default: def+callers+callees) | def | callers | callees | references (every use, classified: definition/call/import/mention - precise, not grep) | tests (which tests reach it, transitively - empty means UNTESTED) | implementations | similar.")]
    async fn symbol(&self, Parameters(a): Parameters<SymbolArgs>) -> String {
        match a.op.as_deref().unwrap_or("context") {
            "def" => {
                let f = self
                    .get_function(Parameters(GetFunctionArgs {
                        name: a.name.clone(),
                        language: a.language.clone(),
                        with_body: a.with_body,
                    }))
                    .await;
                if f != "null" && !f.starts_with("{\"error\"") {
                    return f;
                }
                self.get_class(Parameters(GetClassArgs {
                    name: a.name,
                    language: a.language,
                    with_body: a.with_body,
                }))
                .await
            }
            "callers" => {
                self.get_callers(Parameters(CallersArgs {
                    name: a.name,
                    limit: a.limit,
                }))
                .await
            }
            "callees" => {
                self.get_callees(Parameters(CalleesArgs {
                    name: a.name,
                    limit: a.limit,
                }))
                .await
            }
            "implementations" => {
                self.find_implementations(Parameters(ImplementationsArgs { name: a.name }))
                    .await
            }
            "references" => {
                self.find_references(Parameters(CallersArgs {
                    name: a.name,
                    limit: a.limit,
                }))
                .await
            }
            "tests" => {
                self.find_tests_covering(Parameters(CallersArgs {
                    name: a.name,
                    limit: a.limit,
                }))
                .await
            }
            "similar" => {
                self.find_similar(Parameters(FindSimilarArgs {
                    qualified_name: a.name,
                    limit: a.limit,
                }))
                .await
            }
            _ => {
                self.get_symbol_context(Parameters(SymbolContextArgs {
                    name: a.name,
                    file_hint: a.file_hint,
                }))
                .await
            }
        }
    }

    #[tool(description = "Call/import graph. op: chain (target->to) | impact (reverse deps of a file) | deps (a file's deps).")]
    async fn graph(&self, Parameters(a): Parameters<GraphArgs>) -> String {
        match a.op.as_str() {
            "chain" => {
                let to = match a.to {
                    Some(t) => t,
                    None => return err_json("graph op=chain needs `to`"),
                };
                self.get_call_chain(Parameters(CallChainArgs {
                    from: a.target,
                    to,
                    max_depth: a.depth,
                }))
                .await
            }
            "impact" => {
                self.impact_analysis(Parameters(ImpactArgs {
                    path: self.abs_path(&a.target),
                    depth: a.depth,
                }))
                .await
            }
            "deps" => {
                self.find_dependencies(Parameters(DependenciesArgs {
                    path: self.abs_path(&a.target),
                    depth: a.depth,
                }))
                .await
            }
            other => err_json(&format!("unknown graph op `{other}` (chain|impact|deps)")),
        }
    }

    #[tool(description = "Architecture overview: stats, languages, hotspots, entry points, complex symbols. In a monorepo, call it WITHOUT scope first to list the sub-projects, then with scope=<member> to map one.")]
    async fn map(&self, Parameters(a): Parameters<MapArgs>) -> String {
        let store = self.store.clone();
        let top = a.top.unwrap_or(30);
        let scope = a.scope.clone();
        let result = tokio::task::spawn_blocking(move || {
            if let Some(scope) = scope.filter(|s| !s.trim().is_empty()) {
                return store.repo_map_scoped(top, &scope).map(MapReply::Scoped);
            }
            // No scope. In a MONOREPO, dumping the whole tree is not a map — measured on
            // a real one it returned 4485 files, ~800 entry points and a hotspot list
            // topped by a `get` with 6324 callers, all from a vendored upstream service
            // the caller never touches: ~10k tokens, a handful of them useful. So when
            // the tree has real members, answer with the MEMBERS and let the caller pick
            // one. A single-project repo has no members and gets the full map as before.
            let modules = store.list_modules()?;
            let members: Vec<(String, u64)> =
                modules.into_iter().filter(|(m, _)| !m.is_empty()).collect();
            if members.len() > 1 {
                return Ok(MapReply::Members(members));
            }
            store.repo_map(top).map(MapReply::Scoped)
        })
        .await;
        match result {
            Ok(Ok(MapReply::Scoped(map))) => to_json(&map),
            Ok(Ok(MapReply::Members(members))) => serde_json::json!({
                "monorepo": true,
                "hint": "this tree holds several sub-projects; call map again with scope=<name> \
                         (and search/symbol accept the same scope) — an unscoped map here is a \
                         dump, not a map",
                "sub_projects": members
                    .iter()
                    .map(|(m, n)| serde_json::json!({ "name": m, "files": n }))
                    .collect::<Vec<_>>(),
            })
            .to_string(),
            Ok(Err(e)) => err_json(&e.to_string()),
            Err(e) => err_json(&e.to_string()),
        }
    }

    #[tool(description = "Files. op: outline (symbols in a file) | list | stat | read (needs start/end).")]
    async fn file(&self, Parameters(a): Parameters<FileArgs>) -> String {
        match a.op.as_str() {
            "outline" => match a.path {
                Some(path) => {
                    let path = self.abs_path(&path);
                    self.get_file_outline(Parameters(FileOutlineArgs { path })).await
                }
                None => err_json("file op=outline needs `path`"),
            },
            "stat" => match a.path {
                Some(path) => {
                    let path = self.abs_path(&path);
                    self.stat_file(Parameters(StatFileArgs { path })).await
                }
                None => err_json("file op=stat needs `path`"),
            },
            "read" => match a.path {
                Some(path) => {
                    let path = self.abs_path(&path);
                    self.read_file(Parameters(ReadFileArgs {
                        path,
                        start: a.start,
                        end: a.end,
                    }))
                    .await
                }
                None => err_json("file op=read needs `path`"),
            },
            "list" => {
                self.list_files(Parameters(ListFilesArgs {
                    pattern: a.path,
                    language: a.language,
                    limit: a.limit,
                }))
                .await
            }
            other => err_json(&format!("unknown file op `{other}` (outline|list|stat|read)")),
        }
    }

    #[tool(description = "Regex search over stored symbol bodies. JSON array of GrepHit (path/line/snippet).")]
    async fn grep(&self, Parameters(a): Parameters<GrepCodeArgs>) -> String {
        self.grep_code(Parameters(a)).await
    }

    #[tool(description = "Codebase audit. op: dead (unreferenced) | unreachable (from entrypoints) | complex | routes | health (index drift).")]
    async fn audit(&self, Parameters(a): Parameters<AuditArgs>) -> String {
        let analysis = AnalysisArgs {
            language: a.language,
            limit: a.limit,
        };
        match a.op.as_str() {
            "dead" => self.find_dead_code(Parameters(analysis)).await,
            "unreachable" => self.find_unreachable(Parameters(analysis)).await,
            "complex" => self.find_complex_functions(Parameters(analysis)).await,
            "health" => self.doctor().await,
            "routes" => {
                self.find_routes(Parameters(FindRoutesArgs {
                    method: a.method,
                    path: a.path,
                    handler: a.handler,
                    limit: a.limit,
                }))
                .await
            }
            other => err_json(&format!(
                "unknown audit op `{other}` (dead|unreachable|complex|routes|health)"
            )),
        }
    }

    #[tool(description = "Grounded existence oracle: does a feature matching this description ACTUALLY exist? Returns VERDICT (EXISTS/WEAK/ABSENT) + evidence. Use before claiming something is missing.")]
    async fn check_exists(&self, Parameters(a): Parameters<CheckExistsArgs>) -> String {
        self.check_exists_impl(Parameters(a)).await
    }

    #[tool(description = "Flagship recall: one query -> relevant CODE and relevant MEMORY, ranked separately.")]
    async fn recall(&self, Parameters(a): Parameters<RecallArgs>) -> String {
        self.recall_impl(Parameters(a)).await
    }

    #[tool(description = "Cross-session memory. op: add | search | list | update | delete | resolve | search_all (all projects) | projects | for_symbol | why (decisions behind a symbol).")]
    async fn memory(&self, Parameters(a): Parameters<MemoryArgs>) -> String {
        let need = |v: Option<String>, what: &str| -> Result<String, String> {
            v.ok_or_else(|| err_json(&format!("memory op={} needs `{}`", a.op, what)))
        };
        match a.op.as_str() {
            "add" => {
                let (project, content) = match (a.project.clone(), a.content.clone()) {
                    (Some(p), Some(c)) => (p, c),
                    _ => return err_json("memory op=add needs `project` and `content`"),
                };
                self.add_memory(Parameters(AddMemoryArgs {
                    project,
                    content,
                    category: a.category,
                    tags: a.tags,
                    importance: a.importance,
                }))
                .await
            }
            "search" => {
                let (project, query) = match (a.project.clone(), a.query.clone()) {
                    (Some(p), Some(q)) => (p, q),
                    _ => return err_json("memory op=search needs `project` and `query`"),
                };
                self.search_memory(Parameters(SearchMemoryArgs {
                    project,
                    query,
                    n_results: a.limit,
                    category: a.category,
                    include_resolved: a.include_resolved,
                }))
                .await
            }
            "search_all" => match need(a.query.clone(), "query") {
                Ok(query) => {
                    self.search_all(Parameters(SearchAllArgs {
                        query,
                        n_results: a.limit,
                        category: a.category,
                        include_resolved: a.include_resolved,
                    }))
                    .await
                }
                Err(e) => e,
            },
            "list" => match need(a.project.clone(), "project") {
                Ok(project) => {
                    self.list_memories(Parameters(ListMemoriesArgs {
                        project,
                        category: a.category,
                        limit: a.limit,
                        include_resolved: a.include_resolved,
                    }))
                    .await
                }
                Err(e) => e,
            },
            "projects" => self.list_projects().await,
            "update" => match need(a.memory_id.clone(), "memory_id") {
                Ok(memory_id) => {
                    self.update_memory(Parameters(UpdateMemoryArgs {
                        memory_id,
                        content: a.content,
                        tags: a.tags,
                    }))
                    .await
                }
                Err(e) => e,
            },
            "delete" => match need(a.memory_id.clone(), "memory_id") {
                Ok(memory_id) => self.delete_memory(Parameters(DeleteMemoryArgs { memory_id })).await,
                Err(e) => e,
            },
            "resolve" => {
                let (memory_id, reason) = match (a.memory_id.clone(), a.reason.clone()) {
                    (Some(m), Some(r)) => (m, r),
                    _ => return err_json("memory op=resolve needs `memory_id` and `reason`"),
                };
                self.resolve_memory(Parameters(ResolveMemoryArgs { memory_id, reason }))
                    .await
            }
            "for_symbol" => {
                let (project, symbol) = match (a.project.clone(), a.symbol.clone()) {
                    (Some(p), Some(s)) => (p, s),
                    _ => return err_json("memory op=for_symbol needs `project` and `symbol`"),
                };
                self.code_to_memory(Parameters(CodeToMemoryArgs {
                    project,
                    symbol,
                    limit: a.limit,
                }))
                .await
            }
            "why" => {
                let (project, symbol) = match (a.project.clone(), a.symbol.clone()) {
                    (Some(p), Some(s)) => (p, s),
                    _ => return err_json("memory op=why needs `project` and `symbol`"),
                };
                self.why_this_exists(Parameters(WhyThisExistsArgs {
                    project,
                    symbol,
                    limit: a.limit,
                }))
                .await
            }
            other => err_json(&format!("unknown memory op `{other}`")),
        }
    }

    #[tool(description = "Session lifecycle. op: start (load developer profile + project context; call FIRST) | end (persist summary/decisions/completed/todos).")]
    async fn session(&self, Parameters(a): Parameters<SessionArgs>) -> String {
        match a.op.as_str() {
            "start" => {
                // Full MemoryRecords measured 19 KB (~4.8k tokens) per session start —
                // access_count, timestamps and ids the agent never reads. Compact to
                // `[category] content` lines; ids stay fetchable via memory op=list.
                let json = self
                    .session_start(Parameters(SessionStartArgs {
                        project: a.project,
                        n_project_memories: a.limit,
                        n_profile_notes: None,
                    }))
                    .await;
                compact_session_start(&json)
            }
            "end" => {
                self.session_end(Parameters(SessionEndArgs {
                    project: a.project,
                    summary: a.summary.unwrap_or_default(),
                    completed: a.completed,
                    decisions: a.decisions,
                    todos: a.todos,
                    session_id: a.session_id,
                }))
                .await
            }
            other => err_json(&format!("unknown session op `{other}` (start|end)")),
        }
    }

    #[tool(description = "Cross-project developer profile (preferences/rules true in EVERY project). op: note (save) | profile (read).")]
    async fn developer(&self, Parameters(a): Parameters<DeveloperArgs>) -> String {
        match a.op.as_str() {
            "note" => match a.content {
                Some(content) => {
                    self.add_developer_note(Parameters(AddDeveloperNoteArgs {
                        content,
                        category: a.category,
                        tags: a.tags,
                        importance: a.importance,
                        force: a.force,
                    }))
                    .await
                }
                None => err_json("developer op=note needs `content`"),
            },
            "profile" => {
                self.get_developer_profile(Parameters(GetDeveloperProfileArgs {
                    query: a.query,
                    n_results: a.limit,
                }))
                .await
            }
            other => err_json(&format!("unknown developer op `{other}` (note|profile)")),
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
/// Backfill AT MOST one small batch of missing chunk vectors on an interactive tool
/// call. Capped on purpose: `embed_pending` drains the queue until it is EMPTY, so an
/// uncapped call here turns a single `recall` / `semantic_search_code` into a
/// full-project embed that pins the GPU for the length of the tool call. The rest of
/// the queue is filled by an explicit `icode embed` (or the opt-in `serve` drain).
fn jit_embed(store: &SqliteCodeStore, embedder: &dyn Embedder) {
    if let Err(e) =
        icode_engine::embed_pending_capped(store, embedder, JIT_EMBED_BATCH, JIT_EMBED_BATCH)
    {
        eprintln!("icode: on-demand embed skipped ({e})");
    }
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| err_json(&e.to_string()))
}

/// `map` answers with either a real map or, in an unscoped monorepo, the list of members
/// to scope into.
enum MapReply {
    Scoped(icode_core::model::RepoMap),
    Members(Vec<(String, u64)>),
}

fn err_json(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

/// Per-memory content cap in the compacted session-start block.
const SESSION_LINE_MAX: usize = 240;

/// Compact the `session_start` payload.
///
/// The raw form serializes full `MemoryRecord`s — ids, timestamps, `access_count`,
/// tags — and measured 19 KB (~4.8k tokens) on a real project. An MCP tool result
/// stays in the model's context for the REST of the session, so that is 4.8k tokens
/// of rent paid once per session for fields the agent never reads. This renders the
/// same information as `[category] content` lines, truncated; the full records remain
/// one `memory op=list` away.
///
/// Best-effort: anything unexpected (an error object, a shape change) is passed
/// through untouched rather than swallowed.
fn compact_session_start(json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.to_string();
    };
    if v.get("error").is_some() {
        return json.to_string();
    }

    fn lines(v: &serde_json::Value, key: &str, out: &mut String) {
        let Some(arr) = v.get(key).and_then(|x| x.as_array()) else {
            return;
        };
        for rec in arr {
            let cat = rec.get("category").and_then(|c| c.as_str()).unwrap_or("general");
            let content = rec.get("content").and_then(|c| c.as_str()).unwrap_or("");
            let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
            let text: String = if flat.chars().count() > SESSION_LINE_MAX {
                flat.chars().take(SESSION_LINE_MAX).collect::<String>() + "…"
            } else {
                flat
            };
            out.push_str(&format!("- [{cat}] {text}\n"));
        }
    }

    let mut out = String::new();
    out.push_str("## Developer profile (cross-project — honour these):\n");
    lines(&v, "developer_profile", &mut out);
    out.push_str("\n## Recent project memory:\n");
    lines(&v, "project_context", &mut out);
    out
}

/// Guard for `add_developer_note`: the developer profile is CROSS-PROJECT — every
/// note is injected into every session of every project. A note that names a
/// concrete filesystem path is almost always a project- or machine-specific fact
/// misfiled here, and would then surface as noise in every unrelated project (the
/// exact leak this gate exists to stop). Returns the offending marker if the
/// content looks project-specific, else `None`.
///
/// Deliberately NARROW — only absolute filesystem paths — so it does not block
/// genuine universal rules (e.g. "apply SOLID in all projects", "answer in
/// Russian"), which virtually never embed an absolute path. The caller offers a
/// `force` escape for the rare universal rule that legitimately cites a path.
fn looks_project_specific(content: &str) -> Option<String> {
    const PATH_MARKERS: &[&str] =
        &["/home/", "/Users/", "/opt/", "/srv/", "/var/", "/mnt/", "/root/", "~/"];
    for m in PATH_MARKERS {
        if content.contains(m) {
            return Some((*m).to_string());
        }
    }
    // Windows drive path (e.g. `C:\`): a letter, ':', backslash.
    let b = content.as_bytes();
    for i in 0..b.len().saturating_sub(2) {
        if b[i].is_ascii_alphabetic() && b[i + 1] == b':' && b[i + 2] == b'\\' {
            return Some(format!("{}:\\", b[i] as char));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_note_gate_flags_paths_not_universal_rules() {
        // Project/machine-specific → flagged (would leak into every project).
        assert!(looks_project_specific("Vexa Pay lives at /home/worker/PROJECTS/vexa/pay").is_some());
        assert!(looks_project_specific("config under ~/.icode/icode.db").is_some());
        assert!(looks_project_specific("checkout is C:\\src\\app").is_some());
        // Genuine cross-project developer rules → NOT flagged.
        assert!(looks_project_specific("Apply SOLID in all projects").is_none());
        assert!(looks_project_specific("Always answer the user in Russian").is_none());
        assert!(
            looks_project_specific("Never put real names in tests/commits (all projects)").is_none()
        );
    }

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

        // find_similar is NO LONGER a semantic tool: it is MinHash over token
        // shingles, so it works with the embedder absent. On an empty db that means
        // an empty list — NOT the unavailable error it used to return.
        let fs = server
            .find_similar(Parameters(FindSimilarArgs {
                qualified_name: "whatever".into(),
                limit: None,
            }))
            .await;
        assert_eq!(fs, "[]", "find_similar works with no embedder (empty db → [])");

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
