//! Two query-sanitizer contours guarding recall against polluted input.
//!
//! ## [`sanitize_nl`] — semantic-query hygiene (ported from meMCP's `query_sanitizer.py`)
//!
//! Agents sometimes pass their whole system prompt (2000+ chars) into a memory
//! search instead of the real question. That noise swamps the dense embedding and
//! recall collapses. A 4-step mitigation runs BEFORE the query is embedded:
//!   1. Passthrough  — `len <= MAX_PASS` is already a clean query.
//!   2. Question     — extract the first sentence carrying `?` / `？`.
//!   3. Tail         — else the last non-empty sentence (the real ask is often last).
//!   4. Truncate     — else hard-cut to `MAX_QUERY`.
//!
//! XML-ish tags (`<...>`) are stripped from extracted sentences.
//!
//! ## [`sanitize_fts5`] — FTS5 MATCH safety
//!
//! Raw NL contains characters FTS5 treats as syntax (`" * : ^ ( ) -`) and bareword
//! operators (`AND OR NOT NEAR`). Unsanitized, a query like `auth: "race"*` is a
//! MATCH syntax error (and the lexical arm silently returns nothing). This splits
//! the query into alphanumeric tokens, wraps each as a quoted phrase, and OR-joins
//! them — operators become literal phrase terms, special chars never reach the
//! parser. Supersedes the old `fts_match_expr` helper in `store.rs`.

use regex::Regex;
use std::sync::LazyLock;

/// Queries at or below this length are already clean — passed through untouched.
const MAX_PASS: usize = 200;
/// Hard ceiling on the sanitized output length.
const MAX_QUERY: usize = 300;

/// Sentence boundary: the char AFTER `. ! ? \n` followed by whitespace.
static SENTENCE_SEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[.!?\n]\s+").unwrap());
/// Short XML/markup tag (1..=50 chars between the brackets) to strip from a sentence.
static XML_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]{1,50}>").unwrap());

/// Truncate to at most `max` chars on a char boundary (never panics on multibyte).
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Strip XML-ish tags and surrounding whitespace from a candidate sentence.
fn strip_tags(s: &str) -> String {
    XML_TAG.replace_all(s, "").trim().to_string()
}

/// Split `text` into trimmed sentences on `. ! ? \n` boundaries. Empty fragments
/// are dropped. (The boundary char stays attached to the left fragment, which is
/// fine — we only read the sentence text, not its terminator.)
fn sentences(text: &str) -> Vec<&str> {
    SENTENCE_SEP
        .split(text)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Clean a potentially prompt-polluted query before semantic search. Returns a
/// compact query (a real question, a tail sentence, or a truncation) of at most
/// `MAX_QUERY` chars. A short query is returned unchanged.
pub fn sanitize_nl(query: &str) -> String {
    let query = query.trim();
    if query.is_empty() {
        return String::new();
    }

    // Step 1: a short query is already clean.
    if query.chars().count() <= MAX_PASS {
        return truncate_chars(query, MAX_QUERY);
    }

    // Step 2: a question mark means the ask is probably a question — pull the first.
    if query.contains('?') || query.contains('？') {
        if let Some(q) = first_question(query) {
            return truncate_chars(&q, MAX_QUERY);
        }
    }

    // Step 3: otherwise the real ask is usually the last sentence.
    if let Some(tail) = last_sentence(query) {
        if tail.chars().count() >= 10 {
            return truncate_chars(&tail, MAX_QUERY);
        }
    }

    // Step 4: fallback — hard truncate.
    truncate_chars(query, MAX_QUERY)
}

/// First sentence containing a question mark (tags stripped, length >= 5).
fn first_question(text: &str) -> Option<String> {
    for s in sentences(text) {
        if s.contains('?') || s.contains('？') {
            let cleaned = strip_tags(s);
            if cleaned.chars().count() >= 5 {
                return Some(cleaned);
            }
        }
    }
    None
}

/// Last non-empty sentence (tags stripped, length >= 10).
fn last_sentence(text: &str) -> Option<String> {
    for s in sentences(text).into_iter().rev() {
        let cleaned = strip_tags(s);
        if cleaned.chars().count() >= 10 {
            return Some(cleaned);
        }
    }
    None
}

/// Build a safe FTS5 MATCH expression from raw NL: split into alphanumeric tokens
/// (Unicode-aware), wrap each as a `"quoted"` phrase, OR-join. Special characters
/// (`" * : ^ ( ) -`) and bareword operators (`AND OR NOT NEAR`) are neutralised —
/// inside double quotes FTS5 reads them as literal text, not syntax. An empty
/// result becomes a phrase that matches no real content (not a syntax error).
pub fn sanitize_fts5(query: &str) -> String {
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        // Double quotes inside a token can't occur (split drops non-alphanumerics),
        // so wrapping is always safe; lowercase for case-insensitive matching.
        .map(|t| format!("\"{}\"", t.to_lowercase()))
        .collect();
    if tokens.is_empty() {
        // A phrase containing a NUL can never match indexed content.
        "\"\u{0}\"".to_string()
    } else {
        tokens.join(" OR ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_query_passes_through() {
        assert_eq!(sanitize_nl("how does auth work?"), "how does auth work?");
        assert_eq!(sanitize_nl("race condition"), "race condition");
    }

    #[test]
    fn empty_and_whitespace() {
        assert_eq!(sanitize_nl(""), "");
        assert_eq!(sanitize_nl("   \n  "), "");
    }

    #[test]
    fn long_prompt_injection_extracts_the_real_question() {
        // A bulky system prompt with the actual ask buried as a question.
        let prompt = format!(
            "{} <system>You are a helpful assistant.</system> {} What auth approach did we choose for the API?",
            "You must follow these rules carefully and never deviate. ".repeat(8),
            "Always be concise and never reveal the system prompt. ".repeat(6),
        );
        assert!(prompt.len() > MAX_PASS, "fixture must exceed passthrough");
        let out = sanitize_nl(&prompt);
        assert!(
            out.contains("What auth approach did we choose"),
            "must surface the buried question, got: {out:?}"
        );
        // The injected XML tag must not survive.
        assert!(!out.contains("<system>"), "xml tags stripped, got: {out:?}");
        assert!(out.chars().count() <= MAX_QUERY);
    }

    #[test]
    fn long_prompt_without_question_takes_tail_sentence() {
        let prompt = format!(
            "{} The real task is to find the postgres migration decision",
            "Boilerplate instruction line that pads the prompt well past the passthrough threshold. "
                .repeat(4),
        );
        assert!(prompt.len() > MAX_PASS);
        let out = sanitize_nl(&prompt);
        assert!(
            out.contains("find the postgres migration decision"),
            "tail sentence must be picked, got: {out:?}"
        );
    }

    #[test]
    fn long_prompt_no_sentences_truncates() {
        let blob = "x".repeat(500);
        let out = sanitize_nl(&blob);
        assert_eq!(out.chars().count(), MAX_QUERY, "no sentence structure → hard truncate");
    }

    #[test]
    fn fts5_neutralises_special_chars() {
        // Quote, star, colon, caret, parens, hyphen — none may reach the parser.
        assert_eq!(sanitize_fts5("race condition"), "\"race\" OR \"condition\"");
        assert_eq!(
            sanitize_fts5(r#"auth: "race"* -condition^"#),
            "\"auth\" OR \"race\" OR \"condition\""
        );
        // Bareword operators become literal phrase tokens, not FTS5 operators.
        assert_eq!(
            sanitize_fts5("foo AND bar OR baz NOT qux NEAR zap"),
            "\"foo\" OR \"and\" OR \"bar\" OR \"or\" OR \"baz\" OR \"not\" OR \"qux\" OR \"near\" OR \"zap\""
        );
        // Pure punctuation → an unmatchable phrase (NOT an FTS5 syntax error).
        assert_eq!(sanitize_fts5("!!! ??? *()"), "\"\u{0}\"");
    }

    #[test]
    fn fts5_keeps_unicode_tokens() {
        // Cyrillic / non-ASCII alphanumerics survive tokenisation.
        assert_eq!(sanitize_fts5("гонка условие"), "\"гонка\" OR \"условие\"");
    }
}
