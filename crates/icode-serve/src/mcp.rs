//! rmcp MCP server (stdio). Exposes the M0.5 code tools: `get_stats` and
//! `search_function`.
//!
//! Rule (frozen): `#[tool]` functions contain NO logic — they deserialize args,
//! call into `icode-engine` (the heavy sync call wrapped in `spawn_blocking`),
//! and project the result to JSON. rmcp API drift stays isolated here.

use std::sync::Arc;

use icode_core::model::{CodeQuery, SearchMode, SymbolKind};
use icode_core::traits::CodeReadStore;
use icode_engine::SqliteCodeStore;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};

#[derive(Clone)]
pub struct CodeMcpServer {
    store: Arc<SqliteCodeStore>,
    // Consumed by the `#[tool_handler]` macro expansion (routes tool calls);
    // dead-code analysis can't see that path, so the read is suppressed here.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SearchFunctionArgs {
    /// Lexical query matched against function name / qualified name.
    pub query: String,
    /// Max hits to return (default 10).
    pub limit: Option<usize>,
}

#[tool_router]
impl CodeMcpServer {
    pub fn new(store: Arc<SqliteCodeStore>) -> Self {
        Self { store, tool_router: Self::tool_router() }
    }

    #[tool(description = "Return code-graph statistics (file/function counts) as JSON.")]
    async fn get_stats(&self) -> String {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || store.stats()).await;
        match result {
            Ok(Ok(stats)) => to_json(&stats),
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
}

#[tool_handler]
impl ServerHandler for CodeMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some("iCode v2 code-graph MCP server (M0.5)".into());
        info
    }
}

/// Serve the MCP protocol over stdio until the client disconnects.
pub async fn serve_stdio(store: Arc<SqliteCodeStore>) -> anyhow::Result<()> {
    let service = CodeMcpServer::new(store)
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;
    service.waiting().await?;
    Ok(())
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| err_json(&e.to_string()))
}

fn err_json(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}
