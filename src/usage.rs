//! Best-effort token accounting. opencode doesn't reliably print token counts
//! in its run output, but it persists per-message JSON (with a `tokens`
//! object) under its data directory. Each turn runs in a fresh session, so
//! summing the token objects from files modified since the turn started
//! attributes usage to the turn without depending on the storage layout.
//! Anything unexpected degrades to None rather than failing the turn.

use std::path::PathBuf;
use std::time::SystemTime;

use crate::status::TokenUsage;

const MAX_DEPTH: usize = 8;
const MAX_FILES: usize = 10_000;

/// opencode's data dir: `$XDG_DATA_HOME/opencode`, else
/// `$HOME/.local/share/opencode`.
fn opencode_data_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        return Some(PathBuf::from(dir).join("opencode"));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".local/share/opencode"))
}

/// Sum the token counts recorded since `since`, or None if none were found.
pub fn collect_since(since: SystemTime) -> Option<TokenUsage> {
    let storage = opencode_data_dir()?.join("storage");
    let mut sum = TokenUsage::default();
    let mut found = false;
    let mut budget = MAX_FILES;
    let mut dirs = vec![(storage, 0usize)];
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let path = entry.path();
            if path.is_dir() {
                if depth < MAX_DEPTH {
                    dirs.push((path, depth + 1));
                }
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let recent = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| modified >= since);
            if !recent {
                continue;
            }
            let Some(value) = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            else {
                continue;
            };
            if let Some(tokens) = tokens_of(&value) {
                sum.add(&tokens);
                found = true;
            }
        }
    }
    found.then_some(sum)
}

/// The `tokens` object of one opencode message, if present.
fn tokens_of(value: &serde_json::Value) -> Option<TokenUsage> {
    let tokens = value.get("tokens")?.as_object()?;
    let count =
        |value: Option<&serde_json::Value>| value.and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
    Some(TokenUsage {
        input: count(tokens.get("input")),
        output: count(tokens.get("output")),
        reasoning: count(tokens.get("reasoning")),
        cache_read: count(tokens.get("cache").and_then(|c| c.get("read"))),
        cache_write: count(tokens.get("cache").and_then(|c| c.get("write"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_of_reads_message_json() {
        let value = serde_json::json!({
            "id": "msg_123",
            "role": "assistant",
            "tokens": {
                "input": 1200, "output": 340, "reasoning": 0,
                "cache": { "read": 56000, "write": 1800 }
            }
        });
        let tokens = tokens_of(&value).unwrap();
        assert_eq!(tokens.input, 1200);
        assert_eq!(tokens.output, 340);
        assert_eq!(tokens.cache_read, 56000);
        assert_eq!(tokens.cache_write, 1800);
        assert_eq!(tokens.reasoning, 0);
    }

    #[test]
    fn tokens_of_tolerates_missing_fields() {
        let value = serde_json::json!({ "tokens": { "input": 5, "output": 2 } });
        let tokens = tokens_of(&value).unwrap();
        assert_eq!(tokens.input, 5);
        assert_eq!(tokens.cache_read, 0);
    }

    #[test]
    fn tokens_of_rejects_other_json() {
        assert!(tokens_of(&serde_json::json!({ "id": "ses_1" })).is_none());
        assert!(tokens_of(&serde_json::json!({ "tokens": 7 })).is_none());
    }
}
