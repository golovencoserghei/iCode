//! `icode-serve` — the rmcp MCP server (M0.5 stdio first; HTTP in M8) and the
//! axum web dashboard (M7, bound to 127.0.0.1).
//!
//! Rule (frozen): `#[tool]` functions contain NO logic — they only deserialize
//! args, call into `icode-engine`, and project the result to JSON. This keeps
//! rmcp API drift isolated to one routing layer.

pub mod mcp;
pub mod web;

pub use mcp::{serve_stdio, CodeMcpServer};
pub use web::{serve as serve_web, WebState};
