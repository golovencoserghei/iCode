//! The watcher daemon exits by itself once it has seen no changes and no live
//! `serve` for its idle period, and stays while a client keeps the heartbeat fresh.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

fn spawn_daemon(dir: &std::path::Path, idle_secs: u64) -> Child {
    std::fs::write(dir.join("lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
    Command::new(env!("CARGO_BIN_EXE_icode"))
        .args(["daemon", "run"])
        .arg(dir)
        .env("HOME", dir)
        .env("ICODE_DAEMON_IDLE_SECS", idle_secs.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn icode daemon run")
}

fn exited_within(child: &mut Child, limit: Duration, mut tick: impl FnMut()) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if child.try_wait().unwrap().is_some() {
            return true;
        }
        tick();
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn an_idle_daemon_with_no_client_exits_and_frees_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_daemon(dir.path(), 1);
    let exited = exited_within(&mut child, Duration::from_secs(20), || {});
    if !exited {
        let _ = child.kill();
    }
    assert!(exited, "daemon must exit after its idle period");
    // The lock is free again: a new holder can take it.
    let lock = icode_engine::DaemonLock::acquire(dir.path());
    assert!(lock.is_ok(), "the idle exit must release the lock");
}

#[test]
fn a_fresh_heartbeat_keeps_the_daemon_alive() {
    let dir = tempfile::tempdir().unwrap();
    let beat = dir.path().join(".icode").join(icode_engine::SERVE_HEARTBEAT);
    std::fs::create_dir_all(beat.parent().unwrap()).unwrap();
    let mut child = spawn_daemon(dir.path(), 2);
    let exited = exited_within(&mut child, Duration::from_secs(6), || {
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&beat).unwrap();
        f.set_modified(SystemTime::now()).unwrap();
    });
    let _ = child.kill();
    let _ = child.wait();
    assert!(!exited, "a live client's heartbeat must keep the daemon running");
}
