//! Best-effort token accounting. opencode doesn't reliably print token counts
//! in its run output, but it persists each message (with a `tokens` object)
//! under its data directory: current versions in the `message` table of
//! `opencode.db`, older ones as JSON files under `storage/`. Each turn runs
//! in a fresh session, so summing the token objects recorded since the turn
//! started attributes usage to the turn. Anything unexpected degrades to
//! None rather than failing the turn.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::status::TokenUsage;

const MAX_DEPTH: usize = 8;
const MAX_FILES: usize = 10_000;

/// How far past the turn's start the backwards walk keeps reading before it
/// accepts that every remaining row is older still. The walk relies on the
/// `message` table being append-only — a row's rowid is assigned when the
/// message is first recorded, so rowids and `time_created` ascend together —
/// and this is the slack in that: a wall clock stepping backwards mid-turn
/// (an NTP correction, say) is the one thing that can put an older stamp on
/// a later row, and anything larger than this would have moved the turn's
/// own `since` too.
const CLOCK_SLACK: Duration = Duration::from_secs(5 * 60);

/// opencode's data dir: `$XDG_DATA_HOME/opencode`, else
/// `$HOME/.local/share/opencode`.
fn opencode_data_dir() -> PathBuf {
    crate::config::xdg_dir("XDG_DATA_HOME", ".local/share").join("opencode")
}

/// Sum the token counts recorded since `since`, or None if none were found.
pub fn collect_since(since: SystemTime) -> Option<TokenUsage> {
    let data = opencode_data_dir();
    databases(&data)
        .into_iter()
        .find_map(|db| from_database(&db, since))
        .or_else(|| from_files(&data.join("storage"), since))
}

/// The database files opencode may be writing: `opencode.db` historically,
/// `opencode-<channel>.db` (`opencode-stable.db` and friends) since 1.15.
fn databases(data: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(data)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("opencode") && name.ends_with(".db"))
        })
        .collect();
    found.sort();
    found
}

/// Sum the `tokens` objects of the messages recorded in opencode's database
/// since `since`; its `time_created` column is epoch milliseconds and `data`
/// holds the message JSON.
///
/// Read by walking the table backwards and stopping at the first row older
/// than the window rather than by asking for `time_created >= since`.
/// opencode indexes `message` by id alone, so the filtered query is a scan
/// of every message it ever recorded — and this is sampled every few seconds
/// for the whole of a turn, so the cost of reporting one turn's tokens grew
/// with the entire history behind it. Walking back from the newest row costs
/// the turn's own messages plus `CLOCK_SLACK`, whatever the table's size.
fn from_database(path: &Path, since: SystemTime) -> Option<TokenUsage> {
    let since_ms = since.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64;
    let stop_ms = since_ms - CLOCK_SLACK.as_millis() as i64;
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    // Descending rowid is a reverse walk of the table's own b-tree, not a
    // sort: rowid *is* the key, so nothing is materialized and the walk can
    // be abandoned as soon as it reads past the window.
    let mut query = db
        .prepare("SELECT time_created, data FROM message ORDER BY rowid DESC")
        .ok()?;
    let rows = query
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .ok()?;
    let mut sum = TokenUsage::default();
    let mut found = false;
    for (created, data) in rows.flatten() {
        if created < stop_ms {
            break;
        }
        // Inside the slack but still before the turn: not this turn's cost,
        // and not far enough back to end the walk either.
        if created < since_ms {
            continue;
        }
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
                // A fresh file only touches its immediate parent's mtime, so
                // intermediate directories (storage/session/, say) look stale
                // even when new messages landed below them; descend
                // unconditionally and let the file budget bound the scan.
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
        insert(
            "tool",
            since_ms + 3_000,
            r#"{ "id": "msg", "role": "user" }"#,
        );
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

    /// The backwards walk ends on the first row older than the window, so
    /// the slack is what keeps a stamp that steps backward mid-turn — and
    /// every message recorded after it — from being dropped off the sum.
    /// The far side of the slack is the bound on that tolerance, pinned here
    /// so it can't be narrowed without noticing.
    #[test]
    fn database_tolerates_a_stamp_that_steps_back_within_the_slack() {
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
        let insert = |id: &str, at: i64, input: u64| {
            let data = serde_json::json!({ "tokens": { "input": input } }).to_string();
            db.execute(
                "INSERT INTO message VALUES (?1, 'ses', ?2, ?2, ?3)",
                rusqlite::params![id, at, data],
            )
            .unwrap();
        };
        let since_ms = 1_700_000_000_000i64;
        let slack_ms = CLOCK_SLACK.as_millis() as i64;
        let since = UNIX_EPOCH + Duration::from_millis(since_ms as u64);

        // Recorded in order, but the clock stepped back between the first
        // message and the second, putting a pre-turn stamp on a mid-turn row.
        insert("first", since_ms + 1_000, 100);
        insert("stepped", since_ms - 1_000, 7);
        insert("third", since_ms + 2_000, 20);
        // The stepped-back row is neither counted nor allowed to end the
        // walk, so the message behind it still lands.
        assert_eq!(from_database(&path, since).unwrap().input, 120);

        // A row from before the slack does end it: the walk takes the table
        // to be ordered by `time_created` from there on, and anything behind
        // such a row goes unseen.
        insert("ancient", since_ms - slack_ms - 1, 999);
        insert("fourth", since_ms + 3_000, 5);
        assert_eq!(from_database(&path, since).unwrap().input, 5);
    }

    #[test]
    fn databases_finds_channel_named_files() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["opencode-stable.db", "opencode.db", "other.db", "notes"] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        assert_eq!(
            databases(dir.path()),
            vec![
                dir.path().join("opencode-stable.db"),
                dir.path().join("opencode.db"),
            ]
        );
        assert!(databases(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn file_scan_sums_only_recent_files() {
        let dir = tempfile::tempdir().unwrap();
        // Both session directories predate the turn, so every directory
        // mtime on the way to the new message is stale; only the message
        // file itself is recent.
        let old_session = dir.path().join("session/message/ses_old");
        let live_session = dir.path().join("session/message/ses_live");
        std::fs::create_dir_all(&old_session).unwrap();
        std::fs::create_dir_all(&live_session).unwrap();
        std::fs::write(
            old_session.join("msg.json"),
            r#"{ "tokens": { "input": 999, "output": 999 } }"#,
        )
        .unwrap();
        // Let the old tree's mtimes land strictly before `since`.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let since = SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            live_session.join("msg.json"),
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
