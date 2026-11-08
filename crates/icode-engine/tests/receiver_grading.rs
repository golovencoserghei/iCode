//! A call written ON a receiver is graded by what the receiver is.
//!
//! `Math.round()` next to a local `function round`, `timer.unref()` next to a free
//! `unref`: the old grading saw one definition named `round`/`unref` and stamped
//! the edge 0.6 "bare unique", binding a builtin/unknown-value method call to an
//! unrelated free function. Now: a receiver that pins a project type (`this`,
//! `self`, `Class.`) keeps its exact grade; a project module the file imports is
//! module access (a free call, graded as before); anything else is at most
//! ambiguous and never a caller of a free function.

use std::fs;
use std::path::Path;

use icode_core::model::{Call, Certainty};
use icode_core::traits::CodeReadStore;
use icode_engine::{index_path, SqliteCodeStore};

fn indexed(files: &[(&str, &str)]) -> (tempfile::TempDir, SqliteCodeStore) {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in files {
        let p = dir.path().join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
    let store = SqliteCodeStore::open(dir.path()).unwrap();
    index_path(dir.path(), &store).unwrap();
    (dir, store)
}

fn edge<'a>(calls: &'a [Call], caller_suffix: &str, callee: &str) -> &'a Call {
    calls
        .iter()
        .find(|c| c.caller.ends_with(caller_suffix) && c.callee == callee)
        .unwrap_or_else(|| {
            panic!(
                "no {caller_suffix} -> {callee} in {:?}",
                calls.iter().map(|c| (&c.caller, &c.callee, &c.receiver, c.confidence)).collect::<Vec<_>>()
            )
        })
}

fn callees(store: &SqliteCodeStore, name: &str) -> Vec<Call> {
    store.get_callees(name, 100).unwrap()
}

#[test]
fn javascript_builtin_and_value_receivers_are_not_bound_to_free_functions() {
    let (_d, store) = indexed(&[
        (
            "app.js",
            r#"
import * as util from './util';
function round(x) { return x; }
function unref() { return 0; }
function compute(timer) {
  timer.unref();
  util.helper();
  return Math.round(1.5);
}
class Store {
  save() { return 1; }
  run() { return this.save(); }
}
"#,
        ),
        ("util.js", "export function helper() { return 2; }\n"),
    ]);
    let out = callees(&store, "compute");
    let math = edge(&out, "compute", "round");
    assert_eq!(math.certainty(), Certainty::Ambiguous, "Math.round(): {math:?}");
    assert!(math.is_method, "a builtin receiver is a method call, not a free one");
    let unref = edge(&out, "compute", "unref");
    assert_eq!(unref.certainty(), Certainty::Ambiguous);
    // Neither is credited to the local free functions.
    assert!(store.get_callers("round", 50).unwrap().is_empty(), "Math.round bound to local round");
    assert!(store.get_callers("unref", 50).unwrap().is_empty(), "timer.unref bound to free unref");

    // Module access through a project import stays a free call, graded as before.
    let helper = edge(&out, "compute", "helper");
    assert!(!helper.is_method);
    assert_eq!(helper.certainty(), Certainty::Inferred);
    assert_eq!(store.get_callers("helper", 50).unwrap().len(), 1);

    // `this.save()` inside the class that defines `save` is exact.
    let run = callees(&store, "Store.run");
    let save = edge(&run, "run", "save");
    assert_eq!(save.certainty(), Certainty::Exact, "{save:?}");
}

#[test]
fn python_value_receiver_vs_self() {
    let (_d, store) = indexed(&[(
        "svc.py",
        r#"
def x():
    return 1

class Svc:
    def save(self):
        return 2
    def run(self, obj):
        obj.x()
        return self.save()
"#,
    )]);
    let out = callees(&store, "Svc.run");
    assert_eq!(edge(&out, "run", "x").certainty(), Certainty::Ambiguous);
    assert!(store.get_callers("x", 50).unwrap().is_empty(), "obj.x() bound to free def x");
    assert_eq!(edge(&out, "run", "save").certainty(), Certainty::Exact);
}

#[test]
fn rust_method_and_std_paths_never_reach_a_free_fn() {
    let (_d, store) = indexed(&[(
        "lib.rs",
        r#"
pub fn take(v: u32) -> u32 { v }
pub struct S;
impl S {
    pub fn go(&self) -> u32 { 1 }
    pub fn run(&self, other: &S) -> u32 {
        let mut v = 0u32;
        let _ = std::mem::take(&mut v);
        other.go() + self.go() + take(v)
    }
}
"#,
    )]);
    let out = callees(&store, "S::run");
    let std_take = out.iter().find(|c| c.callee == "take" && c.receiver.as_deref().is_some_and(|r| r.starts_with("std")));
    if let Some(t) = std_take {
        assert!(t.is_method && t.certainty() == Certainty::Ambiguous, "{t:?}");
    }
    // Only the real free call `take(v)` is a caller of `take`.
    let callers = store.get_callers("take", 50).unwrap();
    assert_eq!(callers.len(), 1, "{callers:?}");
    // `other.go()` — unknown value type: at most ambiguous; `self.go()` exact.
    let gos: Vec<_> = out.iter().filter(|c| c.callee == "go").collect();
    assert!(gos.iter().any(|c| c.certainty() == Certainty::Exact), "{gos:?}");
    assert!(gos.iter().any(|c| c.certainty() == Certainty::Ambiguous), "{gos:?}");
}

#[test]
fn go_package_access_stays_free_and_value_receivers_do_not() {
    let (_d, store) = indexed(&[
        (
            "main.go",
            r#"package main

import "example.com/proj/store"

func Run() int { return 1 }

func main() {
	var s T
	s.Run()
	store.Get()
}
"#,
        ),
        ("store/store.go", "package store\n\nfunc Get() int { return 2 }\n"),
    ]);
    let out = callees(&store, "main");
    assert_eq!(edge(&out, "main", "Run").certainty(), Certainty::Ambiguous);
    assert!(store.get_callers("Run", 50).unwrap().is_empty(), "s.Run() bound to free Run");
    let get = edge(&out, "main", "Get");
    assert!(!get.is_method, "package access is a free call: {get:?}");
    assert_eq!(store.get_callers("Get", 50).unwrap().len(), 1);
}

#[test]
fn an_index_graded_by_old_rules_is_regraded_without_a_reparse() {
    let (dir, store) = indexed(&[(
        "app.js",
        "function round(x) { return x; }\nfunction f() { return Math.round(1); }\n",
    )]);
    assert!(!store.resolution_outdated().unwrap());
    drop(store);
    // Simulate an index written by the previous rules.
    let db = dir.path().join(".icode/index.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "UPDATE calls SET is_method = 0, confidence = 0.6; DELETE FROM meta WHERE key = 'resolve_version';",
    )
    .unwrap();
    drop(conn);
    let store = SqliteCodeStore::open(dir.path()).unwrap();
    assert!(store.resolution_outdated().unwrap());
    let stats = index_path(Path::new(dir.path()), &store).unwrap();
    assert_eq!(stats.files_indexed, 0, "nothing re-parsed");
    assert!(!store.resolution_outdated().unwrap());
    let c = edge(&callees(&store, "f"), "f", "round").clone();
    assert_eq!(c.certainty(), Certainty::Ambiguous, "{c:?}");
}
