//! rmcp MCP server (stdio). Exposes the full group-A read surface of the
//! code-graph as MCP tools (search / fetch / call-graph / analysis / files).
//!
//! Rule (frozen): `#[tool]` functions contain NO logic — they deserialize args,
//! call into `icode-engine` (the heavy sync call wrapped in `spawn_blocking`),
//! and project the result to JSON. rmcp API drift stays isolated here.

use std::sync::Arc;

use icode_core::model::{CodeQuery, Language, SearchMode, SymbolKind};
use icode_core::traits::{CodeReadStore, Embedder};
use icode_engine::SqliteCodeStore;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};

/// Error string returned by the semantic tools when no embedder was wired up
/// (Ollama unreachable at startup). The server stays useful in lexical-only mode.
const NO_EMBEDDER_ERR: &str =
    "semantic search unavailable: no embedder (is ollama running?)";

#[derive(Clone)]
pub struct CodeMcpServer {
    store: Arc<SqliteCodeStore>,
    /// Optional embedder for the semantic / hybrid tools. `None` when the binary
    /// could not reach Ollama at startup: semantic tools then return a clear
    /// error and `find_existing` / `get_symbol_context` degrade to lexical-only.
    embedder: Option<Arc<dyn Embedder>>,
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

// ──────────────────────────── tools ────────────────────────────

#[tool_router]
impl CodeMcpServer {
    /// Construct the server. `embedder` is `Some` when semantic/hybrid search is
    /// available, `None` for lexical-only (Ollama down at startup).
    pub fn new(store: Arc<SqliteCodeStore>, embedder: Option<Arc<dyn Embedder>>) -> Self {
        Self { store, embedder, tool_router: Self::tool_router() }
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

    #[tool(description = "Find existing functions/classes that already do what you describe (avoid duplicate work). Semantic+lexical RRF fusion when an embedder is available, else lexical-only. JSON array of CodeHit.")]
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

    #[tool(description = "Direct callers of a symbol (by-name call graph). JSON array of Call.")]
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

    #[tool(description = "Direct callees of a symbol (by-name call graph). JSON array of Call.")]
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

    #[tool(description = "Functions never called anywhere (candidate dead code; entry points excluded). JSON array of CodeHit.")]
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

    #[tool(description = "Functions unreachable from any entry point (catches dead clusters, not just zero-in-degree). JSON array of CodeHit.")]
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
/// `Some` to enable the semantic/hybrid tools, `None` for lexical-only.
pub async fn serve_stdio(
    store: Arc<SqliteCodeStore>,
    embedder: Option<Arc<dyn Embedder>>,
) -> anyhow::Result<()> {
    let service = CodeMcpServer::new(store, embedder)
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;
    service.waiting().await?;
    Ok(())
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
        let server = CodeMcpServer::new(store, None);

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
}
