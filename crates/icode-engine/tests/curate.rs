//! Curation-pass behaviour, against an in-memory fake store.
//!
//! Hermetic on purpose: curation is lexical and never embeds, so proving it needs
//! no Ollama and no SQLite. The fake implements the same store traits the real one
//! does, which is also the point — a curator that needed privileged access to the
//! database would be a curator that could not be run read-only.

use std::sync::Mutex;

use icode_core::config::RankingConfig;
use icode_core::error::Result;
use icode_core::ids::MemoryId;
use icode_core::model::{
    AddOutcome, Category, MemoryHit, MemoryRecord, MemoryStatus, NewMemory,
};
use icode_core::traits::{ReadableMemoryStore, WritableMemoryStore};
use icode_engine::{curate, CurateConfig};

/// Minimal store standing in for the real one: holds records, honours `list`'s
/// active-only filter, and records `resolve` calls.
struct FakeStore {
    records: Mutex<Vec<MemoryRecord>>,
}

impl FakeStore {
    fn new(records: Vec<MemoryRecord>) -> Self {
        Self {
            records: Mutex::new(records),
        }
    }

    fn resolved(&self) -> Vec<(String, String)> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.status == MemoryStatus::Resolved)
            .map(|r| {
                (
                    r.id.to_string(),
                    r.resolve_reason.clone().unwrap_or_default(),
                )
            })
            .collect()
    }
}

impl ReadableMemoryStore for FakeStore {
    fn search(
        &self,
        _project: &str,
        _query: &str,
        _n: usize,
        _category: Option<Category>,
        _include_resolved: bool,
    ) -> Result<Vec<MemoryHit>> {
        Ok(vec![])
    }
    fn search_all(&self, _q: &str, _n: usize, _include_resolved: bool) -> Result<Vec<MemoryHit>> {
        Ok(vec![])
    }
    fn list(
        &self,
        project: &str,
        _category: Option<Category>,
        limit: usize,
        include_resolved: bool,
    ) -> Result<Vec<MemoryRecord>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.project == project)
            .filter(|r| include_resolved || r.status == MemoryStatus::Active)
            .take(limit)
            .cloned()
            .collect())
    }
    fn get(&self, id: &MemoryId) -> Result<Option<MemoryRecord>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id.to_string() == id.to_string())
            .cloned())
    }
    fn list_projects(&self) -> Result<Vec<(String, u64)>> {
        Ok(vec![])
    }
}

impl WritableMemoryStore for FakeStore {
    fn add(&self, _mem: NewMemory) -> Result<AddOutcome> {
        unreachable!("curation never adds")
    }
    fn update(&self, _id: &MemoryId, _c: Option<&str>, _t: Option<&[String]>) -> Result<()> {
        unreachable!("curation never edits content")
    }
    fn delete(&self, _id: &MemoryId) -> Result<()> {
        panic!("curation must NEVER delete — archiving is reversible, deleting is not");
    }
    fn resolve(&self, id: &MemoryId, reason: &str) -> Result<()> {
        let mut recs = self.records.lock().unwrap();
        let rec = recs
            .iter_mut()
            .find(|r| r.id.to_string() == id.to_string())
            .expect("resolve of an unknown id");
        rec.status = MemoryStatus::Resolved;
        rec.resolve_reason = Some(reason.to_string());
        Ok(())
    }
    fn record_access(&self, _ids: &[MemoryId]) -> Result<()> {
        Ok(())
    }
    fn upsert_project(&self, _name: &str, _root: &str) -> Result<()> {
        Ok(())
    }
    fn touch_session(&self, _project: &str, _session: &str) -> Result<()> {
        Ok(())
    }
}

fn days_ago(n: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::days(n)).to_rfc3339()
}

fn rec(id: &str, content: &str, importance: f32, idle_days: i64) -> MemoryRecord {
    MemoryRecord {
        id: MemoryId::from(id.to_string()),
        project: "demo".into(),
        content: content.into(),
        category: Category::Context,
        tags: vec![],
        importance,
        status: MemoryStatus::Active,
        resolved_at: None,
        resolve_reason: None,
        session_id: None,
        access_count: 0,
        created_at: days_ago(idle_days),
        last_accessed_at: days_ago(idle_days),
        updated_at: None,
    }
}

fn cfg(apply: bool) -> CurateConfig {
    CurateConfig {
        apply,
        ..Default::default()
    }
}

#[test]
fn a_dry_run_changes_nothing() {
    let store = FakeStore::new(vec![rec("m1", "an old note nobody reads any more", 1.0, 200)]);

    let report = curate(&store, "demo", cfg(false), &RankingConfig::default()).expect("curate");

    assert_eq!(report.stale.len(), 1, "it is reported…");
    assert!(!report.applied);
    assert!(store.resolved().is_empty(), "…but nothing was written");
}

#[test]
fn applying_archives_stale_records_with_a_reason() {
    let store = FakeStore::new(vec![rec("m1", "an old note nobody reads any more", 1.0, 200)]);

    let report = curate(&store, "demo", cfg(true), &RankingConfig::default()).expect("curate");

    assert!(report.applied);
    let resolved = store.resolved();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].0, "m1");
    assert!(
        resolved[0].1.contains("archived, not deleted"),
        "the reason must say what happened: {}",
        resolved[0].1
    );
}

#[test]
fn important_and_l0_records_are_never_archived() {
    let mut always_on = rec("l0", "always answer in Russian", 5.0, 900);
    always_on.tags = vec!["l0".into()];
    let store = FakeStore::new(vec![
        rec("high", "an architectural decision that still holds", 4.0, 900),
        always_on,
        rec("low", "a passing note", 0.0, 900),
    ]);

    let report = curate(&store, "demo", cfg(true), &RankingConfig::default()).expect("curate");

    assert_eq!(report.immune, 2, "importance>=4 and the L0 rule are immune");
    let archived: Vec<String> = store.resolved().into_iter().map(|(id, _)| id).collect();
    assert_eq!(archived, vec!["low".to_string()]);
}

#[test]
fn a_recent_record_is_left_alone_however_unimportant() {
    let store = FakeStore::new(vec![rec("fresh", "noted this yesterday", 0.0, 1)]);

    let report = curate(&store, "demo", cfg(true), &RankingConfig::default()).expect("curate");

    assert!(report.stale.is_empty());
    assert!(store.resolved().is_empty());
}

#[test]
fn near_duplicates_collapse_to_the_better_record() {
    // Same fact, written twice. The keeper is decided by importance/usage, NOT by
    // recency — the later write is often the hastier one.
    let mut original = rec(
        "orig",
        "chose SQLite with sqlite-vec over a separate vector service: one file, no daemon to run",
        3.0,
        5,
    );
    original.access_count = 7;
    let restated = rec(
        "copy",
        "chose SQLite with sqlite-vec over a separate vector service — one file, no daemon to run!",
        1.0,
        1,
    );
    let store = FakeStore::new(vec![original, restated]);

    let report = curate(&store, "demo", cfg(true), &RankingConfig::default()).expect("curate");

    assert_eq!(report.duplicates.len(), 1);
    assert_eq!(report.duplicates[0].kept, "orig");
    assert_eq!(report.duplicates[0].archived, "copy");
    let resolved = store.resolved();
    assert_eq!(resolved.len(), 1);
    assert!(resolved[0].1.contains("near-duplicate of orig"));
}

#[test]
fn distinct_records_are_not_treated_as_duplicates() {
    let store = FakeStore::new(vec![
        rec("a", "the daemon reports RUNNING for a zombie process", 1.0, 5),
        rec("b", "window preferences live on the server, not in localStorage", 1.0, 5),
    ]);

    let report = curate(&store, "demo", cfg(true), &RankingConfig::default()).expect("curate");

    assert!(report.duplicates.is_empty());
    assert!(store.resolved().is_empty());
}

#[test]
fn a_record_is_archived_for_one_reason_only() {
    // Old AND duplicated: it must not be reported twice.
    let store = FakeStore::new(vec![
        rec("keep", "the indexer skips node_modules and every tool cache", 2.0, 200),
        rec("dupe", "the indexer skips node_modules and every tool cache!", 0.0, 200),
    ]);

    let report = curate(&store, "demo", cfg(true), &RankingConfig::default()).expect("curate");

    assert_eq!(report.duplicates.len(), 1);
    let ids: Vec<&str> = report.stale.iter().map(|s| s.id.as_str()).collect();
    assert!(
        !ids.contains(&"dupe"),
        "already archived as a duplicate: {ids:?}"
    );
    assert_eq!(store.resolved().len(), 2, "the keeper is stale in its own right");
}
