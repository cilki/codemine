use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tracing::{info, warn};
use wait_timeout::ChildExt;

use crate::cache;
use crate::config::{Config, Forge, ForgeKind};
use crate::deliveries::{CLOCK_SLACK_SECS, Deliveries, Opened};
use crate::ledger::Ledger;
use crate::precheck;
use crate::scan::{Verdict, last_lines};
use crate::status::{
    Activity, Outcome, Savings, Shared, Status, TokenUsage, TurnRecord, epoch_now,
};
use crate::workspace;

/// What a pass may draw from. The schedule's window gates automatic turns —
/// the ones the runner picks for itself off the random draw — and not the
/// ones the forge asked for, so a pass inside the window draws from anything
/// and a pass outside it draws from the activity queue alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Draw {
    /// The activity queue first, then the random draw over the whole pool.
    Any,
    /// The activity queue alone: a review request or a comment on the bot's
    /// PR starts a turn whenever it arrives, where an automatic turn waits
    /// for the window to open.
    QueuedOnly,
}

/// Why a pass took no turn at all. The two cases differ in kind, and so has
/// to the way they are reported: one is the healthy state of a caught-up
/// runner, the other is a misconfiguration nothing but a human can clear.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Idle {
    /// Every enabled task has nothing to act on anywhere in the pool.
    NothingToDo,
    /// Not one enabled repository is listed by its forge — renamed, deleted,
    /// archived, or out of the token's reach — so there is nothing to draw
    /// from.
    NoListedRepos,
}

/// How a pass went: the draw took no turn, and why, or a turn ran —
/// whatever became of it.
pub enum Report {
    Idle(Idle),
    Ran {
        /// What the turn's log said about it, which the main loop acts on: a
        /// usage notice holds the next turn, a proxy auth failure briefly
        /// gates turns, and a refused model blocks them until another is
        /// picked. A turn that died preparing its clone never wrote a log to
        /// read, and says nothing.
        verdict: Verdict,
        /// What the turn amounted to, as its record shows it: a completed
        /// one counts toward the rate limit, and after a cancelled one the
        /// main loop skips the between-turn sleep.
        outcome: Outcome,
    },
}

/// One turn's identity, as everything that reports on it spells it: the pair
/// the draw landed on, the clone it runs in, the log it writes, and when it
/// began. Settled once and read from everywhere after, so the activity the
/// page shows while the turn runs and the record it lists afterwards cannot
/// disagree about which turn they are describing — those were two separate
/// spellings of the same five fields, built a hundred and fifty lines apart,
/// plus a third for a turn that died preparing its clone.
struct Turn<'a> {
    task: &'a str,
    forge: &'a Forge,
    repo: &'a str,
    /// What the forge comment that asked for this turn wants it to look at.
    focus: Option<crate::events::Focus>,
    /// The repository's persistent clone: the agent's working directory.
    dir: PathBuf,
    /// What earlier turns of the task on the repository left for this one,
    /// and what this one leaves for the next: see `workspace::notes_path`.
    notes: PathBuf,
    /// The turn's log under `<workspace>/logs`, which the page tails while
    /// the turn runs and serves in full once it is over.
    log_path: PathBuf,
    /// The turn's identifier: what its log is named after and what the page
    /// asks for it by. See `next_id`.
    id: u64,
    /// When the turn began by the wall clock: what the page shows, and what
    /// the token and rtk scans and the delivery listing count from.
    wall: SystemTime,
    /// ...and by a monotonic one, for the duration the record carries.
    clock: Instant,
}

impl Turn<'_> {
    fn forge_name(&self) -> &'static str {
        self.forge.kind.name()
    }

    /// The turn's start in epoch seconds.
    fn started(&self) -> u64 {
        self.wall.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
    }

    fn elapsed(&self) -> u64 {
        self.clock.elapsed().as_secs()
    }

    /// What the page shows while the turn runs.
    fn activity(&self) -> Activity {
        Activity::Running {
            task: self.task.to_owned(),
            repo: self.repo.to_owned(),
            forge: self.forge_name().into(),
            log_path: self.log_path.clone(),
            // 0 until the agent's process group exists; `run` fills it in the
            // moment it does, which is what arms the pause button.
            pgid: 0,
            paused: false,
            cancel_requested: false,
            id: self.id,
            started: self.started(),
            tokens: None,
            rtk: None,
        }
    }

    /// Hand the finished turn to the status and the draw's weights: the tail
    /// of its log becomes what an idle page shows, the turn joins the list
    /// and the totals, and the pair it ran on is weighed accordingly next
    /// draw. The one place a turn is recorded, whether it ran to the end or
    /// never got past preparing its clone — a pair whose clone won't prepare
    /// has had its turn too, and shouldn't be the very next draw.
    ///
    /// The turn is counted against its task in `ledger` too, with whatever
    /// it `opened`, which is what the page's per-task summary adds up. What
    /// comes back is the part of `opened` that is this turn's alone: the
    /// rest an earlier turn already recorded inside the clock slack.
    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        status: &Shared,
        ledger: &Deliveries,
        weights: &Ledger,
        tail: &str,
        outcome: Outcome,
        spent: Spent,
        opened: Vec<Opened>,
    ) -> Vec<Opened> {
        weights.record(self.task, self.forge.kind, self.repo, self.started(), outcome);
        let own = ledger.record_turn(
            self.forge.kind,
            self.repo,
            self.task,
            self.started(),
            spent.tokens.as_ref(),
            &opened,
        );
        let record = TurnRecord {
            task: self.task.to_owned(),
            repo: self.repo.to_owned(),
            forge: self.forge_name().into(),
            id: self.id,
            started: self.started(),
            duration_secs: self.elapsed(),
            outcome,
            tokens: spent.tokens,
            deliveries: opened,
            log_path: self.log_path.clone(),
        };
        Status::update(status, |s| {
            s.log_tail = last_lines(tail, 100).to_owned();
            s.record_turn(record, spent.rtk.as_ref());
        });
        own
    }

    /// Comment the turn's receipt on each PR and issue it opened. Best-effort
    /// like `delivered`: a forge that refuses the comment costs the receipt,
    /// never the turn.
    fn post_receipts(&self, model: &str, spent: &Spent, own: &[Opened]) {
        if own.is_empty() {
            return;
        }
        let body = crate::receipt::Receipt {
            task: self.task,
            repo: self.repo,
            model,
            started: self.started(),
            duration_secs: self.elapsed(),
            tokens: spent.tokens.as_ref(),
            rtk: spent.rtk.as_ref(),
        }
        .render();
        for opened in own {
            if let Err(err) =
                precheck::comment(self.forge, self.repo, opened.kind, opened.number, &body)
            {
                warn!("failed to post the turn receipt on {}: {err:#}", opened.url);
            }
        }
    }
}

/// Run one opencode turn against the repository's workspace clone and report
/// how it went. The clone survives across turns, and so does the turn's log
/// under `<workspace>/logs`, for as long as the process runs. `opencode` is
/// the version the agent is started with, which settles its flags, and
/// `draw` is whether the random draw is on the table or only what the forge
/// queued — see `Draw`. `cache` holds the draw back from pairs that have
/// already answered for the current state, and `weights` weighs the rest;
/// `ledger` is where what the turn delivered is counted.
#[allow(clippy::too_many_arguments)]
pub fn run(
    cfg: &Config,
    opencode: &crate::opencode::Version,
    status: &Shared,
    pending: &crate::events::Pending,
    cache: &cache::Cache,
    ledger: &Deliveries,
    weights: &Ledger,
    draw: Draw,
) -> Result<Report> {
    let forge = &cfg.forge;
    let pool = pool(forge)?;
    if pool.is_empty() {
        // An empty enabled set is a settings problem and never reaches here;
        // what does is an enabled repository the forge no longer lists —
        // renamed, deleted, or out of the token's reach. The caller reports
        // it, since it is the one that knows how to show a blocker.
        return Ok(Report::Idle(Idle::NoListedRepos));
    }
    // The precondition probes plus the skip cache, sharing one set of forge
    // lookups across everything this draw asks about.
    let probe = cache::Probe::new(cache, forge).with_pr_budget(cfg.max_open_prs);
    let now = epoch_now();
    let picked = pick(
        forge,
        &pool,
        pending,
        draw,
        |task, repo| probe.has_work(task, repo),
        |task, repo| probe.actionable(task, repo),
        |task, repo| weights.weight(task, forge.kind, repo, now),
    );
    let Some((task, repo, focus)) = picked else {
        return Ok(Report::Idle(Idle::NothingToDo));
    };

    let id = next_id();
    let (log_path, mut log) = open_log(&cfg.workspace.join("logs"), id)?;
    let turn = Turn {
        task,
        forge,
        repo,
        focus,
        dir: workspace::repo_dir(&cfg.workspace, forge.kind.name(), repo),
        notes: workspace::notes_path(&cfg.workspace, forge.kind.name(), repo, task),
        log_path,
        id,
        wall: SystemTime::now(),
        clock: Instant::now(),
    };
    info!(
        "new task: {} ({task} on {} {repo})",
        turn.dir.display(),
        turn.forge_name()
    );
    Status::update(status, |s| s.activity = turn.activity());

    // A failed preparation (deleted repository, network blip, ...) fails the
    // turn, not the runner.
    if let Err(err) = workspace::prepare(&cfg.workspace, forge, repo, &turn.dir, &log) {
        let elapsed = turn.elapsed();
        // The log is all the web UI shows of the turn, and git's own output
        // stops wherever git was stopped — a timed-out clone ends at
        // `Cloning into ...` — so the reason goes in after it.
        if let Err(write) = note(
            &mut log,
            &format!("failed to prepare {repo} after {elapsed}s: {err:#}"),
        ) {
            warn!("failed to write to the turn log: {write:#}");
        }
        let tail = read_tail(&mut log, 64 * 1024)?;
        warn!(
            "failed to prepare {repo} in {elapsed}s: {err:#}\n{}",
            last_lines(&tail, 20)
        );
        let spent = Spent::default();
        let failed = Outcome::Failed;
        turn.finish(status, ledger, weights, &tail, failed, spent, Vec::new());
        return Ok(Report::Ran {
            verdict: Verdict::default(),
            outcome: Outcome::Failed,
        });
    }

    // Whatever this task's answer depends on, read before the agent runs and
    // can move it: the default-branch commit, the notification stamp before
    // any of the feed is marked read, the conflicting PRs or review requests
    // before a force-push or a submitted review. It is the same reading the
    // draw compares a remembered skip against, memoized if the draw already
    // took it. Best-effort: without it the task is simply drawn again.
    let state = probe.state(task, repo);
    // Notes are a convenience: a turn whose notes directory can't be made
    // still runs, it just has nowhere to leave them.
    if let Err(err) = workspace::prepare_notes(&turn.notes) {
        warn!("{err:#}");
    }
    let mut child = spawn_agent(cfg, opencode, &turn, &log)?;
    // Now that the group exists, let the web UI pause and resume it.
    Status::update(status, |s| {
        if let Activity::Running { pgid, .. } = &mut s.activity {
            *pgid = child.id() as i32;
        }
    });
    let (exit, canceled, spent) = supervise(cfg, status, &turn, &mut child)?;
    let elapsed = turn.elapsed();
    if let Err(err) = workspace::cap_notes(&turn.notes, workspace::NOTES_CAP) {
        warn!("{err:#}");
    }

    let tail = read_tail(&mut log, 64 * 1024)?;
    let verdict = Verdict::read(&tail);
    let outcome = outcome_of(exit, canceled, &verdict, elapsed, &tail);

    // A task that came up empty is held back until the state it answered for
    // moves; any other outcome retires whatever was remembered, since a
    // completed turn changed something and a turn that failed, timed out, or
    // was cancelled never got to answer. A turn that died preparing returned
    // above without touching the memory: it never read `state`, so it has
    // nothing to say about whether the old mark still stands. A task that
    // keeps no memory has no `state` and nothing remembered to forget.
    match (outcome, &state) {
        (Outcome::Skipped, Some(state)) => cache.remember(task, forge.kind, repo, state),
        _ => cache.forget(task, forge.kind, repo),
    }

    let opened = match outcome {
        Outcome::Completed => delivered(&turn),
        _ => Vec::new(),
    };
    let own = turn.finish(status, ledger, weights, &tail, outcome, spent.clone(), opened);
    if cfg.post_receipts {
        turn.post_receipts(&cfg.model, &spent, &own);
    }

    Ok(Report::Ran { verdict, outcome })
}

/// What a completed turn delivered: the PRs and issues the bot opened on the
/// repository since the turn started. Only a completed turn is asked about,
/// since only one of those claims to have changed anything on the forge.
/// Best-effort: a forge that can't answer costs the turn its links and the
/// summary its count, not the turn.
fn delivered(turn: &Turn) -> Vec<Opened> {
    let since = turn.started().saturating_sub(CLOCK_SLACK_SECS);
    precheck::opened_since(turn.forge, turn.repo, since).unwrap_or_else(|err| {
        warn!(
            "failed to list what the turn opened on {}: {err:#}",
            turn.repo
        );
        Vec::new()
    })
}

/// Everything the draw may choose from: every enabled repository the forge
/// still lists and hasn't archived.
fn pool(forge: &Forge) -> Result<Vec<String>> {
    let listing = list_repos(forge)
        .with_context(|| format!("failed to list {}'s repositories", forge.kind.name()))?;
    Ok(listing
        .repos
        .into_iter()
        .filter(|repo| forge.repos.contains_key(repo))
        .collect())
}

/// The (task, repository) pair this turn runs on, or None when nothing in
/// the pool is worth one.
///
/// A run-now request goes first of all: the user named the pair, so the
/// skip memory doesn't hold it back and the task needn't be ticked for the
/// repository — only `ready`, the task's precondition, has to hold, so a
/// `rebase` with no conflicting PR still isn't spent on. A request for a
/// repository outside the pool is dropped with a warning, as is one whose
/// precondition fails. A request that came from a forge comment carries what
/// the comment asked the turn to look at, which comes back with the pair.
///
/// Fresh forge activity comes next: the watcher saw a comment or a
/// review request land, so that repository gets a respond or review turn
/// ahead of the random draw. Entries that no longer check out (repository
/// disabled or gone, the forge since switched, the activity already handled,
/// the tasks since turned off for the repository) are dropped rather than
/// requeued — the ordinary draw probes both tasks anyway. Under
/// `Draw::QueuedOnly` the queue is all there is: a pass outside the
/// schedule's window takes the turns the forge asked for and none the
/// runner would pick on its own. `check` is whether a (task, repository)
/// pair has work, which the caller answers from the prechecks and the skip
/// cache; `weigh` is how much the random draw should favour one, which it
/// answers from the ledger.
fn pick<'a>(
    forge: &'a Forge,
    pool: &'a [String],
    pending: &crate::events::Pending,
    draw: Draw,
    ready: impl Fn(&str, &str) -> bool,
    check: impl Fn(&str, &str) -> bool,
    weigh: impl Fn(&str, &str) -> f64,
) -> Option<(&'a str, &'a str, Option<crate::events::Focus>)> {
    while let Some(request) = pending.pop_request() {
        if request.kind != forge.kind {
            continue;
        }
        let crate::events::Request {
            task, repo, focus, ..
        } = request;
        let Some(repo) = pool.iter().find(|name| **name == repo) else {
            warn!(
                "run now: {repo} is not an enabled repository {} lists; dropping {task} on it",
                forge.kind.name()
            );
            continue;
        };
        if !ready(task, repo) {
            warn!("run now: {task} has nothing to act on in {repo}; dropping the request");
            continue;
        }
        match &focus {
            Some(focus) => info!(
                "run now: {task} on {repo}, as @{} asked in {}",
                focus.author, focus.url
            ),
            None => info!("run now: {task} on {repo}, as asked"),
        }
        return Some((task, repo, focus));
    }
    while let Some((kind, repo)) = pending.pop() {
        if kind != forge.kind {
            continue;
        }
        let Some(repo) = pool.iter().find(|name| **name == repo) else {
            continue;
        };
        for task in crate::events::QUEUED_TASKS {
            if forge.runs(task, repo) && check(task, repo) {
                info!("fresh activity on {repo}; drawing {task} on it first");
                return Some((task, repo, None));
            }
        }
    }
    match draw {
        Draw::Any => {
            precheck::draw(forge, pool, check, weigh).map(|(task, repo)| (task, repo, None))
        }
        Draw::QueuedOnly => None,
    }
}

/// A fresh turn id: 1 for the process's first turn, one more for each after.
/// The start second used to double as the id, which only held while turns
/// never start within the same second of each other; a counter keeps holding
/// once they can. Nothing outlives the process — the history and the logs
/// behind it are both gone on restart — so the count need not either.
fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Create the turn's log and the directory it lives in. Opened for reading as
/// well as writing: the runner tails the same handle for the page while the
/// agent appends to it.
///
/// Owner-only, like the directory holding it. A turn log is the agent's whole
/// session transcript — the contents of a private repository, the output of
/// every command it ran, and whatever of its environment a tool happened to
/// print — and the runner's own secrets are already kept out of reach of
/// other local users, so this must not be the way back in. The mode applies
/// only on creation, which is every log: `setup` clears the directory at
/// startup and `next_id` never hands out the same id twice in a process, so
/// a name can never be reused.
fn open_log(logs_dir: &Path, id: u64) -> Result<(PathBuf, File)> {
    workspace::private_dir(logs_dir)?;
    let path = logs_dir.join(format!("{id}.log"));
    let log = File::options()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    Ok((path, log))
}

/// Start the agent on the turn. codemine confines nothing itself: whatever
/// runs it (a container, a VM, a dedicated user) is the boundary, and it is
/// up to the deployment to draw one.
fn spawn_agent(
    cfg: &Config,
    opencode: &crate::opencode::Version,
    turn: &Turn,
    log: &File,
) -> Result<Child> {
    // Commits are attributed to the model that authored them; the email comes
    // from the forge's account via identity::resolve.
    let author = crate::settings::model_author_name(&cfg.model);
    Command::new("opencode")
        .arg("run")
        // Which flags keep the session private and pre-approved depends on the
        // opencode: see `Version::run_flags`.
        .args(opencode.run_flags())
        .args(["--model", &cfg.model])
        // One argument, so a clone path with spaces in it stays whole.
        .arg(crate::prompts::sweep_prompt(
            &crate::prompts::Assignment {
                task: turn.task,
                repo: turn.repo,
                forge: turn.forge_name(),
                dir: &turn.dir,
                notes: &turn.notes,
                others: &crate::prompts::other_tasks(turn.task, |task| {
                    turn.forge.runs(task, turn.repo)
                }),
            },
            crate::workspace::codegraph_available(),
            turn.focus.as_ref(),
        ))
        // Own process group, so the timeout can take down the whole tree.
        .process_group(0)
        .current_dir(&turn.dir)
        // current_dir() changes the real working directory but not the
        // inherited $PWD, and anything trusting the variable over getcwd
        // would resolve the runner's own launch directory instead.
        .env("PWD", &turn.dir)
        .env("NO_COLOR", "1")
        // rtk's telemetry is opt-in and already off; this is the hard switch,
        // so the agent can't report what it ran however rtk is configured.
        .env("RTK_TELEMETRY_DISABLED", "1")
        // Headless runs auto-reject permission prompts, so every tool the
        // agent needs has to be pre-approved (on 2.x `--auto` is the
        // fallback). Whatever runs codemine is the real boundary, and
        // legitimate work (cargo's registry, tool caches) lives outside the
        // clone, so opencode's own external-directory gate stays open too.
        .env(
            "OPENCODE_PERMISSION",
            r#"{"edit":"allow","bash":"allow","webfetch":"allow","external_directory":"allow"}"#,
        )
        // The agent's skills run gh/glab inside the session; the forge's
        // auth has to reach them since nothing is in the process env.
        .envs(cfg.forge.env())
        .env("GIT_AUTHOR_NAME", &author)
        .env("GIT_AUTHOR_EMAIL", &turn.forge.email)
        .env("GIT_COMMITTER_NAME", &author)
        .env("GIT_COMMITTER_EMAIL", &turn.forge.email)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?))
        .spawn()
        .context("failed to spawn opencode")
}

/// What a turn cost and what rtk saved it, each None when its source
/// couldn't be read.
#[derive(Default, Clone, PartialEq)]
struct Spent {
    tokens: Option<TokenUsage>,
    rtk: Option<Savings>,
}

/// Wait for the agent, killing its process group if the timeout runs out or
/// the web UI asks for the turn to be cut short. How it ended comes back as
/// the exit status (None when it was killed), whether the kill was the cancel
/// button's, and what the turn spent.
///
/// Waited in short hops rather than one long one, resampling what the turn
/// has spent between them: opencode records each message as it goes, so the
/// page can watch the cost climb instead of learning it at the end. The
/// samples come off one [`usage::Sampler`](crate::usage::Sampler), which
/// carries the sum from hop to hop and so reads only what the agent recorded
/// since the previous one. rtk's savings are resampled on the same hops.
fn supervise(
    cfg: &Config,
    status: &Shared,
    turn: &Turn,
    child: &mut Child,
) -> Result<(Option<ExitStatus>, bool, Spent)> {
    let run_start = Instant::now();
    let mut usage = crate::usage::Sampler::new(turn.wall);
    let savings = crate::savings::Sampler::new(turn.wall, &turn.dir);
    // Each figure resampled over the last good one, so a failed read
    // (opencode mid-write, say) leaves that up rather than blinking the
    // figure away — on the page as much as in the record.
    let mut resample = |last: &Spent| Spent {
        tokens: usage.sample().or_else(|| last.tokens.clone()),
        rtk: savings.sample().or_else(|| last.rtk.clone()),
    };
    let mut spent = Spent::default();
    let mut canceled = false;
    let exit = loop {
        // Checked first so a cancel clicked during preparation lands too.
        if matches!(
            status.lock().activity,
            Activity::Running {
                cancel_requested: true,
                ..
            }
        ) {
            canceled = true;
            break None;
        }
        let left = cfg.turn_timeout.saturating_sub(run_start.elapsed());
        if left.is_zero() {
            break None;
        }
        if let Some(exit) = child.wait_timeout(SAMPLE_INTERVAL.min(left))? {
            break Some(exit);
        }
        let next = resample(&spent);
        if next != spent {
            Status::update(status, |s| {
                if let Activity::Running { tokens, rtk, .. } = &mut s.activity {
                    *tokens = next.tokens.clone();
                    *rtk = next.rtk.clone();
                }
            });
            spent = next;
        }
    };
    // A killed turn takes its tree down with it, and so does one that exited:
    // the leader exiting says nothing about what it started in the
    // background, which left alone outlives the turn and keeps writing into
    // the clone.
    if workspace::kill_group(child.id() as i32) && exit.is_some() {
        warn!("the agent left processes running; stopped them");
    }
    // One last sample, after the tree is down and so after opencode has
    // written whatever it had left: the answer the turn is recorded with.
    // Nothing is lost if this one fails — the sampler still holds what the
    // hops above counted. Likewise for rtk, whose last command may have
    // finished recording only as the tree went down.
    Ok((exit, canceled, resample(&spent)))
}

/// What the finished turn amounts to, and the one line the runner says about
/// it. How it ended outranks what it said: a turn killed by the cancel button
/// or the timeout is that, whatever markers its log carries, and a nonzero
/// exit (or a session that died on an error report behind a zero one) is a
/// failure before it is anything else.
fn outcome_of(
    exit: Option<ExitStatus>,
    canceled: bool,
    verdict: &Verdict,
    elapsed: u64,
    tail: &str,
) -> Outcome {
    match exit {
        None if canceled => {
            info!("canceled after {elapsed}s");
            Outcome::Canceled
        }
        None => {
            warn!("timed out after {elapsed}s");
            Outcome::Timeout
        }
        Some(exit) if !exit.success() || verdict.died => {
            warn!(
                "failed with status {} in {elapsed}s:\n{}",
                exit.code().unwrap_or(-1),
                last_lines(tail, 20)
            );
            Outcome::Failed
        }
        Some(_) if verdict.completed => {
            info!("ok in {elapsed}s");
            Outcome::Completed
        }
        Some(_) => {
            info!("skipped in {elapsed}s");
            Outcome::Skipped
        }
    }
}

/// How often a running turn's token usage is resampled. Frequent enough to
/// read as live, rare enough that the scan is noise next to the agent.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

const PAGE_SIZE: usize = 50;

/// One forge's repository listing, as `<owner>/<repo>` paths.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Listing {
    /// The repositories that may be shown and worked on.
    pub repos: Vec<String>,
    /// Archived repositories, which are never shown or drawn. They are kept
    /// apart rather than dropped so that an enabled repository that was
    /// archived can be told from one the forge simply stopped listing: the
    /// latter stays visible in the picker, the former is hidden with the rest.
    pub archived: BTreeSet<String>,
}

/// The repositories the bot can reach on a forge. Queried fresh each turn so
/// new repositories join the pool without a restart. Names the forge reports
/// that aren't usable as a path are dropped here, and archived repositories
/// are set apart here, which is the one chokepoint every consumer draws from.
pub fn list_repos(forge: &Forge) -> Result<Listing> {
    let field = match forge.kind {
        ForgeKind::Gitea | ForgeKind::Github => "full_name",
        ForgeKind::Gitlab => "path_with_namespace",
    };
    let mut listing = Listing::default();
    for row in list_rows(forge)? {
        let Some((repo, archived)) = listed_repo(&row, field) else {
            continue;
        };
        if !safe_repo_path(&repo) {
            warn!(
                "ignoring unusable repository name from {}: {repo:?}",
                forge.kind.name()
            );
        } else if archived {
            listing.archived.insert(repo);
        } else {
            listing.repos.push(repo);
        }
    }
    Ok(listing)
}

/// The path and archived flag of one row of a forge's repository listing;
/// None for a row without the path. All three forges spell the flag
/// `archived`, and a row that doesn't carry it is taken as not archived.
fn listed_repo(row: &serde_json::Value, field: &str) -> Option<(String, bool)> {
    let repo = row[field].as_str()?.to_owned();
    Some((repo, row["archived"].as_bool().unwrap_or(false)))
}

/// Whether a forge-reported name is safe to use as the path it is treated as
/// everywhere downstream: a directory under the workspace root (which a
/// reclone wipes first), a segment of a forge API URL, and a segment of the
/// clone URL. The name
/// comes from whatever the forge's listing says, so it is remote input; a
/// `..` in it would put all three outside where they belong.
///
/// Two or more `/`-separated segments, since GitLab nests namespaces, each
/// one made of the characters the forges themselves permit in an owner or
/// repository name. `-` can't lead a segment, which no forge allows anyway
/// and which would otherwise let a name read as a flag when it is passed on
/// to a child process.
fn safe_repo_path(repo: &str) -> bool {
    let segments: Vec<&str> = repo.split('/').collect();
    segments.len() >= 2
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && !segment.starts_with('-')
                && *segment != "."
                && *segment != ".."
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        })
}

/// Ask for one page of a forge listing at a time until a short one ends it.
/// Every listing here takes `PAGE_SIZE` rows and answers with fewer only on
/// the last page, so it is the number of rows the forge sent — not what the
/// caller manages to make of them — that decides whether to ask for another.
/// Counting the kept values instead let one unusable row (a blank `tea`
/// line, a JSON object without the field) read as the end of the listing and
/// silently drop every repository after it from the pool.
fn paginate<R>(mut page: impl FnMut(usize) -> Result<Vec<R>>) -> Result<Vec<R>> {
    let mut rows = Vec::new();
    for number in 1.. {
        let batch = page(number)?;
        let full = batch.len() >= PAGE_SIZE;
        rows.extend(batch);
        if !full {
            break;
        }
    }
    Ok(rows)
}

/// Page through the forge's REST listing of the repositories the token can
/// reach, one JSON object per repository. Gitea goes over its HTTP API rather
/// than `tea repos ls`, whose output has no field for the archived flag; it
/// is the same `user/repos` endpoint tea asks. GitLab is asked to leave
/// archived projects out, while GitHub's `user/repos` has no such filter, so
/// `list_repos` reads the flag on every row either way.
fn list_rows(forge: &Forge) -> Result<Vec<serde_json::Value>> {
    paginate(|page| {
        precheck::forge_rows(
            forge,
            &format!("user/repos?limit={PAGE_SIZE}&page={page}"),
            &format!("user/repos?per_page={PAGE_SIZE}&page={page}"),
            &format!("projects?membership=true&archived=false&per_page={PAGE_SIZE}&page={page}"),
        )
    })
}

/// Append a line of the runner's own to the turn log, after whatever its
/// children wrote there, on a line of its own even when the last of that
/// output (a progress meter, a killed process) stopped mid-line.
fn note(log: &mut File, message: &str) -> Result<()> {
    let len = log.seek(SeekFrom::End(0))?;
    let mut line = String::new();
    if len > 0 {
        let mut last = [0u8; 1];
        log.seek(SeekFrom::End(-1))?;
        log.read_exact(&mut last)?;
        if last[0] != b'\n' {
            line.push('\n');
        }
    }
    line.push_str(&format!("codemine: {message}\n"));
    log.seek(SeekFrom::End(0))?;
    log.write_all(line.as_bytes())?;
    Ok(())
}

/// The last `limit` bytes of the file, lossily decoded.
fn read_tail(file: &mut std::fs::File, limit: u64) -> Result<String> {
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(limit)))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::process::ExitStatusExt;

    use anyhow::{Result, bail};
    use serde_json::json;

    use super::{
        Draw, ExitStatus, Outcome, PAGE_SIZE, Verdict, listed_repo, note, open_log, outcome_of,
        paginate, pick, read_tail, safe_repo_path,
    };
    use crate::config::{Forge, ForgeKind};
    use crate::events::Pending;

    /// Every pair weighted alike: the queue tests are about what the queue
    /// does ahead of the draw, not how the draw orders what is left.
    fn alike(_: &str, _: &str) -> f64 {
        1.0
    }

    /// Fresh activity is drawn whether or not the window is open, and
    /// `review` asks before `respond`.
    #[test]
    fn a_queued_repository_is_drawn_inside_and_outside_the_window() {
        for draw in [Draw::Any, Draw::QueuedOnly] {
            let (forge, pool) =
                Forge::sweep(&["me/one", "me/two"], &["review", "respond", "rebase"]);
            let pending = Pending::new();
            pending.push(ForgeKind::Github, "me/two".into());
            let picked = pick(
                &forge,
                &pool,
                &pending,
                draw,
                |_, _| true,
                |_, _| true,
                alike,
            );
            assert_eq!(picked, Some(("review", "me/two", None)), "{draw:?}");
            assert!(pending.is_empty());
        }
    }

    /// Outside the window only the forge's own asks get a turn: with nothing
    /// queued the pass draws nothing, and probes nothing either.
    #[test]
    fn off_hours_takes_no_automatic_turn() {
        let (forge, pool) = Forge::sweep(&["me/repo"], &["review", "respond", "rebase"]);
        let pending = Pending::new();
        let probed = std::cell::Cell::new(0);
        let picked = pick(
            &forge,
            &pool,
            &pending,
            Draw::QueuedOnly,
            |_, _| true,
            |_, _| {
                probed.set(probed.get() + 1);
                true
            },
            alike,
        );
        assert_eq!(picked, None);
        assert_eq!(probed.get(), 0);
    }

    /// A queued repository whose activity turns out to be handled already is
    /// dropped, and off hours that is the end of the pass — the random draw
    /// is not consulted. Inside the window it is.
    #[test]
    fn a_handled_queue_entry_falls_through_to_the_draw_only_inside_the_window() {
        let (forge, pool) = Forge::sweep(&["me/repo"], &["review", "respond", "rebase"]);
        let queued_only = |task: &str, _: &str| crate::events::QUEUED_TASKS.contains(&task);

        let pending = Pending::new();
        pending.push(ForgeKind::Github, "me/repo".into());
        let picked = pick(
            &forge,
            &pool,
            &pending,
            Draw::QueuedOnly,
            |_, _| true,
            |_, _| false,
            alike,
        );
        assert_eq!(picked, None);
        assert!(pending.is_empty());

        let pending = Pending::new();
        pending.push(ForgeKind::Github, "me/repo".into());
        let seen = std::cell::RefCell::new(Vec::new());
        let picked = pick(
            &forge,
            &pool,
            &pending,
            Draw::Any,
            |_, _| true,
            |task, repo| {
                seen.borrow_mut().push(task.to_owned());
                !queued_only(task, repo)
            },
            alike,
        );
        assert_eq!(picked, Some(("rebase", "me/repo", None)));
        assert!(seen.borrow().iter().any(|task| task == "rebase"));
    }

    /// Entries that no longer check out are dropped rather than drawn: the
    /// forge since switched, or the repository left the pool.
    #[test]
    fn stale_queue_entries_are_dropped() {
        let (forge, pool) = Forge::sweep(&["me/repo"], &["review", "respond"]);
        let pending = Pending::new();
        pending.push(ForgeKind::Gitea, "me/repo".into());
        pending.push(ForgeKind::Github, "me/gone".into());
        let picked = pick(
            &forge,
            &pool,
            &pending,
            Draw::QueuedOnly,
            |_, _| true,
            |_, _| true,
            alike,
        );
        assert_eq!(picked, None);
        assert!(pending.is_empty());
    }

    fn request(repo: &str, task: &'static str) -> crate::events::Request {
        crate::events::Request {
            kind: ForgeKind::Github,
            repo: repo.into(),
            task,
            focus: None,
        }
    }

    /// A run-now request is taken ahead of queued activity and the draw, off
    /// hours too, with the skip memory ignored and the task not even ticked
    /// for the repository.
    #[test]
    fn a_requested_pair_goes_first_whatever_the_memory_says() {
        for draw in [Draw::Any, Draw::QueuedOnly] {
            let (forge, pool) = Forge::sweep(&["me/one", "me/two"], &["review", "respond"]);
            let pending = Pending::new();
            pending.push(ForgeKind::Github, "me/one".into());
            pending.request(request("me/two", "docs"));
            let picked = pick(
                &forge,
                &pool,
                &pending,
                draw,
                |_, _| true,
                |_, _| false,
                alike,
            );
            assert_eq!(picked, Some(("docs", "me/two", None)), "{draw:?}");
            // The activity is still queued for the turn after.
            assert!(!pending.is_empty());
            assert_eq!(pending.pop(), Some((ForgeKind::Github, "me/one".into())));
        }
    }

    /// A request from a forge comment hands its focus on with the pair, so
    /// the turn's prompt can say what was asked.
    #[test]
    fn a_requested_pair_carries_its_focus() {
        let (forge, pool) = Forge::sweep(&["me/repo"], &["review"]);
        let pending = Pending::new();
        let focus = crate::events::Focus {
            author: "me".into(),
            url: "https://example.com/me/repo/issues/1#issuecomment-2".into(),
            text: "the port".into(),
        };
        pending.request(crate::events::Request {
            focus: Some(focus.clone()),
            ..request("me/repo", "docs")
        });
        let picked = pick(
            &forge,
            &pool,
            &pending,
            Draw::Any,
            |_, _| true,
            |_, _| false,
            alike,
        );
        assert_eq!(picked, Some(("docs", "me/repo", Some(focus))));
    }

    /// A request is still held to its task's precondition, and one that
    /// can't be honoured — another forge's, a repository outside the pool —
    /// is dropped, leaving the rest of the pick to go on.
    #[test]
    fn a_request_that_cannot_run_is_dropped() {
        let (forge, pool) = Forge::sweep(&["me/repo"], &["review", "respond"]);
        let pending = Pending::new();
        pending.request(request("me/repo", "rebase"));
        pending.request(request("me/gone", "docs"));
        pending.request(crate::events::Request {
            kind: ForgeKind::Gitea,
            ..request("me/repo", "docs")
        });
        pending.push(ForgeKind::Github, "me/repo".into());
        let ready = |task: &str, _: &str| task != "rebase";
        let picked = pick(
            &forge,
            &pool,
            &pending,
            Draw::QueuedOnly,
            ready,
            |_, _| true,
            alike,
        );
        assert_eq!(picked, Some(("review", "me/repo", None)));
        assert!(pending.is_empty());
    }

    fn verdict(completed: bool, died: bool) -> Verdict {
        Verdict {
            completed,
            died,
            ..Verdict::default()
        }
    }

    /// How a turn ended outranks what its log says, and the log only gets the
    /// last word once the agent exited cleanly and quietly. Every one of
    /// these arms decides something: `Completed` is the only outcome that
    /// counts toward the rate limit, `Skipped` the only one a skip is
    /// remembered against, and `Canceled` the only one the next turn starts
    /// right after.
    #[test]
    fn how_a_turn_ended_outranks_what_its_log_says() {
        let exited = |code: i32| Some(ExitStatus::from_raw(code << 8));

        // Killed: the cancel button is told apart from the timeout, and
        // neither reads the log — a turn that printed the marker and was then
        // cut off did not complete.
        let completed = verdict(true, false);
        assert_eq!(outcome_of(None, true, &completed, 1, ""), Outcome::Canceled);
        assert_eq!(outcome_of(None, false, &completed, 1, ""), Outcome::Timeout);

        // A nonzero exit is a failure, and so is a clean exit whose session
        // died on an error: opencode exits 0 on a provider refusing the
        // request, which is the whole reason the log is scanned at all.
        assert_eq!(
            outcome_of(exited(1), false, &completed, 1, ""),
            Outcome::Failed
        );
        assert_eq!(
            outcome_of(exited(0), false, &verdict(true, true), 1, ""),
            Outcome::Failed
        );

        // Clean and quiet: the marker is what separates a turn that changed
        // something from one that answered and found nothing.
        assert_eq!(
            outcome_of(exited(0), false, &completed, 1, ""),
            Outcome::Completed
        );
        assert_eq!(
            outcome_of(exited(0), false, &verdict(false, false), 1, ""),
            Outcome::Skipped
        );
    }

    /// Paging is driven by what the forge sent, not by what survives the
    /// caller's filtering: a full page of rows the caller throws away still
    /// means there is more listing behind it. Counting the kept values made
    /// one unusable row truncate the repository pool, and an enabled
    /// repository that falls out of the pool silently stops being swept.
    #[test]
    fn paging_ends_on_a_short_page_not_on_an_unusable_row() {
        let asked = std::cell::RefCell::new(Vec::new());
        let rows = paginate(|page| {
            asked.borrow_mut().push(page);
            Ok(match page {
                1 | 2 => vec![page; PAGE_SIZE],
                _ => vec![page; 3],
            })
        })
        .unwrap();
        assert_eq!(*asked.borrow(), [1, 2, 3]);
        assert_eq!(rows.len(), 2 * PAGE_SIZE + 3);

        // A listing that ends on an exactly-full page costs one more, empty
        // request rather than being guessed at.
        let asked = std::cell::RefCell::new(0);
        let rows = paginate(|page| {
            *asked.borrow_mut() += 1;
            Ok(match page {
                1 => vec![0; PAGE_SIZE],
                _ => Vec::new(),
            })
        })
        .unwrap();
        assert_eq!((*asked.borrow(), rows.len()), (2, PAGE_SIZE));

        // A failed page fails the whole listing: a half-read pool would read
        // as repositories the forge no longer lists.
        assert!(paginate(|_| -> Result<Vec<usize>> { bail!("forge said no") }).is_err());
    }

    /// The log and the directory under it are both closed to other local
    /// users: a transcript of a private repository's build and review is not
    /// public reading just because it landed on disk.
    #[test]
    fn the_turn_log_and_its_directory_are_owner_only() {
        let workspace = tempfile::tempdir().unwrap();
        let logs = workspace.path().join("logs");
        let (path, _file) = open_log(&logs, 1_700_000_000).unwrap();
        assert!(path.starts_with(&logs));
        fn mode(path: &std::path::Path) -> u32 {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777
        }
        assert_eq!(mode(&logs), 0o700, "{:o}", mode(&logs));
        assert_eq!(mode(&path), 0o600, "{:o}", mode(&path));
    }

    /// A failed preparation says why at the end of the turn log, on a line
    /// of its own: a clone killed by the timeout used to leave the log at
    /// `Cloning into ...` with nothing after it, and the turn just Failed.
    #[test]
    fn the_runner_notes_land_after_what_the_children_wrote() {
        let workspace = tempfile::tempdir().unwrap();
        let (path, mut log) = open_log(&workspace.path().join("logs"), 1_700_000_001).unwrap();
        // An empty log gets no leading blank line.
        note(&mut log, "first").unwrap();
        // A child holding a descriptor of its own, cut off mid-line.
        let mut child = log.try_clone().unwrap();
        child.write_all(b"Cloning into 'x'...").unwrap();
        note(&mut log, "git timed out after 1800s").unwrap();

        let tail = read_tail(&mut log, 1024).unwrap();
        assert_eq!(
            tail,
            "codemine: first\nCloning into 'x'...\ncodemine: git timed out after 1800s\n"
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), tail);
    }

    /// A repository name is a path component and a URL segment, so anything
    /// that could escape the workspace has to be rejected before it gets
    /// that far.
    #[test]
    fn unsafe_repo_names_are_rejected() {
        for repo in [
            "cilki/codemine",
            "cilki/code.mine",
            "cilki/code-mine_2",
            "group/subgroup/project", // GitLab nests namespaces
            "_owner/repo",
        ] {
            assert!(safe_repo_path(repo), "{repo} should be usable");
        }
        for repo in [
            "",
            "codemine",             // no owner
            "cilki/",               // no name
            "/codemine",            // no owner
            "cilki//codemine",      // empty segment
            "../etc",               // escapes the workspace root
            "cilki/../../../etc",   // escapes it from further in
            "cilki/..",             // escapes it by one level
            "cilki/.",              // resolves to the owner directory
            "-cilki/codemine",      // reads as a flag to a child process
            "cilki/-codemine",      // likewise
            "cilki/code mine",      // whitespace
            "cilki/code\\mine",     // separator on other platforms
            "cilki/code\nmine",     // header and log injection
            "cilki/code\0mine",     // truncates a C string
            "cilki/repo?ref=other", // another forge API query
        ] {
            assert!(!safe_repo_path(repo), "{repo:?} should be rejected");
        }
    }

    /// Archived repositories are never shown or worked on, so every listing
    /// row has to say whether it is one. All three forges spell the flag
    /// `archived`; a row without it is an ordinary repository, and a row
    /// without the path is no repository at all.
    #[test]
    fn listing_rows_carry_the_archived_flag() {
        let row = |value: serde_json::Value| listed_repo(&value, "full_name");
        assert_eq!(
            row(json!({ "full_name": "cilki/turbine", "archived": false })),
            Some(("cilki/turbine".into(), false))
        );
        assert_eq!(
            row(json!({ "full_name": "cilki/turbine", "archived": true })),
            Some(("cilki/turbine".into(), true))
        );
        assert_eq!(
            row(json!({ "full_name": "cilki/turbine" })),
            Some(("cilki/turbine".into(), false))
        );
        assert_eq!(row(json!({ "name": "turbine", "archived": true })), None);
        assert_eq!(
            listed_repo(
                &json!({ "path_with_namespace": "group/sub/project", "archived": true }),
                "path_with_namespace"
            ),
            Some(("group/sub/project".into(), true))
        );
    }
}
