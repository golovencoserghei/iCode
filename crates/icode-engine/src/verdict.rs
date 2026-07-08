//! `check_exists` — the grounded existence oracle.
//!
//! Plain code search (`find_existing`, `semantic_search_code`) always returns the
//! K nearest symbols, so an agent can read "closest neighbour" as "the feature
//! exists" — the exact failure this guards against. A string literal `"calendar"`
//! sitting in a permissions list is NOT a calendar feature; the nearest vector to
//! "calendar" on a codebase with no calendar is still *some* symbol.
//!
//! This module commits to a VERDICT — [`Verdict::Exists`] / [`Verdict::Weak`] /
//! [`Verdict::Absent`] — so the answer can be quoted, not vibed. The design rule
//! that keeps it HONEST: **a confident EXISTS is only asserted from evidence iCode
//! can actually stand behind — a name match strong enough that it cannot be an
//! incidental word coincidence. Meaning-similarity is NOT such evidence** (it is
//! embedder-specific and, worse, self-confirming: when a query shares a verb with a
//! symbol name, both the lexical and the vector pass surface that same symbol, which
//! proves they agree on a word, not on intent).
//!
//! A defined symbol (function/class/route, excluding test files) is a confident
//! EXISTS only when:
//!   * the query is a SINGLE deliberately-typed identifier that names it (`rrf_fuse`), OR
//!   * ≥2 distinct salient query terms land on the symbol's name (a real signal, not one shared verb).
//!
//! Otherwise:
//!   * a single incidental term matching a name in a multi-word query → WEAK ("a symbol named X exists — verify it's what you mean"), never a false EXISTS;
//!   * the term only inside a body/string-literal/doc → WEAK (match_kind body_or_string: a mention, not a feature — the calendar-label case);
//!   * nothing → ABSENT, with the nearest-by-meaning symbol offered as a clearly-labelled LEAD to verify.
//!
//! The embedder is therefore only ever a *lead* (surfaced on WEAK/ABSENT), never a
//! verdict driver. Without it the tool still works fully; a genuine feature hiding
//! under totally different words may come back WEAK/ABSENT-with-a-lead rather than
//! EXISTS — the safe direction (hand the agent a candidate, never a false positive).
//!
//! Honest boundaries: `Absent` means "not in THIS static index" — dynamic,
//! reflective, external or generated code is invisible; name matching is un-stemmed
//! (`limiting` won't match `limiter`). Test files are excluded from the symbol
//! space (a test fixture must not satisfy "does this feature exist"), but by PATH
//! only — a symbol inside an inline `#[cfg(test)] mod` in a src file (Rust's common
//! unit-test convention) is NOT excluded and can still read as EXISTS; a proper fix
//! needs symbol-level `cfg(test)` metadata from the parser (a tracked follow-up).

use std::collections::HashSet;

use icode_core::error::Result;
use icode_core::model::{
    CodeHit, CodeQuery, Evidence, ExistenceVerdict, FunctionDef, MatchKind, SearchMode, SymbolKind,
    Verdict,
};
use icode_core::traits::{CodeReadStore, Embedder};

use crate::search::semantic_search;
use crate::store::SqliteCodeStore;

/// How many nearest vectors to pull for the lead.
const SEM_K: usize = 5;

/// Which symbol space `check_exists` should interrogate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExistScope {
    /// Functions, classes and routes (default).
    Any,
    Function,
    Class,
    Route,
}

/// Answer "does a symbol/feature matching `query` actually exist in the index?"
/// with a calibrated [`ExistenceVerdict`] instead of a raw nearest-neighbour list.
///
/// `embedder` is optional: `None` (or a failing embedder) only drops the lead — the
/// verdict itself is decided purely by the (model-agnostic) lexical signals.
pub fn check_exists(
    store: &SqliteCodeStore,
    embedder: Option<&dyn Embedder>,
    query: &str,
    scope: ExistScope,
) -> Result<ExistenceVerdict> {
    let tokens = salient_tokens(query);
    if tokens.is_empty() {
        // No checkable term — this is a QUERY problem, not evidence of absence.
        return Ok(ExistenceVerdict {
            verdict: Verdict::Absent,
            confidence: 0.0,
            reason: "query has no salient terms to check (too short/generic) — rephrase with a \
                     concrete noun/verb; this is NOT a judgement that the feature is absent"
                .into(),
            best_match: None,
            match_kind: None,
            exact_name_hit: false,
            evidence: vec![],
        });
    }

    // A single-word query is the caller deliberately naming ONE identifier (even a
    // multi-part one like `rrf_fuse`): match it whole and treat it as focused.
    let trimmed = query.trim();
    let single_word = !trimmed.is_empty() && !trimmed.chars().any(char::is_whitespace);
    let raw_word = single_word.then(|| trimmed.to_lowercase());
    let focused = single_word;

    let kind_filter = match scope {
        ExistScope::Function => Some(SymbolKind::Function),
        ExistScope::Class => Some(SymbolKind::Class),
        ExistScope::Any | ExistScope::Route => None,
    };

    // ── lexical candidates (full query + one pass per token), test files excluded ──
    // FTS5 is case-insensitive over name/qualified_name/docstring/body, so a class
    // `Calendar` surfaces for the token "calendar".
    let mut candidates: Vec<CodeHit> = Vec::new();
    if scope != ExistScope::Route {
        candidates.extend(lexical(store, query, kind_filter, 10));
        for tok in &tokens {
            candidates.extend(lexical(store, tok, kind_filter, 5));
        }
        candidates.retain(|h| !is_test_path(&h.path));
        dedup_hits(&mut candidates);
    }

    // Sort each candidate into: best exact-name match, best name-token match, or a
    // body-only mention. "best" = highest count of distinct query terms on the name.
    let mut best_exact: Option<(CodeHit, usize)> = None;
    let mut best_name: Option<(CodeHit, usize)> = None;
    let mut body_hits: Vec<CodeHit> = Vec::new();
    for hit in &candidates {
        let name_lc = hit.name.to_lowercase();
        let name_toks = split_identifier(&hit.name);
        let ov = tokens.iter().filter(|t| name_lc == **t || name_toks.contains(*t)).count();
        let is_exact = raw_word.as_deref() == Some(name_lc.as_str()) || tokens.contains(&name_lc);
        if is_exact {
            if best_exact.as_ref().is_none_or(|(_, o)| ov > *o) {
                best_exact = Some((hit.clone(), ov));
            }
        } else if ov >= 1 {
            if best_name.as_ref().is_none_or(|(_, o)| ov > *o) {
                best_name = Some((hit.clone(), ov));
            }
        } else {
            // Matched FTS only via body/docstring — a mention, not a name.
            body_hits.push(hit.clone());
        }
    }

    // ── route signal: a matching route whose handler resolves to a (non-test) fn ──
    let route_hit = if matches!(scope, ExistScope::Any | ExistScope::Route) {
        route_match(store, &tokens)?
    } else {
        None
    };

    // ── semantic signal: LEAD ONLY (nearest by meaning), test files excluded ──
    let mut semantic: Vec<CodeHit> = embedder
        .and_then(|e| semantic_search(store, e, query, SEM_K).ok())
        .unwrap_or_default();
    semantic.retain(|h| !is_test_path(&h.path));

    // A name match is EXISTS-worthy only when it can't be an incidental word
    // coincidence: the caller typed the identifier (focused), or ≥2 of their terms
    // hit the name AND that overlap carries signal (a distinctive term, or full name
    // coverage) rather than two throwaway generic words — see `exists_worthy`.
    let exact_strong =
        best_exact.as_ref().map(|(h, o)| exists_worthy(h, *o, &tokens, focused)).unwrap_or(false);
    let name_strong =
        best_name.as_ref().map(|(h, o)| exists_worthy(h, *o, &tokens, focused)).unwrap_or(false);
    let route_strong = route_hit.as_ref().map(|(_, o)| focused || *o >= 2).unwrap_or(false);

    // ─────────────────────────── classify (priority order) ───────────────────────────

    // 1. exact symbol name, EXISTS-worthy → EXISTS.
    if exact_strong {
        let (h, ov) = best_exact.unwrap();
        let reason = if focused {
            format!("you queried the identifier `{}`; a {} by that name exists at {}", h.name, kind_word(h.kind), loc(&h))
        } else {
            format!("{ov} of your query terms name the {} `{}` at {}", kind_word(h.kind), h.name, loc(&h))
        };
        let conf = if ov >= 2 { 0.92 } else { 0.85 };
        return Ok(make(Verdict::Exists, h, MatchKind::ExactSymbol, conf, reason, &semantic, true));
    }

    // 1b. exact route (handler-resolved), EXISTS-worthy → EXISTS.
    if route_strong {
        let (h, _) = route_hit.unwrap();
        let reason = format!("route match: {}", h.snippet.clone().unwrap_or_else(|| h.qualified_name.clone()));
        return Ok(make(Verdict::Exists, h, MatchKind::ExactSymbol, 0.80, reason, &semantic, false));
    }

    // 2. ≥2 (or focused) query terms in a defined name → EXISTS.
    if name_strong {
        let (h, ov) = best_name.unwrap();
        let reason = if focused {
            format!("a {} whose name contains your term exists: `{}` at {}", kind_word(h.kind), h.qualified_name, loc(&h))
        } else {
            format!("{ov} of your query terms appear in the {} name `{}` at {}", kind_word(h.kind), h.qualified_name, loc(&h))
        };
        let conf = if ov >= 2 { 0.82 } else { 0.65 };
        return Ok(make(Verdict::Exists, h, MatchKind::NameToken, conf, reason, &semantic, false));
    }

    // 3. a name/exact match on a single INCIDENTAL term of a multi-word query → WEAK.
    //    (finding #1's guard: no false confident EXISTS from one shared word.)
    if let Some((h, _)) = best_exact {
        let reason = format!(
            "a {} named `{}` exists at {}, but only one term of your multi-word query matches its \
             name — likely an incidental coincidence, not the feature you mean; verify",
            kind_word(h.kind),
            h.name,
            loc(&h)
        );
        return Ok(make(Verdict::Weak, h, MatchKind::ExactSymbol, 0.50, reason, &semantic, false));
    }
    if let Some((h, _)) = route_hit {
        let reason = format!(
            "an incidental route match ({}) — only one query term hit it; verify",
            h.snippet.clone().unwrap_or_else(|| h.qualified_name.clone())
        );
        return Ok(make(Verdict::Weak, h, MatchKind::ExactSymbol, 0.45, reason, &semantic, false));
    }
    if let Some((h, _)) = best_name {
        let reason = format!(
            "one term of your query appears in a {} name (`{}` at {}) — likely incidental; verify",
            kind_word(h.kind),
            h.qualified_name,
            loc(&h)
        );
        return Ok(make(Verdict::Weak, h, MatchKind::NameToken, 0.45, reason, &semantic, false));
    }

    // 4. the term appears ONLY inside bodies / string literals → WEAK.
    //    THIS is the calendar-label case: "calendar" in a permissions dict.
    if let Some(b) = body_or_string_evidence(store, &tokens, &body_hits) {
        let where_ = b.snippet.clone().unwrap_or_else(|| loc(&b));
        let reason = format!(
            "the term appears only inside code bodies / string literals ({where_}), with no \
             function/class/route of that name — a mention, not a feature"
        );
        let evidence = vec![Evidence { hit: b.clone(), match_kind: MatchKind::BodyOrString }];
        return Ok(ExistenceVerdict {
            verdict: Verdict::Weak,
            confidence: 0.30,
            reason,
            best_match: Some(b),
            match_kind: Some(MatchKind::BodyOrString),
            exact_name_hit: false,
            evidence,
        });
    }

    // 5. no name/exact/mention signal → ABSENT, with the nearest vector as a LEAD.
    match semantic.first() {
        Some(h) => {
            let reason = format!(
                "no function/class/route matches by name, and the term is in no body — ABSENT from \
                 this index. Nearest by meaning is `{}` at {} (a semantic lead only — verify \
                 manually; dynamic/external/generated code is invisible to a static index)",
                h.qualified_name,
                loc(h)
            );
            Ok(absent(reason, 0.50, Some(h.clone())))
        }
        None => Ok(absent(
            "nothing in the index matches this query — no name, mention, or semantic neighbour \
             (ABSENT from this static index; dynamic/external/generated code is invisible)"
                .into(),
            0.65,
            None,
        )),
    }
}

// ──────────────────────────────── verdict builders ────────────────────────────────

/// Build a verdict whose `best` leads the evidence, followed by up to two distinct
/// semantic neighbours for context.
fn make(
    verdict: Verdict,
    best: CodeHit,
    kind: MatchKind,
    confidence: f32,
    reason: String,
    semantic: &[CodeHit],
    exact_name_hit: bool,
) -> ExistenceVerdict {
    let evidence = assemble_evidence(&best, kind, semantic);
    ExistenceVerdict {
        verdict,
        confidence,
        reason,
        best_match: Some(best),
        match_kind: Some(kind),
        exact_name_hit,
        evidence,
    }
}

fn absent(reason: String, confidence: f32, nearest: Option<CodeHit>) -> ExistenceVerdict {
    let evidence = nearest
        .as_ref()
        .map(|h| vec![Evidence { hit: h.clone(), match_kind: MatchKind::Semantic }])
        .unwrap_or_default();
    ExistenceVerdict {
        verdict: Verdict::Absent,
        confidence,
        reason,
        best_match: nearest,
        match_kind: None,
        exact_name_hit: false,
        evidence,
    }
}

/// `best` first, then up to two distinct semantic neighbours (by qname+path).
fn assemble_evidence(best: &CodeHit, best_kind: MatchKind, semantic: &[CodeHit]) -> Vec<Evidence> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    seen.insert((best.qualified_name.clone(), best.path.clone()));
    let mut evidence = vec![Evidence { hit: best.clone(), match_kind: best_kind }];
    for h in semantic {
        if evidence.len() >= 3 {
            break;
        }
        if seen.insert((h.qualified_name.clone(), h.path.clone())) {
            evidence.push(Evidence { hit: h.clone(), match_kind: MatchKind::Semantic });
        }
    }
    evidence
}

// ──────────────────────────────── signal helpers ────────────────────────────────

/// A lexical (FTS/BM25) pass; errors degrade to no hits (the verdict still forms
/// from the other signals).
fn lexical(store: &SqliteCodeStore, text: &str, kind: Option<SymbolKind>, limit: usize) -> Vec<CodeHit> {
    store
        .search_code(&CodeQuery {
            text: text.to_string(),
            kind,
            lang: None,
            limit,
            mode: SearchMode::Lexical,
            with_body: false,
        })
        .unwrap_or_default()
}

/// A route whose URL/path/handler/name contains query term(s) AND whose handler
/// method resolves to a real, non-test function — returned as `(that fn's CodeHit,
/// #distinct query terms that hit the route)`, with the route summary as its
/// snippet. Unresolvable routes are weak evidence and are skipped.
fn route_match(store: &SqliteCodeStore, tokens: &[String]) -> Result<Option<(CodeHit, usize)>> {
    let routes = store.find_routes(None, None, None, 500)?;
    for r in &routes {
        let hay = format!(
            "{} {} {} {} {}",
            r.route,
            r.path,
            r.handler_method.as_deref().unwrap_or(""),
            r.handler_class.as_deref().unwrap_or(""),
            r.name.as_deref().unwrap_or("")
        )
        .to_lowercase();
        let ov = tokens.iter().filter(|t| hay.contains(t.as_str())).count();
        if ov == 0 {
            continue;
        }
        if let Some(hm) = &r.handler_method {
            if let Some(f) = store.get_function(hm, None, false)? {
                if is_test_path(&f.path) {
                    continue;
                }
                let mut hit = code_hit_from_fn(&f);
                hit.snippet = Some(format!("{} {} → {}", r.method, r.path, hit.qualified_name));
                return Ok(Some((hit, ov)));
            }
        }
    }
    Ok(None)
}

/// Evidence that a term lives ONLY in bodies/literals. Prefer an enclosing symbol
/// candidate whose body actually CONTAINS a matched line (grep-confirmed), enriched
/// with that literal line; otherwise synthesise a hit from the raw grep line. `None`
/// when the term is nowhere in any stored body.
fn body_or_string_evidence(
    store: &SqliteCodeStore,
    tokens: &[String],
    body_hits: &[CodeHit],
) -> Option<CodeHit> {
    let pattern = grep_pattern(tokens);
    let grep = store.grep_code(&pattern, None, 50).unwrap_or_default();
    let grep: Vec<_> = grep.into_iter().filter(|g| !is_test_path(&g.path)).collect();
    if grep.is_empty() {
        return None;
    }

    for bh in body_hits {
        if let Some(g) = grep
            .iter()
            .find(|g| g.path == bh.path && g.line >= bh.line_start && g.line <= bh.line_end)
        {
            let mut hit = bh.clone();
            hit.snippet = Some(format!("{}:{}: {}", g.path, g.line, g.text.trim()));
            return Some(hit);
        }
    }

    let g = &grep[0];
    Some(CodeHit {
        kind: SymbolKind::FileWindow,
        name: tokens.first().cloned().unwrap_or_default(),
        qualified_name: tokens.first().cloned().unwrap_or_default(),
        path: g.path.clone(),
        line_start: g.line,
        line_end: g.line,
        score: 0.0,
        snippet: Some(format!("{}:{}: {}", g.path, g.line, g.text.trim())),
        stale: false,
    })
}

// ──────────────────────────────── small utilities ────────────────────────────────

/// Heuristic: is this path test code? A feature-existence oracle must not let a
/// test fixture satisfy "does this feature exist". Covers common conventions across
/// languages (path segment `tests`/`test`/`__tests__`/`spec`; `test_*`, `*_test.*`,
/// `*.test.*`, `*.spec.*`). KNOWN GAP: inline `#[cfg(test)] mod` symbols in a src
/// file (Rust's dominant unit-test convention) have a src path and are NOT caught,
/// so a test helper there can still read as EXISTS; excluding them needs
/// symbol-level `cfg(test)` metadata from the parser (a tracked follow-up).
fn is_test_path(path: &str) -> bool {
    let p = path.replace('\\', "/").to_lowercase();
    if p.split('/').any(|s| {
        s == "tests" || s == "test" || s == "__tests__" || s == "spec" || s == "specs" || s == "testing"
    }) {
        return true;
    }
    let file = p.rsplit('/').next().unwrap_or(&p);
    file.starts_with("test_")
        || file.ends_with("_test.rs")
        || file.ends_with("_test.go")
        || file.ends_with("_test.py")
        || file.ends_with("_tests.rs")
        || file.contains(".test.")
        || file.contains(".spec.")
}

fn code_hit_from_fn(f: &FunctionDef) -> CodeHit {
    CodeHit {
        kind: SymbolKind::Function,
        name: f.name.clone(),
        qualified_name: f.qualified_name.clone(),
        path: f.path.clone(),
        line_start: f.line_start,
        line_end: f.line_end,
        score: 0.0,
        snippet: None,
        stale: false,
    }
}

fn loc(h: &CodeHit) -> String {
    format!("{}:{}", h.path, h.line_start)
}

fn kind_word(k: SymbolKind) -> &'static str {
    match k {
        SymbolKind::Function => "function",
        SymbolKind::Class => "class",
        SymbolKind::FileWindow => "file",
    }
}

/// Is a name match EXISTS-worthy (vs an incidental coincidence)? Yes if the caller
/// typed the identifier (focused). For a multi-word query, ≥2 distinct terms must
/// hit the name AND the overlap must carry signal: either a DISTINCTIVE (non-generic)
/// matched term, or the query covers ALL of the symbol's name tokens (it fully names
/// the thing). This stops two throwaway words (`get`+`file`) from passing EXISTS when
/// the symbol's load-bearing term (`outline`) went unmentioned.
fn exists_worthy(h: &CodeHit, overlap: usize, tokens: &[String], focused: bool) -> bool {
    if focused {
        return true;
    }
    if overlap < 2 {
        return false;
    }
    let name_lc = h.name.to_lowercase();
    let name_toks = split_identifier(&h.name);
    let matched: Vec<&str> = tokens
        .iter()
        .filter(|t| name_lc == **t || name_toks.contains(*t))
        .map(String::as_str)
        .collect();
    let has_distinctive = matched.iter().any(|t| !is_generic_token(t));
    let full_coverage =
        !name_toks.is_empty() && name_toks.iter().all(|nt| matched.contains(&nt.as_str()));
    has_distinctive || full_coverage
}

/// High-frequency generic code words that carry little intent alone. Used ONLY to
/// judge whether a ≥2-term name overlap is distinctive (see `exists_worthy`); never
/// to drop a token from search.
fn is_generic_token(t: &str) -> bool {
    const GENERIC: &[&str] = &[
        "get", "set", "add", "new", "list", "find", "fetch", "file", "files", "path", "paths",
        "data", "value", "values", "item", "items", "run", "make", "create", "update", "delete",
        "remove", "open", "close", "read", "write", "save", "load", "name", "names", "type",
        "types", "key", "keys", "index", "store", "count", "init", "handle", "process", "build",
        "parse", "check", "call", "exec", "use", "main", "util", "utils", "helper", "func",
        "method", "class", "object", "string", "number", "temp",
    ];
    GENERIC.contains(&t)
}

/// Case-insensitive regex alternation of the (already alphanumeric) query tokens;
/// each token is escaped belt-and-braces.
fn grep_pattern(tokens: &[String]) -> String {
    let alts: Vec<String> = tokens.iter().map(|t| regex::escape(t)).collect();
    format!("(?i)({})", alts.join("|"))
}

/// Deduplicate hits by `(qualified_name, path)`, keeping first (best) occurrence.
fn dedup_hits(hits: &mut Vec<CodeHit>) {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    hits.retain(|h| seen.insert((h.qualified_name.clone(), h.path.clone())));
}

/// Salient query terms: lowercased, length ≥ 3, de-duplicated in order, with a
/// small set of generic English stopwords removed. Domain verbs (`add`/`get`) stay
/// — a lone incidental match on one is gated by the ≥2-term / focused rule in the
/// classifier, not here.
fn salient_tokens(query: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "with", "that", "this", "from", "into", "over", "per", "via", "does",
        "has", "have", "are", "was", "were", "will", "can", "should", "would", "could", "its",
        "any", "all", "not", "but", "how", "such", "when", "where", "there", "already", "you",
        "your", "our", "their", "them", "they", "his", "her", "who", "why", "what",
    ];
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for raw in query.split(|c: char| !c.is_alphanumeric()) {
        if raw.len() < 3 {
            continue;
        }
        let tok = raw.to_lowercase();
        if STOP.contains(&tok.as_str()) {
            continue;
        }
        if seen.insert(tok.clone()) {
            out.push(tok);
        }
    }
    out
}

/// Split an identifier into lowercased sub-tokens on `::`, `.`, `_`, `-` and
/// camelCase boundaries (`calendarSync` → {calendar, sync}). Used to test whether a
/// query term is a *segment* of a symbol name.
fn split_identifier(name: &str) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in name.chars() {
        if ch == ':' || ch == '.' || ch == '_' || ch == '-' || ch.is_whitespace() {
            flush(&mut cur, &mut out);
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower {
            flush(&mut cur, &mut out);
        }
        cur.push(ch.to_ascii_lowercase());
        prev_lower = ch.is_lowercase();
    }
    flush(&mut cur, &mut out);
    out
}

fn flush(cur: &mut String, out: &mut HashSet<String>) {
    if cur.len() >= 2 {
        out.insert(std::mem::take(cur));
    } else {
        cur.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salient_tokens_drops_short_and_stopwords() {
        let t = salient_tokens("does the bot already have a calendar for reminders");
        assert!(t.contains(&"calendar".to_string()));
        assert!(t.contains(&"reminders".to_string()));
        assert!(!t.iter().any(|x| x == "the" || x == "for" || x == "already"));
    }

    #[test]
    fn split_identifier_handles_camel_snake_and_qualified() {
        let s = split_identifier("CalendarSync::add_event");
        assert!(s.contains("calendar"));
        assert!(s.contains("sync"));
        assert!(s.contains("event"));
    }

    #[test]
    fn is_test_path_flags_common_conventions() {
        assert!(is_test_path("crates/foo/tests/verdict.rs"));
        assert!(is_test_path("src/__tests__/x.ts"));
        assert!(is_test_path("pkg/user_test.go"));
        assert!(is_test_path("a/b/test_users.py"));
        assert!(is_test_path("web/app.spec.ts"));
        assert!(!is_test_path("crates/icode-engine/src/verdict.rs"));
        assert!(!is_test_path("src/latest/store.rs")); // "latest" is not "test"
    }
}
