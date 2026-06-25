//! M3 Python-parser integration test: index a temp dir with one `.py` file
//! covering a class with a method (calling `self.other()`), a free function,
//! a plain `import`, and a `from ... import ... as ...`. Verifies the code graph
//! is populated and the qualified-name / receiver / import-kind shapes hold,
//! exercising parse → store → read across the contract seam.

use std::fs;

use icode_core::model::Language;
use icode_core::traits::CodeReadStore;
use icode_engine::parse::parse_python;
use icode_engine::{index_path, SqliteCodeStore};

const SAMPLE: &str = r#"
import os
from a.b import c as d


class Widget(Base):
    """A widget."""

    def render(self, ctx):
        """Render it."""
        return self.other(ctx)

    def other(self, ctx):
        return ctx


def build():
    w = Widget(None)
    return w.render(1)
"#;

#[test]
fn index_python_builds_code_graph() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("sample.py"), SAMPLE).expect("write sample");

    let store = SqliteCodeStore::open(root).expect("open store");
    let stats = index_path(root, &store).expect("index");

    assert_eq!(stats.files_indexed, 1, "one .py file indexed");
    assert_eq!(stats.errors, 0, "no parse errors");

    let db = store.stats().expect("stats");
    assert_eq!(db.files, 1);
    assert!(db.classes >= 1, "expected classes >= 1, got {}", db.classes);
    assert!(db.functions >= 3, "render + other + build, got {}", db.functions);
    assert!(db.imports >= 2, "expected imports >= 2, got {}", db.imports);
    assert!(db.calls > 0, "expected call edges, got {}", db.calls);

    // The method resolves with a `Class.method` qualified name.
    let render = store
        .get_function("render", Some(Language::Python), true)
        .expect("get_function render")
        .expect("render present");
    assert_eq!(render.qualified_name, "Widget.render");
    assert_eq!(render.docstring.as_deref(), Some("Render it."));

    // The class is present with its base and docstring.
    let widget = store
        .get_class("Widget", Some(Language::Python), true)
        .expect("get_class Widget")
        .expect("Widget present");
    assert!(widget.bases.iter().any(|b| b == "Base"), "bases: {:?}", widget.bases);

    // Inspect the parser output directly for the import kinds and the
    // self-receiver method call (store-level call inspection is M-later).
    let parsed = parse_python(SAMPLE, "sample.py");

    assert!(
        parsed.imports.iter().any(|i| i.kind == "import" && i.module == "os"),
        "plain `import os` (kind=import): {:?}",
        parsed.imports
    );
    let from_import = parsed
        .imports
        .iter()
        .find(|i| i.kind == "from")
        .expect("a `from` import");
    assert_eq!(from_import.module, "a.b");
    assert_eq!(from_import.name.as_deref(), Some("c"));
    assert_eq!(from_import.alias.as_deref(), Some("d"));

    let self_call = parsed
        .calls
        .iter()
        .find(|c| c.callee == "other")
        .expect("self.other() call");
    assert_eq!(self_call.caller, "Widget.render");
    assert_eq!(self_call.receiver.as_deref(), Some("self"));
}
