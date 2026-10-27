//! Secret redaction for everything that enters the memory store.
//!
//! Memory is the wrong place for a secret: every record can come back through
//! `recall` / `session start` into a model's context (and from there into a
//! provider's API and a transcript on disk), and `~/.icode/icode.db` is a plain,
//! unencrypted SQLite file. The Stop hook makes it worse — it writes an automatic
//! session summary nobody reviews, so a token pasted into a conversation would
//! land in the db by itself.
//!
//! [`redact_secrets`] runs on every write (`add`, `update`, the WAL preview) and
//! replaces a recognised secret with `[REDACTED:<kind>]`. It REDACTS rather than
//! rejects: the rest of the record is still worth keeping, and a rejected Stop-hook
//! summary would simply be lost.
//!
//! Only shapes with a distinctive structure are matched (PEM blocks, vendor token
//! prefixes, JWTs, URL credentials, `password=…` assignments with a non-trivial
//! value). There is deliberately NO generic high-entropy rule: memories are full
//! of commit SHAs, ULIDs and content hashes, and a rule that eats those would
//! destroy the records it is meant to protect. A secret with no recognisable shape
//! therefore gets through — this is a seatbelt, not a guarantee.

use std::sync::LazyLock;

use regex::{Captures, Regex};

/// One recognised secret shape: a kind label and its pattern. When the pattern has
/// a capture group named `v`, only that group is replaced (the key name of an
/// assignment stays readable); otherwise the whole match is.
struct Rule {
    kind: &'static str,
    re: Regex,
}

fn rule(kind: &'static str, pattern: &str) -> Rule {
    Rule {
        kind,
        re: Regex::new(pattern).expect("static secret pattern compiles"),
    }
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        // PEM / OpenSSH private keys. An END line is optional: a key truncated by
        // a summary is still a key, so an unterminated block runs to the end.
        rule(
            "private-key",
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----(?s:.*?)(?:-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----|\z)",
        ),
        rule("age-secret-key", r"AGE-SECRET-KEY-1[0-9A-Z]{50,}"),
        rule("github-token", r"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,})"),
        rule("anthropic-key", r"\bsk-ant-[A-Za-z0-9_\-]{20,}"),
        rule("openai-key", r"\bsk-(?:proj-|svcacct-)?[A-Za-z0-9_\-]{32,}"),
        rule("aws-access-key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
        rule("slack-token", r"\bxox[abprs]-[A-Za-z0-9\-]{10,}"),
        rule("google-api-key", r"\bAIza[0-9A-Za-z_\-]{35}"),
        rule("stripe-key", r"\b[rs]k_(?:live|test)_[A-Za-z0-9]{20,}"),
        rule(
            "jwt",
            r"\beyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
        ),
        // scheme://user:PASSWORD@host — only the password part goes.
        rule(
            "url-password",
            r"\b[a-zA-Z][a-zA-Z0-9+.\-]*://[^/\s:@]+:(?P<v>[^/\s@]{3,})@",
        ),
        // `password = hunter2x`, `API_KEY: "…"`, `пароль: …`. The value must look
        // like a credential (see `credential_like`), so `token: String` or
        // `api_key = cfg.api_key` in a code memory is left alone.
        rule(
            "assigned-secret",
            r#"(?i)\b(?:password|passwd|pwd|passphrase|secret|token|api[_\-]?key|access[_\-]?key|private[_\-]?key|client[_\-]?secret|пароль)\b["']?\s*[:=]\s*["']?(?P<v>[^\s"'`,;]{8,})"#,
        ),
    ]
});

/// What [`redact_secrets`] found: the cleaned text and the kinds it replaced (one
/// entry per replacement, in rule order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    pub text: String,
    pub kinds: Vec<&'static str>,
}

impl Redaction {
    /// True when nothing was replaced (`text` equals the input).
    pub fn is_clean(&self) -> bool {
        self.kinds.is_empty()
    }
}

/// A value assigned to a secret-sounding key counts as a credential only when it
/// mixes character classes the way generated secrets do: a letter AND a digit, or
/// a letter AND a symbol. It must not be a call, a generic type, an index, an
/// object literal or a field path (`read_token()`, `Option<String>`,
/// `os.environ["X"]`, `{required: true}`, `cfg.api_key`), which code memories are
/// full of. `[` also keeps an already-redacted value from being redacted again.
fn credential_like(v: &str) -> bool {
    if v.contains(['(', '<', '[', '{']) || v.starts_with('$') {
        return false;
    }
    let dotted_path = v.contains('.')
        && v.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_alphanumeric() || c == '_'));
    if dotted_path && !v.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }
    let letter = v.chars().any(|c| c.is_alphabetic());
    let digit = v.chars().any(|c| c.is_ascii_digit());
    let symbol = v.chars().any(|c| !c.is_alphanumeric() && c != '_');
    letter && (digit || symbol)
}

/// Replace every recognised secret in `text` with `[REDACTED:<kind>]`.
pub fn redact_secrets(text: &str) -> Redaction {
    let mut out = text.to_string();
    let mut kinds = Vec::new();
    for r in RULES.iter() {
        if !r.re.is_match(&out) {
            continue;
        }
        let mut hits = 0usize;
        let replaced = r.re.replace_all(&out, |c: &Captures| {
            let whole = c.get(0).expect("group 0 always present");
            match c.name("v") {
                Some(v) => {
                    if r.kind == "assigned-secret" && !credential_like(v.as_str()) {
                        return whole.as_str().to_string();
                    }
                    hits += 1;
                    let start = v.start() - whole.start();
                    let end = v.end() - whole.start();
                    format!(
                        "{}[REDACTED:{}]{}",
                        &whole.as_str()[..start],
                        r.kind,
                        &whole.as_str()[end..]
                    )
                }
                None => {
                    hits += 1;
                    format!("[REDACTED:{}]", r.kind)
                }
            }
        });
        if hits > 0 {
            out = replaced.into_owned();
            kinds.extend(std::iter::repeat_n(r.kind, hits));
        }
    }
    Redaction { text: out, kinds }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<&'static str> {
        redact_secrets(text).kinds
    }

    #[test]
    fn openssh_private_key_block_is_removed_whole() {
        let text = "backup key:\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\nQUFBQUFBQUFB\n-----END OPENSSH PRIVATE KEY-----\nstored offline";
        let r = redact_secrets(text);
        assert_eq!(r.kinds, vec!["private-key"]);
        assert_eq!(r.text, "backup key:\n[REDACTED:private-key]\nstored offline");
    }

    #[test]
    fn truncated_private_key_still_goes() {
        let r = redact_secrets("-----BEGIN RSA PRIVATE KEY-----\nMIIEow…");
        assert_eq!(r.text, "[REDACTED:private-key]");
    }

    #[test]
    fn public_key_is_not_a_secret() {
        let text = "-----BEGIN PUBLIC KEY-----\nMFkwEwYH\n-----END PUBLIC KEY-----\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMq user@host";
        assert!(redact_secrets(text).is_clean());
    }

    #[test]
    fn vendor_tokens_are_recognised() {
        assert_eq!(kinds("use ghp_abcdefghijklmnopqrstuvwxyz0123456789 here"), vec!["github-token"]);
        assert_eq!(kinds("key sk-ant-api03-abcdefghijklmnopqrstuvwx"), vec!["anthropic-key"]);
        assert_eq!(kinds("AKIAIOSFODNN7EXAMPLE"), vec!["aws-access-key"]);
        assert_eq!(kinds("xoxb-1234567890-abcdefghij"), vec!["slack-token"]);
        assert_eq!(
            kinds("AGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ"),
            vec!["age-secret-key"]
        );
        assert_eq!(
            kinds("Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"),
            vec!["jwt"]
        );
    }

    #[test]
    fn url_credentials_lose_only_the_password() {
        let r = redact_secrets("db at postgres://app:s3cr3tPass@10.0.0.5:5432/main");
        assert_eq!(r.text, "db at postgres://app:[REDACTED:url-password]@10.0.0.5:5432/main");
    }

    #[test]
    fn assignments_lose_only_the_value() {
        let r = redact_secrets("prod: PASSWORD=Xk9-mQ2!zzLp, user=root");
        assert_eq!(r.text, "prod: PASSWORD=[REDACTED:assigned-secret], user=root");
        let r = redact_secrets("пароль: Hunter2Hunter2");
        assert_eq!(r.text, "пароль: [REDACTED:assigned-secret]");
    }

    #[test]
    fn code_shaped_assignments_are_left_alone() {
        for text in [
            "fn parse(token: String) -> Result<()>",
            "let api_key = cfg.api_key;",
            "token = read_token(path)",
            "password: Option<SecretString>",
            "secret = $DB_PASSWORD",
            "token=os.environ[\"API_TOKEN\"]",
            "rules: password: {required: true, min: 8}",
        ] {
            assert!(redact_secrets(text).is_clean(), "false positive on {text:?}");
        }
    }

    #[test]
    fn hashes_and_ids_survive() {
        let text = "commit 0f11ec4a9b8c7d6e5f4a3b2c1d0e9f8a7b6c5d4e, id 01J9ZK3Q4R5S6T7V8W9XAYBZC0, sha256 e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(redact_secrets(text).is_clean());
    }

    #[test]
    fn redaction_is_idempotent() {
        let once = redact_secrets("PASSWORD=Xk9-mQ2!zzLp and ghp_abcdefghijklmnopqrstuvwxyz0123456789");
        let twice = redact_secrets(&once.text);
        assert!(twice.is_clean());
        assert_eq!(twice.text, once.text);
    }
}
