//! Local web dashboard (M7) — a read-only HTTP view over the same code-graph and
//! cross-session memory the MCP server exposes.
//!
//! Invariants (frozen):
//!   * **Locality**: the server binds STRICTLY to `127.0.0.1`. Never `0.0.0.0` —
//!     the index/memory are local-only and must not be reachable off-host.
//!   * **No business logic here**: handlers deserialize query params, call into
//!     `icode-engine` (the heavy sync call wrapped in `spawn_blocking`), and
//!     project the result to JSON — mirroring the `mcp.rs` rule so engine drift
//!     stays isolated to thin routing layers.
//!   * **Graceful degradation**: with no embedder, code search falls back to
//!     lexical; with no memory store, the memory/recall endpoints return empty
//!     results (never a panic, never a 500 for "Ollama is down").
//!
//! The front-end is ONE embedded HTML page (`dashboard.html`, vanilla HTML+CSS+JS,
//! no CDN / no network) served at `/`; it polls the REST API on the same origin.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use icode_core::model::{CodeQuery, SearchMode};
use icode_core::traits::{CodeReadStore, Embedder, MemoryStore};
use icode_engine::{recall, search, SqliteCodeStore};
use serde::Deserialize;

/// The single embedded dashboard page (no external resources, no network).
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Shared, cheaply-cloneable handle to the engine surfaces. Carries exactly the
/// same `Arc`s the MCP server holds; the bin constructs them once and hands them
/// to both layers. `embedder`/`memory` are `Option` (None = Ollama was down at
/// startup → degrade, don't fail).
#[derive(Clone)]
pub struct WebState {
    store: Arc<SqliteCodeStore>,
    embedder: Option<Arc<dyn Embedder>>,
    memory: Option<Arc<dyn MemoryStore>>,
}

impl WebState {
    pub fn new(
        store: Arc<SqliteCodeStore>,
        embedder: Option<Arc<dyn Embedder>>,
        memory: Option<Arc<dyn MemoryStore>>,
    ) -> Self {
        Self {
            store,
            embedder,
            memory,
        }
    }
}

/// Build the axum router (state-injected). Exposed separately from [`serve`] so
/// tests can exercise the routes via `tower::ServiceExt::oneshot` without binding
/// a socket.
pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/stats", get(api_stats))
        .route("/api/repo-map", get(api_repo_map))
        .route("/api/search/code", get(api_search_code))
        .route("/api/symbol", get(api_symbol))
        .route("/api/routes", get(api_routes))
        .route("/api/memory/projects", get(api_memory_projects))
        .route("/api/memory", get(api_memory))
        .route("/api/recall", get(api_recall))
        .with_state(state)
}

/// Serve the dashboard on `127.0.0.1:<port>` until the process is killed.
///
/// Binding is hard-coded to loopback (`Ipv4Addr::LOCALHOST`) — the locality
/// invariant is not configurable. The startup line goes to **stderr** so callers
/// piping/redirecting stdout are unaffected.
pub async fn serve(
    store: Arc<SqliteCodeStore>,
    embedder: Option<Arc<dyn Embedder>>,
    memory: Option<Arc<dyn MemoryStore>>,
    port: u16,
) -> anyhow::Result<()> {
    let state = WebState::new(store, embedder, memory);
    let app = router(state);

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("iCode dashboard: http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

// ──────────────────────────── query param structs ────────────────────────────

#[derive(Deserialize)]
struct RepoMapParams {
    top: Option<usize>,
}

#[derive(Deserialize)]
struct SearchCodeParams {
    #[serde(default)]
    q: String,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct SymbolParams {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct RoutesParams {
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct MemoryParams {
    #[serde(default)]
    project: String,
    #[serde(default)]
    q: String,
    limit: Option<usize>,
    #[serde(default)]
    include_resolved: bool,
}

#[derive(Deserialize)]
struct RecallParams {
    #[serde(default)]
    project: String,
    #[serde(default)]
    q: String,
    limit: Option<usize>,
}

// ──────────────────────────── handlers ────────────────────────────

/// `GET /` → the embedded dashboard page.
async fn index() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

/// `GET /api/stats` → `DbStats`.
async fn api_stats(State(st): State<WebState>) -> impl IntoResponse {
    let store = st.store.clone();
    blocking_json(move || store.stats()).await
}

/// `GET /api/repo-map?top=30` → `RepoMap`.
async fn api_repo_map(
    State(st): State<WebState>,
    Query(p): Query<RepoMapParams>,
) -> impl IntoResponse {
    let store = st.store.clone();
    let top = p.top.unwrap_or(30);
    blocking_json(move || store.repo_map(top)).await
}

/// `GET /api/search/code?q=...&limit=20` → `Vec<CodeHit>`.
///
/// Hybrid (semantic + lexical RRF) when an embedder is wired up; lexical-only
/// otherwise — the same degradation the MCP `find_existing` tool uses.
async fn api_search_code(
    State(st): State<WebState>,
    Query(p): Query<SearchCodeParams>,
) -> impl IntoResponse {
    let store = st.store.clone();
    let embedder = st.embedder.clone();
    let q = p.q;
    let limit = p.limit.unwrap_or(20);
    blocking_json(move || match embedder.as_deref() {
        Some(emb) => search::hybrid_search(&store, emb, &q, limit),
        None => store.search_code(&CodeQuery {
            text: q,
            kind: None,
            lang: None,
            limit,
            mode: SearchMode::Lexical,
            with_body: false,
        }),
    })
    .await
}

/// `GET /api/symbol?name=...` → `SymbolContext` (definition + callers/callees +
/// imports/routes/implementations + semantically-similar symbols when available).
async fn api_symbol(State(st): State<WebState>, Query(p): Query<SymbolParams>) -> impl IntoResponse {
    let store = st.store.clone();
    let embedder = st.embedder.clone();
    let name = p.name;
    blocking_json(move || {
        let mut ctx = store.symbol_context(&name, None)?;
        // Enrich `similar_symbols` semantically when an embedder is available; the
        // store leaves the field empty in lexical-only mode (mirrors mcp.rs).
        if let (Some(emb), Some(def)) = (embedder.as_deref(), ctx.definition.as_ref()) {
            let qn = match def {
                icode_core::model::FunctionOrClass::Function(f) => f.qualified_name.clone(),
                icode_core::model::FunctionOrClass::Class(c) => c.qualified_name.clone(),
            };
            if let Ok(similar) = search::find_similar(&store, emb, &qn, 8) {
                ctx.similar_symbols = similar;
            }
        }
        Ok(ctx)
    })
    .await
}

/// `GET /api/routes?limit=50` → `Vec<Route>`.
async fn api_routes(State(st): State<WebState>, Query(p): Query<RoutesParams>) -> impl IntoResponse {
    let store = st.store.clone();
    let limit = p.limit.unwrap_or(50);
    blocking_json(move || store.find_routes(None, None, None, limit)).await
}

/// `GET /api/memory/projects` → `Vec<(project, count)>` (or `[]` with no memory).
async fn api_memory_projects(State(st): State<WebState>) -> impl IntoResponse {
    let mem = match st.memory.clone() {
        Some(m) => m,
        None => return Json(serde_json::json!([])).into_response(),
    };
    blocking_json(move || mem.list_projects()).await
}

/// `GET /api/memory?project=...&q=...&limit=20&include_resolved=false`.
///
/// `q` non-empty → semantic `search`; `q` empty → `list`. Empty array with no
/// memory store (Ollama down). Search returns `MemoryHit`s; list returns
/// `MemoryRecord`s — both serialize transparently for the front-end.
async fn api_memory(State(st): State<WebState>, Query(p): Query<MemoryParams>) -> impl IntoResponse {
    let mem = match st.memory.clone() {
        Some(m) => m,
        None => return Json(serde_json::json!([])).into_response(),
    };
    let project = p.project;
    let q = p.q;
    let limit = p.limit.unwrap_or(20);
    let include_resolved = p.include_resolved;
    if q.trim().is_empty() {
        blocking_json(move || mem.list(&project, None, limit, include_resolved)).await
    } else {
        blocking_json(move || mem.search(&project, &q, limit, None, include_resolved)).await
    }
}

/// `GET /api/recall?project=...&q=...&limit=8` → `RecallResult`
/// (`{relevant_code, relevant_memory, facts}`, each ranked in its own space).
/// Degrades cleanly: no embedder → lexical code; no memory → empty memory section.
async fn api_recall(State(st): State<WebState>, Query(p): Query<RecallParams>) -> impl IntoResponse {
    let store = st.store.clone();
    let embedder = st.embedder.clone();
    let memory = st.memory.clone();
    let project = p.project;
    let q = p.q;
    let limit = p.limit.unwrap_or(8);
    blocking_json(move || {
        recall(
            &store,
            embedder.as_deref(),
            memory.as_deref(),
            &project,
            &q,
            limit,
        )
    })
    .await
}

// ──────────────────────────── plumbing ────────────────────────────

/// Run a heavy sync engine call on the blocking pool and project the result to a
/// JSON response. Engine errors and join failures both degrade to a 200 with an
/// `{"error": "..."}` body (the front-end renders it as a friendly state) — never
/// a panic. The closure returns the engine's `icode_core::error::Result<T>`.
async fn blocking_json<T, F>(f: F) -> axum::response::Response
where
    T: serde::Serialize + Send + 'static,
    F: FnOnce() -> icode_core::error::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(e)) => err_response(&e.to_string()),
        Err(e) => err_response(&e.to_string()),
    }
}

/// A 200 carrying `{"error": "..."}` — the dashboard JS checks for this key and
/// shows a readable message instead of a hard failure.
fn err_response(msg: &str) -> axum::response::Response {
    (StatusCode::OK, Json(serde_json::json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // for `oneshot`

    fn empty_state() -> WebState {
        let store = Arc::new(SqliteCodeStore::open_in_memory().expect("store"));
        // No embedder, no memory — the degraded (lexical-only / memory-less) path.
        WebState::new(store, None, None)
    }

    async fn body_string(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    /// `/` serves the embedded HTML page (starts with a doctype/html tag).
    #[tokio::test]
    async fn index_serves_html() {
        let app = router(empty_state());
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.trim_start().to_lowercase().starts_with("<!doctype"));
    }

    /// `/api/stats` returns a JSON object with the `files` counter (empty store → 0).
    #[tokio::test]
    async fn stats_endpoint_returns_json() {
        let app = router(empty_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(v["files"], 0);
    }

    /// With no memory store, `/api/memory/projects` degrades to an empty array
    /// (not an error, not a 500).
    #[tokio::test]
    async fn memory_projects_empty_without_store() {
        let app = router(empty_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/memory/projects")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "[]");
    }

    /// `/api/recall` degrades cleanly without embedder/memory: a well-formed
    /// `RecallResult` with all three sections present (empty).
    #[tokio::test]
    async fn recall_degrades_without_backends() {
        let app = router(empty_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/recall?project=demo&q=parsing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert!(v["relevant_code"].is_array());
        assert!(v["relevant_memory"].is_array());
        assert!(v["facts"].is_array());
    }
}
