//! `vault` — a small passphrase-encrypted store for the secrets you must be able to
//! get back after losing a machine: a server's break-glass SSH key, recovery codes,
//! a root password.
//!
//! The file is a standard **age** file (scrypt passphrase recipient, ASCII-armored)
//! whose plaintext is a JSON document. That choice is the whole recovery story:
//! with the file and the passphrase, the stock `age -d vault.age` on any machine
//! gives the secrets back — no iCode build, no database, no key file. The file is
//! text, so it can live in a private git repo, a cloud folder, a USB stick or a
//! printed page, as many copies as you like; without the passphrase it is noise.
//!
//! Deliberately NOT reachable over MCP. A secret that an agent can read is a secret
//! that ends up in a model's context; the CLI asks for the passphrase on the
//! terminal, which an agent's non-interactive shell does not have.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use age::armor::{ArmoredReader, ArmoredWriter, Format};
use age::secrecy::SecretString;
use chrono::Utc;
use icode_core::error::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Marker in the plaintext so a vault is recognisable after `age -d`, and so a
/// different age file decrypted with the same passphrase is not mistaken for one.
const FORMAT: &str = "icode-vault";
const VERSION: u32 = 1;

/// scrypt work factor (log2 N) used unless a caller overrides it. age would
/// otherwise derive one from a one-second benchmark of THIS machine, which on a slow
/// or debug build lands as low as 2^14 — too cheap for a file meant to sit in a
/// cloud folder where anyone who gets it can guess offline. 2^18 is about a second
/// and 256 MiB per guess on a current laptop.
const DEFAULT_WORK_FACTOR: u8 = 18;

/// Highest scrypt work factor accepted when decrypting. age's own default derives
/// it from a benchmark of the CURRENT machine, so a vault written on a fast desktop
/// could be refused on a slow laptop — exactly the machine you recover on.
const MAX_WORK_FACTOR: u8 = 26;

/// One stored secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultEntry {
    pub name: String,
    pub value: String,
    /// Free text: what it is, which servers accept it, where else a copy lives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl VaultEntry {
    /// Short sha256 of the value: lets you compare two copies, or a vault entry
    /// with the file it came from, without printing the secret.
    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(self.value.as_bytes());
        digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
    }
}

#[derive(Serialize, Deserialize)]
struct VaultFile {
    format: String,
    version: u32,
    entries: Vec<VaultEntry>,
}

/// The decrypted vault, keyed by entry name.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Vault {
    entries: BTreeMap<String, VaultEntry>,
}

impl Vault {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace `name`. Returns true when an existing entry was replaced
    /// (its `created_at` is kept).
    pub fn put(&mut self, name: &str, value: String, note: Option<String>) -> Result<bool> {
        let name = name.trim();
        if name.is_empty() || name.chars().any(char::is_whitespace) {
            return Err(Error::Invalid(format!(
                "vault entry name must be non-empty with no spaces, got {name:?}"
            )));
        }
        let now = Utc::now().to_rfc3339();
        let created_at = self
            .entries
            .get(name)
            .map(|e| e.created_at.clone())
            .unwrap_or_else(|| now.clone());
        let replaced = self.entries.contains_key(name);
        self.entries.insert(
            name.to_string(),
            VaultEntry {
                name: name.to_string(),
                value,
                note,
                created_at,
                updated_at: now,
            },
        );
        Ok(replaced)
    }

    pub fn get(&self, name: &str) -> Option<&VaultEntry> {
        self.entries.get(name)
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.entries.remove(name).is_some()
    }

    pub fn entries(&self) -> impl Iterator<Item = &VaultEntry> {
        self.entries.values()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Encrypt to ASCII-armored age text. `work_factor` is scrypt's log2(N);
    /// `None` means [`DEFAULT_WORK_FACTOR`] (tests pass a small one to stay fast).
    pub fn encrypt(&self, passphrase: &str, work_factor: Option<u8>) -> Result<String> {
        let plain = serde_json::to_vec_pretty(&VaultFile {
            format: FORMAT.to_string(),
            version: VERSION,
            entries: self.entries.values().cloned().collect(),
        })?;

        let mut recipient = age::scrypt::Recipient::new(SecretString::from(passphrase.to_owned()));
        recipient.set_work_factor(work_factor.unwrap_or(DEFAULT_WORK_FACTOR));
        let encryptor = age::Encryptor::with_recipients(std::iter::once(&recipient as _))
            .map_err(|e| Error::Other(format!("vault encrypt: {e}")))?;

        let mut out = Vec::with_capacity(plain.len() * 2);
        let armor = ArmoredWriter::wrap_output(&mut out, Format::AsciiArmor)?;
        let mut writer = encryptor
            .wrap_output(armor)
            .map_err(|e| Error::Other(format!("vault encrypt: {e}")))?;
        writer.write_all(&plain)?;
        writer.finish()?.finish()?;
        String::from_utf8(out).map_err(|e| Error::Other(format!("vault encrypt: {e}")))
    }

    /// Decrypt an age file (armored or binary). A wrong passphrase and a file that
    /// is not an iCode vault are both reported as [`Error::Invalid`].
    pub fn decrypt(ciphertext: &[u8], passphrase: &str) -> Result<Self> {
        let decryptor = age::Decryptor::new_buffered(ArmoredReader::new(ciphertext))
            .map_err(|e| Error::Invalid(format!("not an age file: {e}")))?;
        if !decryptor.is_scrypt() {
            return Err(Error::Invalid(
                "this age file is not passphrase-encrypted, so it is not an iCode vault".into(),
            ));
        }
        let mut identity = age::scrypt::Identity::new(SecretString::from(passphrase.to_owned()));
        identity.set_max_work_factor(MAX_WORK_FACTOR);

        let mut reader = decryptor
            .decrypt(std::iter::once(&identity as _))
            .map_err(|e| match e {
                age::DecryptError::NoMatchingKeys | age::DecryptError::DecryptionFailed => {
                    Error::Invalid("wrong passphrase".into())
                }
                other => Error::Invalid(format!("cannot decrypt vault: {other}")),
            })?;
        let mut plain = Vec::new();
        reader.read_to_end(&mut plain)?;

        let file: VaultFile = serde_json::from_slice(&plain)
            .map_err(|e| Error::Invalid(format!("decrypted, but not an iCode vault: {e}")))?;
        if file.format != FORMAT {
            return Err(Error::Invalid(format!(
                "decrypted, but format is {:?}, not {FORMAT:?}",
                file.format
            )));
        }
        if file.version > VERSION {
            return Err(Error::Invalid(format!(
                "vault version {} is newer than this iCode understands ({VERSION})",
                file.version
            )));
        }
        Ok(Self {
            entries: file.entries.into_iter().map(|e| (e.name.clone(), e)).collect(),
        })
    }

    /// Read and decrypt the vault at `path`.
    pub fn open(path: &Path, passphrase: &str) -> Result<Self> {
        let bytes = std::fs::read(path)
            .map_err(|e| Error::Io(format!("cannot read vault {}: {e}", path.display())))?;
        Self::decrypt(&bytes, passphrase)
    }

    /// Encrypt and write to `path` without ever leaving a half-written vault: the
    /// new text goes to a sibling temp file (mode 0600) that is fsynced, the
    /// previous vault is kept as `<path>.bak`, then the temp file is renamed over.
    /// The result is decrypted again before the rename, so a vault that cannot be
    /// read back never replaces one that can.
    pub fn save(&self, path: &Path, passphrase: &str, work_factor: Option<u8>) -> Result<()> {
        let text = self.encrypt(passphrase, work_factor)?;
        if Self::decrypt(text.as_bytes(), passphrase)? != *self {
            return Err(Error::Other("vault did not round-trip; nothing written".into()));
        }

        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = sibling(path, "tmp");
        {
            let mut f = private_file(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        if path.exists() {
            std::fs::copy(path, sibling(path, "bak"))?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// `<path>.<ext>` next to `path` (`vault.age` → `vault.age.bak`).
fn sibling(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Create (truncating) a file readable by the owner only.
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap scrypt so the suite stays fast; production uses DEFAULT_WORK_FACTOR.
    const FAST: Option<u8> = Some(4);

    fn sample() -> Vault {
        let mut v = Vault::new();
        v.put(
            "srv-backup-key",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n".into(),
            Some("break-glass key for the prod hosts".into()),
        )
        .unwrap();
        v.put("root-pw", "Xk9-mQ2!zzLp".into(), None).unwrap();
        v
    }

    #[test]
    fn round_trips_through_armored_age_text() {
        let v = sample();
        let text = v.encrypt("correct horse battery", FAST).unwrap();
        assert!(text.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"));
        assert!(!text.contains("Xk9-mQ2"), "the value must not appear in the file");
        assert_eq!(Vault::decrypt(text.as_bytes(), "correct horse battery").unwrap(), v);
    }

    #[test]
    fn wrong_passphrase_is_a_clear_error() {
        let text = sample().encrypt("correct horse battery", FAST).unwrap();
        let err = Vault::decrypt(text.as_bytes(), "wrong").unwrap_err();
        assert!(err.to_string().contains("wrong passphrase"), "{err}");
    }

    #[test]
    fn a_foreign_age_file_is_not_taken_for_a_vault() {
        let mut r = age::scrypt::Recipient::new(SecretString::from("pw".to_owned()));
        r.set_work_factor(4);
        let other = age::encrypt(&r, b"{\"hello\":1}").unwrap();
        let err = Vault::decrypt(&other, "pw").unwrap_err();
        assert!(err.to_string().contains("not an iCode vault"), "{err}");
    }

    #[test]
    fn put_keeps_created_at_and_rejects_bad_names() {
        let mut v = sample();
        let created = v.get("root-pw").unwrap().created_at.clone();
        assert!(v.put("root-pw", "new-value-1".into(), None).unwrap());
        assert_eq!(v.get("root-pw").unwrap().created_at, created);
        assert!(v.put("has space", "x".into(), None).is_err());
        assert!(v.put("  ", "x".into(), None).is_err());
    }

    #[test]
    fn save_is_private_keeps_a_backup_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/vault.age");
        let mut v = sample();
        v.save(&path, "pw-one-two-three", FAST).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(!sibling(&path, "bak").exists(), "first save has nothing to back up");

        v.remove("root-pw");
        v.save(&path, "pw-one-two-three", FAST).unwrap();
        assert_eq!(Vault::open(&path, "pw-one-two-three").unwrap(), v);
        let previous = Vault::open(&sibling(&path, "bak"), "pw-one-two-three").unwrap();
        assert!(previous.get("root-pw").is_some(), ".bak holds the version before");
        assert!(!sibling(&path, "tmp").exists());
    }

    #[test]
    fn fingerprint_identifies_without_revealing() {
        let v = sample();
        let e = v.get("root-pw").unwrap();
        assert_eq!(e.fingerprint().len(), 12);
        assert!(!e.fingerprint().contains("Xk9"));
    }
}
