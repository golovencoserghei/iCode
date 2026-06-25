//! End-to-end LIVE test for the M4.2 memory-quality layer (requires a running
//! Ollama with the configured embedding model — deliberately NOT `#[ignore]`,
//! like `memory_store`/`embed_e2e`). It proves the four quality features against
//! real embeddings:
//!
//!   1. **Dedup gate** — adding the same fact twice yields `Duplicate` pointing at
//!      the first id; a semantically DIFFERENT fact is `Added`.
//!   2. **Usage-aware ranking** — an old, importance-5 (L0) record is not buried
//!      by decay: its `effective_score` stays >= a fresh importance-0 record.
//!   3. **Access warm-up** — a `search` bumps `access_count` on the hits it returns.
//!   4. **WAL audit** — an `add` through `WalStore` appends a JSONL line to the
//!      configured audit file.
//!
//! `icode-embed` is a DEV-dependency only (production `icode-engine` depends on the
//! `Embedder` trait from `icode-core`, never on a concrete embedder crate).

use std::sync::Arc;

use chrono::{Duration, Utc};
use icode_core::config::{EmbedConfig, RankingConfig};
use icode_core::model::{AddOutcome, Category, NewMemory};
use icode_core::traits::{Embedder, MemoryStore, ReadableMemoryStore, WritableMemoryStore};
use icode_embed::OllamaEmbedder;
use icode_engine::memory::ranking::effective_score;
use icode_engine::{SqliteMemoryStore, WalStore};
use rusqlite::params;

/// Build the live Ollama embedder (asserts reachable up front).
fn embedder() -> Arc<dyn Embedder> {
    let cfg = EmbedConfig::default();
    let e = OllamaEmbedder::new(&cfg).expect("build OllamaEmbedder");
    e.health()
        .expect("Ollama must be running with the embedding model for memory_quality");
    Arc::new(e)
}

fn mem(project: &str, content: &str, importance: f32, tags: &[&str]) -> NewMemory {
    NewMemory {
        project: project.into(),
        content: content.into(),
        category: Category::Bug,
        tags: tags.iter().map(|s| s.to_string()).collect(),
        importance,
        session_id: None,
    }
}

// ──────────────────────────── A) dedup gate ────────────────────────────

#[test]
fn dedup_gate_blocks_a_re_add_but_allows_distinct_content() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("icode.db");
    let store = SqliteMemoryStore::open(db.to_str().unwrap(), embedder()).expect("open");

    // First add of a fact → a real insert.
    let first = match store
        .add(mem("demo", "fixed the auth race condition", 0.0, &[]))
        .expect("add #1")
    {
        AddOutcome::Added { id } => id,
        other => panic!("first add must be Added, got {other:?}"),
    };

    // Exact re-add → the dedup gate short-circuits to Duplicate at the SAME id,
    // with a small (below-threshold) distance, and writes NO new row.
    match store
        .add(mem("demo", "fixed the auth race condition", 0.0, &[]))
        .expect("add #2")
    {
        AddOutcome::Duplicate { existing, distance } => {
            assert_eq!(existing, first, "duplicate points at the original id");
            assert!(distance < 0.12, "duplicate distance under the gate, got {distance}");
        }
        other => panic!("a re-add must be Duplicate, got {other:?}"),
    }
    assert_eq!(store.list("demo", None, 100, true).unwrap().len(), 1, "no second row inserted");

    // A semantically DIFFERENT fact in the same project → Added (gate must not be
    // so wide it swallows unrelated content).
    match store
        .add(mem("demo", "chose postgres over sqlite for the production database", 0.0, &[]))
        .expect("add #3")
    {
        AddOutcome::Added { .. } => {}
        other => panic!("a distinct fact must be Added, got {other:?}"),
    }
    assert_eq!(store.list("demo", None, 100, true).unwrap().len(), 2, "distinct fact inserted");

    // Same content but a DIFFERENT project is not a duplicate (gate is per-project).
    match store
        .add(mem("other", "fixed the auth race condition", 0.0, &[]))
        .expect("add #4")
    {
        AddOutcome::Added { .. } => {}
        other => panic!("same content in another project must be Added, got {other:?}"),
    }
}

// ──────────────────────────── B) usage-aware ranking ────────────────────────────

#[test]
fn old_l0_record_outranks_a_fresh_low_importance_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("icode.db");
    let db_path = db.to_str().unwrap().to_string();
    let store = SqliteMemoryStore::open(&db_path, embedder()).expect("open");

    // An importance-5 L0 rule and a fresh importance-0 note (semantically distinct
    // so the dedup gate doesn't fold them).
    let l0_id = match store
        .add(mem("demo", "always respond to the user in Russian", 5.0, &["l0"]))
        .expect("add l0")
    {
        AddOutcome::Added { id } => id,
        other => panic!("expected Added, got {other:?}"),
    };
    let fresh_id = match store
        .add(mem("demo", "the build cache lives under target slash debug", 0.0, &[]))
        .expect("add fresh")
    {
        AddOutcome::Added { id } => id,
        other => panic!("expected Added, got {other:?}"),
    };

    // Backdate the L0 record's last_accessed_at by a year via a second connection
    // (the store keeps a WAL connection; busy_timeout covers the contention). A
    // non-L0 record this old would be deep underwater from decay.
    {
        let conn = rusqlite::Connection::open(&db_path).expect("reopen for backdate");
        let year_ago = (Utc::now() - Duration::days(365)).to_rfc3339();
        let n = conn
            .execute(
                "UPDATE memories SET last_accessed_at = ?1 WHERE id = ?2",
                params![year_ago, l0_id.as_str()],
            )
            .expect("backdate l0");
        assert_eq!(n, 1, "backdated exactly the l0 row");
    }

    // Re-fetch both records and compare effective_score directly: L0 immunity must
    // keep the old, never-accessed rule at or above the fresh note.
    let cfg = RankingConfig::default();
    let now = Utc::now();
    let l0_rec = store.get(&l0_id).unwrap().expect("l0 exists");
    let fresh_rec = store.get(&fresh_id).unwrap().expect("fresh exists");
    let l0_score = effective_score(&l0_rec, &cfg, now);
    let fresh_score = effective_score(&fresh_rec, &cfg, now);
    assert!(
        l0_score >= fresh_score,
        "old L0 (score {l0_score}) must not be buried by decay below the fresh note (score {fresh_score})"
    );
}

// ──────────────────────────── C) access warm-up ────────────────────────────

#[test]
fn search_warms_up_access_count_on_returned_hits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("icode.db");
    let store = SqliteMemoryStore::open(db.to_str().unwrap(), embedder()).expect("open");

    let id = match store
        .add(mem("demo", "fixed the auth race condition with a mutex", 0.0, &[]))
        .expect("add")
    {
        AddOutcome::Added { id } => id,
        other => panic!("expected Added, got {other:?}"),
    };
    assert_eq!(store.get(&id).unwrap().unwrap().access_count, 0, "starts at 0");

    // Two searches that surface the record → access_count climbs (warm-up is what
    // makes usage-aware ranking actually move; without it access_count stays 0).
    let hits = store.search("demo", "race condition bug", 5, None, false).expect("search");
    assert!(hits.iter().any(|h| h.record.id == id), "the record is among the hits");
    store.search("demo", "auth mutex fix", 5, None, false).expect("search 2");

    let after = store.get(&id).unwrap().unwrap().access_count;
    assert!(after >= 2, "two searches bumped access_count to >= 2, got {after}");
}

// ──────────────────────────── D) WAL audit ────────────────────────────

#[test]
fn wal_store_appends_an_audit_line_on_add() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("icode.db");
    let wal_path = dir.path().join("wal.jsonl");

    let base = SqliteMemoryStore::open(db.to_str().unwrap(), embedder()).expect("open");
    let store: Arc<dyn MemoryStore> = Arc::new(base);
    let wal = WalStore::new(store, wal_path.clone());

    assert!(!wal_path.exists(), "no audit file before any write");
    match wal.add(mem("demo", "fixed the auth race condition", 4.0, &["bug"])).expect("add") {
        AddOutcome::Added { .. } => {}
        other => panic!("expected Added, got {other:?}"),
    }

    let text = std::fs::read_to_string(&wal_path).expect("wal file written");
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "exactly one audit line after one add");
    let v: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSON line");
    assert_eq!(v["op"], "add");
    assert_eq!(v["project"], "demo");
    assert_eq!(v["category"], "bug");
    assert_eq!(v["detail"]["outcome"], "added");
    assert!(v["ts"].as_str().is_some(), "audit line carries a timestamp");
}
