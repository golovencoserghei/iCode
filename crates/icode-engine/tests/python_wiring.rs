//! Python wires handlers in two ways a call graph CANNOT see. Both were invisible.
//!
//! 1. **Decorators.** `@router.post("/x")` binds a handler to an endpoint; the
//!    framework invokes it, so no call edge exists. Measured on a real FastAPI
//!    service: 132 endpoints, every one of them absent from the index. An agent
//!    reading that graph cannot see the API surface at all — which is exactly how it
//!    ends up saying "looks fine" about a module it cannot see.
//!
//! 2. **Dispatch tables.** `{"list_tasks": h._list_tasks}` holds a REFERENCE; the call
//!    happens later as `self._handlers[name](args)`, which names no callee. All 34 tool
//!    handlers in that service were reported as dead code.
//!
//! Both make the graph LIE — not by omission but by asserting the opposite: that live,
//! reachable code is unreachable. These tests pin both.

use std::fs;

use icode_core::traits::CodeReadStore;
use icode_engine::{index_path, SqliteCodeStore};

const SAMPLE: &str = r#"
from fastapi import APIRouter

router = APIRouter()
app = FastAPI()


@router.get("/health")
def health_check():
    return {"ok": True}


@router.post("/hooks/jira", tags=["hooks"])
async def jira_hook(payload: dict):
    return handle(payload)


@app.route("/legacy", methods=["POST"])
def legacy_view():
    return "ok"


@self.client.event
async def on_message(msg):
    return msg


@property
def not_a_route(self):
    return 1


class Executor:
    def __init__(self, h):
        # A dispatch table: the handlers are referenced, never called here.
        self._handlers = {
            "list_tasks": h._list_tasks,
            "get_task": h._get_task,
        }

    def run(self, name, args):
        return self._handlers[name](args)
"#;

fn indexed() -> (SqliteCodeStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("api.py"), SAMPLE).expect("write");
    let store = SqliteCodeStore::open(dir.path()).expect("open");
    index_path(dir.path(), &store).expect("index");
    (store, dir)
}

#[test]
fn fastapi_and_flask_routes_are_extracted_with_their_handlers() {
    let (store, _d) = indexed();
    let routes = store.find_routes(None, None, None, 50).expect("find_routes");

    let get = routes
        .iter()
        .find(|r| r.route == "/health")
        .expect("GET /health must be indexed");
    assert_eq!(get.method, "GET");
    assert_eq!(get.handler_method.as_deref(), Some("health_check"));

    let post = routes
        .iter()
        .find(|r| r.route == "/hooks/jira")
        .expect("POST /hooks/jira must be indexed");
    assert_eq!(post.method, "POST");
    assert_eq!(post.handler_method.as_deref(), Some("jira_hook"));

    // Flask: the verb comes from the `methods=` kwarg, not the decorator name.
    let flask = routes
        .iter()
        .find(|r| r.route == "/legacy")
        .expect("Flask @app.route must be indexed");
    assert_eq!(flask.method, "POST", "methods=[\"POST\"] must win over the GET default");
}

#[test]
fn event_hooks_are_wiring_too_not_orphans() {
    let (store, _d) = indexed();
    let routes = store.find_routes(None, None, None, 50).expect("find_routes");
    let ev = routes
        .iter()
        .find(|r| r.handler_method.as_deref() == Some("on_message"))
        .expect("`@client.event` binds a handler — it must be indexed as wiring");
    assert_eq!(ev.method, "EVENT");
}

#[test]
fn a_property_is_not_mistaken_for_a_route() {
    let (store, _d) = indexed();
    let routes = store.find_routes(None, None, None, 50).expect("find_routes");
    assert!(
        !routes.iter().any(|r| r.handler_method.as_deref() == Some("not_a_route")),
        "`@property` is not wiring — guessing would be worse than saying nothing"
    );
}

#[test]
fn dispatch_table_handlers_are_reachable_not_dead_code() {
    let (store, _d) = indexed();
    // The registry references `_list_tasks`; nothing CALLS it. Before this, the graph
    // said "called by nobody" — and an agent believes the graph.
    let callers = store.get_callers("_list_tasks", 20).expect("get_callers");
    assert!(
        callers.iter().any(|c| c.caller.contains("__init__")),
        "a handler in a dispatch table must be reachable from the function that builds \
         the table, got {:?}",
        callers.iter().map(|c| &c.caller).collect::<Vec<_>>()
    );
}
