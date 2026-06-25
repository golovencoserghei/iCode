//! M3 Go / Java / HTML parser integration tests.
//!
//! Each language gets a small but representative sample indexed into a temp dir,
//! exercising parse → store → read across the contract seam, plus a direct
//! `parse_*` inspection for the shapes the store does not yet surface (call
//! receivers, import kinds, struct/interface bases).

use std::fs;

use icode_core::model::Language;
use icode_core::traits::CodeReadStore;
use icode_engine::parse::{parse_go, parse_html, parse_java};
use icode_engine::{index_path, SqliteCodeStore};

// ──────────────────────────── Go ────────────────────────────

const GO_SAMPLE: &str = r#"package store

import (
	"fmt"
	mrand "math/rand"
)

type Reader interface {
	Read() string
}

type Service struct {
	name string
}

func (s *Service) Run() string {
	fmt.Println(s.name)
	return Helper()
}

func Helper() string {
	n := mrand.Int()
	return fmt.Sprintf("%d", n)
}
"#;

#[test]
fn index_go_builds_code_graph() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("service.go"), GO_SAMPLE).expect("write go sample");

    let store = SqliteCodeStore::open(root).expect("open store");
    let stats = index_path(root, &store).expect("index");

    assert_eq!(stats.files_indexed, 1, "one .go file indexed");
    assert_eq!(stats.errors, 0, "no parse errors");

    let db = store.stats().expect("stats");
    assert_eq!(db.files, 1);
    // interface Reader + struct Service (>= 1, expected 2).
    assert!(db.classes >= 1, "expected classes >= 1, got {}", db.classes);
    assert!(db.functions >= 2, "expected functions >= 2, got {}", db.functions);
    assert!(db.imports >= 1, "expected imports >= 1, got {}", db.imports);
    assert!(db.calls > 0, "expected call edges, got {}", db.calls);

    // The method's qualified name carries the receiver type: `Service.Run`.
    let run = store
        .get_function("Run", Some(Language::Go), true)
        .expect("get_function Run")
        .expect("Run present");
    assert_eq!(run.qualified_name, "Service.Run", "receiver-typed method qname");

    // Free function present with a bare qualified name.
    let helper = store
        .get_function("Helper", Some(Language::Go), true)
        .expect("get_function Helper")
        .expect("Helper present");
    assert_eq!(helper.qualified_name, "Helper");

    // Struct + interface are stored as class-like declarations.
    let svc = store
        .get_class("Service", Some(Language::Go), true)
        .expect("get_class Service")
        .expect("Service present");
    assert_eq!(svc.name, "Service");

    // Direct parser inspection for receivers + import shapes.
    let parsed = parse_go(GO_SAMPLE, "service.go");

    // Method has a receiver-typed qualified name.
    assert!(
        parsed.functions.iter().any(|f| f.qualified_name == "Service.Run"),
        "expected a `Service.Run` method, got: {:?}",
        parsed.functions.iter().map(|f| &f.qualified_name).collect::<Vec<_>>()
    );

    // import kind "import"; aliased import keeps the alias; module is the path.
    assert!(parsed.imports.iter().all(|i| i.kind == "import"), "all imports kind=import");
    let fmt_import = parsed
        .imports
        .iter()
        .find(|i| i.module == "fmt")
        .expect("fmt import");
    assert_eq!(fmt_import.name.as_deref(), Some("fmt"));
    let rand_import = parsed
        .imports
        .iter()
        .find(|i| i.module == "math/rand")
        .expect("math/rand import");
    assert_eq!(rand_import.alias.as_deref(), Some("mrand"), "alias captured");

    // selector call `fmt.Println(...)` → callee `Println`, receiver `fmt`.
    let println = parsed
        .calls
        .iter()
        .find(|c| c.callee == "Println")
        .expect("fmt.Println call");
    assert_eq!(println.caller, "Service.Run");
    assert_eq!(println.receiver.as_deref(), Some("fmt"), "selector receiver = operand");

    // free call `Helper()` → callee `Helper`, no receiver.
    let helper_call = parsed
        .calls
        .iter()
        .find(|c| c.callee == "Helper")
        .expect("Helper() call");
    assert!(helper_call.receiver.is_none(), "free call has no receiver");
}

// ──────────────────────────── Java ────────────────────────────

const JAVA_SAMPLE: &str = r#"package com.example.app;

import java.util.List;
import java.util.ArrayList;

interface Greeter {
    String greet();
}

public class HelloService extends BaseService implements Greeter {
    private final List<String> names = new ArrayList<>();

    public HelloService() {
    }

    @Override
    public String greet() {
        return format("hello");
    }

    private String format(String s) {
        return s.toUpperCase();
    }
}
"#;

#[test]
fn index_java_builds_code_graph() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("HelloService.java"), JAVA_SAMPLE).expect("write java sample");

    let store = SqliteCodeStore::open(root).expect("open store");
    let stats = index_path(root, &store).expect("index");

    assert_eq!(stats.files_indexed, 1, "one .java file indexed");
    assert_eq!(stats.errors, 0, "no parse errors");

    let db = store.stats().expect("stats");
    assert_eq!(db.files, 1);
    // interface Greeter + class HelloService (>= 2).
    assert!(db.classes >= 2, "expected classes >= 2 (interface + class), got {}", db.classes);
    // constructor + greet + format (>= 2).
    assert!(db.functions >= 2, "expected functions >= 2, got {}", db.functions);
    assert!(db.imports >= 1, "expected imports >= 1, got {}", db.imports);
    assert!(db.calls > 0, "expected call edges, got {}", db.calls);

    // Class carries both `extends` and `implements` as bases.
    let svc = store
        .get_class("HelloService", Some(Language::Java), true)
        .expect("get_class HelloService")
        .expect("HelloService present");
    assert!(svc.bases.iter().any(|b| b == "BaseService"), "extends base: {:?}", svc.bases);
    assert!(svc.bases.iter().any(|b| b == "Greeter"), "implements interface: {:?}", svc.bases);

    // The interface is present as a class-like declaration.
    let iface = store
        .get_class("Greeter", Some(Language::Java), true)
        .expect("get_class Greeter")
        .expect("Greeter present");
    assert_eq!(iface.name, "Greeter");

    // Direct parser inspection: import shapes + call receiver.
    let parsed = parse_java(JAVA_SAMPLE, "HelloService.java");

    // The concrete class method has a `Class.method` qualified name. (The
    // interface declares its own abstract `Greeter.greet`, so a by-name store
    // lookup is ambiguous — inspect the parser output, which keeps both.)
    assert!(
        parsed.functions.iter().any(|f| f.qualified_name == "HelloService.greet"),
        "expected a `HelloService.greet` method, got: {:?}",
        parsed.functions.iter().map(|f| &f.qualified_name).collect::<Vec<_>>()
    );
    // The constructor is captured as a function named after the class.
    assert!(
        parsed.functions.iter().any(|f| f.qualified_name == "HelloService.HelloService"),
        "expected the constructor `HelloService.HelloService`"
    );

    assert!(parsed.imports.iter().all(|i| i.kind == "import"), "all imports kind=import");
    let list_import = parsed
        .imports
        .iter()
        .find(|i| i.name.as_deref() == Some("List"))
        .expect("List import");
    assert_eq!(list_import.module, "java.util.List");

    // `s.toUpperCase()` → callee `toUpperCase`, receiver `s`.
    let upper = parsed
        .calls
        .iter()
        .find(|c| c.callee == "toUpperCase")
        .expect("s.toUpperCase() call");
    assert_eq!(upper.caller, "HelloService.format");
    assert_eq!(upper.receiver.as_deref(), Some("s"));

    // unqualified `format("hello")` → callee `format`, no receiver.
    let fmt = parsed
        .calls
        .iter()
        .find(|c| c.callee == "format")
        .expect("format() call");
    assert!(fmt.receiver.is_none(), "unqualified call has no receiver");
    assert_eq!(fmt.caller, "HelloService.greet");
}

// ──────────────────────────── HTML ────────────────────────────

const HTML_SAMPLE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <title>Sample</title>
    <style>body { color: red; }</style>
</head>
<body>
    <h1>Hello</h1>
    <p>World</p>
    <script>console.log("hi");</script>
</body>
</html>
"#;

#[test]
fn index_html_is_metadata_only_and_never_errors() {
    // Direct parser: metadata is filled, no symbols, no panic.
    let parsed = parse_html(HTML_SAMPLE, "index.html");
    assert!(parsed.lines_total > 0, "lines_total > 0");
    assert!(!parsed.ast_hash.is_empty(), "ast_hash non-empty");
    assert!(parsed.functions.is_empty(), "html has no functions");
    assert!(parsed.classes.is_empty(), "html has no classes");
    assert!(parsed.imports.is_empty(), "html has no imports");
    assert!(parsed.calls.is_empty(), "html has no calls");
    assert!(parsed.routes.is_empty(), "html has no routes");

    // Malformed HTML must not panic and still yields metadata.
    let broken = parse_html("<html><body><p>oops</body>", "broken.html");
    assert!(broken.lines_total >= 1);
    assert!(!broken.ast_hash.is_empty());

    // Indexing a folder of .html files yields zero parse errors.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("index.html"), HTML_SAMPLE).expect("write html");
    fs::write(root.join("page.htm"), "<html><body>hi</body></html>").expect("write htm");

    let store = SqliteCodeStore::open(root).expect("open store");
    let stats = index_path(root, &store).expect("index");

    assert_eq!(stats.files_indexed, 2, "both .html/.htm indexed");
    assert_eq!(stats.errors, 0, "html files are NOT parse errors");

    let db = store.stats().expect("stats");
    assert_eq!(db.files, 2);
    assert_eq!(db.parse_errors, 0, "no parse errors recorded");
}
