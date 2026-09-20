//! Best-effort token accounting. opencode doesn't reliably print token counts
//! in its run output, but it persists each message (with a `tokens` object)
//! under its data directory: current versions in the `message` table of
//! `opencode.db`, older ones as JSON files under `storage/`. Each turn runs
//! in a fresh session, so summing the token objects recorded since the turn
//! started attributes usage to the turn. Anything unexpected degrades to
//! None rather than failing the turn.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    let data = opencode_data_dir()?;
    from_database(&data.join("opencode.db"), since)
        .or_else(|| from_files(&data.join("storage"), since))
}

/// Sum the `tokens` objects of the messages recorded in opencode's database
/// since `since`; its `time_created` column is epoch milliseconds and `data`
/// holds the message JSON.
fn from_database(path: &Path, since: SystemTime) -> Option<TokenUsage> {
    let since_ms = since.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64;
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let mut query = db
        .prepare("SELECT data FROM message WHERE time_created >= ?1")
        .ok()?;
    let rows = query
        .query_map([since_ms], |row| row.get::<_, String>(0))
        .ok()?;
    let mut sum = TokenUsage::default();
    let mut found = false;
    for data in rows.flatten() {
        let Ok(value) = serde_json::from_str(&data) else {
            continue;
        };
        if let Some(tokens) = tokens_of(&value) {
            sum.add(&tokens);
            found = true;
        }
    }
    found.then_some(sum)
}

/// The pre-database layout: per-message JSON files under `storage/`, summed
/// from the files modified since the turn started.
fn from_files(storage: &Path, since: SystemTime) -> Option<TokenUsage> {
    let mut sum = TokenUsage::default();
    let mut found = false;
    let mut budget = MAX_FILES;
    let mut dirs = vec![(storage.to_path_buf(), 0usize)];
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
                // A directory untouched since the turn started gained no
                // entries during it; skipping it keeps the scan proportional
                // to the turn instead of the whole history.
                let touched = entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .is_ok_and(|modified| modified >= since);
                if touched && depth < MAX_DEPTH {
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

    /// The message-table shape opencode's sqlite migration produces.
    #[test]
    fn database_sums_only_messages_since_the_turn_started() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE message (
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .unwrap();
        let message = |tokens: serde_json::Value| {
            serde_json::json!({ "id": "msg", "role": "assistant", "tokens": tokens }).to_string()
        };
        let insert = |id: &str, at: i64, data: &str| {
            db.execute(
                "INSERT INTO message VALUES (?1, 'ses', ?2, ?2, ?3)",
                rusqlite::params![id, at, data],
            )
            .unwrap();
        };
        let since_ms = 1_700_000_000_000i64;
        let earlier = message(serde_json::json!({ "input": 999, "output": 999 }));
        insert("old", since_ms - 5_000, &earlier);
        let first = message(serde_json::json!({
            "input": 100, "output": 20, "cache": { "read": 4000, "write": 300 }
        }));
        insert("new1", since_ms + 1_000, &first);
        let second = message(serde_json::json!({ "input": 10, "output": 5, "reasoning": 7 }));
        insert("new2", since_ms + 2_000, &second);
        insert("tool", since_ms + 3_000, r#"{ "id": "msg", "role": "user" }"#);
        drop(db);

        let since = UNIX_EPOCH + std::time::Duration::from_millis(since_ms as u64);
        let sum = from_database(&path, since).unwrap();
        assert_eq!(sum.input, 110);
        assert_eq!(sum.output, 25);
        assert_eq!(sum.reasoning, 7);
        assert_eq!(sum.cache_read, 4000);
        assert_eq!(sum.cache_write, 300);

        // Nothing recent, or no database at all: fall through to None.
        let late = UNIX_EPOCH + std::time::Duration::from_millis(since_ms as u64 + 60_000);
        assert!(from_database(&path, late).is_none());
        assert!(from_database(&dir.path().join("missing.db"), since).is_none());
    }

    #[test]
    fn file_scan_sums_only_recent_files() {
        let dir = tempfile::tempdir().unwrap();
        let old_session = dir.path().join("message/ses_old");
        std::fs::create_dir_all(&old_session).unwrap();
        std::fs::write(
            old_session.join("msg.json"),
            r#"{ "tokens": { "input": 999, "output": 999 } }"#,
        )
        .unwrap();
        // Let the old tree's mtimes land strictly before `since`.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let since = SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let new_session = dir.path().join("message/ses_new");
        std::fs::create_dir_all(&new_session).unwrap();
        std::fs::write(
            new_session.join("msg.json"),
            r#"{ "tokens": { "input": 40, "output": 2 } }"#,
        )
        .unwrap();

        let sum = from_files(dir.path(), since).unwrap();
        assert_eq!(sum.input, 40);
        assert_eq!(sum.output, 2);
        assert!(from_files(dir.path(), SystemTime::now()).is_none());
        assert!(from_files(&dir.path().join("missing"), since).is_none());
    }
}
