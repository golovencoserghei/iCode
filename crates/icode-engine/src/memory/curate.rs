//! `curate` — keeping the memory base worth reading.
//!
//! Memory accumulates monotonically: every session adds, nothing ever leaves. The
//! ranking layer softens that (a record nobody touches decays), but decay only
//! reorders — the stale record is still in the corpus, still competing for the
//! eight slots `session_start` injects, still being read by an agent as if it were
//! current. Two failure modes follow, and both were visible in this project's own
//! base: near-duplicate records of the same fact written months apart, and records
//! describing code that no longer exists.
//!
//! What this does about it, and what it deliberately does not:
//!
//! * **Never deletes.** Archiving is `resolve` with a reason, which is the store's
//!   existing reversible "no longer current" state: the record stays, drops out of
//!   default reads, and comes back with `include_resolved`. A curator that deletes
//!   is a curator you cannot afford to run.
//! * **Never touches what matters.** L0 / always-on rules and anything at
//!   importance >= [`IMMUNE_IMPORTANCE`] are skipped outright, whatever their age.
//!   The whole point of an always-on rule is that it survives not being used.
//! * **Dry run by default.** [`curate`] reports what it WOULD do; only an explicit
//!   `apply` writes. The judgement here is heuristic, and a heuristic that acts
//!   before you have read it is how a knowledge base loses something it needed.
//! * **No model required.** Duplicate detection is lexical (Jaccard over the token
//!   sets), so curation works with Ollama down — the same rule the rest of the
//!   maintenance surface follows.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use icode_core::config::RankingConfig;
use icode_core::error::Result;
use icode_core::ids::MemoryId;
use icode_core::model::{MemoryRecord, MemoryStatus};
use icode_core::traits::MemoryStore;
use serde::{Deserialize, Serialize};

use super::ranking::is_l0;

/// Importance at or above which a record is never archived, however old.
const IMMUNE_IMPORTANCE: f32 = 4.0;

/// How many records one pass considers. The duplicate scan is pairwise, so this
/// also bounds it: 2000 records is ~2M comparisons of small token sets, which is
/// milliseconds, while a project that size is already far past needing a curator.
const MAX_SCAN: usize = 2000;

/// Tokens shorter than this are dropped before comparing: they are mostly
/// articles and glue, and they inflate similarity between unrelated records.
const MIN_TOKEN_LEN: usize = 3;

/// Knobs. Defaults are deliberately conservative — the cost of archiving a record
/// that was still wanted is higher than the cost of leaving one more stale row.
#[derive(Clone, Copy, Debug)]
pub struct CurateConfig {
    /// A record untouched for longer than this is a staleness candidate.
    pub stale_days: f64,
    /// ...but only when its importance is at or below this.
    pub max_stale_importance: f32,
    /// Jaccard similarity at which two records are treated as the same fact.
    pub dup_similarity: f64,
    /// Write the changes. False = report only.
    pub apply: bool,
}

impl Default for CurateConfig {
    fn default() -> Self {
        Self {
            stale_days: 90.0,
            max_stale_importance: 2.0,
            dup_similarity: 0.8,
            apply: false,
        }
    }
}

/// One record the pass would archive (or did).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Archived {
    pub id: String,
    pub category: String,
    pub importance: f32,
    /// Days since the record was last read or written.
    pub idle_days: i64,
    pub reason: String,
    /// First line of the content, so the report is readable without a second call.
    pub preview: String,
}

/// Two records that say the same thing; `loser` is the one archived.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DuplicatePair {
    pub kept: String,
    pub archived: String,
    pub similarity: f64,
    pub preview: String,
}

/// What a pass found.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CurateReport {
    /// Active records examined.
    pub scanned: usize,
    /// Records skipped because they are L0 / high-importance.
    pub immune: usize,
    /// Stale records (archived when `apply`).
    pub stale: Vec<Archived>,
    /// Near-duplicates (the loser archived when `apply`).
    pub duplicates: Vec<DuplicatePair>,
    /// False when this was a report-only pass — nothing was written.
    pub applied: bool,
}

/// Lower-cased word tokens of length >= [`MIN_TOKEN_LEN`], deduplicated.
///
/// Splitting on `!char::is_alphanumeric` keeps this working for Cyrillic content
/// (this base is largely Russian): `char::is_alphanumeric` is Unicode-aware, where
/// an ASCII-only split would reduce every Russian record to the empty set and make
/// them all "identical" to each other.
fn tokens(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= MIN_TOKEN_LEN)
        .map(|t| t.to_lowercase())
        .collect()
}

/// Jaccard similarity of two token sets: |A ∩ B| / |A ∪ B|.
fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = (a.len() + b.len()) as f64 - inter;
    if union == 0.0 {
        return 0.0;
    }
    inter / union
}

fn days_since(ts: &str, now: DateTime<Utc>) -> i64 {
    DateTime::parse_from_rfc3339(ts)
        .map(|t| (now - t.with_timezone(&Utc)).num_days())
        .unwrap_or(0)
}

fn preview(content: &str) -> String {
    let line = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let trimmed = line.trim();
    match trimmed.char_indices().nth(100) {
        Some((idx, _)) => format!("{}…", &trimmed[..idx]),
        None => trimmed.to_string(),
    }
}

/// Which of two records to keep: higher importance, then more used, then newer.
///
/// Recency alone would be wrong — the second write of a fact is often a hurried
/// restatement of a carefully written original — so usage and importance decide
/// first.
fn keeps_priority(r: &MemoryRecord) -> (i64, i64, String) {
    (
        (r.importance * 100.0) as i64,
        r.access_count as i64,
        r.updated_at.clone().unwrap_or_else(|| r.created_at.clone()),
    )
}

/// Run one curation pass over a project's active memory.
pub fn curate(
    store: &dyn MemoryStore,
    project: &str,
    cfg: CurateConfig,
    ranking: &RankingConfig,
) -> Result<CurateReport> {
    let now = Utc::now();
    let records = store.list(project, None, MAX_SCAN, false)?;
    let mut report = CurateReport {
        scanned: 0,
        applied: cfg.apply,
        ..Default::default()
    };

    // Candidates = active, non-immune records. Immunity is checked FIRST so an
    // always-on rule is never even considered for archiving.
    let mut candidates: Vec<&MemoryRecord> = Vec::new();
    for rec in &records {
        if rec.status != MemoryStatus::Active {
            continue;
        }
        report.scanned += 1;
        if rec.importance >= IMMUNE_IMPORTANCE || is_l0(rec, ranking) {
            report.immune += 1;
            continue;
        }
        candidates.push(rec);
    }

    // ── near-duplicates ──────────────────────────────────────────────────────
    // Compared within the candidate set only: a duplicate of an immune record is
    // left alone too, because the immune one is exactly the copy worth keeping and
    // the pair may be deliberate (a rule restated in context).
    let token_sets: Vec<HashSet<String>> = candidates.iter().map(|r| tokens(&r.content)).collect();
    let mut archived_ids: HashSet<String> = HashSet::new();

    for i in 0..candidates.len() {
        for j in (i + 1)..candidates.len() {
            let (a, b) = (candidates[i], candidates[j]);
            if archived_ids.contains(a.id.as_str()) || archived_ids.contains(b.id.as_str()) {
                continue;
            }
            let sim = jaccard(&token_sets[i], &token_sets[j]);
            if sim < cfg.dup_similarity {
                continue;
            }
            let (keep, drop) = if keeps_priority(a) >= keeps_priority(b) {
                (a, b)
            } else {
                (b, a)
            };
            archived_ids.insert(drop.id.to_string());
            report.duplicates.push(DuplicatePair {
                kept: keep.id.to_string(),
                archived: drop.id.to_string(),
                similarity: (sim * 100.0).round() / 100.0,
                preview: preview(&drop.content),
            });
        }
    }

    // ── stale ────────────────────────────────────────────────────────────────
    for rec in &candidates {
        if archived_ids.contains(rec.id.as_str()) {
            continue; // already going as a duplicate; one reason is enough
        }
        if rec.importance > cfg.max_stale_importance {
            continue;
        }
        let idle = days_since(&rec.last_accessed_at, now);
        if (idle as f64) < cfg.stale_days {
            continue;
        }
        archived_ids.insert(rec.id.to_string());
        report.stale.push(Archived {
            id: rec.id.to_string(),
            category: format!("{:?}", rec.category).to_lowercase(),
            importance: rec.importance,
            idle_days: idle,
            reason: format!("not read for {idle} days at importance {}", rec.importance),
            preview: preview(&rec.content),
        });
    }

    if cfg.apply {
        for d in &report.duplicates {
            let id = MemoryId::from(d.archived.clone());
            store.resolve(
                &id,
                &format!(
                    "curator: near-duplicate of {} (similarity {:.2}) — archived, not deleted",
                    d.kept, d.similarity
                ),
            )?;
        }
        for a in &report.stale {
            let id = MemoryId::from(a.id.clone());
            store.resolve(&id, &format!("curator: {} — archived, not deleted", a.reason))?;
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jaccard_sees_restatements_but_not_unrelated_facts() {
        let a = tokens("chose ChromaDB embedded over Qdrant — simpler deployment, no service");
        let b = tokens("chose ChromaDB embedded over Qdrant: simpler deployment, no service!");
        assert!(jaccard(&a, &b) > 0.9, "punctuation is not a difference");

        let c = tokens("the daemon lies about its status when the process is a zombie");
        assert!(jaccard(&a, &c) < 0.2, "unrelated records must stay apart");
    }

    #[test]
    fn tokens_work_for_cyrillic_content() {
        // The base is largely Russian. An ASCII-only tokenizer would return the
        // empty set here, and every Russian record would look identical to every
        // other one — the curator would then archive the whole project.
        let a = tokens("демон врёт о статусе когда процесс зомби");
        assert!(a.contains("демон"), "got {a:?}");
        let b = tokens("демон врёт о статусе, когда процесс — зомби");
        assert!(jaccard(&a, &b) > 0.9);
        let c = tokens("настройки окна хранятся на сервере");
        assert!(jaccard(&a, &c) < 0.2);
    }

    #[test]
    fn preview_is_the_first_non_empty_line_bounded() {
        assert_eq!(preview("\n\nfirst real line\nsecond"), "first real line");
        let long = "x".repeat(200);
        assert!(preview(&long).chars().count() <= 101);
    }
}
