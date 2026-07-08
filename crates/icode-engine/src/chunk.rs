//! Chunking: code-graph symbols → embeddable [`CodeChunk`]s.
//!
//! The chunk+embed phase closes the semantic pipeline: symbols → chunks →
//! embeddings → vec0. This module owns the first arrow. For each parsed function
//! and class it emits a *symbol chunk* whose text carries a small parent-context
//! header (path + qualified name + signature/bases + docstring) ahead of the
//! body — this lets the embedder situate the body without a separate retrieval
//! hop. `content_hash` (sha256 of the chunk text) gates incremental re-embedding.
//!
//! Oversized symbols (a body that pushes the chunk past [`CHUNK_BUDGET_BYTES`])
//! are split into overlapping sub-chunks at line boundaries, each repeating the
//! same parent header plus a `part i/N` marker. UTF-8 is never cut mid-codepoint
//! (splits land on `\n` boundaries, which are always codepoint boundaries).
//!
//! File-window chunks (for symbol-less files: config/sql/docs) are deliberately
//! NOT produced here yet.
// TODO(chunk): emit FileWindow chunks for symbol-less files (line-window over raw
// source) so config/sql/docs become semantically searchable.

use std::path::Path;

use icode_core::model::{ClassDef, CodeChunk, FunctionDef, SymbolKind};
use sha2::{Digest, Sha256};

/// Soft byte budget for one chunk's text. qwen3-embedding holds 32k tokens; at a
/// conservative ~4 bytes/token this ≈ 6k tokens, so the overflow split fires only
/// on genuinely huge symbols. It bounds the per-chunk payload either way.
pub const CHUNK_BUDGET_BYTES: usize = 24_000;

/// Bytes of trailing context repeated at the head of the next sub-chunk when a
/// body is split, so a symbol straddling the boundary is not lost to either side.
const CHUNK_OVERLAP_BYTES: usize = 200;

/// Render `path` relative to `root` for the embeddable chunk HEADER, so the chunk
/// text — and thus its `content_hash` (the embed-cache key) — is INDEPENDENT of
/// where the repo is checked out. This is what lets the shared embed cache hit
/// across git worktrees and re-clones: identical code at the same repo-relative
/// path produces byte-identical chunk text everywhere. Falls back to the original
/// path when it is not under `root` (defensive; parity then degrades to per-path).
pub fn rel_display_path(path: &str, root: &Path) -> String {
    Path::new(path)
        .strip_prefix(root)
        .map(|rel| rel.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// Build the embeddable chunks for one file's symbols: one chunk per function and
/// per class (split into overlapping sub-chunks when oversized). The header path is
/// rendered RELATIVE to `root` (see [`rel_display_path`]) so a chunk's hash is
/// independent of where the repo is checked out.
///
/// `symbol_id` is left `None` — chunks are linked back to their owning symbol by
/// `(qualified_name, path)` in a later pass, not at chunk-build time.
pub fn chunks_for_file(
    functions: &[FunctionDef],
    classes: &[ClassDef],
    root: &Path,
) -> Vec<CodeChunk> {
    let mut out = Vec::with_capacity(functions.len() + classes.len());

    for f in functions {
        let header = function_header(&rel_display_path(&f.path, root), f);
        push_symbol_chunks(
            &mut out,
            SymbolKind::Function,
            &f.qualified_name,
            &f.path,
            f.line_start,
            f.line_end,
            &header,
            &f.body,
        );
    }

    for c in classes {
        let header = class_header(&rel_display_path(&c.path, root), c);
        push_symbol_chunks(
            &mut out,
            SymbolKind::Class,
            &c.qualified_name,
            &c.path,
            c.line_start,
            c.line_end,
            &header,
            &c.body,
        );
    }

    out
}

/// Parent-context header for a function chunk: file path, qualified name with the
/// argument list, and the docstring (when present), each on its own comment line.
/// `display_path` is the repo-relative path (see [`rel_display_path`]) so the header
/// — and the chunk hash — is checkout-location-independent.
fn function_header(display_path: &str, f: &FunctionDef) -> String {
    let mut h = format!("// {}\n// {}({})\n", display_path, f.qualified_name, f.args);
    if let Some(doc) = doc_line(f.docstring.as_deref()) {
        h.push_str(&doc);
    }
    h
}

/// Parent-context header for a class chunk: file path, qualified name with its
/// base list (`: Base1, Base2` when non-empty), and the docstring (when present).
/// `display_path` is repo-relative (see [`rel_display_path`]).
fn class_header(display_path: &str, c: &ClassDef) -> String {
    let bases = if c.bases.is_empty() {
        String::new()
    } else {
        format!(": {}", c.bases.join(", "))
    };
    let mut h = format!("// {}\n// {}{}\n", display_path, c.qualified_name, bases);
    if let Some(doc) = doc_line(c.docstring.as_deref()) {
        h.push_str(&doc);
    }
    h
}

/// Render a docstring as a trailing header line (`// <doc>\n`), or `None` when
/// the symbol has no docstring. Internal newlines are flattened to keep the
/// header a compact prefix above the body.
fn doc_line(doc: Option<&str>) -> Option<String> {
    let doc = doc?.trim();
    if doc.is_empty() {
        return None;
    }
    let flat = doc.replace('\n', " ");
    Some(format!("// {flat}\n"))
}

/// Emit one chunk for a symbol, or several `part i/N` sub-chunks when the assembled
/// text exceeds [`CHUNK_BUDGET_BYTES`]. All sub-chunks share the same kind,
/// qualified name, path, and symbol line range.
#[allow(clippy::too_many_arguments)]
fn push_symbol_chunks(
    out: &mut Vec<CodeChunk>,
    kind: SymbolKind,
    qualified_name: &str,
    path: &str,
    line_start: u32,
    line_end: u32,
    header: &str,
    body: &str,
) {
    let whole = format!("{header}{body}");
    if whole.len() <= CHUNK_BUDGET_BYTES {
        out.push(make_chunk(
            kind,
            qualified_name,
            path,
            line_start,
            line_end,
            whole,
        ));
        return;
    }

    // Oversized: split the BODY into budgeted, overlapping pieces at line
    // boundaries; each piece repeats the header plus a `part i/N` marker.
    let pieces = split_body(body, header.len());
    let total = pieces.len();
    for (i, piece) in pieces.into_iter().enumerate() {
        let text = format!("{header}// part {}/{}\n{piece}", i + 1, total);
        out.push(make_chunk(
            kind,
            qualified_name,
            path,
            line_start,
            line_end,
            text,
        ));
    }
}

/// Split `body` into sub-chunks that, once the `header_len`-byte header and the
/// `part i/N` marker are prepended, stay within [`CHUNK_BUDGET_BYTES`]. Splits land
/// on line boundaries (never mid-UTF-8); consecutive pieces overlap by up to
/// [`CHUNK_OVERLAP_BYTES`] of trailing lines so a symbol on the seam survives.
fn split_body(body: &str, header_len: usize) -> Vec<String> {
    // Budget left for the body once the header + a generous marker line are
    // accounted for. Saturating so a pathologically long header can't underflow;
    // a small floor keeps progress guaranteed even then.
    const MARKER_RESERVE: usize = 32; // "// part NN/NN\n" and slack.
    let body_budget = CHUNK_BUDGET_BYTES
        .saturating_sub(header_len + MARKER_RESERVE)
        .max(1024);

    // Collect lines WITH their trailing newline so reassembly is loss-less.
    let lines: Vec<&str> = split_keep_newlines(body);

    let mut pieces: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut overlap_carry = String::new();

    for line in &lines {
        // A single line longer than the budget is hard-split on byte boundaries
        // that are also char boundaries (so we never cut a codepoint).
        if line.len() > body_budget {
            if !cur.is_empty() {
                pieces.push(std::mem::take(&mut cur));
            }
            for seg in hard_split_line(line, body_budget) {
                pieces.push(seg);
            }
            overlap_carry.clear();
            continue;
        }

        if cur.len() + line.len() > body_budget && !cur.is_empty() {
            pieces.push(std::mem::take(&mut cur));
            // Seed the next piece with the overlap tail (whole trailing lines).
            cur.push_str(&overlap_carry);
            overlap_carry.clear();
        }

        cur.push_str(line);
        // Maintain a rolling overlap tail of recent whole lines (≤ overlap bytes).
        overlap_carry.push_str(line);
        if overlap_carry.len() > CHUNK_OVERLAP_BYTES {
            overlap_carry = tail_lines_within(&overlap_carry, CHUNK_OVERLAP_BYTES);
        }
    }
    if !cur.is_empty() {
        pieces.push(cur);
    }
    if pieces.is_empty() {
        pieces.push(String::new());
    }
    pieces
}

/// Split text into lines, each KEEPING its trailing `\n` (the final line keeps
/// whatever it had). Reassembling the pieces reproduces the input exactly.
fn split_keep_newlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            out.push(&s[start..=i]);
            start = i + 1;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Hard-split one over-long line into `<= budget` byte pieces on char boundaries.
fn hard_split_line(line: &str, budget: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = line.len();
    while start < bytes {
        let mut end = (start + budget).min(bytes);
        // Walk back to the nearest char boundary so we never cut a codepoint.
        while end > start && !line.is_char_boundary(end) {
            end -= 1;
        }
        // Degenerate guard: if budget is smaller than the next codepoint, take it
        // whole (boundary search returned `start`), advancing past it.
        if end == start {
            end = (start + 1..=bytes)
                .find(|&e| line.is_char_boundary(e))
                .unwrap_or(bytes);
        }
        out.push(line[start..end].to_string());
        start = end;
    }
    out
}

/// Return the trailing whole lines of `s` whose total size is ≤ `max_bytes`.
/// Used to keep the rolling overlap tail aligned to line boundaries.
fn tail_lines_within(s: &str, max_bytes: usize) -> String {
    let lines = split_keep_newlines(s);
    let mut acc: Vec<&str> = Vec::new();
    let mut total = 0;
    for line in lines.iter().rev() {
        if total + line.len() > max_bytes && !acc.is_empty() {
            break;
        }
        total += line.len();
        acc.push(line);
    }
    acc.reverse();
    acc.concat()
}

/// Assemble a [`CodeChunk`], stamping its `content_hash` as the hex sha256 of the
/// chunk text (the incremental-re-embed gate).
fn make_chunk(
    kind: SymbolKind,
    qualified_name: &str,
    path: &str,
    line_start: u32,
    line_end: u32,
    chunk_text: String,
) -> CodeChunk {
    let content_hash = sha256_hex(&chunk_text);
    CodeChunk {
        symbol_kind: kind,
        symbol_id: None,
        qualified_name: Some(qualified_name.to_string()),
        path: path.to_string(),
        line_start,
        line_end,
        chunk_text,
        content_hash,
    }
}

/// Hex sha256 of `text` (mirrors `parse::rust`'s hash/hex helpers).
fn sha256_hex(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    let digest = h.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}
