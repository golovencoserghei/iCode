//! Bin-level test for `icode hook pre-tool`: a real temp project, indexed by the
//! real binary, then PreToolUse payloads on stdin exactly as Claude Code sends
//! them. Checks the hint, the silences, the cap and that stderr stays empty.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn icode_bin() -> &'static str {
    env!("CARGO_BIN_EXE_icode")
}

/// A two-file Python project with a git marker, indexed with a private HOME and
/// central db so nothing outside the tempdir is touched.
fn indexed_project() -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    let proj = td.path().join("proj");
    std::fs::create_dir_all(proj.join(".git")).unwrap();
    std::fs::create_dir_all(proj.join("src")).unwrap();
    std::fs::write(
        proj.join("src/billing.py"),
        "def compute_invoice_total(items, tax):\n    return apply_tax(sum(items), tax)\n\n\
         def apply_tax(amount, tax):\n    return amount * (1 + tax)\n",
    )
    .unwrap();
    std::fs::write(
        proj.join("src/api.py"),
        "from billing import compute_invoice_total\n\ndef checkout(cart):\n    \
         return compute_invoice_total(cart, 0.2)\n\ndef preview(cart):\n    \
         return compute_invoice_total(cart, 0.0)\n",
    )
    .unwrap();
    let out = Command::new(icode_bin())
        .args(["index"])
        .arg(&proj)
        .env("HOME", td.path())
        .env("ICODE_DB_PATH", td.path().join("central.db"))
        .output()
        .unwrap();
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));
    td
}

/// Run the hook with `payload` on stdin → (stdout, stderr, exit code).
fn pre_tool(home: &Path, payload: &str, cap: Option<&str>) -> (String, String, i32) {
    let mut cmd = Command::new(icode_bin());
    cmd.args(["hook", "pre-tool"])
        .env("HOME", home)
        .env("ICODE_HOOK_STATE_DIR", home.join("state"))
        .env_remove("ICODE_HOOK_HINT_CAP")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(c) = cap {
        cmd.env("ICODE_HOOK_HINT_CAP", c);
    }
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

fn grep_payload(session: &str, cwd: &Path, pattern: &str) -> String {
    serde_json::json!({
        "session_id": session,
        "cwd": cwd,
        "hook_event_name": "PreToolUse",
        "tool_name": "Grep",
        "tool_input": {"pattern": pattern, "output_mode": "content"}
    })
    .to_string()
}

#[test]
fn grep_for_a_symbol_gets_the_graph_answer() {
    let td = indexed_project();
    let proj = td.path().join("proj");
    let (out, err, code) = pre_tool(td.path(), &grep_payload("s1", &proj, "def compute_invoice_total"), None);
    assert_eq!(code, 0);
    assert!(err.is_empty(), "stderr must stay empty: {err}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("valid JSON");
    let h = &v["hookSpecificOutput"];
    assert_eq!(h["hookEventName"], "PreToolUse");
    // Never blocks: no permission decision at all.
    assert!(h.get("permissionDecision").is_none());
    let ctx = h["additionalContext"].as_str().unwrap();
    assert!(ctx.contains("defined: src/billing.py:1"), "{ctx}");
    assert!(ctx.contains("call sites: 2"), "{ctx}");
    assert!(ctx.contains("src/api.py:4 in checkout"), "{ctx}");
    assert!(ctx.contains("op=callers name=compute_invoice_total"), "{ctx}");

    // Same symbol again in the session → silence.
    let (again, _, _) = pre_tool(td.path(), &grep_payload("s1", &proj, "compute_invoice_total("), None);
    assert!(again.is_empty(), "repeat must be silent: {again}");
}

#[test]
fn bash_greps_are_answered_filters_are_not() {
    let td = indexed_project();
    let proj = td.path().join("proj");
    let bash = |cmd: &str| {
        serde_json::json!({"session_id": "s2", "cwd": td.path(), "tool_name": "Bash", "tool_input": {"command": cmd}})
            .to_string()
    };
    // A grep that filters another command's output is not a code search.
    let (filter, _, _) = pre_tool(td.path(), &bash("cat proj/src/billing.py | grep apply_tax"), None);
    assert!(filter.is_empty(), "{filter}");
    // `cd` into the project, a plain-name alternation, output piped to head.
    let cmd = format!("cd {} && grep -rn 'apply_tax\\|checkout' src 2>/dev/null | head -20", proj.display());
    let (out, err, code) = pre_tool(td.path(), &bash(&cmd), None);
    assert_eq!(code, 0);
    assert!(err.is_empty());
    assert!(out.contains("`apply_tax` defined: src/billing.py:4"), "{out}");
    assert!(out.contains("`checkout` defined: src/api.py:3"), "{out}");
}

#[test]
fn silent_without_index_symbol_or_identifier() {
    let td = indexed_project();
    let proj = td.path().join("proj");
    for (cwd, pattern) in [
        (proj.as_path(), "no_such_symbol_here"),
        (proj.as_path(), "apply.*tax"),
        (proj.as_path(), "total amount"),
        (td.path(), "apply_tax"), // not an indexed tree
    ] {
        let (out, err, code) = pre_tool(td.path(), &grep_payload("s3", cwd, pattern), None);
        assert_eq!((out.as_str(), err.as_str(), code), ("", "", 0), "pattern {pattern:?}");
    }
    // Not a search tool / garbage stdin.
    for payload in [
        r#"{"session_id":"s3","tool_name":"Read","tool_input":{"file_path":"/x"}}"#,
        "not json",
        "",
    ] {
        let (out, err, code) = pre_tool(td.path(), payload, None);
        assert_eq!((out.as_str(), err.as_str(), code), ("", "", 0));
    }
    // The hook never created an index where there was none.
    assert!(!td.path().join(".icode").join("index.db").exists());
}

#[test]
fn cap_limits_hints_per_session() {
    let td = indexed_project();
    let proj = td.path().join("proj");
    let (a, _, _) = pre_tool(td.path(), &grep_payload("s4", &proj, "apply_tax"), Some("1"));
    assert!(!a.is_empty());
    let (b, _, _) = pre_tool(td.path(), &grep_payload("s4", &proj, "checkout"), Some("1"));
    assert!(b.is_empty(), "cap of 1 spent");
    let (c, _, _) = pre_tool(td.path(), &grep_payload("s5", &proj, "checkout"), Some("0"));
    assert!(c.is_empty(), "cap 0 disables");
}
