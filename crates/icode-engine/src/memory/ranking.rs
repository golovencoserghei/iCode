//! Usage-aware ranking for memories (the M4.2 "alive" relevance layer).
//!
//! `importance` is set once at creation and never moves, so a recall that sorted
//! by importance alone always surfaced the same records regardless of whether
//! they were ever useful. This module makes ranking dynamic: a record accessed
//! often rises (the access boost), one never touched slowly sinks (time decay).
//! Ported from meMCP's `ranking.py` — same formula, no code copied.
//!
//! ```text
//!   effective_score = importance_weight * (importance / 5)
//!                   + min(access_boost * access_count, access_boost_cap)
//!                   - decay_per_hour * hours_since(last_accessed_at)
//! ```
//!
//! **L0 immunity.** An always-on rule (e.g. "respond in Russian") must NEVER sink
//! via decay, even if never explicitly accessed. For an L0 record the decay term
//! is forced to zero, so its score is purely importance + access boost.

use chrono::{DateTime, Utc};
use icode_core::config::RankingConfig;
use icode_core::model::MemoryRecord;

/// Max importance on the 0..=5 scale (the normalisation denominator).
const MAX_IMPORTANCE: f32 = 5.0;

/// L0 tags marking an always-on rule. A record with `importance >= l0_min` AND any
/// of these tags is decay-immune (mirrors meMCP's `_L0_TAGS`).
const L0_TAGS: [&str; 3] = ["l0", "always-on", "critical"];

/// True if a memory is an always-on rule: very high importance (>= 5.0), or
/// `importance >= cfg.l0_min_importance` carrying an explicit L0 tag. L0 records
/// are decay-immune in [`effective_score`].
pub fn is_l0(rec: &MemoryRecord, cfg: &RankingConfig) -> bool {
    let imp = rec.importance;
    if imp < cfg.l0_min_importance {
        return false;
    }
    if imp >= MAX_IMPORTANCE {
        return true;
    }
    rec.tags
        .iter()
        .any(|t| L0_TAGS.contains(&t.trim().to_lowercase().as_str()))
}

/// Hours elapsed since an ISO-8601 timestamp relative to `now`. Returns 0.0 for a
/// missing/unparseable timestamp or one in the future (clock skew) — i.e. no decay
/// penalty when recency is unknown, matching meMCP's `_hours_since`.
fn hours_since(ts: &str, now: DateTime<Utc>) -> f32 {
    if ts.is_empty() {
        return 0.0;
    }
    let Ok(when) = DateTime::parse_from_rfc3339(ts) else {
        return 0.0;
    };
    let delta_hours = (now - when.with_timezone(&Utc)).num_seconds() as f32 / 3600.0;
    if delta_hours > 0.0 {
        delta_hours
    } else {
        0.0
    }
}

/// Dynamic relevance score blending importance, access frequency, and recency.
///
/// Resilient to legacy rows: `access_count` defaults to 0, an unparseable
/// `last_accessed_at` yields no decay. L0 records (see [`is_l0`]) have the decay
/// term forced to 0 so they can never sink below freshly-touched noise.
pub fn effective_score(rec: &MemoryRecord, cfg: &RankingConfig, now: DateTime<Utc>) -> f32 {
    let importance = rec.importance.clamp(0.0, MAX_IMPORTANCE);
    let importance_norm = importance / MAX_IMPORTANCE;

    let access_boost = (cfg.access_boost * rec.access_count as f32).min(cfg.access_boost_cap);

    // L0 rules are decay-immune: they must stay on top regardless of how long ago
    // they were last touched (otherwise "respond in Russian" could sink).
    let decay = if is_l0(rec, cfg) {
        0.0
    } else {
        cfg.decay_per_hour * hours_since(&rec.last_accessed_at, now)
    };

    cfg.importance_weight * importance_norm + access_boost - decay
}

/// Records ordered by [`effective_score`] descending. Pure (clones into a new
/// vec); `now` is sampled once by the caller. A stable sort keeps equal-score
/// records in their input order.
pub fn rank(records: &[MemoryRecord], cfg: &RankingConfig, now: DateTime<Utc>) -> Vec<MemoryRecord> {
    let mut scored: Vec<(f32, MemoryRecord)> = records
        .iter()
        .map(|r| (effective_score(r, cfg, now), r.clone()))
        .collect();
    // Descending by score; partial_cmp can't be NaN here (all inputs finite).
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use icode_core::ids::MemoryId;
    use icode_core::model::{Category, MemoryStatus};

    /// A record knob-able on the fields ranking actually reads.
    fn rec(importance: f32, access_count: u64, last_accessed_at: String, tags: Vec<&str>) -> MemoryRecord {
        MemoryRecord {
            id: MemoryId("mem_x__p".into()),
            project: "p".into(),
            content: "c".into(),
            category: Category::General,
            tags: tags.into_iter().map(String::from).collect(),
            importance,
            status: MemoryStatus::Active,
            resolved_at: None,
            resolve_reason: None,
            session_id: None,
            access_count,
            created_at: "2026-01-01T00:00:00+00:00".into(),
            last_accessed_at,
            updated_at: None,
        }
    }

    fn iso(now: DateTime<Utc>, hours_ago: i64) -> String {
        (now - chrono::Duration::hours(hours_ago)).to_rfc3339()
    }

    #[test]
    fn effective_score_matches_formula() {
        let cfg = RankingConfig::default(); // iw=1, boost=0.03, cap=1, decay=0.005/h, l0_min=4
        let now = Utc::now();
        // importance 2/5 = 0.4*1.0; access 10*0.03 = 0.30; decay 0.005*10h = 0.05.
        let r = rec(2.0, 10, iso(now, 10), vec![]);
        let s = effective_score(&r, &cfg, now);
        let expected = 1.0 * (2.0 / 5.0) + 0.30 - 0.05;
        assert!((s - expected).abs() < 1e-5, "got {s}, expected {expected}");
    }

    #[test]
    fn access_boost_is_capped() {
        let cfg = RankingConfig::default(); // cap = 1.0
        let now = Utc::now();
        // 1000 accesses → boost would be 30.0 but is clamped to the 1.0 cap.
        let r = rec(0.0, 1000, iso(now, 0), vec![]);
        let s = effective_score(&r, &cfg, now);
        assert!((s - 1.0).abs() < 1e-5, "access boost must cap at 1.0, got {s}");
    }

    #[test]
    fn l0_is_immune_to_decay() {
        let cfg = RankingConfig::default();
        let now = Utc::now();
        // An importance-5 record last touched a full year ago: a normal record
        // would be deep underwater (decay 0.005 * 8760h ≈ 43.8), but L0 immunity
        // pins the score at importance + access boost (here exactly 1.0).
        let l0 = rec(5.0, 0, iso(now, 24 * 365), vec![]);
        assert!(is_l0(&l0, &cfg), "importance 5.0 is L0 by itself");
        let s = effective_score(&l0, &cfg, now);
        assert!((s - 1.0).abs() < 1e-5, "L0 decay must be zero, got {s}");

        // A non-L0 record with the SAME age sinks far below zero.
        let normal = rec(4.0, 0, iso(now, 24 * 365), vec![]);
        assert!(!is_l0(&normal, &cfg), "importance 4.0 without an L0 tag is not L0");
        assert!(effective_score(&normal, &cfg, now) < 0.0);
    }

    #[test]
    fn l0_by_tag_at_min_importance() {
        let cfg = RankingConfig::default(); // l0_min = 4.0
        // importance == l0_min with a recognised tag → L0.
        assert!(is_l0(&rec(4.0, 0, String::new(), vec!["always-on"]), &cfg));
        assert!(is_l0(&rec(4.0, 0, String::new(), vec!["L0"]), &cfg)); // case-insensitive
        assert!(is_l0(&rec(4.5, 0, String::new(), vec!["critical"]), &cfg));
        // Below l0_min, the tag does not promote it.
        assert!(!is_l0(&rec(3.9, 0, String::new(), vec!["l0"]), &cfg));
        // At/over l0_min but no L0 tag and under 5.0 → not L0.
        assert!(!is_l0(&rec(4.0, 0, String::new(), vec!["bug"]), &cfg));
    }

    #[test]
    fn rank_orders_l0_above_fresh_low_importance() {
        let cfg = RankingConfig::default();
        let now = Utc::now();
        // Old, never-accessed L0 vs fresh, low-importance: L0 must win.
        let old_l0 = rec(5.0, 0, iso(now, 24 * 90), vec!["l0"]);
        let fresh_low = rec(0.0, 0, iso(now, 0), vec![]);
        let ranked = rank(&[fresh_low.clone(), old_l0.clone()], &cfg, now);
        assert_eq!(ranked[0].importance, 5.0, "L0 must rank first despite age");
    }

    #[test]
    fn future_timestamp_does_not_boost_score() {
        let cfg = RankingConfig::default();
        let now = Utc::now();
        // last_accessed_at in the future (clock skew) → no negative decay term.
        let r = rec(0.0, 0, iso(now, -100), vec![]);
        let s = effective_score(&r, &cfg, now);
        assert!((s - 0.0).abs() < 1e-5, "future ts must not add positive decay, got {s}");
    }
}
