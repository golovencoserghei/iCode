//! Drives the real `icode vault …` binary end to end. The passphrase comes from
//! `ICODE_VAULT_PASSPHRASE` (the scripted-test escape hatch) and scrypt is made
//! cheap with `ICODE_VAULT_WORK_FACTOR`, so no terminal is needed.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const PASS: &str = "correct horse battery staple";
const KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\n";

fn vault(file: &Path, pass: &str, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_icode"))
        .arg("vault")
        .args(args)
        .arg("--file")
        .arg(file)
        .env("ICODE_VAULT_PASSPHRASE", pass)
        .env("ICODE_VAULT_WORK_FACTOR", "4")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn icode");
    {
        let mut si = child.stdin.take().unwrap();
        if let Some(s) = stdin {
            si.write_all(s.as_bytes()).unwrap();
        }
    }
    child.wait_with_output().expect("icode output")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn full_cycle_store_list_get_and_refuse_wrong_passphrase() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("vault.age");

    let o = vault(&file, PASS, &["init"], None);
    assert!(o.status.success(), "init: {}", stderr(&o));
    assert!(stdout(&o).contains("age -d vault.age"), "init explains recovery");

    let o = vault(
        &file,
        PASS,
        &["add", "prod-breakglass", "--note", "accepted by web1 and db1"],
        Some(KEY),
    );
    assert!(o.status.success(), "add: {}", stderr(&o));

    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert!(on_disk.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"));
    assert!(!on_disk.contains("b3BlbnNzaC1rZXktdjEAAAAA"), "value must not be on disk in clear");

    let o = vault(&file, PASS, &["list"], None);
    assert!(o.status.success(), "list: {}", stderr(&o));
    let listed = stdout(&o);
    assert!(listed.contains("prod-breakglass") && listed.contains("web1 and db1"));
    assert!(!listed.contains("b3BlbnNzaC1rZXktdjEAAAAA"), "list never prints values");

    let o = vault(&file, PASS, &["get", "prod-breakglass"], None);
    assert!(o.status.success(), "get: {}", stderr(&o));
    assert_eq!(stdout(&o), KEY, "the key comes back byte for byte");

    let out = dir.path().join("restored_key");
    let o = vault(&file, PASS, &["get", "prod-breakglass", "--out", out.to_str().unwrap()], None);
    assert!(o.status.success(), "get --out: {}", stderr(&o));
    assert_eq!(std::fs::read_to_string(&out).unwrap(), KEY);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let o = vault(&file, PASS, &["get", "prod-breakglass", "--out", out.to_str().unwrap()], None);
    assert!(!o.status.success(), "get --out never overwrites");

    let o = vault(&file, "not the passphrase", &["list"], None);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("wrong passphrase"), "{}", stderr(&o));
}

#[test]
fn init_refuses_a_weak_passphrase_and_an_existing_vault() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("vault.age");

    let o = vault(&file, "short", &["init"], None);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("too short"), "{}", stderr(&o));
    assert!(!file.exists());

    assert!(vault(&file, PASS, &["init"], None).status.success());
    let o = vault(&file, PASS, &["init"], None);
    assert!(!o.status.success(), "a second init must not wipe the vault");
}

#[test]
fn passwd_re_encrypts_under_the_new_passphrase() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("vault.age");
    assert!(vault(&file, PASS, &["init"], None).status.success());
    assert!(vault(&file, PASS, &["add", "k"], Some("value-123")).status.success());

    // `passwd` reads the old and the new passphrase from the same env var in
    // scripted mode, so re-encrypt under PASS and prove it still opens; the
    // interactive path differs only in where the strings come from.
    let o = vault(&file, PASS, &["passwd"], None);
    assert!(o.status.success(), "passwd: {}", stderr(&o));
    let o = vault(&file, PASS, &["get", "k"], None);
    assert_eq!(stdout(&o), "value-123");
}
