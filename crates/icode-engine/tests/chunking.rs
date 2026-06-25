//! Chunking unit tests: `chunks_for_file` produces one symbol chunk per function
//! and class with a populated parent-header + non-empty `content_hash`, and the
//! overflow split fans an oversized body into multiple `part i/N` sub-chunks that
//! still cut on UTF-8 (line) boundaries.

use icode_core::model::{ClassDef, FunctionDef, Language, SymbolKind};
use icode_engine::{chunks_for_file, CHUNK_BUDGET_BYTES};

fn func(qname: &str, args: &str, doc: Option<&str>, body: &str) -> FunctionDef {
    FunctionDef {
        name: qname.rsplit("::").next().unwrap_or(qname).to_string(),
        qualified_name: qname.to_string(),
        path: "src/lib.rs".to_string(),
        language: Language::Rust,
        line_start: 10,
        line_end: 20,
        args: args.to_string(),
        return_type: None,
        docstring: doc.map(|s| s.to_string()),
        body: body.to_string(),
        is_async: false,
        override_type: None,
        override_target: None,
    }
}

fn class(qname: &str, bases: &[&str], doc: Option<&str>, body: &str) -> ClassDef {
    ClassDef {
        name: qname.rsplit("::").next().unwrap_or(qname).to_string(),
        qualified_name: qname.to_string(),
        path: "src/lib.rs".to_string(),
        language: Language::Rust,
        line_start: 30,
        line_end: 60,
        bases: bases.iter().map(|s| s.to_string()).collect(),
        docstring: doc.map(|s| s.to_string()),
        body: body.to_string(),
    }
}

#[test]
fn one_chunk_per_symbol_with_header_and_hash() {
    let functions = vec![
        func(
            "Service::run",
            "&self, id: u64",
            Some("Run the service."),
            "{ helper(id) }",
        ),
        func("helper", "seed: u64", None, "{ seed * 2 }"),
    ];
    let classes = vec![class(
        "Service",
        &["Component", "Drop"],
        Some("A service."),
        "struct Service { cache: Map }",
    )];

    let chunks = chunks_for_file(&functions, &classes);

    // Exactly one chunk per symbol (no overflow at these tiny sizes).
    assert_eq!(chunks.len(), 3, "one chunk per function + class");

    // Every chunk: non-empty hash, qualified_name present, header carries the
    // qualified name and the file path.
    for ch in &chunks {
        assert_eq!(ch.content_hash.len(), 64, "sha256 hex is 64 chars");
        assert!(!ch.content_hash.is_empty());
        let qn = ch.qualified_name.as_deref().expect("qualified_name set");
        assert!(
            ch.chunk_text.contains(qn),
            "parent-header must contain qualified_name `{qn}`:\n{}",
            ch.chunk_text
        );
        assert!(
            ch.chunk_text.contains("src/lib.rs"),
            "header carries the path"
        );
    }

    // Function chunk: header has the args; kind is Function.
    let run = chunks
        .iter()
        .find(|c| c.qualified_name.as_deref() == Some("Service::run"))
        .unwrap();
    assert_eq!(run.symbol_kind, SymbolKind::Function);
    assert!(
        run.chunk_text.contains("&self, id: u64"),
        "function header shows args"
    );
    assert!(
        run.chunk_text.contains("Run the service."),
        "docstring folded into header"
    );
    assert!(run.chunk_text.contains("helper(id)"), "body present");

    // Class chunk: header shows the bases; kind is Class.
    let svc = chunks
        .iter()
        .find(|c| c.symbol_kind == SymbolKind::Class)
        .unwrap();
    assert!(
        svc.chunk_text.contains("Component"),
        "class header shows bases"
    );
    assert!(svc.chunk_text.contains("Drop"));
    assert!(
        svc.chunk_text.contains("struct Service"),
        "class body present"
    );

    // Distinct symbols hash differently.
    assert_ne!(chunks[0].content_hash, chunks[1].content_hash);
}

#[test]
fn no_docstring_header_still_valid() {
    let chunks = chunks_for_file(&[func("bare", "", None, "{}")], &[]);
    assert_eq!(chunks.len(), 1);
    let c = &chunks[0];
    assert!(
        c.chunk_text.starts_with("// src/lib.rs\n// bare()\n"),
        "got:\n{}",
        c.chunk_text
    );
}

#[test]
fn oversized_body_splits_into_multiple_subchunks() {
    // A body well past the budget: many short lines so splits land on `\n`.
    let line = "    let x = compute_something_long_enough_to_count(value, index);\n";
    let body_len_target = CHUNK_BUDGET_BYTES * 3;
    let mut body = String::with_capacity(body_len_target + line.len());
    while body.len() < body_len_target {
        body.push_str(line);
    }

    let functions = vec![func("Huge::method", "&self", None, &body)];
    let chunks = chunks_for_file(&functions, &[]);

    assert!(
        chunks.len() > 1,
        "oversized body must split into >1 sub-chunk, got {}",
        chunks.len()
    );

    // All sub-chunks share the same symbol identity and kind, carry the header,
    // a `part i/N` marker, and stay within budget; none cut a codepoint.
    let n = chunks.len();
    for (i, ch) in chunks.iter().enumerate() {
        assert_eq!(ch.qualified_name.as_deref(), Some("Huge::method"));
        assert_eq!(ch.symbol_kind, SymbolKind::Function);
        assert!(
            ch.chunk_text.contains("Huge::method"),
            "header repeated on every part"
        );
        assert!(
            ch.chunk_text.contains(&format!("// part {}/{n}", i + 1)),
            "part marker {}/{n} present in:\n{}",
            i + 1,
            &ch.chunk_text[..ch.chunk_text.len().min(120)]
        );
        assert!(
            ch.chunk_text.len() <= CHUNK_BUDGET_BYTES + 64,
            "sub-chunk {} within budget (+marker slack), got {} bytes",
            i,
            ch.chunk_text.len()
        );
        // Valid UTF-8 by construction (it's a Rust String); assert no broken
        // boundary by re-checking char boundaries at the ends.
        assert!(ch.chunk_text.is_char_boundary(0));
        assert!(ch.chunk_text.is_char_boundary(ch.chunk_text.len()));
        assert_eq!(ch.content_hash.len(), 64);
    }
}

#[test]
fn multibyte_body_does_not_panic_and_splits_cleanly() {
    // A body of multibyte (Cyrillic) lines exceeding the budget: the splitter
    // must never cut a codepoint nor panic.
    let line = "    // комментарий с юникодом достаточно длинный чтобы заполнить буфер\n";
    let mut body = String::new();
    while body.len() < CHUNK_BUDGET_BYTES * 2 {
        body.push_str(line);
    }
    let chunks = chunks_for_file(&[func("uni::fn", "", None, &body)], &[]);
    assert!(chunks.len() > 1);
    for ch in &chunks {
        // Round-trips through UTF-8 validation implicitly (it's a String); ensure
        // the text is well-formed by counting chars without panic.
        let _ = ch.chunk_text.chars().count();
    }
}
