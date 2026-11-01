//! What rtk saved a turn, read out of rtk's own history. Every command rtk
//! runs in place of the agent's records a row in the `commands` table of its
//! `history.db`, stamped with the time, the directory it ran in, and the
//! bytes/4 estimate of the output before and after filtering. A turn's
//! savings are the rows from its start onwards that ran inside its clone.
//! Like `usage`, anything unexpected — most often rtk not being installed,
//! so there is no database at all — degrades to None.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::status::Savings;

/// Where rtk keeps its history, resolved the way rtk resolves it:
/// `$RTK_DB_PATH` (a file) first, else `$XDG_DATA_HOME/rtk/history.db`,
/// else `~/.local/share/rtk/history.db`. rtk also honors a
/// `[tracking] database_path` in its config, which codemine doesn't write
/// and so doesn't look for.
fn history_db() -> PathBuf {
    match std::env::var_os("RTK_DB_PATH") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => crate::config::xdg_dir("XDG_DATA_HOME", ".local/share").join("rtk/history.db"),
    }
}

/// One turn's rtk savings, resampled while the turn runs.
///
/// Each sample is one aggregate over rtk's timestamp index rather than a
/// cursor carried from sample to sample the way `usage::Sampler` does it:
/// rtk writes one small row per command, so even a long turn's window is a
/// few hundred rows of numbers, with none of the patch text that made
/// re-reading opencode's table expensive.
pub struct Sampler {
    since: SystemTime,
    /// The clone, as rtk records it: rtk canonicalizes the directory a
    /// command ran in, so the comparison has to be against the same form.
    dir: String,
    db: PathBuf,
}

impl Sampler {
    pub fn new(since: SystemTime, dir: &Path) -> Sampler {
        Sampler::at(since, dir, history_db())
    }

    fn at(since: SystemTime, dir: &Path, db: PathBuf) -> Sampler {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
        Sampler {
            since,
            dir: dir.to_string_lossy().into_owned(),
            db,
        }
    }

    /// What rtk has saved the turn so far, or None when its history can't be
    /// read. A turn rtk hasn't rewritten anything for yet reads as zero
    /// commands, which the page tells apart from no rtk at all.
    pub fn sample(&self) -> Option<Savings> {
        let since = self.since.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
        // Opened read-only, and never created: a missing file is rtk not
        // being installed, not something to make on its behalf.
        let db = rusqlite::Connection::open_with_flags(
            &self.db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .ok()?;
        // rtk stamps rows with RFC 3339 in UTC (`2026-11-01T12:34:56.123+00:00`),
        // so the turn's start in the same layout, to the second, compares
        // against them as plain strings — and so as a range on rtk's own
        // index. Rows from a command run in a subdirectory of the clone
        // count too; GLOB rather than LIKE, as rtk queries it, because `_`
        // is common in paths and LIKE would read it as a wildcard.
        db.query_row(
            "SELECT COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0)
             FROM commands
             WHERE timestamp >= strftime('%Y-%m-%dT%H:%M:%S', ?1, 'unixepoch')
               AND (project_path = ?2 OR project_path GLOB ?3)",
            rusqlite::params![since, self.dir, format!("{}/*", glob_escape(&self.dir))],
            |row| {
                let count = |i| row.get::<_, i64>(i).map(|n| n.max(0) as u64);
                Ok(Savings {
                    commands: count(0)?,
                    input: count(1)?,
                    output: count(2)?,
                })
            },
        )
        .ok()
    }
}

/// `path` with GLOB's metacharacters each wrapped in a one-character class,
/// so it matches only itself.
fn glob_escape(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '*' | '?' | '[' => {
                escaped.push('[');
                escaped.push(c);
                escaped.push(']');
            }
            _ => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// rtk's `commands` table as its migrations leave it.
    const SCHEMA: &str = "
        CREATE TABLE commands (
            id INTEGER PRIMARY KEY,
            timestamp TEXT NOT NULL,
            original_cmd TEXT NOT NULL,
            rtk_cmd TEXT NOT NULL,
            input_tokens INTEGER NOT NULL,
            output_tokens INTEGER NOT NULL,
            saved_tokens INTEGER NOT NULL,
            savings_pct REAL NOT NULL,
            exec_time_ms INTEGER DEFAULT 0,
            project_path TEXT DEFAULT ''
        );
        CREATE INDEX idx_timestamp ON commands(timestamp);";

    fn insert(db: &rusqlite::Connection, timestamp: &str, project: &str, input: i64, output: i64) {
        db.execute(
            "INSERT INTO commands (timestamp, original_cmd, rtk_cmd, project_path,
                 input_tokens, output_tokens, saved_tokens, savings_pct)
             VALUES (?1, 'git status', 'rtk git status', ?2, ?3, ?4, ?3 - ?4, 0)",
            rusqlite::params![timestamp, project, input, output],
        )
        .unwrap();
    }

    /// 2026-11-01T12:00:00Z.
    fn noon() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_793_534_400)
    }

    /// Only the turn's rows count: from its start on, in its clone or below
    /// it. Rows before the start, in another directory, or in a sibling
    /// whose name merely begins with the clone's, are someone else's.
    #[test]
    fn sums_the_turns_rows_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let clone = tmp.path().join("o_r");
        std::fs::create_dir(&clone).unwrap();
        let clone = clone.canonicalize().unwrap();
        let path = clone.to_str().unwrap();
        let db_path = tmp.path().join("history.db");
        let db = rusqlite::Connection::open(&db_path).unwrap();
        db.execute_batch(SCHEMA).unwrap();
        // Inside the window, in the clone and in a subdirectory of it.
        insert(&db, "2026-11-01T12:00:00.000000001+00:00", path, 1000, 100);
        insert(
            &db,
            "2026-11-01T13:30:00.5+00:00",
            &format!("{path}/sub"),
            400,
            50,
        );
        // A row that grew rather than shrank still counts as it is.
        insert(&db, "2026-11-01T14:00:00+00:00", path, 10, 12);
        // Before the turn started.
        insert(&db, "2026-11-01T11:59:59.999999+00:00", path, 9000, 1);
        // Elsewhere, and in a sibling that shares the clone's prefix.
        insert(&db, "2026-11-01T12:30:00+00:00", "/home/someone", 9000, 1);
        insert(
            &db,
            "2026-11-01T12:30:00+00:00",
            &format!("{path}x"),
            9000,
            1,
        );
        // Recorded before rtk tracked the directory.
        insert(&db, "2026-11-01T12:30:00+00:00", "", 9000, 1);
        drop(db);

        let sampler = Sampler::at(noon(), &clone, db_path);
        assert_eq!(
            sampler.sample(),
            Some(Savings {
                commands: 3,
                input: 1410,
                output: 162,
            })
        );
    }

    /// A history with nothing for the turn yet is zero commands, not None:
    /// rtk is there, it just hasn't been asked for anything.
    #[test]
    fn nothing_yet_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("history.db");
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute_batch(SCHEMA)
            .unwrap();
        let sampler = Sampler::at(noon(), tmp.path(), db_path);
        assert_eq!(sampler.sample(), Some(Savings::default()));
    }

    /// No rtk, no history: None, and no file left behind.
    #[test]
    fn missing_history_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("rtk/history.db");
        let sampler = Sampler::at(noon(), tmp.path(), db_path.clone());
        assert_eq!(sampler.sample(), None);
        assert!(!db_path.exists());
    }

    #[test]
    fn glob_metacharacters_match_only_themselves() {
        assert_eq!(glob_escape("/w/a*b?c[d]"), "/w/a[*]b[?]c[[]d]");
    }
}
