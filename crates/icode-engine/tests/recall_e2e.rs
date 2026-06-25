//! End-to-end LIVE test of the flagship `recall` (code + memory synergy in one
//! call). Requires a running Ollama with the configured embedding model — like
//! the other `*_e2e` tests, deliberately NOT `#[ignore]`.
//!
//! Flow: index + embed a temp Rust project (a file-reading function) into a code
//! store; add one memory ("we cache file reads…") to a memory store; then
//! `recall(code, Some(embedder), Some(memory), "p", "reading files from disk", 5)`
//! and assert the THREE INDEPENDENT sections:
//!   * relevant_code   — non-empty, contains the file-reading function,
//!   * relevant_memory — non-empty, contains the cache memory,
//!   * facts           — empty (knowledge graph is a later milestone).
//!
//! Decoupling: `icode-embed` is a DEV-dependency only; production `icode-engine`
//! (incl. `recall`) depends solely on the `Embedder` / `MemoryStore` contracts
//! from `icode-core`. This dev artefact may build the concrete impls.

use std::fs;
use std::sync::Arc;

use icode_core::config::EmbedConfig;
use icode_core::model::{AddOutcome, Category, NewMemory};
use icode_core::traits::{Embedder, WritableMemoryStore};
use icode_embed::OllamaEmbedder;
use icode_engine::{embed_pending, index_path, recall, SqliteCodeStore, SqliteMemoryStore};

/// A small Rust file whose `read_file` function is the clear semantic match for a
/// "reading files from disk" query.
const SAMPLE: &str = r#"
use std::fs;

/// Read the entire contents of a file from disk into a String.
pub fn read_file(path: &str) -> std::io::Result<String> {
    fs::read_to_string(path)
}

/// Send an HTTP GET request to a URL and return the response body as text.
pub fn http_get(url: &str) -> String {
    format!("GET {url}")
}
"#;

#[test]
fn recall_returns_separate_code_and_memory_sections() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("io.rs"), SAMPLE).expect("write sample");

    // ── live embedder (Ollama must be up) ──
    let cfg = EmbedConfig::default();
    let embedder = OllamaEmbedder::new(&cfg).expect("build OllamaEmbedder");
    embedder
        .health()
        .expect("Ollama must be running with the embedding model for recall_e2e");
    let embedder: Arc<dyn Embedder> = Arc::new(embedder);

    // ── code store: index + embed ──
    let code = SqliteCodeStore::open(root).expect("open code store");
    let stats = index_path(root, &code).expect("index");
    assert!(stats.code_chunks > 0, "indexer wrote code_chunks");
    let emb = embed_pending(&code, embedder.as_ref(), cfg.batch).expect("embed_pending");
    assert!(emb.embedded > 0, "embedded at least one chunk");

    // ── memory store (its own central db in the tempdir) + one memory ──
    let mem_db = root.join("icode.db");
    let memory =
        SqliteMemoryStore::open(mem_db.to_str().unwrap(), embedder.clone()).expect("open memory");
    match memory
        .add(NewMemory {
            project: "p".into(),
            content: "we cache file reads to avoid disk IO".into(),
            category: Category::Decision,
            tags: vec![],
            importance: 0.0,
            session_id: None,
        })
        .expect("add memory")
    {
        AddOutcome::Added { .. } => {}
        AddOutcome::Duplicate { existing, .. } => panic!("unexpected Duplicate({existing})"),
    }

    // ── the flagship call ──
    let result = recall(
        &code,
        Some(embedder.as_ref()),
        Some(&memory),
        "p",
        "reading files from disk",
        5,
    )
    .expect("recall");

    // relevant_code: non-empty and contains the file-reading function.
    assert!(
        !result.relevant_code.is_empty(),
        "relevant_code is non-empty"
    );
    assert!(
        result
            .relevant_code
            .iter()
            .any(|h| h.name == "read_file" || h.qualified_name.contains("read_file")),
        "relevant_code contains the file-reading function, got {:?}",
        result
            .relevant_code
            .iter()
            .map(|h| (&h.name, h.score))
            .collect::<Vec<_>>()
    );

    // relevant_memory: non-empty and contains the cache memory.
    assert!(
        !result.relevant_memory.is_empty(),
        "relevant_memory is non-empty"
    );
    assert!(
        result
            .relevant_memory
            .iter()
            .any(|m| m.record.content.contains("cache file reads")),
        "relevant_memory contains the cache memory, got {:?}",
        result
            .relevant_memory
            .iter()
            .map(|m| &m.record.content)
            .collect::<Vec<_>>()
    );

    // facts: empty (knowledge graph is a later milestone).
    assert!(result.facts.is_empty(), "facts section is empty for now");
}
