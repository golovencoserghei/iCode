//! Turning a shell command the agent ran into a ledger entry.
//!
//! The ledger is only as good as its coverage, and a ledger that depends on the
//! agent remembering to file its own evidence has none. Claude Code fires a
//! `PostToolUse` hook after every tool call; `icode hook post-tool` reads that
//! payload, and this module decides three things from it:
//!
//!   1. **Was this a verification at all?** (`cargo test` yes, `ls` no.)
//!   2. **How wide was it?** A filtered run proves less than a full one, and
//!      conflating the two is the failure mode the whole feature guards against.
//!   3. **Did it pass?** Read from the output, because the hook payload carries no
//!      exit status.
//!
//! On (3) the rule is deliberate: **when the output does not clearly say, nothing
//! is recorded.** A guessed pass is a lie the ledger would then repeat with a
//! straight face; a missing entry merely leaves the file reported as unverified,
//! which is what it actually is.

use serde::Deserialize;

use super::RunKind;

/// The fields we use out of a `PostToolUse` payload. Everything else is ignored,
/// so an upstream addition to the envelope cannot break the parse.
#[derive(Debug, Default, Deserialize)]
pub struct PostToolInput {
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: serde_json::Value,
    #[serde(default)]
    pub tool_response: serde_json::Value,
}

impl PostToolInput {
    /// The shell command, for a Bash tool call.
    pub fn command(&self) -> Option<&str> {
        if !self.tool_name.eq_ignore_ascii_case("bash") {
            return None;
        }
        self.tool_input.get("command")?.as_str()
    }

    /// stdout+stderr as one blob. The response is an object for Bash, but older
    /// payloads (and other tools) hand back a plain string — both are accepted
    /// because a parse that only knows one shape silently records nothing.
    pub fn output(&self) -> String {
        match &self.tool_response {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Object(map) => {
                let mut out = String::new();
                for key in ["stdout", "stderr", "output", "content"] {
                    if let Some(serde_json::Value::String(s)) = map.get(key) {
                        out.push_str(s);
                        out.push('\n');
                    }
                }
                out
            }
            _ => String::new(),
        }
    }
}

/// What a command turned out to be, once recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recognised {
    pub kind: RunKind,
    /// True when the command exercises the WHOLE project.
    pub wide: bool,
}

pub fn parse_post_tool_input(json: &str) -> Option<PostToolInput> {
    serde_json::from_str(json).ok()
}

/// Split a command line on shell separators and return the segments, so
/// `cd x && cargo test` is recognised by its second segment rather than its first.
fn segments(cmd: &str) -> Vec<String> {
    // `split` takes one pattern, so normalise every separator to `\n` first: the
    // shell operators differ in meaning, but for "which command ran" they are all
    // just boundaries.
    let mut normalised = cmd.to_string();
    for sep in ["&&", "||", ";", "|", "\n"] {
        normalised = normalised.replace(sep, "\u{1}");
    }
    normalised
        .split('\u{1}')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Drop shell redirections and their targets.
///
/// This is not cosmetic. Every token that is not a flag reads as a NARROWING
/// argument (a test-name filter, a path), so leaving `2>&1` or `> out.log` in the
/// token list turns a full-project run into a filtered one — and a filtered run
/// never clears the ledger. Since an agent's command is almost always
/// `cargo test 2>&1 | tail -80`, that single misread was enough to make the whole
/// feature report a green repo as unverified forever.
///
/// Handles `>`, `>>`, `<`, `<<`, an fd prefix (`2>`, `1>>`), `&>`, and both
/// spellings of the target — attached (`2>/dev/null`) or separate (`> out.log`).
/// A bare `&` (background) goes too, for the same reason.
fn strip_redirections<'a>(tokens: &[&'a str]) -> Vec<&'a str> {
    let mut out: Vec<&'a str> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if t == "&" {
            i += 1;
            continue;
        }
        // Strip an optional leading file descriptor (`2>`) or `&` (`&>`), then see
        // whether what remains starts with a redirection operator.
        let rest = t.trim_start_matches(|c: char| c.is_ascii_digit()).trim_start_matches('&');
        if rest.starts_with('>') || rest.starts_with('<') {
            let target = rest.trim_start_matches(['>', '<', '&']);
            // `> out.log` carries its target in the NEXT token; `2>/dev/null`
            // carries it inline.
            i += if target.is_empty() { 2 } else { 1 };
            continue;
        }
        out.push(t);
        i += 1;
    }
    out
}

/// Drop leading environment assignments and wrappers (`RUST_LOG=x`, `env`, `time`,
/// `nice`) so `RUST_BACKTRACE=1 cargo test` still reads as a cargo test run.
fn strip_prefixes(tokens: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut skipping = true;
    for t in tokens {
        if skipping
            && (t.contains('=') && !t.starts_with('-')
                || matches!(*t, "env" | "time" | "nice" | "sudo" | "command"))
        {
            continue;
        }
        skipping = false;
        out.push((*t).to_string());
    }
    out
}

/// Whether a command is a verification run, and how wide.
///
/// Unrecognised commands return `None` — the ledger records checks, not history.
pub fn classify_command(cmd: &str) -> Option<Recognised> {
    for seg in segments(cmd) {
        let tokens: Vec<&str> = seg.split_whitespace().collect();
        let tokens = strip_redirections(&tokens);
        let tokens = strip_prefixes(&tokens);
        let toks: Vec<&str> = tokens.iter().map(String::as_str).collect();
        if let Some(r) = classify_tokens(&toks) {
            return Some(r);
        }
    }
    None
}

/// True when any token narrows a run to part of the project: a package/target
/// selector, or a bare positional argument (a test-name filter or a path).
fn is_narrowed(args: &[&str], flags_with_value: &[&str], widening: &[&str]) -> bool {
    let mut i = 0;
    let mut narrowed = false;
    while i < args.len() {
        let a = args[i];
        if widening.contains(&a) {
            return false;
        }
        if flags_with_value.contains(&a) {
            return true;
        }
        if a.starts_with('-') {
            // `--package=x` style, and flags whose value is attached.
            if flags_with_value.iter().any(|f| a.starts_with(&format!("{f}="))) {
                return true;
            }
            i += 1;
            continue;
        }
        // A bare positional: a filter (`cargo test my_case`) or a path (`pytest tests/x.py`).
        narrowed = true;
        i += 1;
    }
    narrowed
}

fn classify_tokens(toks: &[&str]) -> Option<Recognised> {
    let (head, rest) = toks.split_first()?;
    let head = head.rsplit('/').next().unwrap_or(head);

    match head {
        "cargo" => {
            let sub = rest.first()?;
            let args = &rest[1..];
            let narrowing = ["-p", "--package", "--test", "--bin", "--lib", "--example", "--bench"];
            let widening = ["--workspace", "--all"];
            match *sub {
                "test" | "nextest" => Some(Recognised {
                    kind: RunKind::Test,
                    wide: !is_narrowed(args, &narrowing, &widening),
                }),
                "build" => Some(Recognised {
                    kind: RunKind::Build,
                    wide: !is_narrowed(args, &narrowing, &widening),
                }),
                "check" => Some(Recognised {
                    kind: RunKind::Typecheck,
                    wide: !is_narrowed(args, &narrowing, &widening),
                }),
                "clippy" | "fmt" => Some(Recognised {
                    kind: RunKind::Lint,
                    wide: !is_narrowed(args, &narrowing, &widening),
                }),
                _ => None,
            }
        }
        "pytest" | "py.test" => Some(Recognised {
            kind: RunKind::Test,
            wide: !is_narrowed(rest, &["-k", "-m", "--deselect"], &[]),
        }),
        "tox" | "phpunit" | "rspec" | "vitest" | "jest" => Some(Recognised {
            kind: RunKind::Test,
            wide: !is_narrowed(rest, &["-t", "--testNamePattern", "--filter"], &[]),
        }),
        "go" => match *rest.first()? {
            "test" => {
                let args = &rest[1..];
                // `go test ./...` is the whole module; anything narrower is not.
                let wide = args.contains(&"./...") || !is_narrowed(args, &["-run"], &["./..."]);
                Some(Recognised { kind: RunKind::Test, wide })
            }
            "build" => Some(Recognised { kind: RunKind::Build, wide: rest.contains(&"./...") }),
            "vet" => Some(Recognised { kind: RunKind::Lint, wide: rest.contains(&"./...") }),
            _ => None,
        },
        "npm" | "pnpm" | "yarn" | "bun" => {
            // `npm test`, `npm run test`, `yarn test:unit`, `npm run build`.
            let mut it = rest.iter().copied();
            let first = it.next()?;
            let script = if first == "run" || first == "run-script" {
                it.next()?
            } else {
                first
            };
            let args: Vec<&str> = it.collect();
            let kind = if script.starts_with("test") {
                RunKind::Test
            } else if script.starts_with("build") {
                RunKind::Build
            } else if script.starts_with("lint") || script.starts_with("format") {
                RunKind::Lint
            } else if script.starts_with("typecheck") || script.starts_with("tsc") {
                RunKind::Typecheck
            } else {
                return None;
            };
            Some(Recognised { kind, wide: !is_narrowed(&args, &["-t", "--filter"], &[]) })
        }
        "make" => {
            let target = rest.first().copied().unwrap_or("all");
            let kind = match target {
                t if t.starts_with("test") => RunKind::Test,
                t if t.starts_with("lint") => RunKind::Lint,
                t if t.starts_with("build") || t == "all" => RunKind::Build,
                t if t.starts_with("check") => RunKind::Typecheck,
                _ => return None,
            };
            Some(Recognised { kind, wide: true })
        }
        "tsc" => Some(Recognised { kind: RunKind::Typecheck, wide: true }),
        "mvn" | "gradle" | "./gradlew" | "gradlew" => {
            let target = rest.first().copied().unwrap_or("");
            let kind = match target {
                t if t.contains("test") => RunKind::Test,
                t if t.contains("build") || t.contains("assemble") => RunKind::Build,
                _ => return None,
            };
            Some(Recognised { kind, wide: !target.contains(':') })
        }
        "dotnet" => match *rest.first()? {
            "test" => Some(Recognised {
                kind: RunKind::Test,
                wide: !is_narrowed(&rest[1..], &["--filter"], &[]),
            }),
            "build" => Some(Recognised { kind: RunKind::Build, wide: true }),
            _ => None,
        },
        _ => None,
    }
}

/// Markers that a run FAILED. Checked first: a suite that prints both a pass line
/// and a failure line failed.
const FAILURE_MARKERS: &[&str] = &[
    "test result: FAILED",
    "panicked at",
    "error[E",
    "error: could not compile",
    "error: test failed",
    "FAILED (",
    "--- FAIL",
    "\nFAIL",
    "AssertionError",
    "Build FAILED",
    "BUILD FAILED",
    "compilation terminated",
];

/// Markers that a run PASSED.
const SUCCESS_MARKERS: &[&str] = &[
    "test result: ok.",
    "Finished ",
    "passed,",
    " passed in ",
    " passed (",
    "\nok  ",
    "\nPASS",
    "OK (",
    "BUILD SUCCESS",
    "Tests:",
    "0 failed",
];

/// Whether the output reports a NON-ZERO failure count.
///
/// Searching for the word "failed" cannot do this job in either direction: cargo
/// prints `7 passed; 0 failed` on success, and pytest prints `2 failed, 10 passed`
/// on failure — so a substring check reads the first as red and (because the pass
/// marker also matches) the second as green. That second case is the dangerous one:
/// a suite with real failures filed as evidence that the code works.
///
/// So the count is read: for every occurrence of "failed"/"failures", take the
/// number immediately before it and ask whether it is greater than zero.
fn nonzero_failures(output: &str) -> bool {
    let bytes = output.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = output[from..].find("fail") {
        let at = from + rel;
        from = at + 4;
        let word_ok = output[at..].starts_with("failed")
            || output[at..].starts_with("failures")
            || output[at..].starts_with("failing");
        if !word_ok {
            continue;
        }
        // Walk back over spaces to the digits that precede the word, if any.
        let mut i = at;
        while i > 0 && (bytes[i - 1] as char).is_whitespace() {
            i -= 1;
        }
        let end = i;
        while i > 0 && (bytes[i - 1] as char).is_ascii_digit() {
            i -= 1;
        }
        if i == end {
            // No count at all ("failures:", "test failed") — a bare failure word.
            return true;
        }
        if output[i..end].parse::<u64>().map(|n| n > 0).unwrap_or(true) {
            return true;
        }
    }
    false
}

/// Did the run pass? `None` when the output does not say — the caller then records
/// NOTHING, which is the whole point: an unverified file must never be cleared by
/// a guess.
pub fn outcome_from_output(output: &str) -> Option<bool> {
    if output.trim().is_empty() {
        return None;
    }
    if FAILURE_MARKERS.iter().any(|m| output.contains(m)) || nonzero_failures(output) {
        return Some(false);
    }
    if SUCCESS_MARKERS.iter().any(|m| output.contains(m)) {
        return Some(true);
    }
    None
}

/// The tail of the output, for the ledger's `summary`.
pub fn output_tail(output: &str, lines: usize) -> Option<String> {
    let trimmed = output.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    let tail: Vec<&str> = trimmed.lines().rev().take(lines).collect();
    Some(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_full_and_narrowed_cargo_runs() {
        let full = classify_command("cargo test").unwrap();
        assert_eq!(full.kind, RunKind::Test);
        assert!(full.wide);

        assert!(classify_command("cargo test --workspace").unwrap().wide);
        // A package selector or a name filter proves less than the whole suite.
        assert!(!classify_command("cargo test -p icode-engine").unwrap().wide);
        assert!(!classify_command("cargo test my_case").unwrap().wide);
        assert!(!classify_command("cargo test --test verify_e2e").unwrap().wide);
    }

    #[test]
    fn recognises_other_ecosystems() {
        assert_eq!(classify_command("pytest").unwrap().kind, RunKind::Test);
        assert!(!classify_command("pytest tests/test_auth.py").unwrap().wide);
        assert!(!classify_command("pytest -k auth").unwrap().wide);
        assert!(classify_command("go test ./...").unwrap().wide);
        assert!(!classify_command("go test ./pkg/auth").unwrap().wide);
        assert_eq!(classify_command("npm run build").unwrap().kind, RunKind::Build);
        assert_eq!(classify_command("cargo clippy").unwrap().kind, RunKind::Lint);
        assert_eq!(classify_command("tsc --noEmit").unwrap().kind, RunKind::Typecheck);
    }

    #[test]
    fn ignores_everything_that_is_not_a_check() {
        for cmd in ["ls -la", "git status", "cat src/main.rs", "rg TODO", "echo cargo test"] {
            assert!(classify_command(cmd).is_none(), "should be ignored: {cmd}");
        }
    }

    /// A redirection is not a test filter. `cargo test 2>&1 | tail -80` is how an
    /// agent actually runs a suite, and reading `2>&1` as a narrowing positional
    /// made every full run record as filtered — so the ledger never went green.
    #[test]
    fn redirections_do_not_narrow_a_run() {
        for cmd in [
            "cargo test 2>&1 | tail -80",
            "cargo test > /tmp/out.log",
            "cargo test >/tmp/out.log 2>&1",
            "cargo test 2>/dev/null",
            "cargo test &> out.log",
            "cargo test 2>&1 | tee log &",
        ] {
            let r = classify_command(cmd).unwrap_or_else(|| panic!("not recognised: {cmd}"));
            assert!(r.wide, "redirection must not narrow: {cmd}");
        }
        assert!(classify_command("pytest 2>&1 | tee log").unwrap().wide);
        // ...while a REAL filter still narrows, redirection or not.
        assert!(!classify_command("cargo test my_case 2>&1 | tail -5").unwrap().wide);
        assert!(!classify_command("pytest -k auth 2>&1").unwrap().wide);
    }

    #[test]
    fn sees_through_env_prefixes_and_chained_commands() {
        assert!(classify_command("RUST_BACKTRACE=1 cargo test").is_some());
        assert!(classify_command("cd crates/icode-engine && cargo test").is_some());
        assert_eq!(
            classify_command("cargo build && cargo test").unwrap().kind,
            RunKind::Build,
            "the first recognised segment wins; each is recorded on its own hook fire"
        );
    }

    #[test]
    fn outcome_reads_real_runner_output() {
        assert_eq!(
            outcome_from_output("running 7 tests\ntest result: ok. 7 passed; 0 failed;"),
            Some(true)
        );
        assert_eq!(
            outcome_from_output("test result: FAILED. 5 passed; 2 failed;"),
            Some(false)
        );
        assert_eq!(outcome_from_output("=== 12 passed in 1.20s ==="), Some(true));
        assert_eq!(outcome_from_output("=== 2 failed, 10 passed in 1.20s ==="), Some(false));
        assert_eq!(
            outcome_from_output("error[E0425]: cannot find function `x`"),
            Some(false)
        );
    }

    #[test]
    fn a_pass_line_next_to_failures_is_a_failure() {
        // pytest and jest both print passes and failures on the SAME line. Reading
        // the pass half and stopping is how a red suite becomes "verified".
        assert_eq!(outcome_from_output("=== 2 failed, 10 passed in 1.20s ==="), Some(false));
        assert_eq!(
            outcome_from_output("Tests:       1 failed, 3 passed, 4 total"),
            Some(false)
        );
        // ...and a zero count is not a failure.
        assert_eq!(
            outcome_from_output("test result: ok. 7 passed; 0 failed; 0 ignored"),
            Some(true)
        );
        assert_eq!(outcome_from_output("Tests:       4 passed, 4 total"), Some(true));
    }

    #[test]
    fn an_unreadable_outcome_records_nothing() {
        // The single most important case: silence, or output that says neither
        // pass nor fail, must NOT be optimistically filed as a green run.
        assert_eq!(outcome_from_output(""), None);
        assert_eq!(outcome_from_output("   \n  "), None);
        assert_eq!(outcome_from_output("Compiling icode-engine v0.1.0"), None);
    }

    #[test]
    fn parses_the_hook_envelope_in_both_response_shapes() {
        let obj = r#"{"cwd":"/p","tool_name":"Bash","tool_input":{"command":"cargo test"},
                      "tool_response":{"stdout":"test result: ok.","stderr":""}}"#;
        let input = parse_post_tool_input(obj).unwrap();
        assert_eq!(input.command(), Some("cargo test"));
        assert!(input.output().contains("test result: ok."));

        let string_shape = r#"{"tool_name":"Bash","tool_input":{"command":"pytest"},
                               "tool_response":"=== 3 passed in 0.1s ==="}"#;
        let input = parse_post_tool_input(string_shape).unwrap();
        assert_eq!(outcome_from_output(&input.output()), Some(true));
    }

    #[test]
    fn a_non_bash_tool_has_no_command() {
        let payload = r#"{"tool_name":"Read","tool_input":{"file_path":"/x"},"tool_response":"..."}"#;
        assert!(parse_post_tool_input(payload).unwrap().command().is_none());
    }
}
