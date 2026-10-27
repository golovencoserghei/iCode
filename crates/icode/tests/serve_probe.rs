//! Drives the real `icode serve` binary over stdio the way Claude Code opens it:
//! a `server/discover` version-negotiation probe FIRST, then `initialize`.
//!
//! Before the fix the probe aborted the handshake (rmcp rejects any pre-init
//! request but `ping`), and the process then lingered until the background
//! startup sync finished — over a minute on a large tree, past the client's 30 s
//! connect budget. The server must now answer the probe with a JSON-RPC
//! "method not found" that echoes the id, initialize on the SAME connection, and
//! exit promptly once stdin closes.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const PROBE: &str =
    r#"{"jsonrpc":"2.0","id":"server-discover-probe-1","method":"server/discover","params":{}}"#;
const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"probe-test","version":"0"}}}"#;

/// A throwaway project of `files` Rust files, so the startup sync has something
/// to index; an isolated central db; no watcher daemon. stderr is kept: it is
/// where the server reports the sync's progress.
fn spawn_serve(files: usize) -> (tempfile::TempDir, Child, mpsc::Receiver<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    for i in 0..files {
        std::fs::write(
            dir.path().join(format!("m{i}.rs")),
            format!("pub fn f{i}(x: u32) -> u32 {{ g{i}(x) + 1 }}\nfn g{i}(x: u32) -> u32 {{ x * 2 }}\n"),
        )
        .unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_icode"))
        .arg("serve")
        .arg(dir.path())
        .env("ICODE_NO_DAEMON", "1")
        .env("ICODE_DB_PATH", dir.path().join("central.db"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn icode serve");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (dir, child, rx)
}

fn next_frame(rx: &mpsc::Receiver<String>, what: &str) -> serde_json::Value {
    let line = rx
        .recv_timeout(Duration::from_secs(20))
        .unwrap_or_else(|_| panic!("no {what} within 20s"));
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("{what} is not JSON ({e}): {line}"))
}

#[test]
fn the_pre_init_probe_gets_an_error_reply_and_initialize_still_succeeds() {
    let (_dir, mut child, rx) = spawn_serve(1);
    let mut stdin = child.stdin.take().unwrap();

    writeln!(stdin, "{PROBE}").unwrap();
    let reply = next_frame(&rx, "probe reply");
    assert_eq!(
        reply["id"],
        serde_json::json!("server-discover-probe-1"),
        "the reply must echo the probe id, that is what the client keys on: {reply}"
    );
    assert_eq!(reply["error"]["code"], serde_json::json!(-32601), "{reply}");

    writeln!(stdin, "{INITIALIZE}").unwrap();
    let reply = next_frame(&rx, "initialize reply");
    assert_eq!(reply["id"], serde_json::json!(1));
    assert!(
        reply["result"]["protocolVersion"].is_string(),
        "initialize must succeed on the same connection: {reply}"
    );

    // Closing stdin ends the session; the process must not outlive it by much.
    drop(stdin);
    let started = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            panic!("icode serve still alive 10s after stdin closed");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "clean exit after a completed session: {status}"
    );
}

/// A client that closes the connection before `initialize` (Claude Code shutting
/// down, or a client giving up on the handshake) must see the process exit at
/// once — not after the startup sync. That wait is what turned a probe into a
/// connect timeout on a large tree, so the tree here is large enough for the
/// sync to still be running when stdin closes: the exit must come BEFORE the
/// "graph synced" line, and it is a clean exit, nothing failed.
#[test]
fn a_connection_closed_before_initialize_exits_promptly() {
    let (_dir, mut child, _rx) = spawn_serve(3000);
    let stderr = child.stderr.take().unwrap();
    let stdin = child.stdin.take().unwrap();
    drop(stdin);
    let started = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            panic!("icode serve still alive 10s after the client closed pre-init");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let exited_after = started.elapsed();
    let mut log = String::new();
    std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut log).unwrap();

    assert!(
        status.success(),
        "a client going away is not a server error: {status}\n{log}"
    );
    assert!(
        exited_after < Duration::from_secs(3),
        "exit took {exited_after:?}\n{log}"
    );
    assert!(
        !log.contains("graph synced"),
        "the process waited for the startup sync to finish before exiting:\n{log}"
    );
}
