//! Identifier-aware search text.
//!
//! SQLite's default FTS5 tokenizer (`unicode61`) splits on non-alphanumerics, so
//! `snake_case` falls apart into words for free — but a `camelCase` or `PascalCase`
//! identifier stays ONE opaque token. Measured on the default tokenizer:
//!
//! ```text
//! indexed `HttpRequestHandler` → MATCH 'Handler'  ⇒ NO HIT
//! indexed `embedPending`       → MATCH 'embed'    ⇒ NO HIT
//! ```
//!
//! That makes lexical code search blind for most of JS/TS/Go/Java/C# and every Rust
//! type name — the gap that a vector index was papering over at the cost of a GPU.
//!
//! The fix needs no custom tokenizer and no embedding model: alongside the raw text
//! we index a derived `search_text` in which every identifier is ALSO emitted as its
//! constituent words ([`split_identifiers`]). The original token is kept, so an exact
//! query still matches exactly and BM25 ranking survives intact (a trigram tokenizer
//! would find the substring too, but it destroys word-level ranking and inflates the
//! index — see the module test).

/// Split one identifier into its constituent words.
///
/// Handles the three conventions that actually occur, and their mixtures:
///   * `snake_case` / `SCREAMING_SNAKE` → split on `_`
///   * `camelCase` / `PascalCase`       → split on the lower→upper boundary
///   * acronym runs (`HTTPServer`, `parseJSONBody`) → the run stays whole and the
///     following capitalised word starts a new piece (`HTTP`, `Server`)
///
/// Digits attach to the word they follow (`utf8Decode` → `utf8`, `Decode`). Returns
/// an empty vec for an empty identifier; a single-word identifier returns that one
/// word (callers dedupe it against the original).
pub fn split_identifier(ident: &str) -> Vec<String> {
    let chars: Vec<char> = ident.chars().collect();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();

    for i in 0..chars.len() {
        let c = chars[i];
        if c == '_' || c == '-' || c == '.' {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if c.is_uppercase() && !cur.is_empty() {
            let prev = chars[i - 1];
            let next_is_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            // Boundary when coming out of a lowercase/digit run (`fooBar`), or when
            // an acronym run ends and a new word begins (`HTTPServer` → HTTP|Server).
            if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next_is_lower)
            {
                words.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// Build the derived `search_text` for a symbol: every identifier found in `parts`,
/// kept verbatim AND followed by its constituent words when it is a compound.
///
/// The result is fed to FTS5 as an extra indexed column, so `Handler` finds
/// `HttpRequestHandler` while an exact `HttpRequestHandler` query still matches the
/// whole token. Non-identifier characters (punctuation, operators) are dropped — the
/// raw body column is still indexed separately, so nothing is lost.
///
/// Words are deduped in-order to keep the column (and thus the FTS index) small: a
/// symbol that repeats an identifier 40 times contributes it once.
pub fn search_text(parts: &[&str]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for part in parts {
        for ident in identifiers(part) {
            let lower = ident.to_lowercase();
            if seen.insert(lower) {
                out.push(ident.clone());
            }
            let pieces = split_identifier(&ident);
            if pieces.len() > 1 {
                for w in pieces {
                    let lower = w.to_lowercase();
                    if seen.insert(lower) {
                        out.push(w);
                    }
                }
            }
        }
    }
    out.join(" ")
}

/// Does `line` contain `ident` as a WHOLE identifier?
///
/// This is the difference between a reference and a grep hit. `grep walk` matches
/// inside `walk_source_files`, `walker`, and `sidewalk`; a reference to `walk` does
/// not. A match requires the character on each side to be a non-identifier character
/// (or the string edge), which is exactly the boundary the language's lexer uses.
///
/// An empty `ident` never matches (it would otherwise match everywhere).
pub fn contains_identifier(line: &str, ident: &str) -> bool {
    if ident.is_empty() {
        return false;
    }
    let bytes = line.as_bytes();
    let needle = ident.as_bytes();
    let is_ident_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';

    let mut from = 0usize;
    while let Some(rel) = line[from..].find(ident) {
        let start = from + rel;
        let end = start + needle.len();
        let left_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let right_ok = end == bytes.len() || !is_ident_byte(bytes[end]);
        if left_ok && right_ok {
            return true;
        }
        // Advance past this occurrence; `find` works on byte offsets and `ident` is
        // ASCII-identifier-shaped, so start+1 is always a char boundary here.
        from = start + 1;
        if from >= line.len() {
            break;
        }
    }
    false
}

/// Every identifier-shaped run in `text` (`[A-Za-z_][A-Za-z0-9_]*`). Hand-rolled
/// rather than a regex: this runs over every symbol body on every index pass.
fn identifiers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() || c == '_' {
            // An identifier cannot START with a digit; a digit run alone is noise.
            if cur.is_empty() && c.is_ascii_digit() {
                continue;
            }
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str) -> Vec<String> {
        split_identifier(s)
    }

    #[test]
    fn splits_snake_camel_pascal_and_acronyms() {
        assert_eq!(split("embed_pending"), ["embed", "pending"]);
        assert_eq!(split("embedPending"), ["embed", "Pending"]);
        assert_eq!(split("HttpRequestHandler"), ["Http", "Request", "Handler"]);
        // An acronym run stays whole; the next capitalised word starts a new piece.
        assert_eq!(split("HTTPServer"), ["HTTP", "Server"]);
        assert_eq!(split("parseJSONBody"), ["parse", "JSON", "Body"]);
        assert_eq!(split("SCREAMING_SNAKE"), ["SCREAMING", "SNAKE"]);
        // Digits attach to the word they follow.
        assert_eq!(split("utf8Decode"), ["utf8", "Decode"]);
        // Single word → itself.
        assert_eq!(split("handler"), ["handler"]);
        assert_eq!(split(""), Vec::<String>::new());
    }

    #[test]
    fn search_text_keeps_original_and_adds_pieces() {
        let t = search_text(&["fn HttpRequestHandler(req)"]);
        // The whole identifier survives (exact queries still match)...
        assert!(t.contains("HttpRequestHandler"), "{t}");
        // ...and so do its parts (this is what unblocks `Handler`).
        for piece in ["Http", "Request", "Handler"] {
            assert!(t.split_whitespace().any(|w| w == piece), "missing {piece} in {t}");
        }
    }

    #[test]
    fn search_text_dedupes_so_the_index_stays_small() {
        // A body repeating one identifier must not blow the column up.
        let body = "handler handler handler handler";
        let t = search_text(&[body]);
        assert_eq!(t, "handler", "repeated identifier is emitted once, got {t:?}");
    }

    #[test]
    fn contains_identifier_respects_token_boundaries() {
        // The whole point: grep would match all three of these; a reference must not.
        assert!(!contains_identifier("let x = walk_source_files(root);", "walk"));
        assert!(!contains_identifier("struct Walker;", "Walk"));
        assert!(!contains_identifier("// sidewalk", "walk"));

        // Real references, in the shapes they actually occur.
        assert!(contains_identifier("walk(node, src);", "walk"));
        assert!(contains_identifier("let f = walk;", "walk"));
        assert!(contains_identifier("self.walk(x)", "walk"));
        assert!(contains_identifier("Vec<walk>", "walk"));
        assert!(contains_identifier("walk", "walk"));

        assert!(!contains_identifier("anything", ""));
    }

    #[test]
    fn search_text_drops_punctuation_and_bare_digits() {
        let t = search_text(&["let x = foo(1, 2);"]);
        let words: Vec<&str> = t.split_whitespace().collect();
        assert!(words.contains(&"foo"), "{t}");
        assert!(!words.iter().any(|w| w.chars().all(|c| c.is_ascii_digit())), "{t}");
    }
}
