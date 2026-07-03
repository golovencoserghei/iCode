//! Stop-hook safety net: keep ONE auto-summary memory per session so knowledge
//! survives even when the agent never calls `session_end`.
//!
//! Claude Code fires the `Stop` hook at the end of EVERY assistant turn, not
//! once per session. The design accounts for that:
//! - the summary is UPSERTED by session id (tag `sid:<8>`), so repeated firings
//!   refresh one record instead of spamming near-duplicates;
//! - when the transcript shows an explicit `session_end` tool call, the draft is
//!   deleted — a properly closed session leaves no auto residue.
//!
//! Everything here is pure/deterministic except [`upsert_auto_summary`] /
//! [`remove_auto_summary`], which take the store as `&dyn` so the bin decides
//! how (and whether) to build an embedder.

use icode_core::model::{Category, NewMemory};
use icode_core::traits::MemoryStore;
use icode_core::{MemoryId, Result};

/// The payload Claude Code pipes to a Stop hook on stdin. Only the fields the
/// hook acts on; unknown fields are ignored.
#[derive(Debug, serde::Deserialize)]
pub struct StopHookInput {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub transcript_path: String,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Parse the Stop-hook stdin JSON. `None` on malformed input or when either of
/// the two load-bearing fields is missing — the hook then exits silently.
pub fn parse_stop_input(json: &str) -> Option<StopHookInput> {
    let input: StopHookInput = serde_json::from_str(json).ok()?;
    if input.session_id.trim().is_empty() || input.transcript_path.trim().is_empty() {
        return None;
    }
    Some(input)
}

/// What the transcript scan found: whether the agent explicitly closed the
/// session, and the material for a fallback summary if it did not.
#[derive(Debug, Default, PartialEq)]
pub struct TranscriptDigest {
    /// A `session_end` MCP tool call appeared anywhere in the transcript.
    pub session_end_called: bool,
    /// The last real user request (harness injections filtered out).
    pub last_user: Option<String>,
    /// The last substantial assistant text — usually the wrap-up summary.
    pub last_assistant: Option<String>,
}

/// A user text that is harness plumbing rather than a human request: injected
/// continuation summaries, system reminders/notifications, interrupt markers.
fn is_noise_user_text(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with('<')
        || t.starts_with("[Request interrupted")
        || t.starts_with("Caveat:")
        || {
            let lower = t.to_lowercase();
            lower.starts_with("this session is being continued")
                || lower.starts_with("эта сессия продолжает")
        }
}

/// Minimum length for an assistant text to count as "substantial".
const MIN_ASSISTANT_TEXT: usize = 80;
/// Minimum length for a user text to count as a real request.
const MIN_USER_TEXT: usize = 8;

/// Scan a transcript (Claude Code session JSONL) line by line. Unparseable
/// lines and unknown shapes are skipped — the digest is best-effort by design.
pub fn digest_transcript(jsonl: &str) -> TranscriptDigest {
    let mut digest = TranscriptDigest::default();
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
            continue;
        }
        let role = v.get("type").and_then(|t| t.as_str());
        let content = v.get("message").and_then(|m| m.get("content"));
        match (role, content) {
            (Some("user"), Some(content)) => {
                if let Some(text) = first_text_block(content) {
                    let text = text.trim();
                    if text.chars().count() >= MIN_USER_TEXT && !is_noise_user_text(text) {
                        digest.last_user = Some(text.to_string());
                    }
                }
            }
            (Some("assistant"), Some(content)) => {
                if let Some(items) = content.as_array() {
                    for item in items {
                        match item.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                    if text.trim().chars().count() >= MIN_ASSISTANT_TEXT {
                                        digest.last_assistant = Some(text.trim().to_string());
                                    }
                                }
                            }
                            Some("tool_use") => {
                                if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                                    if name.ends_with("session_end") {
                                        digest.session_end_called = true;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    digest
}

/// A user-message `content` is either a plain string or an array of blocks;
/// return its first `text` block (tool_result-only messages yield `None`).
fn first_text_block(content: &serde_json::Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    content.as_array()?.iter().find_map(|item| {
        (item.get("type").and_then(|t| t.as_str()) == Some("text"))
            .then(|| item.get("text").and_then(|t| t.as_str()).map(str::to_string))
            .flatten()
    })
}

/// Caps keeping the auto-summary within the "~200 words per memory" guidance.
const USER_CAP: usize = 220;
const ASSISTANT_CAP: usize = 900;

/// Build the auto-summary content, or `None` when the transcript held nothing
/// substantial (no assistant text → nothing worth saving).
pub fn auto_summary_content(digest: &TranscriptDigest) -> Option<String> {
    let assistant = digest.last_assistant.as_deref()?;
    let mut out = String::from("Авто-сводка сессии (Stop-хук; session_end не вызывался). ");
    if let Some(user) = digest.last_user.as_deref() {
        out.push_str(&format!("Задача: «{}». ", cap_chars(user, USER_CAP)));
    }
    out.push_str(&format!("Итог: {}", cap_chars(assistant, ASSISTANT_CAP)));
    Some(out)
}

/// Char-boundary-safe cap with an ellipsis, whitespace collapsed to one line.
fn cap_chars(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max).collect();
    out.push('…');
    out
}

/// Tag marking a Stop-hook auto-summary record.
pub const AUTO_TAG: &str = "auto-session";

/// The per-session tag (`sid:<first 8 chars>`), the upsert key.
pub fn sid_tag(session_id: &str) -> String {
    format!("sid:{}", session_id.chars().take(8).collect::<String>())
}

/// Find this session's existing auto-summary record in `project`, if any.
fn find_auto_summary(
    store: &dyn MemoryStore,
    project: &str,
    session_id: &str,
) -> Result<Option<MemoryId>> {
    let sid = sid_tag(session_id);
    let recent = store.list(project, None, 200, false)?;
    Ok(recent
        .into_iter()
        .find(|r| {
            r.tags.iter().any(|t| t == &sid) && r.tags.iter().any(|t| t == AUTO_TAG)
        })
        .map(|r| r.id))
}

/// Create or refresh the session's single auto-summary record. Requires a
/// store with a live embedder (both `add` and a content `update` embed).
pub fn upsert_auto_summary(
    store: &dyn MemoryStore,
    project: &str,
    session_id: &str,
    content: &str,
) -> Result<()> {
    match find_auto_summary(store, project, session_id)? {
        Some(id) => store.update(&id, Some(content), None),
        None => {
            // A dedup-gate `Duplicate` outcome is a fine no-op here.
            store
                .add(NewMemory {
                    project: project.to_string(),
                    content: content.to_string(),
                    category: Category::General,
                    tags: vec![AUTO_TAG.to_string(), sid_tag(session_id)],
                    importance: 1.0,
                    session_id: Some(session_id.to_string()),
                })
                .map(|_| ())
        }
    }
}

/// Delete the session's auto-summary draft (the agent called `session_end`, so
/// the proper summary supersedes it). Works WITHOUT an embedder.
pub fn remove_auto_summary(
    store: &dyn MemoryStore,
    project: &str,
    session_id: &str,
) -> Result<()> {
    if let Some(id) = find_auto_summary(store, project, session_id)? {
        store.delete(&id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_user(text: &str) -> String {
        serde_json::json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":text}]}})
            .to_string()
    }
    fn line_assistant_text(text: &str) -> String {
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}})
            .to_string()
    }
    fn line_assistant_tool(name: &str) -> String {
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":name,"input":{}}]}})
            .to_string()
    }

    const LONG: &str = "Починил парсер конфига: parse_config теперь трактует пустой файл как пустой словарь настроек вместо падения с TypeError; добавлен тест на пустой ввод.";

    #[test]
    fn parse_stop_input_requires_fields() {
        assert!(parse_stop_input("not json").is_none());
        assert!(parse_stop_input(r#"{"session_id":"abc"}"#).is_none());
        let ok = parse_stop_input(
            r#"{"session_id":"abc12345-x","transcript_path":"/tmp/t.jsonl","cwd":"/p/x"}"#,
        )
        .unwrap();
        assert_eq!(ok.session_id, "abc12345-x");
        assert_eq!(ok.cwd.as_deref(), Some("/p/x"));
    }

    #[test]
    fn digest_finds_last_task_and_summary() {
        let jsonl = [
            line_user("почини баг с пустым конфигом"),
            line_assistant_text(LONG),
            // tool_result-only user message must not become the "last user" text
            serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}).to_string(),
        ]
        .join("\n");
        let d = digest_transcript(&jsonl);
        assert!(!d.session_end_called);
        assert_eq!(d.last_user.as_deref(), Some("почини баг с пустым конфигом"));
        assert_eq!(d.last_assistant.as_deref(), Some(LONG));
    }

    #[test]
    fn digest_detects_session_end_and_filters_noise() {
        let jsonl = [
            line_user("<system-reminder>инъекция</system-reminder>"),
            line_user("Caveat: injected by the harness"),
            line_user("This session is being continued from a previous one"),
            line_assistant_tool("mcp__icode__session_end"),
        ]
        .join("\n");
        let d = digest_transcript(&jsonl);
        assert!(d.session_end_called);
        assert_eq!(d.last_user, None);
        // garbage lines are skipped, not fatal
        let d2 = digest_transcript("{{{\nnot json\n");
        assert_eq!(d2, TranscriptDigest::default());
    }

    #[test]
    fn auto_summary_needs_assistant_text() {
        let mut d = TranscriptDigest {
            last_user: Some("сделай X".into()),
            ..Default::default()
        };
        assert!(auto_summary_content(&d).is_none());
        d.last_assistant = Some(LONG.into());
        let content = auto_summary_content(&d).unwrap();
        assert!(content.contains("Задача: «сделай X»"));
        assert!(content.contains("Итог: Починил парсер"));
    }

    #[test]
    fn caps_are_char_safe() {
        let d = TranscriptDigest {
            session_end_called: false,
            last_user: Some("я".repeat(500)),
            last_assistant: Some("ю".repeat(2000)),
        };
        let content = auto_summary_content(&d).unwrap();
        assert!(content.chars().count() < USER_CAP + ASSISTANT_CAP + 120);
        assert!(content.contains('…'));
    }

    #[test]
    fn sid_tag_is_stable_prefix() {
        assert_eq!(sid_tag("9a896bc9-7184-43d2"), "sid:9a896bc9");
        assert_eq!(sid_tag("ab"), "sid:ab");
    }
}
