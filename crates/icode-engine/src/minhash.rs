//! MinHash over token shingles — "find similar code" with no model and no GPU.
//!
//! `find_similar` was the last code tool that needed an embedding backend: it embedded
//! a symbol and ran a vector KNN. For CODE that is both expensive and a poor fit — a
//! dense vector captures *topic*, while what an agent actually asks for ("show me code
//! like this") is *structural overlap*: the same call sequence, the same operations,
//! the same shape with different names.
//!
//! Jaccard similarity over token k-shingles is the classic answer (the family behind
//! MOSS and every clone detector). MinHash makes it cheap: a fixed-length signature
//! whose fraction of matching positions is an unbiased estimate of the Jaccard index,
//! so comparing two symbols is a 64-word scan rather than a dot product over 1024
//! floats — and building it needs nothing but the source text.
//!
//! Cost: [`SIG_WORDS`] × 4 B = 512 B per symbol (a 1024-dim f32 vector is 4 KB, 8×
//! more) and a full project scan of ~1k symbols is microseconds, so no LSH banding is
//! needed at the scale a single repo has.

use std::collections::HashSet;

/// Hashes per view. 64 puts the standard error of the Jaccard estimate at
/// ~1/sqrt(64) ≈ 12% — plenty to rank neighbours.
pub const SIG_LEN: usize = 64;

/// A signature is TWO stacked views of the same code, 64 hashes each (512 B total):
///
///   * **lexical** — the raw token stream. Sensitive to names, so it separates two
///     structurally identical functions that do different things.
///   * **structural** — the same stream with every non-keyword identifier collapsed to
///     one placeholder. Immune to renaming, so it still recognises a copy-pasted clone
///     whose variables were all renamed.
///
/// Neither view is sufficient alone. Measured on a renamed clone vs unrelated code:
///
/// ```text
///                lexical  structural  combined
///   clone          0.41      1.00       0.70
///   unrelated      0.00      0.09       0.05
/// ```
///
/// Lexical alone scores the clone at 0.41 (renaming breaks every shingle that
/// contains the renamed token); structural alone would rank every getter as a perfect
/// match. Their mean separates the two cases by ~15×.
pub const SIG_WORDS: usize = SIG_LEN * 2;

/// Tokens that carry the SKELETON and must survive normalisation — collapse these and
/// `if x > 0` becomes indistinguishable from `while x > 0`. A deliberate union across
/// the languages the indexer parses (Rust, Python, Go, Java, JS/TS, PHP): a keyword of
/// one language appearing as a plain identifier in another only makes that one token
/// slightly more specific, which is harmless.
const KEYWORDS: &[&str] = &[
    "if", "else", "elif", "while", "for", "foreach", "loop", "do", "switch", "case",
    "default", "match", "break", "continue", "return", "yield", "goto", "try", "catch",
    "except", "finally", "throw", "throws", "raise", "panic", "defer", "go", "async",
    "await", "fn", "func", "function", "def", "lambda", "class", "struct", "enum",
    "trait", "interface", "impl", "extends", "implements", "let", "const", "var",
    "mut", "static", "final", "public", "private", "protected", "new", "delete",
    "import", "from", "use", "package", "module", "export", "self", "this", "super",
    "true", "false", "null", "nil", "none", "some", "ok", "err", "and", "or", "not",
    "in", "is", "as", "with", "pass", "void",
];

/// True when `tok` is an identifier-shaped token that is NOT a keyword — i.e. a name
/// the author chose, and therefore the thing a clone is free to rename.
fn is_renameable(tok: &str) -> bool {
    let mut cs = tok.chars();
    match cs.next() {
        Some(c) if c.is_alphabetic() || c == '_' => {}
        _ => return false,
    }
    !KEYWORDS.contains(&tok.to_lowercase().as_str())
}

/// The structural view: every renameable identifier becomes one placeholder, so the
/// stream keeps only keywords, operators, literals and shape.
fn normalize(toks: &[String]) -> Vec<String> {
    toks.iter()
        .map(|t| {
            if is_renameable(t) {
                "\u{2}".to_string()
            } else {
                t.clone()
            }
        })
        .collect()
}

/// Tokens per shingle. 3 is the usual choice for code: long enough that a shingle
/// encodes a small amount of *structure* (`if x ==`, `store . lock`) rather than a
/// bag of words, short enough to survive local edits.
const SHINGLE_K: usize = 3;

/// Split code into comparison tokens: identifiers/numbers as whole words, and each
/// operator/punctuation character as its own token.
///
/// Punctuation is KEPT on purpose — it is what carries the shape. Two functions that
/// share every identifier but not the control flow should not look identical, and two
/// clones with renamed variables still share their operator skeleton.
fn tokenize(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in code.chars() {
        if c.is_alphanumeric() || c == '_' {
            cur.push(c);
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            if !c.is_whitespace() {
                out.push(c.to_string());
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// FNV-1a — a fast, dependency-free 64-bit hash. Quality is ample here: the values
/// are only ever compared for minimum and equality.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Cheap 64-bit avalanche (splitmix64 finalizer). Used to derive `SIG_LEN`
/// independent hash functions from one shingle hash: `mix(shingle ^ seed_i)`.
fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    x
}

/// The distinct k-shingles of a token stream, as hashes. A stream shorter than one
/// shingle falls back to its individual tokens so it still gets a signature.
fn shingles(toks: &[String]) -> HashSet<u64> {
    let mut out = HashSet::new();
    if toks.is_empty() {
        return out;
    }
    if toks.len() < SHINGLE_K {
        for t in toks {
            out.insert(fnv1a(t.as_bytes()));
        }
        return out;
    }
    for w in toks.windows(SHINGLE_K) {
        out.insert(fnv1a(w.join("\u{1}").as_bytes()));
    }
    out
}

/// One 64-word MinHash over a shingle set.
fn minhash(sh: &HashSet<u64>) -> impl Iterator<Item = u32> + '_ {
    (0..SIG_LEN).map(move |i| {
        let seed = mix(i as u64 + 0x9e37_79b9_7f4a_7c15);
        sh.iter()
            .map(|&h| (mix(h ^ seed) >> 32) as u32)
            .min()
            .unwrap_or(u32::MAX)
    })
}

/// Signature of `code`: the lexical view followed by the structural view
/// ([`SIG_WORDS`] words, 512 B). Token-less input yields an all-`u32::MAX` signature,
/// which is similar to nothing — including another empty one (see [`similarity`]).
pub fn signature(code: &str) -> Vec<u32> {
    let toks = tokenize(code);
    let lex = shingles(&toks);
    let structural = shingles(&normalize(&toks));
    minhash(&lex).chain(minhash(&structural)).collect()
}

/// Fraction of agreeing positions in one view — the unbiased Jaccard estimate.
fn view_similarity(a: &[u32], b: &[u32]) -> f32 {
    if a.iter().all(|&x| x == u32::MAX) || b.iter().all(|&x| x == u32::MAX) {
        return 0.0;
    }
    let same = a.iter().zip(b).filter(|(x, y)| x == y).count();
    same as f32 / a.len() as f32
}

/// Combined similarity in `0.0..=1.0`: the mean of the lexical and structural Jaccard
/// estimates (see [`SIG_WORDS`] for why both are needed). Signatures of an unexpected
/// length — e.g. written by an older build before the structural view existed — score
/// 0 rather than comparing garbage.
///
/// Two EMPTY symbols score 0, not 1: "neither has any tokens" is not evidence that
/// they are alike, and scoring it 1 would float every stub to the top of the results.
pub fn similarity(a: &[u32], b: &[u32]) -> f32 {
    if a.len() != SIG_WORDS || b.len() != SIG_WORDS {
        return 0.0;
    }
    let lex = view_similarity(&a[..SIG_LEN], &b[..SIG_LEN]);
    let structural = view_similarity(&a[SIG_LEN..], &b[SIG_LEN..]);
    0.5 * lex + 0.5 * structural
}

/// Encode a signature as a little-endian byte blob for SQLite.
pub fn to_blob(sig: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(sig.len() * 4);
    for v in sig {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Decode a signature blob written by [`to_blob`]. A trailing partial word is ignored.
pub fn from_blob(blob: &[u8]) -> Vec<u32> {
    blob.chunks_exact(4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL: &str = r#"
        fn load_user(db: &Db, id: u64) -> Option<User> {
            let row = db.query("SELECT * FROM users WHERE id = ?", id)?;
            let user = User::from_row(row)?;
            cache.insert(id, user.clone());
            Some(user)
        }
    "#;

    /// Same code, every local name changed — a textbook renamed clone.
    const RENAMED_CLONE: &str = r#"
        fn load_account(store: &Db, key: u64) -> Option<User> {
            let record = store.query("SELECT * FROM users WHERE id = ?", key)?;
            let account = User::from_row(record)?;
            cache.insert(key, account.clone());
            Some(account)
        }
    "#;

    /// Completely unrelated logic.
    const UNRELATED: &str = r#"
        fn render_chart(points: &[f64], width: usize) -> String {
            let mut svg = String::new();
            for (i, p) in points.iter().enumerate() {
                svg.push_str(&format!("<circle cx='{}' cy='{}'/>", i * width, p));
            }
            svg
        }
    "#;

    fn sim(a: &str, b: &str) -> f32 {
        similarity(&signature(a), &signature(b))
    }

    #[test]
    fn identical_code_is_maximally_similar() {
        assert_eq!(sim(ORIGINAL, ORIGINAL), 1.0);
    }

    #[test]
    fn a_renamed_clone_scores_far_above_unrelated_code() {
        let clone = sim(ORIGINAL, RENAMED_CLONE);
        let other = sim(ORIGINAL, UNRELATED);
        // The whole point: structure survives renaming, so the clone is still found.
        // Measured ~0.70 vs ~0.05 — a lexical-only signature scored the clone at just
        // 0.41, which is why the structural view exists.
        assert!(clone > 0.6, "renamed clone must stay similar, got {clone}");
        assert!(other < 0.15, "unrelated code must not look similar, got {other}");
        assert!(
            clone > other * 4.0,
            "clone ({clone}) must rank far above unrelated ({other})"
        );
    }

    #[test]
    fn the_structural_view_is_what_survives_renaming() {
        let a = signature(ORIGINAL);
        let b = signature(RENAMED_CLONE);
        let lex = view_similarity(&a[..SIG_LEN], &b[..SIG_LEN]);
        let structural = view_similarity(&a[SIG_LEN..], &b[SIG_LEN..]);
        // Renaming shreds the lexical shingles but leaves the skeleton intact.
        assert!(structural > lex, "structural {structural} should beat lexical {lex}");
        assert!(structural > 0.9, "a pure rename keeps the skeleton, got {structural}");
    }

    #[test]
    fn control_flow_keywords_are_not_normalised_away() {
        // `if` vs `while` must remain distinguishable in the STRUCTURAL view — if
        // keywords were collapsed like other identifiers, these would be identical.
        let a = signature("fn f(x) { if cond(x) { g(x); } }");
        let b = signature("fn f(x) { while cond(x) { g(x); } }");
        let structural = view_similarity(&a[SIG_LEN..], &b[SIG_LEN..]);
        assert!(structural < 1.0, "if/while must differ structurally, got {structural}");
    }

    #[test]
    fn similarity_is_symmetric_and_bounded() {
        let a = sim(ORIGINAL, UNRELATED);
        let b = sim(UNRELATED, ORIGINAL);
        assert!((a - b).abs() < f32::EPSILON, "{a} vs {b}");
        assert!((0.0..=1.0).contains(&a));
    }

    #[test]
    fn empty_symbols_are_not_similar_to_anything() {
        // Two empty stubs must NOT score 1.0 — that would float every stub to the top.
        assert_eq!(sim("", ""), 0.0);
        assert_eq!(sim("", ORIGINAL), 0.0);
    }

    #[test]
    fn blob_round_trips() {
        let sig = signature(ORIGINAL);
        assert_eq!(sig.len(), SIG_WORDS);
        assert_eq!(from_blob(&to_blob(&sig)), sig);
        // 512 B per symbol vs 4 KB for a 1024-dim f32 vector — 8x smaller, and it
        // needs no model to produce.
        assert_eq!(to_blob(&sig).len(), SIG_WORDS * 4);
    }

    #[test]
    fn structure_matters_not_just_the_word_bag() {
        // Same identifiers, different control flow → must not look identical.
        let a = "fn f(x: u32) { if x > 0 { g(x); } }";
        let b = "fn f(x: u32) { while x > 0 { g(x); } }";
        assert!(sim(a, b) < 1.0, "different control flow must not be identical");
    }
}
