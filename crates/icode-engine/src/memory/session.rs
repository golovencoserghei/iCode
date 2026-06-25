//! Session orchestration over the central memory store (the MCP `session_start` /
//! `session_end` lifecycle). These are FREE functions over `&dyn MemoryStore`,
//! deliberately decoupled from any concrete embedder crate: they only compose the
//! frozen trait methods (`list` / `search` / `add` / `resolve` / `record_access` /
//! `touch_session`) the store already exposes.
//!
//! Behaviour mirrors the legacy meMCP `server.py`:
//!   * `session_start` loads the cross-project developer profile AND the recent
//!     project context in one call, warming up the returned ids (so usage-aware
//!     ranking sees the read).
//!   * `session_end` persists the wrap-up (summary / decisions / completed / todos)
//!     and AUTO-RESOLVES the open bug/todo memories each completed item closes.

use icode_core::error::Result;
use icode_core::ids::DEVELOPER_PROJECT;
use icode_core::model::{Category, MemoryRecord, NewMemory};
use icode_core::traits::MemoryStore;

/// Default number of recent project memories `session_start` returns.
pub const DEFAULT_PROJECT_MEMORIES: usize = 8;
/// Default number of developer-profile notes `session_start` returns.
pub const DEFAULT_PROFILE_NOTES: usize = 5;

/// Auto-resolve relevance gate (vec0 cosine *distance*, lower = closer). A
/// completed item only resolves an open bug/todo whose nearest search hit sits
/// AT OR BELOW this distance — i.e. the wrap-up text is genuinely about that
/// memory. The memory search fuses dense + lexical via RRF and reports a fused
/// *score* (higher = better), so the gate is applied to that score via
/// [`AUTO_RESOLVE_MIN_SCORE`] rather than a raw distance; this constant documents
/// the intent and is the empirical floor the score threshold was chosen to match.
pub const AUTO_RESOLVE_MAX_DISTANCE: f32 = 0.6;

/// Minimum fused memory-search score for a completed item to auto-resolve a
/// bug/todo. `search` returns RRF-fused scores (`Σ 1/(60+rank)` plus a small
/// ranking nudge); a single-list head sits at ~1/60 ≈ 0.0167 and an item that
/// tops BOTH the dense and lexical arms reaches ~2/60 ≈ 0.0333. We require the
/// hit to be a real top match, not incidental: only the FIRST (best) bug/todo hit
/// of each completed item is considered, and only if its score clears this floor.
/// Set conservatively low because RRF scores are small in absolute terms; the
/// real selectivity comes from "first hit only + must be bug/todo".
pub const AUTO_RESOLVE_MIN_SCORE: f32 = 0.016;

/// How many candidates each completed item pulls when hunting bug/todo to resolve.
const AUTO_RESOLVE_SEARCH_N: usize = 5;

/// Result of [`session_start`]: the cross-project developer profile plus the
/// recent project context, ready to prime the agent before any work. The fields
/// are `MemoryRecord`s (already `Serialize` in `icode-core`); the MCP layer
/// projects them to JSON — so this struct stays serde-free to keep the engine
/// off a direct `serde` dependency.
#[derive(Clone, Debug)]
pub struct SessionStart {
    /// Developer profile notes (from the reserved `__developer__` project),
    /// newest first, capped at the requested count.
    pub developer_profile: Vec<MemoryRecord>,
    /// Recent project memories (newest first), capped at the requested count.
    pub project_context: Vec<MemoryRecord>,
}

/// Result of [`session_end`]: how many memories were written and how many open
/// bug/todo records the completed items auto-resolved.
#[derive(Clone, Copy, Debug)]
pub struct SessionEnd {
    /// Memories actually inserted (a deduped `add` is NOT counted).
    pub saved: usize,
    /// Open bug/todo memories closed by the completed items.
    pub auto_resolved: usize,
}

/// Load the developer profile + recent project context for a fresh session.
///
/// * `developer_profile` = newest `n_profile` notes from the reserved
///   `__developer__` project (cross-project, how this dev likes to work).
/// * `project_context` = newest `n_project` active memories of `project`.
///
/// Both lists are warmed up via `record_access` (best-effort) so the returned ids
/// count as "used this session" for usage-aware ranking. `n == 0` skips that side.
pub fn session_start(
    store: &dyn MemoryStore,
    project: &str,
    n_project: usize,
    n_profile: usize,
) -> Result<SessionStart> {
    // Developer profile: list the reserved project directly (search needs a query;
    // session start has none, so "recent + important" via list-by-time is right).
    let developer_profile = if n_profile == 0 {
        Vec::new()
    } else {
        store.list(DEVELOPER_PROJECT, None, n_profile, /* include_resolved = */ false)?
    };

    // Project context: newest active memories. `list` already orders by
    // created_at DESC, which is the "what happened recently" view a session needs.
    let project_context = if n_project == 0 {
        Vec::new()
    } else {
        store.list(project, None, n_project, /* include_resolved = */ false)?
    };

    // Warm-up (best-effort): bump access_count/last_accessed_at for every id we
    // surfaced, so the ranking signal reflects that the session read them.
    let ids: Vec<_> = developer_profile
        .iter()
        .chain(project_context.iter())
        .map(|r| r.id.clone())
        .collect();
    if !ids.is_empty() {
        let _ = store.record_access(&ids);
    }

    Ok(SessionStart {
        developer_profile,
        project_context,
    })
}

/// Persist a session wrap-up and auto-resolve the bug/todo it closes.
///
/// Saves (skipping near-duplicates, which the store reports as `Duplicate`):
///   * `summary`   → one `Progress` memory (the session narrative).
///   * `decisions` → one `Decision` memory each.
///   * `completed` → one `Progress` memory each.
///   * `todos`     → one `Todo` memory each.
///
/// Then AUTO-RESOLVES: for every completed item, searches the project for the
/// nearest open memory; if the best hit is a `Bug` or `Todo` that clears
/// [`AUTO_RESOLVE_MIN_SCORE`], it is `resolve`d (the completed work closed it).
/// `session_id`, when given, records a session touch (`touch_session`).
///
/// Returns `{ saved, auto_resolved }`. Individual `add`/`resolve`/`search`
/// failures are NOT swallowed — a wrap-up that silently lost data would be worse
/// than a surfaced error — except the dedup outcome, which is the normal "already
/// known" path and simply isn't counted.
pub fn session_end(
    store: &dyn MemoryStore,
    project: &str,
    summary: &str,
    completed: &[String],
    decisions: &[String],
    todos: &[String],
    session_id: Option<&str>,
) -> Result<SessionEnd> {
    let mut saved = 0usize;

    // Summary → Progress (the session narrative). Empty summary is skipped so a
    // caller can pass "" when there is nothing to narrate.
    if !summary.trim().is_empty() {
        saved += add_one(store, project, summary, Category::Progress)?;
    }
    for d in decisions {
        if !d.trim().is_empty() {
            saved += add_one(store, project, d, Category::Decision)?;
        }
    }
    for c in completed {
        if !c.trim().is_empty() {
            saved += add_one(store, project, c, Category::Progress)?;
        }
    }
    for t in todos {
        if !t.trim().is_empty() {
            saved += add_one(store, project, t, Category::Todo)?;
        }
    }

    // Auto-resolve: each completed item closes the open bug/todo it is about.
    //
    // We search the Bug and Todo categories SEPARATELY (the trait's `search` takes
    // one category filter) rather than an unfiltered search — an unfiltered search
    // would surface the Progress memory we JUST saved for this same completed item
    // (verbatim text → nearest hit) and mask the real bug/todo. Category-scoped
    // search sidesteps that self-match entirely. We then resolve the single best
    // open task across the two arms that clears the relevance floor, and never
    // resolve the same id twice across completed items.
    let mut auto_resolved = 0usize;
    let mut already: std::collections::HashSet<String> = std::collections::HashSet::new();
    for c in completed {
        if c.trim().is_empty() {
            continue;
        }
        // include_resolved = false: never re-resolve an already-closed memory.
        let mut bugs = store.search(project, c, AUTO_RESOLVE_SEARCH_N, Some(Category::Bug), false)?;
        let todos_hits =
            store.search(project, c, AUTO_RESOLVE_SEARCH_N, Some(Category::Todo), false)?;
        bugs.extend(todos_hits);
        // Best (highest-scoring) open task this completed item is about.
        bugs.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if let Some(best) = bugs.into_iter().find(|h| !already.contains(&h.record.id.0)) {
            if best.score >= AUTO_RESOLVE_MIN_SCORE {
                let reason = format!("auto-resolved by completed work: {}", truncate(c, 160));
                store.resolve(&best.record.id, &reason)?;
                already.insert(best.record.id.0.clone());
                auto_resolved += 1;
            }
        }
    }

    if let Some(sid) = session_id {
        // Best-effort bookkeeping: a session touch must not sink the wrap-up.
        let _ = store.touch_session(project, sid);
    }

    Ok(SessionEnd {
        saved,
        auto_resolved,
    })
}

/// Add one memory, returning 1 if a NEW row was written, 0 if the store deduped
/// it (a `Duplicate` is the normal "already known" outcome, not an error).
fn add_one(store: &dyn MemoryStore, project: &str, content: &str, category: Category) -> Result<usize> {
    use icode_core::model::AddOutcome;
    let outcome = store.add(NewMemory {
        project: project.to_string(),
        content: content.to_string(),
        category,
        tags: Vec::new(),
        importance: 0.0,
        session_id: None,
    })?;
    Ok(match outcome {
        AddOutcome::Added { .. } => 1,
        AddOutcome::Duplicate { .. } => 0,
    })
}

/// Char-boundary-safe truncation for the resolve reason (avoids copying a huge
/// completed item into every resolved row).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}
