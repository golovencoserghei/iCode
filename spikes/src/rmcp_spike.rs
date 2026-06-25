//! M0 spike: lock the rmcp 1.8 tool-server API pattern (compile gate).
//!
//! Goal: confirm the current rmcp version's `#[tool_router]` / `#[tool]` /
//! `ServerHandler` / `serve(stdio())` shape compiles in our setup, so the
//! contract freeze + M0.5 walking skeleton build on a known-good pattern.
//! The live stdio handshake against Claude Code happens in M0.5; here we only
//! need it to BUILD.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};

#[derive(Clone)]
struct DemoServer {
    tool_router: ToolRouter<Self>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct EchoArgs {
    /// text to echo back
    text: String,
}

#[tool_router]
impl DemoServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Echo back the provided text")]
    async fn echo(&self, Parameters(args): Parameters<EchoArgs>) -> String {
        args.text
    }
}

#[tool_handler]
impl ServerHandler for DemoServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some("icode v2 rmcp spike".into());
        info
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Build-gate: we don't block on a real client here. Guard the serve loop so
    // the binary is runnable but the spike's purpose (compile) is satisfied.
    if std::env::var("ICODE_RUN_STDIO").is_ok() {
        let service = DemoServer::new().serve((tokio::io::stdin(), tokio::io::stdout())).await?;
        service.waiting().await?;
    } else {
        // Touch the constructor so it isn't dead-code-eliminated.
        let _ = DemoServer::new();
        println!("rmcp_spike: built OK (set ICODE_RUN_STDIO=1 to serve over stdio)");
    }
    Ok(())
}
