//! Wave 1 — receiver-aware call resolution + edge confidence.
//!
//! Proves, end-to-end (parse → store → resolve pass → read surface):
//!   (a) two same-named methods in different classes reached via `self`/`$this`
//!       no longer MERGE in get_callers / get_callees;
//!   (b) an explicit `Class::method` call resolves exactly to that class;
//!   (c) `confidence` is stamped per the deterministic scale (0.9/0.7/0.6/0.4/0.3);
//!   (d) a dynamic receiver (`$var`) degrades to bare-name at low confidence.

use std::fs;

use icode_core::model::Call;
use icode_core::traits::CodeReadStore;
use icode_engine::{index_path, SqliteCodeStore};

// ──────────────────────────── PHP: the full matrix ────────────────────────────

// a.php — ServiceA (self→handle 0.9, free helper 0.6, explicit Model::find 0.9),
// plus a "probe" method carrying the dynamic (0.4) and unresolved (0.3) edges so
// ServiceA::run stays clean for the get_callers exclusivity check.
const A_PHP: &str = r#"<?php

class ServiceA {
    public function run() {
        $this->handle();     // self  → ServiceA::handle (unique)   → 0.9
        helper_fn();         // free  → helper_fn        (unique)   → 0.6
        return Model::find(); // Class → Model::find      (unique)   → 0.9
    }
    public function handle() {}
    public function probe($x) {
        $x->handle();        // dynamic $x, 'handle' has 2 defs      → 0.4
        $x->missing_method();// dynamic $x, no such def anywhere     → 0.3
    }
}

function helper_fn() {}

class Model {
    public static function find() {}
}
"#;

// b.php — ServiceB with its OWN handle(); its $this->handle() must resolve to
// ServiceB::handle, never ServiceA::handle (the merge bug).
const B_PHP: &str = r#"<?php

class ServiceB {
    public function run() {
        $this->handle();     // self → ServiceB::handle → 0.9
    }
    public function handle() {}
}
"#;

// d.php + e.php — two classes with the SAME qualified name Widget::paint, so a
// self-call to paint() resolves to a qualified target with >1 candidate → 0.7.
const D_PHP: &str = r#"<?php

class Widget {
    public function render() {
        $this->paint();      // self → Widget::paint (2 defs) → 0.7
    }
    public function paint() {}
}
"#;

const E_PHP: &str = r#"<?php

class Widget {
    public function paint() {}
}
"#;

/// Find the (single) call with the given callee whose caller matches `caller`.
fn call_of<'a>(calls: &'a [Call], caller: &str, callee: &str) -> &'a Call {
    calls
        .iter()
        .find(|c| c.caller == caller && c.callee == callee)
        .unwrap_or_else(|| panic!("no {caller} → {callee} in {:?}",
            calls.iter().map(|c| (&c.caller, &c.callee, &c.resolved_callee, c.confidence)).collect::<Vec<_>>()))
}

#[test]
fn php_receiver_aware_resolution_and_confidence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("a.php"), A_PHP).unwrap();
    fs::write(root.join("b.php"), B_PHP).unwrap();
    fs::write(root.join("d.php"), D_PHP).unwrap();
    fs::write(root.join("e.php"), E_PHP).unwrap();

    let store = SqliteCodeStore::open(root).expect("open store");
    index_path(root, &store).expect("index");

    // ── (a) homonyms do NOT merge in get_callers ──────────────────────────────
    // Callers of ServiceA::handle = ONLY ServiceA::run (via $this→ServiceA::handle).
    let callers_a = store.get_callers("ServiceA::handle", 50).expect("callers A");
    assert!(!callers_a.is_empty(), "ServiceA::handle must have a caller");
    assert!(
        callers_a.iter().all(|c| c.caller == "ServiceA::run"),
        "ServiceA::handle callers must be ServiceA::run ONLY, got {:?}",
        callers_a.iter().map(|c| &c.caller).collect::<Vec<_>>()
    );
    assert!(
        !callers_a.iter().any(|c| c.caller == "ServiceB::run"),
        "ServiceB::run must NOT be a caller of ServiceA::handle (merge bug)"
    );

    // Symmetric: callers of ServiceB::handle = ONLY ServiceB::run.
    let callers_b = store.get_callers("ServiceB::handle", 50).expect("callers B");
    assert!(
        !callers_b.is_empty() && callers_b.iter().all(|c| c.caller == "ServiceB::run"),
        "ServiceB::handle callers must be ServiceB::run ONLY, got {:?}",
        callers_b.iter().map(|c| &c.caller).collect::<Vec<_>>()
    );

    // ── (a) get_callees carries the disambiguated target ──────────────────────
    let callees_a = store.get_callees("ServiceA::run", 50).expect("callees A");
    let a_handle = call_of(&callees_a, "ServiceA::run", "handle");
    assert_eq!(a_handle.resolved_callee.as_deref(), Some("ServiceA::handle"));
    let callees_b = store.get_callees("ServiceB::run", 50).expect("callees B");
    let b_handle = call_of(&callees_b, "ServiceB::run", "handle");
    assert_eq!(b_handle.resolved_callee.as_deref(), Some("ServiceB::handle"));

    // ── (b) explicit Class::method resolves exactly ───────────────────────────
    let find_callers = store.get_callers("Model::find", 50).expect("callers find");
    assert!(
        find_callers.iter().any(|c| c.caller == "ServiceA::run"),
        "Model::find must be called by ServiceA::run, got {:?}",
        find_callers.iter().map(|c| &c.caller).collect::<Vec<_>>()
    );
    let a_find = call_of(&callees_a, "ServiceA::run", "find");
    assert_eq!(a_find.resolved_callee.as_deref(), Some("Model::find"));

    // ── (c) confidence scale ──────────────────────────────────────────────────
    // 0.9 — self-call, qualified target unique (ServiceA::handle).
    assert_eq!(a_handle.confidence, 0.9, "self→unique method = 0.9");
    // 0.9 — explicit Class::method, unique (Model::find).
    assert_eq!(a_find.confidence, 0.9, "Class::method unique = 0.9");
    // 0.6 — free call, bare name unique (helper_fn).
    let a_helper = call_of(&callees_a, "ServiceA::run", "helper_fn");
    assert!(a_helper.resolved_callee.is_none(), "free call stays bare");
    assert_eq!(a_helper.confidence, 0.6, "bare unique = 0.6");
    // 0.7 — self-call whose qualified target has >1 definition (Widget::paint).
    let callees_render = store.get_callees("Widget::render", 50).expect("callees render");
    let paint = call_of(&callees_render, "Widget::render", "paint");
    assert_eq!(paint.resolved_callee.as_deref(), Some("Widget::paint"));
    assert_eq!(paint.confidence, 0.7, "qualified but 2 defs = 0.7");

    // ── (d) dynamic receiver → bare-name, low confidence ──────────────────────
    let probe = store.get_callees("ServiceA::probe", 50).expect("callees probe");
    // 0.4 — dynamic $x->handle(): bare 'handle' collides (2 defs).
    let dyn_handle = call_of(&probe, "ServiceA::probe", "handle");
    assert!(dyn_handle.resolved_callee.is_none(), "dynamic receiver stays bare");
    assert_eq!(dyn_handle.confidence, 0.4, "bare collision (2 defs) = 0.4");
    // 0.3 — dynamic $x->missing_method(): unresolved (0 defs).
    let missing = call_of(&probe, "ServiceA::probe", "missing_method");
    assert!(missing.resolved_callee.is_none());
    assert_eq!(missing.confidence, 0.3, "unresolved = 0.3");
}

// ──────────────────────────── Rust: self.method disambiguation ─────────────────

// The flagship example (`Service::run` vs `Other::run`) in Rust: two impls with a
// same-named `handle`, each called via `self.handle()`. Cross-file targets, self
// receiver, `::` qualifier — proves the qualifier is language-correct.
const SVC_A_RS: &str = r#"
pub struct ServiceA;
impl ServiceA {
    pub fn run(&self) { self.handle(); }
    pub fn handle(&self) {}
}
"#;

const SVC_B_RS: &str = r#"
pub struct ServiceB;
impl ServiceB {
    pub fn run(&self) { self.handle(); }
    pub fn handle(&self) {}
}
"#;

#[test]
fn rust_self_calls_do_not_merge() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("svc_a.rs"), SVC_A_RS).unwrap();
    fs::write(root.join("svc_b.rs"), SVC_B_RS).unwrap();

    let store = SqliteCodeStore::open(root).expect("open store");
    index_path(root, &store).expect("index");

    let callers_a = store.get_callers("ServiceA::handle", 50).expect("callers A");
    assert!(
        !callers_a.is_empty() && callers_a.iter().all(|c| c.caller == "ServiceA::run"),
        "ServiceA::handle callers must be ServiceA::run ONLY, got {:?}",
        callers_a.iter().map(|c| &c.caller).collect::<Vec<_>>()
    );

    let callees_a = store.get_callees("ServiceA::run", 50).expect("callees A");
    let edge = call_of(&callees_a, "ServiceA::run", "handle");
    assert_eq!(edge.resolved_callee.as_deref(), Some("ServiceA::handle"));
    assert_eq!(edge.confidence, 0.9, "self→unique method = 0.9");

    // Bare query keeps recall: get_callers("handle") sees BOTH classes' edges.
    let bare = store.get_callers("handle", 50).expect("bare callers");
    assert!(
        bare.iter().any(|c| c.caller == "ServiceA::run") && bare.iter().any(|c| c.caller == "ServiceB::run"),
        "bare 'handle' query must retain full recall, got {:?}",
        bare.iter().map(|c| &c.caller).collect::<Vec<_>>()
    );
}
