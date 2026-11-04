//! Shared runtime state for the optional web UI. The main loop and turn
//! runner write into it; the web server only reads, and is woken on every
//! write so it can push updates instead of polling.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::watch;
use tracing::debug;

use crate::settings::{MAX_WINDOW_HOURS, RateLimit};

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

#[derive(Serialize, Clone, Default, PartialEq, Eq)]
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

/// What rtk trimmed off the command output the agent read, as rtk recorded
/// it: `input` is the raw output and `output` what the agent got instead,
/// both in rtk's bytes/4 estimate, so the ratio between them is reliable
/// and the absolute figures approximate.
#[derive(Serialize, Clone, Default, PartialEq, Eq, Debug)]
pub struct Savings {
    pub commands: u64,
    pub input: u64,
    pub output: u64,
}

impl Savings {
    pub fn add(&mut self, other: &Savings) {
        self.commands += other.commands;
        self.input += other.input;
        self.output += other.output;
    }
}

/// How a finished turn went, for display only; whether a turn counts toward
/// the rate limit is decided separately in the turn runner.
#[derive(Serialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Skipped,
    Failed,
    Timeout,
    /// Cut short from the web UI; the runner went straight to the next turn.
    Canceled,
}

#[derive(Serialize, Clone)]
pub struct TurnRecord {
    pub task: String,
    pub repo: String,
    pub forge: String,
    /// Epoch seconds; doubles as the turn's identifier in the web UI, which
    /// is unambiguous because the runner sleeps between turns.
    pub started: u64,
    pub duration_secs: u64,
    pub outcome: Outcome,
    /// None when opencode's session storage couldn't be read.
    pub tokens: Option<TokenUsage>,
    /// None when rtk's history couldn't be read, which includes rtk not
    /// being installed at all.
    pub rtk: Option<Savings>,
    /// The turn's full log on disk, kept until the record ages out.
    #[serde(skip)]
    pub log_path: PathBuf,
}

/// How many finished turns are remembered. The list is the whole of the
/// runner's history — nothing survives a restart — but it is also carried in
/// full by every status event, rebuilt as DOM rows by every page that
/// receives one, and backed by one log file per record in the workspace, so
/// an unbounded one grows all three for as long as the process runs. This
/// runner takes a turn every twenty minutes or so and is meant to be left
/// alone for weeks, which is where that stops being theoretical: measured on
/// the Pi this runs on, 2000 records (about a month) serialize to 398 KB in
/// 845µs, pushed to each open page every two seconds.
///
/// Set where a few days of turns still fit, since that is as far back as the
/// list is any use: the page shows it as one scrolling column with no way to
/// search or filter it.
const HISTORY: usize = 200;

#[derive(Serialize, Clone)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Activity {
    Starting,
    /// The settings aren't runnable yet; the web UI marks the fields that
    /// need filling in.
    Unconfigured {
        problems: Vec<crate::settings::Problem>,
    },
    /// `opencode --version` gave no answer, so there is no knowing which
    /// flags a turn would start it with; nothing in the settings fixes that,
    /// only the install does. `reason` is what went wrong asking.
    IncompleteInstall {
        reason: String,
    },
    Running {
        task: String,
        repo: String,
        forge: String,
        #[serde(skip)]
        log_path: PathBuf,
        /// The agent's process group, for the web UI's pause and resume
        /// signals; 0 until the process is actually spawned.
        #[serde(skip)]
        pgid: i32,
        started: u64,
        /// What the turn has spent so far, resampled while it runs; None
        /// until opencode has recorded its first message.
        tokens: Option<TokenUsage>,
        /// What rtk has trimmed for the turn so far; None while its
        /// history can't be read.
        rtk: Option<Savings>,
    },
    Sleeping {
        until: u64,
    },
    /// Nothing in the pool is worth a turn: every enabled task has already
    /// answered for the state the repositories are in. Kept apart from
    /// `Sleeping`, which is the ordinary gap after a turn that did run — the
    /// two look identical on the page otherwise, and one of them means the
    /// runner has been doing nothing since it started.
    Idle {
        until: u64,
    },
    UsageLimit {
        until: u64,
    },
    /// The rate limit's window is full of completed tasks; the next turn may
    /// start at this epoch, when the oldest of them ages out.
    RateLimited {
        until: u64,
    },
    /// The clock is outside the configured daily window; the next automatic
    /// turn may start at this epoch, though forge activity starts one sooner.
    OffHours {
        until: u64,
    },
}

/// The rate limit's sliding window: when the recent completed tasks
/// finished, checked against "at most N in any W hours". A turn may start
/// while fewer than N completions fall inside the last W hours; once N do,
/// the next one waits for the oldest of them to age out. There is no burst
/// to bank and no refill rate: what the page shows is just what happened.
///
/// It lives in the shared status rather than in the main loop because the
/// loop sits inside a turn for hours at a time: a limit edited in the web UI
/// has to land there and then, not at the next turn boundary. Since the
/// window only ever reads the completion times against whatever limit is
/// current, an edit is nothing more than swapping the limit.
pub struct Window {
    /// Completion epochs, oldest first, kept as far back as the widest
    /// window a limit may span — recorded under no limit too, so a limit set
    /// later applies to what actually ran inside it.
    completions: VecDeque<u64>,
    /// The limit the completions are checked against; None means turns run
    /// back to back.
    limit: Option<RateLimit>,
}

/// The window as the page reads it: the limit, and the completions still
/// inside it — the newest `tasks` at most, since no older one can decide
/// anything. The page counts them against its own clock, so "used" and the
/// next slot tick down between server events.
#[derive(Serialize)]
struct WindowView<'a> {
    tasks: u32,
    hours: u32,
    window_secs: u64,
    completions: Vec<&'a u64>,
}

/// Serialized as the `allowance` the page reads: the window while a limit
/// applies, `null` while none does.
impl Serialize for Window {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.limit
            .map(|limit| WindowView {
                tasks: limit.tasks,
                hours: limit.hours,
                window_secs: limit.window_secs(),
                completions: self.deciding(limit, epoch_now()).collect(),
            })
            .serialize(serializer)
    }
}

impl Window {
    /// No completions yet and no limit; a restart starts here, so nothing
    /// that ran before it counts.
    pub fn new() -> Window {
        Window {
            completions: VecDeque::new(),
            limit: None,
        }
    }

    /// Check against `limit` from now on. Nothing is carried over or handed
    /// out: a raised limit or a narrowed window lets turns through as soon
    /// as the completions on record allow it, a lowered one holds them.
    pub fn set_limit(&mut self, limit: Option<RateLimit>) {
        self.limit = limit;
    }

    /// Note a task completed at `now`, and forget the ones too old for any
    /// window to reach.
    pub fn record(&mut self, now: u64) {
        self.completions.push_back(now);
        let horizon = u64::from(MAX_WINDOW_HOURS) * 3600;
        while let Some(&oldest) = self.completions.front() {
            if oldest + horizon > now {
                break;
            }
            self.completions.pop_front();
        }
    }

    /// The completions inside `limit`'s window at `now`, oldest first.
    fn inside(&self, limit: RateLimit, now: u64) -> impl Iterator<Item = &u64> {
        let window = limit.window_secs();
        self.completions.iter().filter(move |&&t| t + window > now)
    }

    /// The newest `tasks` completions inside the window, oldest first: the
    /// only ones that can decide when the next turn may start.
    fn deciding(&self, limit: RateLimit, now: u64) -> impl Iterator<Item = &u64> {
        let inside = self.inside(limit, now).count();
        self.inside(limit, now)
            .skip(inside.saturating_sub(limit.tasks as usize))
    }

    /// How many completions the window holds at `now`, against the limit;
    /// None while no limit applies.
    pub fn used(&self, now: u64) -> Option<(usize, RateLimit)> {
        self.limit
            .map(|limit| (self.inside(limit, now).count(), limit))
    }

    /// How long until a turn may start, in whole seconds; None when one may
    /// start right now. With the window full, that is when the oldest of
    /// the newest `tasks` completions ages out — always at least a second
    /// off, since a completion still inside the window hasn't aged out yet.
    pub fn hold(&self, now: u64) -> Option<u64> {
        let limit = self.limit?;
        let deciding: Vec<_> = self.deciding(limit, now).collect();
        (deciding.len() >= limit.tasks as usize).then(|| deciding[0] + limit.window_secs() - now)
    }
}

#[derive(Serialize, Clone, Default)]
pub struct Totals {
    pub turns: u64,
    pub completed: u64,
    pub skipped: u64,
    pub failed: u64,
    pub timeout: u64,
    pub canceled: u64,
    pub tokens: TokenUsage,
    pub rtk: Savings,
}

#[derive(Serialize)]
pub struct Status {
    /// Process start, epoch seconds.
    pub started: u64,
    pub activity: Activity,
    /// Whether the running turn's process tree is currently SIGSTOPped
    /// through the web UI.
    pub paused: bool,
    /// The web UI asked for the running turn to be cancelled; the turn
    /// runner notices at its next wait hop, kills the agent's tree, and
    /// clears the flag when the next turn starts.
    #[serde(skip)]
    pub cancel_requested: bool,
    /// The rate limit's window, which the main loop records completions in
    /// and the web UI sets the limit of; serialized as what the page shows,
    /// or null when no limit is configured and turns run back to back.
    pub allowance: Window,
    pub totals: Totals,
    /// The last `HISTORY` turns to finish, newest first; the process owns no
    /// history across restarts, so this is the whole list the UI shows.
    /// `totals` counts every turn, including the ones that have aged out.
    ///
    /// Skipped by the derived serialization, because the list is two orders
    /// of magnitude bigger than everything else here put together and moves
    /// only when a turn finishes, where the rest of the status moves every
    /// few seconds while one runs. Both of its readers serialize it
    /// separately — the event stream as an event of its own, `/api/status`
    /// spliced back in — so neither pays for it on a status that didn't
    /// touch it. See `webui::turns_json`.
    #[serde(skip)]
    pub turns: VecDeque<TurnRecord>,
    /// Bumped on every change to `turns`, so the event stream can tell a
    /// moved history from an unmoved one without serializing 44 KB of it to
    /// compare — which is the whole cost the separate event exists to avoid.
    #[serde(skip)]
    pub turns_revision: u64,
    /// Tail of the last finished turn's log. Skipped by the serialization:
    /// the page is sent it as a `log` event, and carrying up to 100 lines of
    /// it on every status event as well sent it again with each token sample.
    #[serde(skip)]
    pub log_tail: String,
    /// When set, turns are held after one died on proxy auth: the epoch at
    /// which the main loop will try again even without a fresh login.
    pub oauth_gated_until: Option<u64>,
}

impl Shared {
    /// A fresh status, with no subscribers yet.
    pub fn new() -> Shared {
        Shared {
            status: Arc::new(Mutex::new(Status {
                started: epoch_now(),
                activity: Activity::Starting,
                paused: false,
                cancel_requested: false,
                allowance: Window::new(),
                totals: Totals::default(),
                turns: VecDeque::new(),
                turns_revision: 0,
                log_tail: String::new(),
                oauth_gated_until: None,
            })),
            changes: watch::channel(0).0,
        }
    }
}

impl Status {
    /// Lock and mutate, then wake the web UI, handing back whatever the
    /// mutation worked out — the rate limit's remaining hold, say, which the
    /// caller would otherwise have to take the lock a second time to read.
    /// The lock is released before the wakeup so a subscriber can read the
    /// new state immediately.
    pub fn update<T>(shared: &Shared, f: impl FnOnce(&mut Status) -> T) -> T {
        let value = f(&mut shared.lock());
        shared.changes.send_modify(|revision| *revision += 1);
        value
    }

    pub fn record_turn(&mut self, record: TurnRecord) {
        self.totals.turns += 1;
        match record.outcome {
            Outcome::Completed => self.totals.completed += 1,
            Outcome::Skipped => self.totals.skipped += 1,
            Outcome::Failed => self.totals.failed += 1,
            Outcome::Timeout => self.totals.timeout += 1,
            Outcome::Canceled => self.totals.canceled += 1,
        }
        if let Some(tokens) = &record.tokens {
            self.totals.tokens.add(tokens);
        }
        if let Some(rtk) = &record.rtk {
            self.totals.rtk.add(rtk);
        }
        self.turns.push_front(record);
        self.turns_revision += 1;
        // A record is the only way to reach a turn's log — the web UI asks
        // for one by the start epoch this list is keyed on — so the file
        // behind an aged-out record is unreachable bytes from here on, and
        // goes with it. Already gone is the ordinary case for a log the
        // runner never got to write, so a failed unlink is not worth a line
        // at info.
        while self.turns.len() > HISTORY {
            let Some(aged) = self.turns.pop_back() else {
                break;
            };
            if let Err(err) = std::fs::remove_file(&aged.log_path) {
                debug!("failed to remove {}: {err}", aged.log_path.display());
            }
        }
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
            rtk: Some(Savings {
                commands: 1,
                input: 100,
                output: 20,
            }),
            log_path: PathBuf::new(),
        }
    }

    #[test]
    fn record_turn_keeps_recent_turns_and_counts_them_all() {
        let shared = Shared::new();
        Status::update(&shared, |status| {
            for _ in 0..60 {
                status.record_turn(record(Outcome::Completed));
            }
            status.record_turn(record(Outcome::Skipped));
        });

        let status = shared.lock();
        assert_eq!(status.turns.len(), 61);
        // The revision is how the event stream notices the list moved — it
        // compares nothing else — so every record has to move it.
        assert_eq!(status.turns_revision, 61);
        assert_eq!(status.turns[0].outcome, Outcome::Skipped);
        assert_eq!(status.totals.turns, 61);
        assert_eq!(status.totals.completed, 60);
        assert_eq!(status.totals.skipped, 1);
        assert_eq!(status.totals.tokens.input, 61 * 10);
    }

    /// The history is bounded, and the log files behind it with it: every
    /// status event carries the whole list and every page rebuilds it, so a
    /// runner left alone for weeks must not grow either without end. The
    /// totals still count the turns that aged out, since they are what the
    /// page's tallies are read from.
    #[test]
    fn an_aged_out_turn_takes_its_log_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Shared::new();
        let log = |started: u64| dir.path().join(format!("{started}.log"));
        let turns = HISTORY as u64 + 10;
        Status::update(&shared, |status| {
            for started in 0..turns {
                std::fs::write(log(started), b"agent output").unwrap();
                let mut record = record(Outcome::Completed);
                record.started = started;
                record.log_path = log(started);
                status.record_turn(record);
            }
        });

        let status = shared.lock();
        assert_eq!(status.turns.len(), HISTORY);
        assert_eq!(status.totals.turns, turns);
        assert_eq!(status.totals.tokens.input, turns * 10);
        // Newest first, and the oldest ten are gone from both the list and
        // the disk.
        assert_eq!(status.turns.front().unwrap().started, turns - 1);
        assert_eq!(status.turns.back().unwrap().started, turns - HISTORY as u64);
        for started in 0..turns {
            let kept = started >= turns - HISTORY as u64;
            assert_eq!(log(started).exists(), kept, "{started}");
        }
    }

    /// A record whose log the runner never wrote — a turn that failed before
    /// the file existed, or one removed under the process — ages out like any
    /// other rather than wedging the pruning.
    #[test]
    fn a_missing_log_does_not_stop_a_turn_aging_out() {
        let shared = Shared::new();
        Status::update(&shared, |status| {
            for _ in 0..HISTORY + 5 {
                status.record_turn(record(Outcome::Failed));
            }
        });
        assert_eq!(shared.lock().turns.len(), HISTORY);
    }

    const HOUR: u64 = 3600;

    fn limited(tasks: u32, hours: u32) -> Window {
        let mut window = Window::new();
        window.set_limit(Some(RateLimit { tasks, hours }));
        window
    }

    fn used(window: &Window, now: u64) -> usize {
        window.used(now).expect("a limited window counts").0
    }

    #[test]
    fn a_full_window_holds_until_its_oldest_completion_ages_out() {
        // Two an hour: two turns may go back to back, and the third waits
        // for the first of them to be an hour old — not half an hour, the
        // way a refilling bucket would have let it.
        let mut window = limited(2, 1);
        assert_eq!(window.hold(0), None);
        window.record(0);
        assert_eq!(window.hold(0), None);
        window.record(600);
        assert_eq!(used(&window, 600), 2);
        assert_eq!(window.hold(600), Some(HOUR - 600));
        assert_eq!(window.hold(1800), Some(HOUR - 1800));
        // The first ages out at the hour, and the second ten minutes later.
        assert_eq!(window.hold(HOUR), None);
        assert_eq!(used(&window, HOUR), 1);
        window.record(HOUR);
        assert_eq!(window.hold(HOUR), Some(600));
        assert_eq!(window.hold(HOUR + 600), None);
    }

    #[test]
    fn a_wide_window_spaces_out_a_slow_rate() {
        // One every two hours, which used to be written as half an hour's
        // worth of a turn.
        let mut window = limited(1, 2);
        window.record(100);
        assert_eq!(window.hold(100), Some(2 * HOUR));
        assert_eq!(window.hold(HOUR + 100), Some(HOUR));
        assert_eq!(window.hold(2 * HOUR + 100), None);
    }

    #[test]
    fn an_edited_limit_applies_to_the_completions_on_record() {
        let mut window = limited(1, 1);
        window.record(0);
        assert_eq!(window.hold(60), Some(HOUR - 60));
        // Raised: the second slot is free at once.
        window.set_limit(Some(RateLimit { tasks: 2, hours: 1 }));
        assert_eq!(window.hold(60), None);
        // Widened: the completion now holds for three hours, not one.
        window.set_limit(Some(RateLimit { tasks: 1, hours: 3 }));
        assert_eq!(window.hold(60), Some(3 * HOUR - 60));
        // Lifted: no hold at all, and no figures to show.
        window.set_limit(None);
        assert_eq!(window.hold(60), None);
        assert!(window.used(60).is_none());
    }

    /// Completions are recorded under no limit too, so a limit set after an
    /// unlimited stretch holds for what actually ran inside its window.
    #[test]
    fn completions_without_a_limit_still_count_once_one_is_set() {
        let mut window = Window::new();
        for at in [0, 60, 120] {
            window.record(at);
        }
        window.set_limit(Some(RateLimit { tasks: 2, hours: 1 }));
        assert_eq!(used(&window, 120), 3);
        // The newest two decide: the next slot opens when the one at 60 is
        // an hour old.
        assert_eq!(window.hold(120), Some(HOUR + 60 - 120));
    }

    /// Nothing older than the widest window is kept, so a runner left
    /// alone for weeks doesn't grow the list without end.
    #[test]
    fn completions_older_than_any_window_are_forgotten() {
        let mut window = Window::new();
        let horizon = u64::from(MAX_WINDOW_HOURS) * HOUR;
        for at in 0..100 {
            window.record(at * HOUR);
        }
        assert_eq!(window.completions.len(), MAX_WINDOW_HOURS as usize);
        assert!(window.completions.iter().all(|&t| t + horizon > 99 * HOUR));
    }

    /// The page reads the window under the name the bucket used to carry:
    /// the limit and the completions that decide it while one applies, null
    /// while none does.
    #[test]
    fn the_window_serializes_as_the_pages_allowance() {
        let shared = Shared::new();
        let idle = serde_json::to_value(&*shared.lock()).unwrap();
        assert!(idle["allowance"].is_null());

        let now = epoch_now();
        Status::update(&shared, |s| {
            // Out of the window, then three inside it: only the newest two
            // can decide anything at two an hour.
            for at in [now - 2 * HOUR, now - 300, now - 200, now - 100] {
                s.allowance.record(at);
            }
            s.allowance
                .set_limit(Some(RateLimit { tasks: 2, hours: 1 }));
        });
        let limited = serde_json::to_value(&*shared.lock()).unwrap();
        assert_eq!(limited["allowance"]["tasks"], 2);
        assert_eq!(limited["allowance"]["hours"], 1);
        assert_eq!(limited["allowance"]["window_secs"], HOUR);
        assert_eq!(
            limited["allowance"]["completions"],
            serde_json::json!([now - 200, now - 100])
        );
    }
}
