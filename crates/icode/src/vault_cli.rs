//! `icode vault …` — the terminal front-end of `icode_engine::Vault`.
//!
//! Every passphrase is read from the TERMINAL (`/dev/tty`), never from argv (shell
//! history, `ps`) and never from stdin (stdin carries the secret value for `add`).
//! An agent's non-interactive shell has no terminal, so it cannot open the vault —
//! which is the point. `ICODE_VAULT_PASSPHRASE` exists for scripted tests only.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use clap::Subcommand;
use icode_engine::Vault;

/// A new passphrase shorter than this is refused: the file is meant to be copied
/// to places you do not control, so the passphrase is all that protects it.
const MIN_PASSPHRASE_CHARS: usize = 12;

#[derive(Subcommand)]
pub enum VaultAction {
    /// Create an empty vault (asks for a new passphrase twice).
    Init,
    /// Store a secret. The value comes from --from-file, from stdin when it is
    /// piped (`icode vault add srv-key < ~/.ssh/id_backup`), or from a hidden prompt.
    /// An existing entry with the same name is replaced.
    Add {
        /// Entry name, no spaces (e.g. `prod-ssh-breakglass`).
        name: String,
        /// Read the value from this file instead of stdin/prompt.
        #[arg(long)]
        from_file: Option<PathBuf>,
        /// What it is, which servers accept it, where other copies live.
        #[arg(long)]
        note: Option<String>,
    },
    /// List entry names, notes and value fingerprints — never the values.
    List,
    /// Print one value to stdout, or write it to --out (mode 0600).
    Get {
        name: String,
        /// Write here instead of stdout (refuses to overwrite an existing file).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Remove one entry.
    Rm { name: String },
    /// Re-encrypt the vault under a new passphrase.
    Passwd,
    /// Prove the vault opens, and print how to recover it on another machine.
    Check,
}

/// Where the vault lives: `--file`, else `$ICODE_VAULT`, else `~/.icode/vault.age`.
pub fn vault_path(file: Option<PathBuf>) -> PathBuf {
    if let Some(f) = file {
        return f;
    }
    if let Ok(p) = std::env::var("ICODE_VAULT") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".icode").join("vault.age")
}

pub fn run(file: Option<PathBuf>, action: VaultAction) -> anyhow::Result<()> {
    let path = vault_path(file);
    match action {
        VaultAction::Init => {
            if path.exists() {
                bail!("{} already exists — refusing to overwrite a vault", path.display());
            }
            let pass = new_passphrase()?;
            Vault::new().save(&path, &pass, work_factor())?;
            println!("created {}", path.display());
            print_recovery_advice(&path);
        }
        VaultAction::Add { name, from_file, note } => {
            let (mut vault, pass) = open(&path)?;
            let value = read_value(from_file.as_deref())?;
            if value.is_empty() {
                bail!("empty value — nothing stored");
            }
            let replaced = vault.put(&name, value, note)?;
            vault.save(&path, &pass, work_factor())?;
            let fp = vault.get(&name).map(|e| e.fingerprint()).unwrap_or_default();
            println!("{} {name} (fingerprint {fp})", if replaced { "replaced" } else { "stored" });
            println!("remember: copy {} somewhere off this machine", path.display());
        }
        VaultAction::List => {
            let (vault, _) = open(&path)?;
            if vault.is_empty() {
                println!("vault is empty");
            }
            for e in vault.entries() {
                println!("{}  fp={}  updated {}", e.name, e.fingerprint(), &e.updated_at[..10]);
                if let Some(n) = &e.note {
                    println!("    {n}");
                }
            }
        }
        VaultAction::Get { name, out } => {
            let (vault, _) = open(&path)?;
            let entry = vault
                .get(&name)
                .with_context(|| format!("no entry {name:?} in the vault (see `icode vault list`)"))?;
            match out {
                Some(out) => {
                    write_private_new(&out, entry.value.as_bytes())?;
                    eprintln!("wrote {name} to {} (mode 0600)", out.display());
                }
                None => {
                    let mut stdout = std::io::stdout().lock();
                    stdout.write_all(entry.value.as_bytes())?;
                    stdout.flush()?;
                }
            }
        }
        VaultAction::Rm { name } => {
            let (mut vault, pass) = open(&path)?;
            if !vault.remove(&name) {
                bail!("no entry {name:?} in the vault");
            }
            vault.save(&path, &pass, work_factor())?;
            println!("removed {name} (the previous version stays in {}.bak until the next save)", path.display());
        }
        VaultAction::Passwd => {
            let (vault, _) = open(&path)?;
            let pass = new_passphrase()?;
            vault.save(&path, &pass, work_factor())?;
            // The .bak still opens with the OLD passphrase; it is replaced by the
            // next save, but say so instead of letting it surprise anyone.
            println!(
                "re-encrypted {} — note {}.bak is the previous version under the OLD passphrase",
                path.display(),
                path.display()
            );
            println!("replace every off-machine copy with the new file");
        }
        VaultAction::Check => {
            let (vault, _) = open(&path)?;
            println!("OK: {} opens, {} entr{}", path.display(), vault.len(), if vault.len() == 1 { "y" } else { "ies" });
            for e in vault.entries() {
                println!("  {}  fp={}", e.name, e.fingerprint());
            }
            print_recovery_advice(&path);
        }
    }
    Ok(())
}

/// Open an existing vault, asking for its passphrase.
fn open(path: &Path) -> anyhow::Result<(Vault, String)> {
    if !path.exists() {
        bail!(
            "no vault at {} — create one with `icode vault init` (or pass --file / set ICODE_VAULT)",
            path.display()
        );
    }
    let pass = passphrase("Vault passphrase: ")?;
    let vault = Vault::open(path, &pass)?;
    Ok((vault, pass))
}

/// scrypt work factor override for scripted tests (`ICODE_VAULT_WORK_FACTOR`);
/// otherwise the engine's default (2^18) applies.
fn work_factor() -> Option<u8> {
    std::env::var("ICODE_VAULT_WORK_FACTOR").ok()?.parse().ok()
}

fn passphrase(prompt: &str) -> anyhow::Result<String> {
    if let Ok(p) = std::env::var("ICODE_VAULT_PASSPHRASE") {
        return Ok(p);
    }
    rpassword::prompt_password(prompt).context(
        "cannot read a passphrase: no terminal. The vault only opens from an interactive \
         terminal (by design, an agent's shell cannot unlock it)",
    )
}

fn new_passphrase() -> anyhow::Result<String> {
    let pass = passphrase("New vault passphrase: ")?;
    if pass.chars().count() < MIN_PASSPHRASE_CHARS {
        bail!(
            "passphrase too short: use at least {MIN_PASSPHRASE_CHARS} characters \
             (several random words work well) — it is the only thing protecting every copy"
        );
    }
    if std::env::var("ICODE_VAULT_PASSPHRASE").is_err() && passphrase("Repeat passphrase: ")? != pass {
        bail!("passphrases do not match");
    }
    Ok(pass)
}

/// The secret for `add`: a file, piped stdin, or a hidden prompt — never argv.
fn read_value(from_file: Option<&Path>) -> anyhow::Result<String> {
    if let Some(f) = from_file {
        let bytes = std::fs::read(f).with_context(|| format!("cannot read {}", f.display()))?;
        return String::from_utf8(bytes)
            .with_context(|| format!("{} is not UTF-8 text; the vault stores text secrets", f.display()));
    }
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut s = String::new();
        stdin
            .lock()
            .read_to_string(&mut s)
            .context("stdin is not UTF-8 text; the vault stores text secrets")?;
        return Ok(s);
    }
    Ok(rpassword::prompt_password("Secret value (hidden): ")?)
}

/// Write a new file readable by the owner only; never overwrite.
fn write_private_new(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("cannot create {} (it must not exist yet)", path.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

fn print_recovery_advice(path: &Path) {
    let home_icode = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".icode"))
        .ok();
    println!();
    println!("Recovery on another machine needs exactly two things:");
    println!("  1. a copy of this file  — {}", path.display());
    println!("  2. the passphrase       — keep it in your head AND on paper in a safe place");
    println!("Then either `icode vault list --file vault.age`, or with no iCode at all:");
    println!("  age -d vault.age      (prints the JSON with every entry)");
    if home_icode.is_some_and(|d| path.starts_with(d)) {
        println!();
        println!("WARNING: this vault exists only on THIS machine. Copy it off now —");
        println!("  a private git repo, a cloud folder, a USB stick, a printout (it is text).");
    }
}
