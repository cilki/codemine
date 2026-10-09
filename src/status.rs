//! Shared runtime state for the optional web UI. The main loop and turn
//! runner write into it; the web server only reads, and is woken on every
//! write so it can push updates instead of polling.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::watch;
use tracing::debug;

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

/// How a finished turn went, for display only; whether a turn counts toward
/// the hourly limit is decided separately in the turn runner.
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
    Running {
        task: String,
        repo: String,
        forge: String,
        workspace: String,
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
    /// The hourly task limit is spent; the next turn may start at this
    /// epoch.
    RateLimited {
        until: u64,
    },
    /// The clock is outside the configured daily window; the next turn may
    /// start at this epoch.
    OffHours {
        until: u64,
    },
}

/// The turn budget the hourly limit hands out, as the page reads it: how
/// many turns may start right now, out of how many the bucket holds when
/// full.
#[derive(Serialize, Clone, Copy)]
pub struct Allowance {
    pub available: f64,
    pub capacity: f64,
}

const HOUR: f64 = 3600.0;

/// The hourly limit's token bucket. It refills at the configured rate and
/// holds at most an hour's worth, so a limit of 2 runs two turns back to
/// back and then one every half hour.
///
/// It lives in the shared status rather than in the main loop because the
/// loop sits inside a turn for hours at a time: a limit edited in the web UI
/// has to resize the bucket there and then, not at the next turn boundary.
/// So both sides spend and resize the one bucket, where the loop used to own
/// the real figures and the page a mirror of them that the UI resized with a
/// second copy of the same arithmetic.
#[derive(Clone, Copy)]
pub struct Bucket {
    /// Turns in hand. Meaningless while `limit` is None: an unlimited
    /// stretch isn't accounted for at all, and the limit set after one
    /// starts the bucket full.
    available: f64,
    /// The limit the bucket is currently sized against; None means turns run
    /// back to back.
    limit: Option<f64>,
    /// When the bucket was last topped up, epoch seconds.
    refilled: u64,
}

/// Serialized as the `allowance` the page reads: the figures while a limit
/// applies, `null` while none does.
impl Serialize for Bucket {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.allowance().serialize(serializer)
    }
}

impl Bucket {
    /// An unlimited bucket, its clock started; the first limit to arrive
    /// fills it.
    pub fn new() -> Bucket {
        Bucket {
            available: 0.0,
            limit: None,
            refilled: epoch_now(),
        }
    }

    /// How many turns the bucket holds when full: an hour's worth, but never
    /// less than one, so a fractional limit still lets a turn through — 0.5
    /// an hour is one turn every two hours rather than none at all.
    fn capacity(limit: f64) -> f64 {
        limit.floor().max(1.0)
    }

    /// Size the bucket against `limit` instead of waiting for the old one to
    /// run out: the growth is handed over at once, so a raised limit buys
    /// turns now rather than an hour from now, and whatever a shrunken bucket
    /// can't hold spills. A limit that hasn't moved changes nothing.
    pub fn resize(&mut self, limit: Option<f64>) {
        if limit == self.limit {
            return;
        }
        if let Some(new) = limit {
            let new = Self::capacity(new);
            self.available = match self.limit {
                // An unlimited stretch drains nothing, so there is no
                // accounting to carry into the limit that follows it.
                None => new,
                Some(old) => (self.available + (new - Self::capacity(old)).max(0.0)).min(new),
            };
        }
        self.limit = limit;
    }

    /// Grow the bucket for the time since it was last topped up, capped at
    /// its capacity so an idle runner banks at most one hour. The clock
    /// advances whether or not a limit applies, so the time it grew for is
    /// counted exactly once.
    pub fn refill(&mut self, now: u64) {
        let since = std::mem::replace(&mut self.refilled, now);
        if let Some(limit) = self.limit {
            let earned = now.saturating_sub(since) as f64 * limit / HOUR;
            self.available = (self.available + earned).min(Self::capacity(limit));
        }
    }

    /// How long until a turn may start, in whole seconds rounded up so the
    /// wait can't expire a hair early and spin the loop; None when one may
    /// start right now.
    pub fn hold(&self) -> Option<u64> {
        let limit = self.limit?;
        (self.available < 1.0).then(|| ((1.0 - self.available) * HOUR / limit).ceil() as u64)
    }

    /// Take one turn out of the bucket and report what is left; None when no
    /// limit applies and there was nothing to spend. Clamped at zero: the
    /// turn that spends the last of it leaves a hair less than none behind,
    /// and "-0.0 of 2" reads as a bug.
    pub fn spend(&mut self) -> Option<Allowance> {
        self.limit?; // nothing is accounted for while turns run back to back
        self.available = (self.available - 1.0).max(0.0);
        self.allowance()
    }

    /// The figures the page shows, or None while turns run back to back.
    pub fn allowance(&self) -> Option<Allowance> {
        self.limit.map(|limit| Allowance {
            available: self.available,
            capacity: Self::capacity(limit),
        })
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
    /// The hourly limit's bucket, which the main loop spends from and the
    /// web UI resizes; serialized as the figures the page shows, or null
    /// when no limit is configured and turns run back to back.
    pub allowance: Bucket,
    pub totals: Totals,
    /// The last `HISTORY` turns to finish, newest first; the process owns no
    /// history across restarts, so this is the whole list the UI shows.
    /// `totals` counts every turn, including the ones that have aged out.
    pub turns: VecDeque<TurnRecord>,
    /// Tail of the last finished turn's log.
    pub log_tail: String,
    /// When set, turns are held after one died on proxy auth: the epoch at
    /// which the main loop will try again even without a fresh login.
    pub oauth_gated_until: Option<u64>,
    /// Who the bot is on each forge, as "login <email>" keyed by forge slug;
    /// resolved from each forge's whoami endpoint, so empty until the first
    /// successful resolve.
    pub identities: BTreeMap<String, String>,
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
                allowance: Bucket::new(),
                totals: Totals::default(),
                turns: VecDeque::new(),
                log_tail: String::new(),
                oauth_gated_until: None,
                identities: BTreeMap::new(),
            })),
            changes: watch::channel(0).0,
        }
    }
}

impl Status {
    /// Lock and mutate, then wake the web UI, handing back whatever the
    /// mutation worked out — the bucket's remaining hold, say, which the
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
        self.turns.push_front(record);
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

    /// A bucket whose clock starts at epoch zero, so a `refill` is a plain
    /// "this many seconds have passed".
    fn limited(limit: f64) -> Bucket {
        let mut bucket = Bucket {
            available: 0.0,
            limit: None,
            refilled: 0,
        };
        bucket.resize(Some(limit));
        bucket
    }

    fn spendable(bucket: &Bucket) -> (f64, f64) {
        let allowance = bucket
            .allowance()
            .expect("a limited bucket has an allowance");
        (allowance.available, allowance.capacity)
    }

    #[test]
    fn the_bucket_allows_a_burst_then_the_rate() {
        // Two an hour: two turns in hand at once, one back every half hour.
        let mut bucket = limited(2.0);
        assert_eq!(spendable(&bucket), (2.0, 2.0));
        bucket.spend();
        bucket.spend();
        bucket.refill(0);
        assert_eq!(spendable(&bucket), (0.0, 2.0));
        bucket.refill(1800);
        assert_eq!(spendable(&bucket), (1.0, 2.0));
        // An idle day banks an hour's worth and not a turn more.
        bucket.refill(86_400);
        assert_eq!(spendable(&bucket), (2.0, 2.0));
    }

    #[test]
    fn a_fractional_limit_holds_one_turn() {
        // Half a turn an hour still buys a bucket of one, filled over two.
        let mut bucket = limited(0.5);
        assert_eq!(spendable(&bucket), (1.0, 1.0));
        bucket.spend();
        bucket.refill(3600);
        assert_eq!(spendable(&bucket), (0.5, 1.0));
        bucket.refill(7200);
        assert_eq!(spendable(&bucket), (1.0, 1.0));
        bucket.refill(86_400);
        assert_eq!(spendable(&bucket), (1.0, 1.0));
    }

    #[test]
    fn a_changed_limit_resizes_the_bucket() {
        // Spent out at one an hour, then raised to four: the three turns the
        // new limit adds are in hand right away, not an hour from now.
        let mut bucket = limited(1.0);
        bucket.spend();
        bucket.resize(Some(4.0));
        assert_eq!(spendable(&bucket), (3.0, 4.0));

        // A full bucket grows with the limit and stops at the new ceiling.
        let mut bucket = limited(1.0);
        bucket.resize(Some(4.0));
        assert_eq!(spendable(&bucket), (4.0, 4.0));

        // A raise too small to fit another turn hands over nothing; the wait
        // still shortens, since the bucket refills faster.
        let mut bucket = limited(1.0);
        bucket.spend();
        bucket.resize(Some(1.5));
        assert_eq!(spendable(&bucket), (0.0, 1.0));

        // Lowered: what the bucket can no longer hold spills, and what fits
        // stays — slowing down mid-hour doesn't bank a debt.
        let mut bucket = limited(4.0);
        bucket.resize(Some(1.0));
        assert_eq!(spendable(&bucket), (1.0, 1.0));
        let mut bucket = limited(4.0);
        for _ in 0..4 {
            bucket.spend();
        }
        bucket.refill(450); // half a turn, at four an hour
        bucket.resize(Some(1.0));
        assert_eq!(spendable(&bucket), (0.5, 1.0));

        // An unlimited stretch isn't accounted for at all, so the limit set
        // after one starts from a full bucket however spent it was before.
        let mut bucket = limited(4.0);
        for _ in 0..4 {
            bucket.spend();
        }
        bucket.resize(None);
        assert!(bucket.allowance().is_none());
        assert!(bucket.spend().is_none());
        bucket.resize(Some(2.0));
        assert_eq!(spendable(&bucket), (2.0, 2.0));
    }

    #[test]
    fn an_empty_bucket_holds_until_the_next_turn_accrues() {
        // A turn in hand is no hold at all.
        let mut bucket = limited(2.0);
        assert_eq!(bucket.hold(), None);
        // Spent out at two an hour: the next turn is half an hour off, less
        // whatever has already been waited out.
        bucket.spend();
        bucket.spend();
        assert_eq!(bucket.hold(), Some(1800));
        bucket.refill(900);
        assert_eq!(bucket.hold(), Some(900));

        // A fractional limit stretches the wait rather than denying the turn.
        let mut slow = limited(0.5);
        slow.spend();
        assert_eq!(slow.hold(), Some(7200));

        // No limit, no hold, whatever the bucket happens to hold.
        slow.resize(None);
        assert_eq!(slow.hold(), None);
    }

    /// The turn that spends the last whole turn can leave a sliver behind;
    /// "-0.0 of 2" reads as a bug on the page.
    #[test]
    fn the_bucket_never_shows_a_negative_allowance() {
        let mut bucket = Bucket {
            available: 1.0 - f64::EPSILON,
            limit: Some(2.0),
            refilled: 0,
        };
        let left = bucket.spend().expect("a limited bucket spends a turn");
        assert_eq!((left.available, left.capacity), (0.0, 2.0));
        // A fractional limit still reads "1 / 1" rather than "1 / 0.25".
        assert_eq!(spendable(&limited(0.25)), (1.0, 1.0));
    }

    /// The page reads the bucket under the name the mirror used to carry:
    /// the figures while a limit applies, null while none does.
    #[test]
    fn the_bucket_serializes_as_the_pages_allowance() {
        let shared = Shared::new();
        let idle = serde_json::to_value(&*shared.lock()).unwrap();
        assert!(idle["allowance"].is_null());

        Status::update(&shared, |s| s.allowance.resize(Some(3.0)));
        let limited = serde_json::to_value(&*shared.lock()).unwrap();
        assert_eq!(limited["allowance"]["capacity"], 3.0);
        assert_eq!(limited["allowance"]["available"], 3.0);
    }
}
