//! `WalStore` — a write-ahead audit-log decorator over any `MemoryStore`.
//!
//! Top of the memory decorator chain (`Wal → … → SqliteMemoryStore`). It is a
//! pure pass-through for reads, and for every WRITE it first delegates to the
//! inner store and THEN appends one JSONL line describing the mutation to an audit
//! file (default `~/.icode/wal.jsonl`). The log is an append-only trail of *what
//! changed* — id, op, project, timestamp (plus category + a content prefix for
//! `add`) — for forensics / replay, NOT the SQLite WAL journal (that is the db's
//! own concern).
//!
//! Composition (decorator pattern, per ISP/DIP):
//! ```ignore
//! let store: Arc<dyn MemoryStore> =
//!     Arc::new(WalStore::new(Arc::new(SqliteMemoryStore::open(...)?), wal_path));
//! ```
//!
//! **Best-effort, never load-bearing.** The audit append happens AFTER the inner
//! op has already succeeded, so a WAL write failure must not undo or fail the
//! operation — it is logged via `tracing` and swallowed. Reads are never logged.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use icode_core::error::Result;
use icode_core::ids::MemoryId;
use icode_core::model::*;
use icode_core::traits::{MemoryStore, ReadableMemoryStore, WritableMemoryStore};
use serde_json::json;

/// Max chars of an added memory's content recorded in the audit line (enough to
/// recognise the entry without copying large bodies into the log).
const CONTENT_PREVIEW_CHARS: usize = 120;

/// Audit-log decorator. Cheap to share (inner is an `Arc`).
pub struct WalStore {
    inner: Arc<dyn MemoryStore>,
    path: PathBuf,
}

impl WalStore {
    /// Wrap `inner`, appending the audit trail to `path` (created with parents on
    /// first write). For production pass `~/.icode/wal.jsonl`; for tests a temp file.
    pub fn new(inner: Arc<dyn MemoryStore>, path: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            path: path.into(),
        }
    }

    /// Append one audit record (best-effort). Builds the JSON line, then writes it;
    /// any IO error is logged and swallowed so the already-committed op survives.
    fn audit(&self, entry: serde_json::Value) {
        if let Err(e) = self.try_append(&entry) {
            tracing::warn!(target: "icode.wal", error = %e, "WAL audit append failed (op already committed)");
        }
    }

    /// The fallible append: ensure the parent dir exists, open in append mode, write
    /// one compact JSON line + newline.
    fn try_append(&self, entry: &serde_json::Value) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = serde_json::to_string(entry).unwrap_or_else(|_| "{}".to_string());
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(())
    }
}

/// First `CONTENT_PREVIEW_CHARS` chars of `content` (char-boundary safe).
fn content_preview(content: &str) -> String {
    content.chars().take(CONTENT_PREVIEW_CHARS).collect()
}

// ── reads: pure delegation, NEVER logged ──

impl ReadableMemoryStore for WalStore {
    fn search(
        &self,
        project: &str,
        query: &str,
        n: usize,
        category: Option<Category>,
        include_resolved: bool,
    ) -> Result<Vec<MemoryHit>> {
        self.inner.search(project, query, n, category, include_resolved)
    }

    fn search_all(&self, query: &str, n: usize, include_resolved: bool) -> Result<Vec<MemoryHit>> {
        self.inner.search_all(query, n, include_resolved)
    }

    fn list(
        &self,
        project: &str,
        category: Option<Category>,
        limit: usize,
        include_resolved: bool,
    ) -> Result<Vec<MemoryRecord>> {
        self.inner.list(project, category, limit, include_resolved)
    }

    fn get(&self, id: &MemoryId) -> Result<Option<MemoryRecord>> {
        self.inner.get(id)
    }

    fn list_projects(&self) -> Result<Vec<(String, u64)>> {
        self.inner.list_projects()
    }
}

// ── writes: delegate first, THEN audit (best-effort) ──

impl WritableMemoryStore for WalStore {
    fn add(&self, mem: NewMemory) -> Result<AddOutcome> {
        // Capture audit fields before `mem` is moved into the inner store.
        let project = mem.project.clone();
        let category = category_str(mem.category);
        let preview = content_preview(&mem.content);
        let outcome = self.inner.add(mem)?;
        // Record the real outcome: a deduped add is an "add" attempt that resolved
        // to an existing id (no new row), distinguished by `outcome`.
        let (op, id, extra) = match &outcome {
            AddOutcome::Added { id } => ("add", id.0.clone(), json!({ "outcome": "added" })),
            AddOutcome::Duplicate { existing, distance } => (
                "add",
                existing.0.clone(),
                json!({ "outcome": "duplicate", "distance": distance }),
            ),
        };
        self.audit(json!({
            "ts": Utc::now().to_rfc3339(),
            "op": op,
            "id": id,
            "project": project,
            "category": category,
            "content_preview": preview,
            "detail": extra,
        }));
        Ok(outcome)
    }

    fn update(&self, id: &MemoryId, content: Option<&str>, tags: Option<&[String]>) -> Result<()> {
        self.inner.update(id, content, tags)?;
        self.audit(json!({
            "ts": Utc::now().to_rfc3339(),
            "op": "update",
            "id": id.0,
            "project": id.project(),
            "content_changed": content.is_some(),
            "tags_changed": tags.is_some(),
        }));
        Ok(())
    }

    fn delete(&self, id: &MemoryId) -> Result<()> {
        self.inner.delete(id)?;
        self.audit(json!({
            "ts": Utc::now().to_rfc3339(),
            "op": "delete",
            "id": id.0,
            "project": id.project(),
        }));
        Ok(())
    }

    fn resolve(&self, id: &MemoryId, reason: &str) -> Result<()> {
        self.inner.resolve(id, reason)?;
        self.audit(json!({
            "ts": Utc::now().to_rfc3339(),
            "op": "resolve",
            "id": id.0,
            "project": id.project(),
            "reason": reason,
        }));
        Ok(())
    }

    // record_access / upsert_project / touch_session are bookkeeping, not durable
    // content mutations — delegated WITHOUT an audit line (would flood the log;
    // access warm-up fires on every search).
    fn record_access(&self, ids: &[MemoryId]) -> Result<()> {
        self.inner.record_access(ids)
    }

    fn upsert_project(&self, name: &str, root_path: &str) -> Result<()> {
        self.inner.upsert_project(name, root_path)
    }

    fn touch_session(&self, project: &str, session_id: &str) -> Result<()> {
        self.inner.touch_session(project, session_id)
    }
}

/// Local copy of the category → wire string mapping (kept private to `store.rs`
/// otherwise). Small and stable; avoids widening that module's surface.
fn category_str(c: Category) -> &'static str {
    match c {
        Category::Decision => "decision",
        Category::Progress => "progress",
        Category::Context => "context",
        Category::Bug => "bug",
        Category::Todo => "todo",
        Category::Code => "code",
        Category::General => "general",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A trivial in-memory fake `MemoryStore` so the WAL tests need no Ollama/SQLite.
    /// Records the writes it received and hands back canned outcomes.
    #[derive(Default)]
    struct FakeStore {
        added: Mutex<Vec<String>>,
        next_duplicate: Mutex<Option<MemoryId>>,
    }

    impl ReadableMemoryStore for FakeStore {
        fn search(
            &self,
            _: &str,
            _: &str,
            _: usize,
            _: Option<Category>,
            _: bool,
        ) -> Result<Vec<MemoryHit>> {
            Ok(vec![])
        }
        fn search_all(&self, _: &str, _: usize, _: bool) -> Result<Vec<MemoryHit>> {
            Ok(vec![])
        }
        fn list(&self, _: &str, _: Option<Category>, _: usize, _: bool) -> Result<Vec<MemoryRecord>> {
            Ok(vec![])
        }
        fn get(&self, _: &MemoryId) -> Result<Option<MemoryRecord>> {
            Ok(None)
        }
        fn list_projects(&self) -> Result<Vec<(String, u64)>> {
            Ok(vec![])
        }
    }

    impl WritableMemoryStore for FakeStore {
        fn add(&self, mem: NewMemory) -> Result<AddOutcome> {
            self.added.lock().unwrap().push(mem.content.clone());
            if let Some(existing) = self.next_duplicate.lock().unwrap().take() {
                return Ok(AddOutcome::Duplicate { existing, distance: 0.05 });
            }
            Ok(AddOutcome::Added {
                id: MemoryId(format!("mem_test__{}", mem.project)),
            })
        }
        fn update(&self, _: &MemoryId, _: Option<&str>, _: Option<&[String]>) -> Result<()> {
            Ok(())
        }
        fn delete(&self, _: &MemoryId) -> Result<()> {
            Ok(())
        }
        fn resolve(&self, _: &MemoryId, _: &str) -> Result<()> {
            Ok(())
        }
        fn record_access(&self, _: &[MemoryId]) -> Result<()> {
            Ok(())
        }
        fn upsert_project(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn touch_session(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
    }

    fn new_mem(project: &str, content: &str) -> NewMemory {
        NewMemory {
            project: project.into(),
            content: content.into(),
            category: Category::Bug,
            tags: vec![],
            importance: 0.0,
            session_id: None,
        }
    }

    fn read_lines(path: &std::path::Path) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("each WAL line is valid JSON"))
            .collect()
    }

    #[test]
    fn add_appends_one_jsonl_line() {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("wal.jsonl");
        let store = WalStore::new(Arc::new(FakeStore::default()), wal.clone());

        store.add(new_mem("demo", "fixed the auth race condition")).unwrap();

        let lines = read_lines(&wal);
        assert_eq!(lines.len(), 1, "exactly one audit line after one add");
        assert_eq!(lines[0]["op"], "add");
        assert_eq!(lines[0]["project"], "demo");
        assert_eq!(lines[0]["category"], "bug");
        assert_eq!(lines[0]["detail"]["outcome"], "added");
        assert!(lines[0]["content_preview"].as_str().unwrap().contains("auth race"));
    }

    #[test]
    fn duplicate_add_is_audited_as_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("wal.jsonl");
        let fake = FakeStore::default();
        *fake.next_duplicate.lock().unwrap() = Some(MemoryId("mem_existing__demo".into()));
        let store = WalStore::new(Arc::new(fake), wal.clone());

        let outcome = store.add(new_mem("demo", "dup content")).unwrap();
        assert!(matches!(outcome, AddOutcome::Duplicate { .. }));

        let lines = read_lines(&wal);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["op"], "add");
        assert_eq!(lines[0]["id"], "mem_existing__demo");
        assert_eq!(lines[0]["detail"]["outcome"], "duplicate");
    }

    #[test]
    fn writes_accumulate_reads_do_not_log() {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("wal.jsonl");
        let store = WalStore::new(Arc::new(FakeStore::default()), wal.clone());

        store.add(new_mem("p", "a")).unwrap();
        store.update(&MemoryId("mem_1__p".into()), Some("b"), None).unwrap();
        store.delete(&MemoryId("mem_1__p".into())).unwrap();
        store.resolve(&MemoryId("mem_2__p".into()), "done").unwrap();
        // Reads + bookkeeping must add NO lines.
        let _ = store.search("p", "q", 5, None, false).unwrap();
        let _ = store.get(&MemoryId("mem_1__p".into())).unwrap();
        store.record_access(&[MemoryId("mem_1__p".into())]).unwrap();
        store.upsert_project("p", "/root").unwrap();

        let ops: Vec<String> = read_lines(&wal)
            .iter()
            .map(|l| l["op"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ops, vec!["add", "update", "delete", "resolve"], "only the 4 mutations are logged");
    }

    #[test]
    fn wal_write_failure_does_not_fail_the_op() {
        // Point the WAL at a path whose "parent" is actually a FILE, so creating
        // the directory fails — the add must still succeed (best-effort audit).
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("blocker");
        std::fs::write(&not_a_dir, b"x").unwrap();
        let wal = not_a_dir.join("nested").join("wal.jsonl");
        let store = WalStore::new(Arc::new(FakeStore::default()), wal);

        let outcome = store.add(new_mem("p", "still works")).unwrap();
        assert!(matches!(outcome, AddOutcome::Added { .. }), "op survives a WAL append failure");
    }
}
