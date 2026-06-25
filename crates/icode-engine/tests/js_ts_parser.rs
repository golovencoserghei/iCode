//! JS/TS parser integration tests: index temp dirs with one `.js` and one `.ts`
//! file and verify the code graph (classes, functions, qualified names, call
//! receivers, imports, Express routes) is populated. Exercises parse → store →
//! read across the contract seam, mirroring the PHP/Python parser tests.

use std::fs;

use icode_core::model::Language;
use icode_core::traits::CodeReadStore;
use icode_engine::parse::{parse_javascript, parse_typescript};
use icode_engine::{index_path, SqliteCodeStore};

const JS_SAMPLE: &str = r#"
import { helper } from 'utils';
import def from 'other';
import * as ns from 'nsmod';

class Widget extends Base {
    constructor() {
        super();
    }

    async render(a, b) {
        this.helper();
        free();
    }

    helper() {}
}

const makeWidget = (config) => {
    return new Widget();
};

let onClick = async (e) => {
    handle(e);
};

function topLevel(x) {
    return x + 1;
}

app.get('/u', handler);
router.post('/p', (req, res) => {});
"#;

const TS_SAMPLE: &str = r#"
import type { Foo } from 'foo';
import { Bar } from 'bar';

interface Renderable extends Base {
    render(): string;
}

enum Color {
    Red,
    Green,
}

class Widget extends BaseWidget implements Renderable {
    private x: number = 0;

    render(): string {
        return this.compute();
    }

    compute(): number {
        return this.x;
    }
}

const typed = (p: number): number => p + 1;
"#;

#[test]
fn index_js_builds_code_graph_and_routes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("sample.js"), JS_SAMPLE).expect("write js");

    let store = SqliteCodeStore::open(root).expect("open store");
    let stats = index_path(root, &store).expect("index");

    assert_eq!(stats.files_indexed, 1, "one .js file indexed");
    assert_eq!(stats.errors, 0, "no parse errors");

    let db = store.stats().expect("stats");
    // class Widget (>= 1).
    assert!(db.classes >= 1, "expected classes >= 1, got {}", db.classes);
    assert!(db.functions >= 1, "expected functions >= 1, got {}", db.functions);
    assert!(db.imports >= 1, "expected imports >= 1, got {}", db.imports);
    assert!(db.routes >= 1, "expected routes >= 1, got {}", db.routes);

    // The method resolves with a `Class.method` qualified name.
    let render = store
        .get_function("render", Some(Language::JavaScript), true)
        .expect("get_function render")
        .expect("render present");
    assert_eq!(render.qualified_name, "Widget.render");
    assert!(render.is_async, "async render flagged async");

    // The class is present with `extends Base` as a base.
    let widget = store
        .get_class("Widget", Some(Language::JavaScript), true)
        .expect("get_class Widget")
        .expect("Widget present");
    assert!(widget.bases.iter().any(|b| b == "Base"), "extends base: {:?}", widget.bases);

    // Parser-level inspection for arrow functions, import shapes, call receivers,
    // and route shapes.
    let parsed = parse_javascript(JS_SAMPLE, "sample.js");

    // Arrow function bound to a `const` is found by its declarator name.
    let arrow = parsed
        .functions
        .iter()
        .find(|f| f.name == "makeWidget")
        .expect("const arrow function makeWidget");
    assert_eq!(arrow.qualified_name, "makeWidget");

    // async arrow bound to a `let`.
    let on_click = parsed
        .functions
        .iter()
        .find(|f| f.name == "onClick")
        .expect("let arrow function onClick");
    assert!(on_click.is_async, "async arrow flagged async");

    // Named import.
    assert!(
        parsed.imports.iter().any(|i| i.name.as_deref() == Some("helper") && i.module == "utils"),
        "named import {{ helper }} from 'utils': {:?}",
        parsed.imports
    );
    // Default import.
    assert!(
        parsed.imports.iter().any(|i| i.name.as_deref() == Some("def") && i.module == "other"),
        "default import def from 'other': {:?}",
        parsed.imports
    );
    // Namespace import (alias, no name).
    assert!(
        parsed.imports.iter().any(|i| i.alias.as_deref() == Some("ns") && i.module == "nsmod"),
        "namespace import * as ns from 'nsmod': {:?}",
        parsed.imports
    );

    // member call `this.helper()` with receiver `this`.
    let helper_call = parsed
        .calls
        .iter()
        .find(|c| c.callee == "helper" && c.receiver.as_deref() == Some("this"))
        .expect("this.helper() call");
    assert_eq!(helper_call.caller, "Widget.render");

    // Express GET route with a named handler.
    let get_route = parsed.routes.iter().find(|r| r.method == "GET").expect("a GET route");
    assert_eq!(get_route.route, "/u");
    assert_eq!(get_route.handler_method.as_deref(), Some("handler"));

    // Express POST route with an inline handler (no named handler).
    let post_route = parsed.routes.iter().find(|r| r.method == "POST").expect("a POST route");
    assert_eq!(post_route.route, "/p");
    assert!(post_route.handler_method.is_none(), "inline handler has no name");
}

#[test]
fn index_ts_builds_code_graph() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("sample.ts"), TS_SAMPLE).expect("write ts");

    let store = SqliteCodeStore::open(root).expect("open store");
    let stats = index_path(root, &store).expect("index");

    assert_eq!(stats.files_indexed, 1, "one .ts file indexed");
    assert_eq!(stats.errors, 0, "no parse errors");

    let db = store.stats().expect("stats");
    // interface Renderable + enum Color + class Widget → classes >= 2.
    assert!(db.classes >= 2, "expected classes >= 2 (interface + class), got {}", db.classes);
    assert!(db.functions >= 1, "expected functions >= 1, got {}", db.functions);
    assert!(db.imports >= 1, "expected imports >= 1, got {}", db.imports);

    // Typed method resolves as `Class.method`.
    let render = store
        .get_function("render", Some(Language::TypeScript), true)
        .expect("get_function render")
        .expect("render present");
    assert_eq!(render.qualified_name, "Widget.render");

    // The interface is present as a class-like declaration with its extended base.
    let iface = store
        .get_class("Renderable", Some(Language::TypeScript), true)
        .expect("get_class Renderable")
        .expect("Renderable present");
    assert_eq!(iface.name, "Renderable");
    assert!(iface.bases.iter().any(|b| b == "Base"), "interface extends Base: {:?}", iface.bases);

    // The class carries both `extends` and `implements` as bases.
    let widget = store
        .get_class("Widget", Some(Language::TypeScript), true)
        .expect("get_class Widget")
        .expect("Widget present");
    assert!(widget.bases.iter().any(|b| b == "BaseWidget"), "extends BaseWidget: {:?}", widget.bases);
    assert!(widget.bases.iter().any(|b| b == "Renderable"), "implements Renderable: {:?}", widget.bases);

    // Parser-level inspection: `import type` and a typed const arrow.
    let parsed = parse_typescript(TS_SAMPLE, "sample.ts");
    assert!(
        parsed.imports.iter().any(|i| i.name.as_deref() == Some("Foo") && i.module == "foo"),
        "import type {{ Foo }} from 'foo': {:?}",
        parsed.imports
    );
    let typed = parsed
        .functions
        .iter()
        .find(|f| f.name == "typed")
        .expect("typed const arrow function");
    assert_eq!(typed.qualified_name, "typed");

    // The enum is present as a class-like declaration.
    let color = store
        .get_class("Color", Some(Language::TypeScript), true)
        .expect("get_class Color")
        .expect("Color present");
    assert_eq!(color.name, "Color");
}
