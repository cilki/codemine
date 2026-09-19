//! Shared runtime state for the optional web UI. The main loop and turn
//! runner write into it; the web server only reads, and is woken on every
//! write so it can push updates instead of polling.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::watch;

/// A handle on the status plus its change signal. Cloning is cheap and every
/// clone shares both.
#[derive(Clone)]
pub struct Shared {
    status: Arc<Mutex<Status>>,
    /// Bumped on every update, which wakes each subscribed SSE stream. The
    /// revision number itself is never read — only the wakeup matters.
    changes: watch::Sender<u64>,
}

impl Shared {
    /// Lock for reading, shrugging off poisoning so a panic on either side of
    /// the mutex can't wedge the other.
    pub fn lock(&self) -> MutexGuard<'_, Status> {
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A receiver whose `changed()` resolves on every later update.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
}

/// How many finished turns the UI remembers.
const RECENT_CAP: usize = 50;

#[derive(Serialize, Clone, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl TokenUsage {
    pub fn add(&mut self, other: &TokenUsage) {
        self.input += other.input;
        self.output += other.output;
        self.reasoning += other.reasoning;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }
}

/// How a finished turn went, for display only; whether a turn counts toward
/// the daily limit is decided separately in the turn runner.
#[derive(Serialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Skipped,
    Failed,
    Timeout,
}

#[derive(Serialize, Clone)]
pub struct TurnRecord {
    pub task: String,
    pub repo: String,
    pub forge: String,
    /// Epoch seconds.
    pub started: u64,
    pub duration_secs: u64,
    pub outcome: Outcome,
    /// None when opencode's session storage couldn't be read.
    pub tokens: Option<TokenUsage>,
}

#[derive(Serialize, Clone)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Activity {
    Starting,
    /// The settings aren't runnable yet; the web UI marks the fields that
    /// need filling in.
    Unconfigured {
        problems: Vec<crate::settings::Problem>,
    },
    Running {
        task: String,
        repo: String,
        forge: String,
        workspace: String,
        #[serde(skip)]
        log_path: PathBuf,
        started: u64,
    },
    Sleeping {
        until: u64,
    },
    UsageLimit {
        until: u64,
    },
    WaitingForTomorrow {
        day: String,
    },
}

#[derive(Serialize, Clone, Default)]
pub struct Totals {
    pub turns: u64,
    pub completed: u64,
    pub skipped: u64,
    pub failed: u64,
    pub timeout: u64,
    pub tokens: TokenUsage,
}

#[derive(Serialize)]
pub struct Status {
    /// Process start, epoch seconds.
    pub started: u64,
    pub activity: Activity,
    pub day: String,
    pub completed_today: u32,
    pub daily_limit: Option<u32>,
    pub totals: Totals,
    /// Finished turns, newest first.
    pub recent: VecDeque<TurnRecord>,
    /// Tail of the last finished turn's log.
    pub log_tail: String,
}

impl Shared {
    /// A fresh status, with no subscribers yet.
    pub fn new() -> Shared {
        Shared {
            status: Arc::new(Mutex::new(Status {
                started: epoch_now(),
                activity: Activity::Starting,
                day: String::new(),
                completed_today: 0,
                daily_limit: None,
                totals: Totals::default(),
                recent: VecDeque::new(),
                log_tail: String::new(),
            })),
            changes: watch::channel(0).0,
        }
    }
}

impl Status {
    /// Lock and mutate, then wake the web UI. The lock is released before the
    /// wakeup so a subscriber can read the new state immediately.
    pub fn update(shared: &Shared, f: impl FnOnce(&mut Status)) {
        f(&mut shared.lock());
        shared.changes.send_modify(|revision| *revision += 1);
    }

    pub fn record_turn(&mut self, record: TurnRecord) {
        self.totals.turns += 1;
        match record.outcome {
            Outcome::Completed => self.totals.completed += 1,
            Outcome::Skipped => self.totals.skipped += 1,
            Outcome::Failed => self.totals.failed += 1,
            Outcome::Timeout => self.totals.timeout += 1,
        }
        if let Some(tokens) = &record.tokens {
            self.totals.tokens.add(tokens);
        }
        self.recent.truncate(RECENT_CAP - 1);
        self.recent.push_front(record);
    }
}

pub fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(outcome: Outcome) -> TurnRecord {
        TurnRecord {
            task: "todo".into(),
            repo: "owner/repo".into(),
            forge: "gitea".into(),
            started: 1,
            duration_secs: 2,
            outcome,
            tokens: Some(TokenUsage {
                input: 10,
                output: 5,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn record_turn_caps_and_totals() {
        let shared = Shared::new();
        Status::update(&shared, |status| {
            for _ in 0..RECENT_CAP + 10 {
                status.record_turn(record(Outcome::Completed));
            }
            status.record_turn(record(Outcome::Skipped));
        });

        let status = shared.lock();
        assert_eq!(status.recent.len(), RECENT_CAP);
        assert_eq!(status.recent[0].outcome, Outcome::Skipped);
        assert_eq!(status.totals.turns, RECENT_CAP as u64 + 11);
        assert_eq!(status.totals.completed, RECENT_CAP as u64 + 10);
        assert_eq!(status.totals.skipped, 1);
        assert_eq!(status.totals.tokens.input, (RECENT_CAP as u64 + 11) * 10);
    }
}
