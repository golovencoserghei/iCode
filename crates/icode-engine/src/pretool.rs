//! Graph-first hints for an agent that is about to grep for a symbol.
//!
//! Claude Code fires a `PreToolUse` hook before every tool call. When the call is a
//! search for one identifier (`Grep` with pattern `fn foo`, or a plain `rg -n foo`),
//! the code graph already knows the answer the agent is after — where `foo` is
//! defined and who calls it. `icode hook pre-tool` hands that answer over as
//! `additionalContext`, so the agent can skip reading the files the grep would have
//! pointed at.
//!
//! The grep is NEVER blocked (Graphify's hook forbids it; we do not): the hint is a
//! shortcut, not a gate, and a wrong refusal costs far more than a redundant search.
//!
//! Everything here is deliberately conservative — **when in doubt, stay silent.**
//! A hint for the wrong symbol, or a hint on every call, would teach the agent to
//! ignore hints. So: only a single identifier is recognised (a phrase, a regex with
//! alternation or a path yields nothing), only a simple command (no pipe, no
//! substitution), at most N hints per session and never the same symbol twice.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use icode_core::model::{Call, CodeHit, SymbolKind};
use icode_core::traits::CodeReadStore;

use crate::store::SqliteCodeStore;

/// Default number of hints per session (`ICODE_HOOK_HINT_CAP` overrides).
pub const DEFAULT_HINT_CAP: usize = 8;
/// Hard ceiling on the rendered hint, in characters.
pub const HINT_MAX_CHARS: usize = 600;
/// A name defined in more places than this is too ambiguous to hint.
const MAX_DEFINITIONS: usize = 4;
/// Call edges read to count callers; beyond this the count is shown as `N+`.
const CALLER_SCAN: usize = 2000;

/// The fields we use out of a `PreToolUse` payload. Unknown fields are ignored.
#[derive(Debug, Default, Deserialize)]
pub struct PreToolInput {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: serde_json::Value,
}

pub fn parse_pre_tool_input(json: &str) -> Option<PreToolInput> {
    serde_json::from_str(json).ok()
}

/// What the agent is searching for, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchCall {
    pub pattern: String,
    /// Search paths as written (may be relative to the shell cwd). Empty = cwd.
    pub paths: Vec<String>,
    /// `-F` / `--fixed-strings`: the pattern is a literal, not a regex.
    pub fixed: bool,
    /// Directory a preceding `cd DIR &&` moved the shell to (relative to the
    /// payload cwd, or absolute). `None` = the payload cwd.
    pub dir: Option<String>,
}

impl PreToolInput {
    /// The searches this tool call performs, in order — empty when it is not a
    /// grep, or none of its greps is understood well enough to answer.
    pub fn search_calls(&self) -> Vec<SearchCall> {
        match self.tool_name.as_str() {
            "Grep" => {
                let Some(pattern) = self.tool_input.get("pattern").and_then(|p| p.as_str()) else {
                    return Vec::new();
                };
                let paths = self
                    .tool_input
                    .get("path")
                    .and_then(|p| p.as_str())
                    .filter(|p| !p.trim().is_empty())
                    .map(|p| vec![p.to_string()])
                    .unwrap_or_default();
                vec![SearchCall { pattern: pattern.to_string(), paths, fixed: false, dir: None }]
            }
            "Bash" => self
                .tool_input
                .get("command")
                .and_then(|c| c.as_str())
                .map(search_calls_in_command)
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }
}

// ───────────────────────────── shell command ─────────────────────────────

/// Prefix of a word that carried an unquoted glob (its expansion is unknown).
pub const GLOB_MARK: char = '\u{1}';

/// One `&&`/`||`/`;`/newline-separated command: its pipeline stages as words.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Segment {
    pub stages: Vec<Vec<String>>,
    /// Something in it makes its meaning depend on the shell (a substitution,
    /// subshell, glob, real redirection, background `&`): never answered.
    pub poisoned: bool,
}

/// Split a command line into segments → pipeline stages → words, honouring
/// quotes, backslashes and `#` comments.
///
/// Conservative by construction. A segment is POISONED when its text cannot be
/// known without running the shell: ANY `$` or backtick (even inside double
/// quotes), `(`/`)`, an unquoted glob outside an option word, a background `&`,
/// or a redirection other than the noise agents append (`2>/dev/null`, `2>&1`,
/// `>/dev/null`, `&>/dev/null`). A here-doc (`<<`) or an unterminated quote ENDS
/// the parse: the lines after it are data, not commands, so they are dropped.
pub fn shell_segments(cmd: &str) -> Vec<Segment> {
    let mut segs: Vec<Segment> = Vec::new();
    let mut seg = Segment::default();
    let mut stage: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut raw = String::new(); // the current word as typed
    let mut in_word = false;
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;

    fn end_word(stage: &mut Vec<String>, cur: &mut String, raw: &mut String, in_word: &mut bool) {
        if *in_word {
            let mut w = std::mem::take(cur);
            if raw.starts_with(GLOB_MARK) {
                w.insert(0, GLOB_MARK);
            }
            stage.push(w);
        }
        cur.clear();
        raw.clear();
        *in_word = false;
    }
    fn end_stage(seg: &mut Segment, stage: &mut Vec<String>) {
        if !stage.is_empty() {
            seg.stages.push(std::mem::take(stage));
        }
    }
    fn end_segment(segs: &mut Vec<Segment>, seg: &mut Segment) {
        if !seg.stages.is_empty() {
            segs.push(std::mem::take(seg));
        } else {
            *seg = Segment::default();
        }
    }

    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match c {
            ' ' | '\t' => end_word(&mut stage, &mut cur, &mut raw, &mut in_word),
            '#' if !in_word => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '\n' | ';' => {
                end_word(&mut stage, &mut cur, &mut raw, &mut in_word);
                end_stage(&mut seg, &mut stage);
                end_segment(&mut segs, &mut seg);
            }
            '|' | '&' if chars.get(i) == Some(&c) => {
                i += 1;
                end_word(&mut stage, &mut cur, &mut raw, &mut in_word);
                end_stage(&mut seg, &mut stage);
                end_segment(&mut segs, &mut seg);
            }
            '|' => {
                end_word(&mut stage, &mut cur, &mut raw, &mut in_word);
                end_stage(&mut seg, &mut stage);
            }
            '>' | '<' | '&' => {
                if c == '<' && chars.get(i) == Some(&'<') {
                    // Here-doc / here-string: what follows is data. Stop here.
                    seg.poisoned = true;
                    end_word(&mut stage, &mut cur, &mut raw, &mut in_word);
                    end_stage(&mut seg, &mut stage);
                    end_segment(&mut segs, &mut seg);
                    return segs;
                }
                if c == '&' && chars.get(i) != Some(&'>') {
                    seg.poisoned = true; // background job
                    continue;
                }
                // A redirection: `[fd]>target`, `&>target`, `<file`. Read its word.
                let fd_only = cur.chars().all(|d| d.is_ascii_digit());
                let mut op = String::from(c);
                while i < chars.len() && !matches!(chars[i], ' ' | '\t' | '\n' | ';' | '|') {
                    op.push(chars[i]);
                    i += 1;
                }
                // `> /dev/null` with a space: take the next word as the target.
                if matches!(op.as_str(), ">" | "&>" | ">>") {
                    while i < chars.len() && matches!(chars[i], ' ' | '\t') {
                        i += 1;
                    }
                    while i < chars.len() && !matches!(chars[i], ' ' | '\t' | '\n' | ';' | '|') {
                        op.push(chars[i]);
                        i += 1;
                    }
                }
                let harmless = fd_only
                    && matches!(op.as_str(), ">/dev/null" | ">&1" | ">&2" | "&>/dev/null" | ">>/dev/null");
                if !harmless {
                    seg.poisoned = true;
                }
                cur.clear();
                raw.clear();
                in_word = false;
            }
            '$' | '`' | '(' | ')' => {
                seg.poisoned = true;
                in_word = true;
                cur.push(c);
                raw.push(c);
            }
            '*' | '?' | '[' | '{' if !(in_word && raw.starts_with('-')) => {
                // An unquoted glob is expanded by the shell — except inside an
                // option word (`--include=*.rs`), where it only ever matches itself.
                // The word is MARKED, not the segment poisoned: a glob in a path
                // argument (`grep -n foo tests/*.mjs`) still leaves the pattern
                // known; a marked pattern or `cd` target is refused downstream.
                if !raw.starts_with(GLOB_MARK) {
                    raw.insert(0, GLOB_MARK);
                }
                in_word = true;
                cur.push(c);
                raw.push(c);
            }
            '~' if !in_word => {
                seg.poisoned = true;
                in_word = true;
                cur.push(c);
                raw.push(c);
            }
            '\'' => {
                in_word = true;
                raw.push(c);
                loop {
                    let Some(&n) = chars.get(i) else {
                        seg.poisoned = true;
                        end_segment(&mut segs, &mut seg);
                        return segs;
                    };
                    i += 1;
                    if n == '\'' {
                        break;
                    }
                    cur.push(n);
                }
            }
            '"' => {
                in_word = true;
                raw.push(c);
                loop {
                    let Some(&n) = chars.get(i) else {
                        seg.poisoned = true;
                        end_segment(&mut segs, &mut seg);
                        return segs;
                    };
                    i += 1;
                    match n {
                        '"' => break,
                        '$' | '`' => {
                            seg.poisoned = true;
                            cur.push(n);
                        }
                        '\\' => {
                            let Some(&e) = chars.get(i) else { break };
                            i += 1;
                            // Inside double quotes a backslash only escapes these.
                            if !matches!(e, '"' | '\\' | '$' | '`' | '\n') {
                                cur.push('\\');
                            }
                            if e != '\n' {
                                cur.push(e);
                            }
                        }
                        _ => cur.push(n),
                    }
                }
            }
            '\\' => {
                let Some(&e) = chars.get(i) else { break };
                i += 1;
                if e == '\n' {
                    continue; // line continuation
                }
                in_word = true;
                raw.push(c);
                raw.push(e);
                cur.push(e);
            }
            _ => {
                in_word = true;
                raw.push(c);
                cur.push(c);
            }
        }
    }
    end_word(&mut stage, &mut cur, &mut raw, &mut in_word);
    end_stage(&mut seg, &mut stage);
    end_segment(&mut segs, &mut seg);
    segs
}

/// Every symbol search in a Bash command, in order.
///
/// A segment counts when its FIRST pipeline stage is `rg`/`grep`/`egrep`/`fgrep`/
/// `git grep` (later stages — `| head`, `| sort` — only shape the output, so they
/// do not change what is searched; a grep that is itself a later stage filters
/// another command's output and is NOT a code search). A plain `cd DIR` segment
/// moves the directory the following greps run in; a `cd` we cannot follow
/// (`cd -`, `cd ~`, a substitution) ends the scan, since every later relative path
/// would be a guess.
pub fn search_calls_in_command(cmd: &str) -> Vec<SearchCall> {
    let mut out = Vec::new();
    let mut dir: Option<String> = None;
    for seg in shell_segments(cmd) {
        let first = &seg.stages[0];
        if first.first().map(String::as_str) == Some("cd") {
            match (seg.poisoned, seg.stages.len(), first.as_slice()) {
                (false, 1, [_, d]) if !d.starts_with('-') && !d.starts_with(GLOB_MARK) => {
                    dir = Some(match &dir {
                        Some(prev) if !d.starts_with('/') => format!("{prev}/{d}"),
                        _ => d.clone(),
                    });
                }
                _ => break,
            }
            continue;
        }
        if seg.poisoned {
            continue;
        }
        if let Some(mut call) = parse_grep_words(first) {
            call.dir = dir.clone();
            out.push(call);
        }
    }
    out
}

/// The single search in `cmd`, when it holds exactly one (tests, tooling).
pub fn parse_grep_command(cmd: &str) -> Option<SearchCall> {
    let mut calls = search_calls_in_command(cmd);
    (calls.len() == 1).then(|| calls.remove(0))
}

/// Recognise the words of ONE `rg …` / `grep …` / `git grep …` with ONE pattern
/// and pull out pattern + paths.
///
/// A pattern file (`-f`), several `-e` patterns or an env prefix are not
/// understood and return `None`.
fn parse_grep_words(words: &[String]) -> Option<SearchCall> {
    let words: &[String] = if words.len() > 1 && words[0] == "git" && words[1] == "grep" {
        &words[1..]
    } else {
        words
    };
    let (prog, args) = words.split_first()?;
    let base = prog.rsplit('/').next().unwrap_or(prog);
    let is_rg = match base {
        "rg" => true,
        "grep" | "egrep" | "fgrep" => false,
        _ => return None,
    };
    // Short options that take a value (attached `-A3` or as the next word).
    let short_val: &[char] = if is_rg {
        &['e', 'f', 'g', 't', 'T', 'A', 'B', 'C', 'm', 'M', 'j', 'r', 'd', 'E']
    } else {
        &['e', 'f', 'A', 'B', 'C', 'm', 'd', 'D']
    };
    // Long options that take a value when written without `=`.
    let long_val: &[&str] = if is_rg {
        &[
            "regexp", "file", "glob", "iglob", "type", "type-not", "type-add", "type-clear", "after-context",
            "before-context", "context", "max-count", "max-columns", "threads", "replace", "max-depth",
            "encoding", "pre", "pre-glob", "sort", "sortr", "color", "colors", "max-filesize", "path-separator",
            "context-separator", "field-match-separator", "field-context-separator", "ignore-file", "engine",
            "dfa-size-limit", "regex-size-limit", "hostname-bin", "hyperlink-format", "generate",
        ]
    } else {
        &[
            "regexp", "file", "after-context", "before-context", "context", "max-count", "directories", "devices",
            "include", "exclude", "exclude-dir", "exclude-from", "label", "binary-files", "color", "colour",
            "group-separator",
        ]
    };

    let mut patterns: Vec<String> = Vec::new();
    let mut positionals: Vec<String> = Vec::new();
    let mut fixed = base == "fgrep";
    let mut i = 0;
    let mut opts_done = false;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if opts_done || !a.starts_with('-') || a == "-" {
            positionals.push(a.clone());
            continue;
        }
        if a == "--" {
            opts_done = true;
            continue;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            if name == "fixed-strings" {
                fixed = true;
            }
            // `--files` / `--type-list` do not search at all.
            if matches!(name, "files" | "type-list") {
                return None;
            }
            if long_val.contains(&name) {
                let value = match value {
                    Some(v) => v,
                    None => {
                        let v = args.get(i)?.clone();
                        i += 1;
                        v
                    }
                };
                match name {
                    "regexp" => patterns.push(value),
                    "file" => return None,
                    _ => {}
                }
            }
            continue;
        }
        // Short cluster: `-rn`, `-A3`, `-nA 3`, `-e foo`.
        let cluster: Vec<char> = a[1..].chars().collect();
        let mut k = 0;
        while k < cluster.len() {
            let c = cluster[k];
            k += 1;
            if c == 'F' {
                fixed = true;
            }
            if short_val.contains(&c) {
                let attached: String = cluster[k..].iter().collect();
                let value = if attached.is_empty() {
                    let v = args.get(i)?.clone();
                    i += 1;
                    v
                } else {
                    attached
                };
                match c {
                    'e' => patterns.push(value),
                    'f' => return None,
                    _ => {}
                }
                break;
            }
        }
    }
    let (pattern, paths) = if patterns.is_empty() {
        let (p, rest) = positionals.split_first()?;
        (p.clone(), rest.to_vec())
    } else if patterns.len() == 1 {
        (patterns.remove(0), positionals)
    } else {
        return None;
    };
    if pattern.starts_with(GLOB_MARK) {
        return None;
    }
    let paths = paths.into_iter().map(|p| p.trim_start_matches(GLOB_MARK).to_string()).collect();
    Some(SearchCall { pattern, paths, fixed, dir: None })
}

// ───────────────────────────── pattern → symbol ─────────────────────────────

/// Keywords that, written before a name, say "the definition of".
const DEF_KEYWORDS: &[&str] = &[
    "pub(crate) fn", "pub async fn", "pub fn", "async fn", "fn", "async def", "def", "export default function",
    "export async function", "export function", "async function", "function", "export default class",
    "export class", "export interface", "export type", "export const", "public static function",
    "public function", "protected function", "private function", "static function", "class", "func", "struct",
    "enum", "trait", "interface", "type", "impl", "module", "mod", "const",
];

/// Names that are language keywords or so generic that a definition of them is
/// almost never what a grep for the word is after (`export`, `default`, `data`).
/// A hint there is noise the agent learns to skip, so these stay silent.
const GENERIC_WORDS: &[&str] = &[
    "abstract", "action", "async", "await", "break", "case", "catch", "class", "const", "continue", "data",
    "default", "delete", "else", "enum", "error", "export", "extends", "false", "final", "finally", "from",
    "function", "global", "import", "interface", "items", "key", "lambda", "message", "messages", "name", "none",
    "null", "package", "pass", "private", "protected", "public", "raise", "result", "return", "self", "static",
    "string", "struct", "super", "switch", "this", "throw", "true", "type", "undefined", "value", "var", "void",
    "where", "while", "with", "yield",
];

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A qualified name `A::b` / `A.b` (both halves identifiers).
fn is_qualified(s: &str) -> bool {
    if let Some((a, b)) = s.rsplit_once("::") {
        return (is_identifier(a) || is_qualified(a)) && is_identifier(b);
    }
    false
}

/// Strip ONE leading token from `s` if present.
fn strip_any<'a>(s: &'a str, prefixes: &[&str]) -> &'a str {
    for p in prefixes {
        if let Some(rest) = s.strip_prefix(p) {
            return rest;
        }
    }
    s
}

/// The identifier a search pattern is looking for, or `None` when the pattern is
/// not a search for ONE symbol.
///
/// Recognised (regex mode): `name`, `\bname\b`, `\<name\>`, `^name`, `fn name`,
/// `def\s+name`, `class Name\b`, `function name`, `name(`, `name\(`, `fn name\(`,
/// `A::b`. With `fixed` the pattern is literal, so only `name`, `name(` and
/// `fn name` forms apply. Rejected: phrases, alternations, character classes,
/// paths, anything shorter than 3 characters, and pure numbers.
pub fn symbol_from_pattern(pattern: &str, fixed: bool) -> Option<String> {
    let mut s = pattern.trim();
    if s.is_empty() || s.len() > 120 {
        return None;
    }
    if !fixed {
        s = s.strip_prefix('^').unwrap_or(s);
        s = strip_any(s, &["\\b", "\\<", "\\s*", "\\s+", " *"]);
        s = s.strip_suffix('$').unwrap_or(s);
    }
    // Trailing call paren / word boundary.
    let tail: &[&str] = if fixed {
        &[" {", "{", "()", "("]
    } else {
        &[" {", " \\{", "\\{", "\\b", "\\>", "\\(\\)", "\\(", "\\s*\\(", " *\\(", "\\s*", "()", "("]
    };
    loop {
        let before = s;
        for t in tail {
            if let Some(rest) = s.strip_suffix(t) {
                s = rest;
            }
        }
        if s == before {
            break;
        }
    }
    // Definition keyword followed by separator.
    for kw in DEF_KEYWORDS {
        if let Some(rest) = s.strip_prefix(kw) {
            let seps: &[&str] = if fixed { &[" "] } else { &["\\s+", "\\s*", " +", " ", "\\s"] };
            let after = strip_any(rest, seps);
            if after.len() < rest.len() {
                s = if fixed { after } else { strip_any(after, &["\\b", "\\<"]) };
                break;
            }
        }
    }
    if !fixed {
        s = s.strip_suffix("\\b").unwrap_or(s);
    }
    let s = s.trim();
    if s.len() < 3 || GENERIC_WORDS.contains(&s.to_ascii_lowercase().as_str()) {
        return None;
    }
    if is_identifier(s) || is_qualified(s) {
        return Some(s.to_string());
    }
    None
}

/// Every symbol a pattern names: one for a plain pattern, several for an
/// alternation of plain names (`foo\\|bar` in grep, `foo|bar` in rg / `-E`) —
/// agents batch lookups that way. ALL alternatives must be recognised: one
/// regex-looking branch makes the whole pattern a regex search, and it stays
/// silent. At most six branches.
pub fn symbols_from_pattern(pattern: &str, fixed: bool) -> Vec<String> {
    if fixed {
        return symbol_from_pattern(pattern, true).into_iter().collect();
    }
    let branches: Vec<&str> = pattern.split("\\|").flat_map(|b| b.split('|')).collect();
    if branches.len() > 6 {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for b in branches {
        match symbol_from_pattern(b, false) {
            Some(sym) if !out.contains(&sym) => out.push(sym),
            Some(_) => {}
            None => return Vec::new(),
        }
    }
    out
}

// ───────────────────────────── the answer ─────────────────────────────

/// What the graph knows about one symbol.
#[derive(Debug, Clone)]
pub struct SymbolBrief {
    pub name: String,
    pub definitions: Vec<CodeHit>,
    pub definitions_total: usize,
    pub calls_total: usize,
    /// `calls_total` hit the scan ceiling (there may be more).
    pub calls_capped: bool,
    pub top_callers: Vec<Call>,
}

/// Look `name` up. `None` when the index has no definition of it — the grep is for
/// something the graph cannot answer, and the hint must stay silent.
pub fn symbol_brief(store: &SqliteCodeStore, name: &str) -> Option<SymbolBrief> {
    let (definitions, definitions_total) = store.find_definitions(name, 3).ok()?;
    // No definition: nothing to answer. Too many: the name is generic (`POST`,
    // `call`, `init`), and "defined in 14 places" does not save the agent a read.
    if definitions.is_empty() || definitions_total > MAX_DEFINITIONS {
        return None;
    }
    let mut callers = store.get_callers(name, CALLER_SCAN).unwrap_or_default();
    let calls_total = callers.len();
    callers.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    // Two calls on one line are one place to look.
    let mut sites = std::collections::HashSet::new();
    callers.retain(|c| sites.insert((c.path.clone(), c.line)));
    callers.truncate(3);
    Some(SymbolBrief {
        name: name.to_string(),
        definitions,
        definitions_total,
        calls_total,
        calls_capped: calls_total >= CALLER_SCAN,
        top_callers: callers,
    })
}

/// Confidence label of a call edge (see `Call::confidence`).
pub fn confidence_label(c: f32) -> &'static str {
    if c >= 0.85 {
        "exact"
    } else if c >= 0.55 {
        "inferred"
    } else {
        "ambiguous"
    }
}

/// `path` relative to `root`. The index keeps paths as they were spelled when it
/// was built, which may go through a symlink the hook's root does not; the
/// canonical form is tried before falling back to the stored path.
fn rel(path: &str, root: &Path) -> String {
    let p = Path::new(path);
    if let Ok(r) = p.strip_prefix(root) {
        return r.to_string_lossy().to_string();
    }
    if p.is_absolute() {
        if let (Ok(cp), Ok(cr)) = (std::fs::canonicalize(p), std::fs::canonicalize(root)) {
            if let Ok(r) = cp.strip_prefix(&cr) {
                return r.to_string_lossy().to_string();
            }
        }
    }
    path.to_string()
}

/// Render the hint for one or more symbols (≤ [`HINT_MAX_CHARS`] in total).
pub fn render_hint(briefs: &[SymbolBrief], root: &Path) -> String {
    let many = briefs.len() > 1;
    let mut out = String::from("iCode code graph already answers this search (index may lag unsaved edits):\n");
    for b in briefs {
        let max_defs = if many { 2 } else { 3 };
        let defs: Vec<String> = b
            .definitions
            .iter()
            .take(max_defs)
            .map(|d| {
                let kind = match d.kind {
                    SymbolKind::Class => "type",
                    _ => "fn",
                };
                format!("{}:{} ({kind} {})", rel(&d.path, root), d.line_start, d.qualified_name)
            })
            .collect();
        let shown = defs.len();
        out.push_str(&format!("`{}` defined: {}", b.name, defs.join("; ")));
        if b.definitions_total > shown {
            out.push_str(&format!(" (+{} more)", b.definitions_total - shown));
        }
        out.push('\n');
        if b.calls_total == 0 {
            out.push_str("  call sites: none in the graph\n");
        } else {
            let plus = if b.calls_capped { "+" } else { "" };
            let tops: Vec<String> = b
                .top_callers
                .iter()
                .take(match briefs.len() {
                    1 => 3,
                    2 => 2,
                    _ => 1,
                })
                .map(|c| format!("{}:{} in {} [{}]", rel(&c.path, root), c.line, c.caller, confidence_label(c.confidence)))
                .collect();
            out.push_str(&format!("  call sites: {}{plus}; top: {}\n", b.calls_total, tops.join("; ")));
        }
    }
    let names: Vec<&str> = briefs.iter().map(|b| b.name.as_str()).collect();
    out.push_str(&format!("full list: mcp__icode__symbol op=callers name={}", names.join("|")));
    if out.chars().count() > HINT_MAX_CHARS {
        out = out.chars().take(HINT_MAX_CHARS - 1).collect();
        out.push('…');
    }
    out
}

// ───────────────────────────── per-session ledger ─────────────────────────────

/// Session ledger: which symbols were already hinted, so the cap and the
/// no-repeat rule hold across the separate hook processes of one session.
///
/// One small file per session under `dir` (`<session>.hints`, one symbol per
/// line). Files untouched for two days are swept when a new session starts.
pub struct HintLedger {
    file: PathBuf,
    seen: Vec<String>,
}

fn safe_session(id: &str) -> Option<String> {
    let s: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(80)
        .collect();
    (!s.is_empty()).then_some(s)
}

impl HintLedger {
    /// `None` without a usable session id: no ledger → no way to cap → no hints.
    pub fn open(dir: &Path, session_id: &str) -> Option<Self> {
        let sid = safe_session(session_id)?;
        let file = dir.join(format!("{sid}.hints"));
        let seen = match std::fs::read_to_string(&file) {
            Ok(s) => s.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect(),
            Err(_) => Vec::new(),
        };
        Some(Self { file, seen })
    }

    /// Hints given so far (one line per hint).
    pub fn count(&self) -> usize {
        self.seen.len()
    }

    /// Was `symbol` already part of a hint?
    pub fn contains(&self, symbol: &str) -> bool {
        self.seen.iter().any(|l| l.split(' ').any(|s| s == symbol))
    }

    /// May a hint for `symbol` be given under `cap`?
    pub fn allows(&self, symbol: &str, cap: usize) -> bool {
        self.count() < cap && !self.contains(symbol)
    }

    /// Record one hint covering `symbols`. Errors are reported so the caller can
    /// stay silent rather than risk exceeding the cap on a read-only state dir.
    pub fn record(&mut self, symbols: &[&str]) -> std::io::Result<()> {
        let symbol = symbols.join(" ");
        use std::io::Write;
        let dir = self.file.parent().unwrap_or(Path::new("."));
        let fresh = !self.file.exists();
        std::fs::create_dir_all(dir)?;
        if fresh {
            sweep_old(dir, std::time::Duration::from_secs(2 * 24 * 3600));
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.file)?;
        writeln!(f, "{symbol}")?;
        self.seen.push(symbol);
        Ok(())
    }
}

/// Remove `*.hints` files older than `age`. Best-effort.
pub fn sweep_old(dir: &Path, age: std::time::Duration) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let now = std::time::SystemTime::now();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("hints") {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|d| d > age);
        if old {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// Hint cap from `ICODE_HOOK_HINT_CAP` (0 disables), else [`DEFAULT_HINT_CAP`].
pub fn hint_cap() -> usize {
    std::env::var("ICODE_HOOK_HINT_CAP")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_HINT_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(p: &str) -> Option<String> {
        symbol_from_pattern(p, false)
    }

    #[test]
    fn patterns_that_name_one_symbol_are_caught() {
        for (p, want) in [
            ("hook_output_json", "hook_output_json"),
            ("\\bhook_project\\b", "hook_project"),
            ("\\<parse_grep\\>", "parse_grep"),
            ("^run_index", "run_index"),
            ("fn run_hook_post_tool", "run_hook_post_tool"),
            ("pub fn run_hook_post_tool", "run_hook_post_tool"),
            ("fn\\s+get_callers", "get_callers"),
            ("def\\s+handle_request\\b", "handle_request"),
            ("class UserService", "UserService"),
            ("function renderApp", "renderApp"),
            ("struct SqliteCodeStore", "SqliteCodeStore"),
            ("export function command", "command"),
            ("orphanSleep()", "orphanSleep"),
            ("  async function init() {", "init"),
            ("export async function runHeld", "runHeld"),
            ("public function handle(", "handle"),
            ("get_callers(", "get_callers"),
            ("get_callers\\(", "get_callers"),
            ("fn get_callers\\(", "get_callers"),
            ("SqliteCodeStore::open", "SqliteCodeStore::open"),
        ] {
            assert_eq!(sym(p).as_deref(), Some(want), "pattern {p:?}");
        }
    }

    #[test]
    fn patterns_that_are_not_one_symbol_stay_silent() {
        for p in [
            "",
            "fn",
            "ab",
            "foo|bar",
            "foo.*bar",
            "TODO: fix this",
            "src/main.rs",
            "[A-Z]+_KEY",
            "hook (pre|post)",
            "123",
            "\"quoted\"",
            "foo bar",
            "\\w+Handler",
            "import React",
            "export",
            "default",
        ] {
            assert_eq!(sym(p), None, "pattern {p:?}");
        }
    }

    #[test]
    fn fixed_strings_are_literal() {
        assert_eq!(symbol_from_pattern("get_callers(", true).as_deref(), Some("get_callers"));
        assert_eq!(symbol_from_pattern("fn get_callers", true).as_deref(), Some("get_callers"));
        // A literal backslash-b is not a word boundary under -F.
        assert_eq!(symbol_from_pattern("\\bfoo_bar\\b", true), None);
    }

    #[test]
    fn simple_grep_commands_are_parsed() {
        let c = parse_grep_command("rg -n 'fn get_callers' crates/").unwrap();
        assert_eq!(c.pattern, "fn get_callers");
        assert_eq!(c.paths, vec!["crates/"]);
        let c = parse_grep_command("grep -rn --include=*.rs hook_project src").unwrap();
        assert_eq!(c.pattern, "hook_project");
        let c = parse_grep_command("rg -t rust -A 3 -e symbol_brief").unwrap();
        assert_eq!(c.pattern, "symbol_brief");
        assert!(c.paths.is_empty());
        let c = parse_grep_command("rg -nA3 render_hint . 2>/dev/null").unwrap();
        assert_eq!(c.pattern, "render_hint");
        assert_eq!(c.paths, vec!["."]);
        let c = parse_grep_command("rg -F 'run(' -g '*.rs'").unwrap();
        assert!(c.fixed);
        assert_eq!(c.pattern, "run(");
        let c = parse_grep_command("/usr/bin/grep -E \"\\bfoo_bar\\b\" -- -weird").unwrap();
        assert_eq!(c.pattern, "\\bfoo_bar\\b");
        assert_eq!(c.paths, vec!["-weird"]);
    }

    #[test]
    fn unknown_or_unknowable_commands_stay_silent() {
        for cmd in [
            "rg $NAME",
            "rg \"$(cat f)\"",
            "rg `x`",
            "rg foo > out.txt",
            "rg foo &",
            "FOO=1 rg foo",
            "rg -f patterns.txt",
            "rg -e a -e b",
            "rg --files",
            "ls -la",
            "rg foo* src",
            "cd sub* && rg foo_bar",
            "rg 'unterminated",
            "rg",
            "cat f | grep foo_bar",
            "cd ~ && rg foo_bar",
            "cd - && rg foo_bar",
            "python3 - <<'EOF'\ngrep foo_bar x\nEOF",
        ] {
            assert_eq!(parse_grep_command(cmd), None, "command {cmd:?}");
        }
    }

    #[test]
    fn compound_commands_yield_each_search() {
        let c = parse_grep_command("rg -n foo_bar src | head -20").unwrap();
        assert_eq!(c.pattern, "foo_bar");
        let c = parse_grep_command("cd /repo/sub && rg -n foo_bar src 2>&1 | sort").unwrap();
        assert_eq!(c.dir.as_deref(), Some("/repo/sub"));
        let c = parse_grep_command("grep -n \"orphanSleep()\" tests/*.test.mjs").unwrap();
        assert_eq!(c.paths, vec!["tests/*.test.mjs"]);
        let c = parse_grep_command("git grep -n foo_bar").unwrap();
        assert_eq!(c.pattern, "foo_bar");
        let calls = search_calls_in_command(
            "# look around\nsed -n 1,20p a.rs; grep -n \"fn alpha_one\" a.rs && echo --- && rg beta_two lib/ | wc -l",
        );
        let pats: Vec<&str> = calls.iter().map(|c| c.pattern.as_str()).collect();
        assert_eq!(pats, vec!["fn alpha_one", "beta_two"]);
        // A poisoned segment is skipped, the rest still counts.
        let calls = search_calls_in_command("echo $HOME; grep -rn gamma_three .");
        assert_eq!(calls.len(), 1);
        // An unfollowable `cd` ends the scan.
        assert!(search_calls_in_command("cd $(mktemp -d) && grep -rn gamma_three .").is_empty());
        // Line continuation and `> /dev/null`.
        let c = parse_grep_command("rg -n \\\n  delta_four src 2> /dev/null").unwrap();
        assert_eq!(c.pattern, "delta_four");
    }

    #[test]
    fn alternations_of_names_are_split() {
        assert_eq!(symbols_from_pattern("providerCommand\\|stripNested\\|execFile(", false), vec![
            "providerCommand",
            "stripNested",
            "execFile"
        ]);
        assert_eq!(symbols_from_pattern("fn alpha|fn beta", false), vec!["alpha", "beta"]);
        assert!(symbols_from_pattern("key\\b\\|key)\\|key,", false).is_empty());
        assert!(symbols_from_pattern("a1|b2|c3|d4|e5|f6|g7", false).is_empty());
        assert_eq!(symbols_from_pattern("foo_bar", true), vec!["foo_bar"]);
    }

    #[test]
    fn tool_input_routing() {
        let g = parse_pre_tool_input(r#"{"session_id":"s","tool_name":"Grep","tool_input":{"pattern":"fn foo_bar","path":"/r/src"}}"#)
            .unwrap();
        let c = g.search_calls().remove(0);
        assert_eq!(c.pattern, "fn foo_bar");
        assert_eq!(c.paths, vec!["/r/src"]);
        let b = parse_pre_tool_input(r#"{"tool_name":"Bash","tool_input":{"command":"rg -n foo_bar"}}"#).unwrap();
        assert_eq!(b.search_calls()[0].pattern, "foo_bar");
        let r = parse_pre_tool_input(r#"{"tool_name":"Read","tool_input":{"file_path":"/x"}}"#).unwrap();
        assert!(r.search_calls().is_empty());
    }

    #[test]
    fn ledger_caps_and_dedups() {
        let td = tempfile::tempdir().unwrap();
        let mut l = HintLedger::open(td.path(), "sess-1").unwrap();
        assert!(l.allows("a", 2));
        l.record(&["a"]).unwrap();
        assert!(!l.allows("a", 2), "same symbol twice");
        assert!(l.allows("b", 2));
        l.record(&["b", "d"]).unwrap();
        // A fresh process sees the same state.
        let l2 = HintLedger::open(td.path(), "sess-1").unwrap();
        assert_eq!(l2.count(), 2);
        assert!(l2.contains("d"), "every symbol of a multi-symbol hint is remembered");
        assert!(!l2.allows("c", 2), "cap reached");
        // Another session is independent; an unusable id gives no ledger.
        assert!(HintLedger::open(td.path(), "sess-2").unwrap().allows("a", 2));
        assert!(HintLedger::open(td.path(), "../..").is_none());
    }

    #[test]
    fn sweep_removes_only_old_hint_files() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("a.hints"), "x\n").unwrap();
        std::fs::write(td.path().join("keep.txt"), "x\n").unwrap();
        sweep_old(td.path(), std::time::Duration::from_secs(3600));
        assert!(td.path().join("a.hints").exists(), "fresh file kept");
        sweep_old(td.path(), std::time::Duration::ZERO);
        std::thread::sleep(std::time::Duration::from_millis(5));
        sweep_old(td.path(), std::time::Duration::ZERO);
        assert!(!td.path().join("a.hints").exists());
        assert!(td.path().join("keep.txt").exists());
    }

    fn call(path: &str, line: u32, caller: &str, conf: f32) -> Call {
        Call {
            path: path.into(),
            caller: caller.into(),
            callee: "foo".into(),
            receiver: None,
            is_method: false,
            line,
            resolved_callee: None,
            confidence: conf,
        }
    }

    #[test]
    fn hint_format_is_short_and_relative() {
        let root = Path::new("/r");
        let def = CodeHit {
            kind: SymbolKind::Function,
            name: "foo".into(),
            qualified_name: "foo".into(),
            path: "/r/src/a.rs".into(),
            line_start: 10,
            line_end: 20,
            score: 1.0,
            snippet: None,
            stale: false,
        };
        let b = SymbolBrief {
            name: "foo".into(),
            definitions: vec![def],
            definitions_total: 4,
            calls_total: 12,
            calls_capped: false,
            top_callers: vec![call("/r/src/b.rs", 5, "bar", 0.9), call("/r/src/c.rs", 7, "baz", 0.4)],
        };
        let h = render_hint(std::slice::from_ref(&b), root);
        assert!(h.contains("`foo` defined: src/a.rs:10 (fn foo) (+3 more)"), "{h}");
        assert!(h.contains("call sites: 12; top: src/b.rs:5 in bar [exact]; src/c.rs:7 in baz [ambiguous]"), "{h}");
        let two = render_hint(&[b.clone(), SymbolBrief { name: "qux".into(), ..b }], root);
        assert!(two.contains("`qux` defined"), "{two}");
        assert!(two.contains("name=foo|qux"), "{two}");
        assert!(two.chars().count() <= HINT_MAX_CHARS);
        assert!(h.contains("mcp__icode__symbol op=callers name=foo"));
        assert!(h.chars().count() <= HINT_MAX_CHARS);
        assert_eq!(confidence_label(0.7), "inferred");
        assert_eq!(confidence_label(0.6), "inferred");
        assert_eq!(confidence_label(0.3), "ambiguous");
    }

    #[test]
    fn hint_is_bounded_even_for_long_names() {
        let long = "x".repeat(110);
        let def = CodeHit {
            kind: SymbolKind::Class,
            name: long.clone(),
            qualified_name: format!("{long}::{long}"),
            path: format!("/r/{long}/{long}.rs"),
            line_start: 1,
            line_end: 2,
            score: 1.0,
            snippet: None,
            stale: false,
        };
        let b = SymbolBrief {
            name: long.clone(),
            definitions: vec![def.clone(), def.clone(), def],
            definitions_total: 3,
            calls_total: 0,
            calls_capped: false,
            top_callers: vec![],
        };
        let many = [b.clone(), b.clone(), b];
        assert!(render_hint(&many, Path::new("/r")).chars().count() <= HINT_MAX_CHARS);
    }
}
