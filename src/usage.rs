//! Best-effort token accounting. opencode doesn't reliably print token counts
//! in its run output, but it persists each message (with a `tokens` object)
//! in the `message` table of `opencode*.db` under its data directory. Each
//! turn runs in a fresh session, so summing the token objects recorded since
//! the turn started attributes usage to the turn. Anything unexpected
//! degrades to None rather than failing the turn.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::status::TokenUsage;

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

/// One turn's token accounting, resampled while the turn runs.
///
/// The sum is carried across samples rather than rebuilt from the window
/// each time. opencode's `message` table is append-only — the same property
/// [`CLOCK_SLACK`] documents — so a row a sample has already counted can
/// neither change nor disappear, and the next sample only has to read what
/// has been appended since. A sample therefore costs the messages the agent
/// wrote in the last few seconds, where re-summing the window cost the whole
/// turn so far: six hours in, several hundred messages, among them the
/// session-summary records that carry their diffs with them at a few hundred
/// kilobytes of patch text each.
pub struct Sampler {
    /// The turn's start; messages recorded before it are not its cost.
    since: SystemTime,
    /// The sum so far. None until a message with a `tokens` object has been
    /// seen at all, which is how a turn opencode hasn't recorded anything
    /// for yet is told from one that has genuinely cost nothing.
    sum: Option<TokenUsage>,
    /// Which database the sum is being read from, and the rowid it is
    /// complete up to. None until one holding this turn's messages is
    /// found — see [`read`](Self::read).
    cursor: Option<(PathBuf, i64)>,
}

impl Sampler {
    pub fn new(since: SystemTime) -> Sampler {
        Sampler {
            since,
            sum: None,
            cursor: None,
        }
    }

    /// The turn's cost as of now, or None while nothing has been recorded
    /// for it. Cheap enough to call on a timer: once a database has been
    /// found, each call reads only the rows appended since the last one.
    pub fn sample(&mut self) -> Option<TokenUsage> {
        self.read(&opencode_data_dir())
    }

    /// A sample out of the databases under `data`, which is opencode's data
    /// directory — the tests pass one of their own.
    fn read(&mut self, data: &Path) -> Option<TokenUsage> {
        match self.cursor.clone() {
            Some((path, from)) => self.advance(&path, from),
            // Settle on the first database with any of this turn's messages
            // in it: which file opencode writes depends on its release
            // channel, and a stale one from another channel can sort ahead
            // of the live one. The directory is listed only while the
            // sampler is still looking — once it has settled on a file,
            // re-reading the directory every few seconds answers a question
            // it has stopped asking.
            None => {
                for path in databases(data) {
                    if self.open_window(&path) {
                        break;
                    }
                }
            }
        }
        self.sum.clone()
    }

    /// Sum the turn's messages as the database has them now and remember the
    /// rowid that leaves the sum complete up to, so later samples can read
    /// forward from it. False when the database holds nothing of this turn's,
    /// which leaves the sampler looking.
    fn open_window(&mut self, path: &Path) -> bool {
        let Some((db, since_ms)) = self.open(path) else {
            return false;
        };
        let stop_ms = since_ms - CLOCK_SLACK.as_millis() as i64;
        // Descending rowid is a reverse walk of the table's own b-tree, not
        // a sort: rowid *is* the key, so nothing is materialized and the
        // walk can be abandoned as soon as it reads past the window.
        let Ok(mut query) =
            db.prepare("SELECT rowid, time_created, data FROM message ORDER BY rowid DESC")
        else {
            return false;
        };
        let Ok(mut rows) = query.query([]) else {
            return false;
        };
        let mut sum = TokenUsage::default();
        let mut found = false;
        // The newest row the walk saw, which is where the next sample picks
        // up. Taken whether or not the row counted: a row that didn't is
        // still one no later sample needs to look at again.
        let mut reached = 0;
        while let Ok(Some(row)) = rows.next() {
            let (Ok(rowid), Ok(created)) = (row.get::<_, i64>(0), row.get::<_, i64>(1)) else {
                continue;
            };
            reached = reached.max(rowid);
            if created < stop_ms {
                break;
            }
            // Inside the slack but still before the turn: not this turn's
            // cost, and not far enough back to end the walk either.
            if created < since_ms {
                continue;
            }
            if let Ok(rusqlite::types::ValueRef::Text(data)) = row.get_ref(2)
                && let Some(tokens) = tokens_in(data)
            {
                sum.add(&tokens);
                found = true;
            }
        }
        if found {
            self.sum = Some(sum);
            self.cursor = Some((path.to_owned(), reached));
        }
        found
    }

    /// Add the rows appended past `from`. `rowid > ?` is a seek into the
    /// table's own key, so this costs the new rows and nothing for the ones
    /// already counted — the whole point of carrying the sum.
    ///
    /// A row stamped before the turn started is skipped rather than ending
    /// the walk the way [`open_window`](Self::open_window) would: every row
    /// here is newer than everything already counted, so an older stamp on
    /// one is the clock stepping back, never the edge of the window.
    fn advance(&mut self, path: &Path, from: i64) {
        let Some((db, since_ms)) = self.open(path) else {
            // The file opencode was writing is gone — a channel switch, or a
            // reinstall mid-turn. Keep what was counted and go looking again
            // at the next sample rather than reporting nothing from here on.
            self.cursor = None;
            return;
        };
        let Ok(mut query) = db.prepare(
            "SELECT rowid, time_created, data FROM message WHERE rowid > ?1 ORDER BY rowid",
        ) else {
            return;
        };
        let Ok(mut rows) = query.query([from]) else {
            return;
        };
        let mut sum = self.sum.clone().unwrap_or_default();
        let mut reached = from;
        while let Ok(Some(row)) = rows.next() {
            let (Ok(rowid), Ok(created)) = (row.get::<_, i64>(0), row.get::<_, i64>(1)) else {
                continue;
            };
            reached = reached.max(rowid);
            if created < since_ms {
                continue;
            }
            if let Ok(rusqlite::types::ValueRef::Text(data)) = row.get_ref(2)
                && let Some(tokens) = tokens_in(data)
            {
                sum.add(&tokens);
            }
        }
        self.sum = Some(sum);
        self.cursor = Some((path.to_owned(), reached));
    }

    /// The database open read-only, with the turn's start in the column's
    /// own units. None when either can't be had, which is a sample that
    /// reports what it had before.
    fn open(&self, path: &Path) -> Option<(rusqlite::Connection, i64)> {
        let since_ms = self.since.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64;
        let db =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .ok()?;
        Some((db, since_ms))
    }
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

/// The one field of a recorded message this module is after. Deserializing
/// into this walks past every other field without building it, where asking
/// for the record as a `serde_json::Value` built the whole thing — and
/// opencode's records are not uniformly small: the ones that carry a session
/// summary carry the diffs with it, a few hundred kilobytes of patch text
/// each. A turn's window holds a handful of those among its several hundred
/// messages, and it is re-read every `turn::SAMPLE_INTERVAL` for the whole of
/// the turn, so they were most of the sampling cost.
///
/// The `tokens` object itself stays a `Value`: it is five numbers, and the
/// tolerance of [`token_fields`] towards a field that is missing or isn't a
/// number is the point of reading it that way.
#[derive(serde::Deserialize)]
struct Recorded {
    /// Absent on the messages that aren't a model's answer — a user turn, a
    /// tool result — which is how a record with no cost on it is told from
    /// one whose cost is zero.
    tokens: Option<serde_json::Value>,
}

/// The `tokens` object of one opencode message record, if it has one.
fn tokens_in(data: &[u8]) -> Option<TokenUsage> {
    let record: Recorded = serde_json::from_slice(data).ok()?;
    token_fields(&record.tokens?)
}

/// One message's token counts, read out of its `tokens` object. Anything
/// that isn't an object has no counts in it; a count that is missing or isn't
/// a number reads as zero, since a partially recorded message is still worth
/// what it does say.
fn token_fields(tokens: &serde_json::Value) -> Option<TokenUsage> {
    let tokens = tokens.as_object()?;
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

    /// A record is read for its `tokens` object alone, so whatever else it
    /// carries has to be walked past rather than tripped over: the fields
    /// around it, in any order, and the hundreds of kilobytes of patch text
    /// the session-summary records carry.
    #[test]
    fn tokens_in_reads_a_message_record() {
        let record = serde_json::json!({
            "id": "msg_123",
            "role": "assistant",
            "tokens": {
                "input": 1200, "output": 340, "reasoning": 0,
                "cache": { "read": 56000, "write": 1800 }
            },
            "summary": { "diffs": [{ "file": "a.rs", "patch": "x".repeat(4096) }] },
            "time": { "created": 1, "completed": 2 },
        })
        .to_string();
        let tokens = tokens_in(record.as_bytes()).unwrap();
        assert_eq!(tokens.input, 1200);
        assert_eq!(tokens.output, 340);
        assert_eq!(tokens.cache_read, 56000);
        assert_eq!(tokens.cache_write, 1800);
        assert_eq!(tokens.reasoning, 0);
    }

    #[test]
    fn tokens_in_tolerates_missing_fields() {
        let tokens = tokens_in(br#"{ "tokens": { "input": 5, "output": 2 } }"#).unwrap();
        assert_eq!(tokens.input, 5);
        assert_eq!(tokens.cache_read, 0);
    }

    /// A record with no `tokens` object has no cost to report, which is what
    /// tells a user turn or a tool result from a model's answer that happened
    /// to cost nothing. Neither has a record that isn't a JSON object at all,
    /// or one opencode was still writing when the sample read it.
    #[test]
    fn tokens_in_rejects_other_json() {
        assert!(tokens_in(br#"{ "id": "ses_1" }"#).is_none());
        assert!(tokens_in(br#"{ "tokens": 7 }"#).is_none());
        assert!(tokens_in(br#"{ "tokens": null }"#).is_none());
        assert!(tokens_in(b"[1, 2, 3]").is_none());
        assert!(tokens_in(br#"{ "tokens": { "input": 5 "#).is_none());
        assert!(tokens_in(b"").is_none());
    }

    const SINCE_MS: i64 = 1_700_000_000_000;

    fn at(ms: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    }

    /// An empty database with the `message` table opencode's sqlite
    /// migration produces, plus a hand for appending rows to it.
    fn message_table(path: &Path) -> rusqlite::Connection {
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute_batch(
            "CREATE TABLE message (
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .unwrap();
        db
    }

    fn record(db: &rusqlite::Connection, id: &str, at: i64, data: &str) {
        db.execute(
            "INSERT INTO message VALUES (?1, 'ses', ?2, ?2, ?3)",
            rusqlite::params![id, at, data],
        )
        .unwrap();
    }

    /// A model's answer costing `input` tokens, as opencode records one.
    fn answer(input: u64) -> String {
        serde_json::json!({ "tokens": { "input": input } }).to_string()
    }

    #[test]
    fn database_sums_only_messages_since_the_turn_started() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        let db = message_table(&path);
        let message = |tokens: serde_json::Value| {
            serde_json::json!({ "id": "msg", "role": "assistant", "tokens": tokens }).to_string()
        };
        let earlier = message(serde_json::json!({ "input": 999, "output": 999 }));
        record(&db, "old", SINCE_MS - 5_000, &earlier);
        let first = message(serde_json::json!({
            "input": 100, "output": 20, "cache": { "read": 4000, "write": 300 }
        }));
        record(&db, "new1", SINCE_MS + 1_000, &first);
        let second = message(serde_json::json!({ "input": 10, "output": 5, "reasoning": 7 }));
        record(&db, "new2", SINCE_MS + 2_000, &second);
        record(
            &db,
            "tool",
            SINCE_MS + 3_000,
            r#"{ "id": "msg", "role": "user" }"#,
        );
        drop(db);

        let sum = Sampler::new(at(SINCE_MS)).read(dir.path()).unwrap();
        assert_eq!(sum.input, 110);
        assert_eq!(sum.output, 25);
        assert_eq!(sum.reasoning, 7);
        assert_eq!(sum.cache_read, 4000);
        assert_eq!(sum.cache_write, 300);

        // Nothing recorded for this turn, or no database at all, reports
        // None and not a cost of zero: the page shows a turn opencode hasn't
        // accounted for yet as unknown, and "unknown" is also what a turn
        // that died before its first message is recorded with.
        assert!(
            Sampler::new(at(SINCE_MS + 60_000))
                .read(dir.path())
                .is_none()
        );
        let empty = tempfile::tempdir().unwrap();
        assert!(Sampler::new(at(SINCE_MS)).read(empty.path()).is_none());
    }

    /// The first sample walks the table backwards and ends on the first row
    /// older than the window, so the slack is what keeps a stamp that steps
    /// backward mid-turn — and every message recorded after it — from being
    /// dropped off the sum. The far side of the slack is the bound on that
    /// tolerance, pinned here so it can't be narrowed without noticing.
    #[test]
    fn database_tolerates_a_stamp_that_steps_back_within_the_slack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        let db = message_table(&path);
        let slack_ms = CLOCK_SLACK.as_millis() as i64;
        let fresh = || Sampler::new(at(SINCE_MS)).read(dir.path()).unwrap().input;

        // Recorded in order, but the clock stepped back between the first
        // message and the second, putting a pre-turn stamp on a mid-turn row.
        record(&db, "first", SINCE_MS + 1_000, &answer(100));
        record(&db, "stepped", SINCE_MS - 1_000, &answer(7));
        record(&db, "third", SINCE_MS + 2_000, &answer(20));
        // The stepped-back row is neither counted nor allowed to end the
        // walk, so the message behind it still lands.
        assert_eq!(fresh(), 120);

        // A row from before the slack does end it: the walk takes the table
        // to be ordered by `time_created` from there on, and anything behind
        // such a row goes unseen.
        record(&db, "ancient", SINCE_MS - slack_ms - 1, &answer(999));
        record(&db, "fourth", SINCE_MS + 3_000, &answer(5));
        assert_eq!(fresh(), 5);
    }

    /// A resample reads only what was appended since the last one, and has
    /// to land on the figure a full re-read of the window would: counting a
    /// row twice or not at all is the whole risk of carrying the sum
    /// forward. Checked against a fresh sampler at every step, which is the
    /// reading this replaced.
    #[test]
    fn resampling_counts_each_message_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        let db = message_table(&path);
        let fresh = || Sampler::new(at(SINCE_MS)).read(dir.path()).map(|t| t.input);

        let mut live = Sampler::new(at(SINCE_MS));
        // Sampled before opencode has recorded anything: nothing to report,
        // and nothing remembered either, so the next sample still looks.
        assert!(live.read(dir.path()).is_none());
        assert!(live.cursor.is_none());

        record(&db, "first", SINCE_MS + 1_000, &answer(100));
        assert_eq!(live.read(dir.path()).map(|t| t.input), Some(100));
        // A sample with nothing new to read repeats itself rather than
        // double-counting what it already has.
        assert_eq!(live.read(dir.path()).map(|t| t.input), Some(100));
        assert_eq!(fresh(), Some(100));

        // The rows that carry no cost — a user turn, a tool result — move
        // the cursor without moving the sum.
        record(&db, "tool", SINCE_MS + 2_000, r#"{ "role": "user" }"#);
        record(&db, "second", SINCE_MS + 3_000, &answer(20));
        assert_eq!(live.read(dir.path()).map(|t| t.input), Some(120));
        assert_eq!(fresh(), Some(120));

        // A row appended with a stamp from before the turn is the clock
        // stepping back, not the edge of the window: it isn't this turn's
        // cost, and — unlike in the backwards walk — it can't end a read
        // that is going forwards through rows newer than everything counted.
        record(&db, "stepped", SINCE_MS - 1, &answer(999));
        record(&db, "third", SINCE_MS + 4_000, &answer(5));
        assert_eq!(live.read(dir.path()).map(|t| t.input), Some(125));

        // The database opencode was writing going away mid-turn leaves the
        // sum standing and sets the sampler looking again, where reporting
        // nothing from then on would blank the figure for the rest of the
        // turn.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(live.read(dir.path()).map(|t| t.input), Some(125));
        assert!(live.cursor.is_none());
    }

    /// Which database holds the turn is settled once, by the first sample
    /// that finds this turn's messages in one: a stale file from an older
    /// channel sorts ahead of the live one and must not be the one the rest
    /// of the turn is read from.
    #[test]
    fn the_database_holding_the_turn_is_the_one_sampled() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("opencode-dev.db");
        let live = dir.path().join("opencode-stable.db");
        let stale_db = message_table(&stale);
        let live_db = message_table(&live);
        // The stale file has messages, but all of them predate the turn.
        record(&stale_db, "old", SINCE_MS - 10_000, &answer(999));
        record(&live_db, "first", SINCE_MS + 1_000, &answer(100));

        // `databases` sorts, so the stale `-dev` file is the one tried first.
        let mut sampler = Sampler::new(at(SINCE_MS));
        assert_eq!(sampler.read(dir.path()).map(|t| t.input), Some(100));
        assert_eq!(sampler.cursor.as_ref().map(|(path, _)| path), Some(&live));

        // And it stays settled: a later write to the stale file is not this
        // turn's, however recently stamped.
        record(&stale_db, "intruder", SINCE_MS + 2_000, &answer(42));
        record(&live_db, "second", SINCE_MS + 3_000, &answer(20));
        assert_eq!(sampler.read(dir.path()).map(|t| t.input), Some(120));
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
}
