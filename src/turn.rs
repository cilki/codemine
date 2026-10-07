use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};
use wait_timeout::ChildExt;

use crate::cache;
use crate::config::{Config, Forge, ForgeKind};
use crate::precheck;
use crate::scan;
use crate::status::{Activity, Outcome, Shared, Status, TokenUsage, TurnRecord, epoch_now};
use crate::workspace;

#[derive(Default)]
pub enum Backoff {
    /// The usage window is exhausted until this epoch.
    UsageLimit(u64),
    #[default]
    Normal,
}

/// Why a pass took no turn at all. The two cases differ in kind, and so has
/// to the way they are reported: one is the healthy state of a caught-up
/// runner, the other is a misconfiguration nothing but a human can clear.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Idle {
    /// Every enabled task has nothing to act on anywhere in the pool.
    NothingToDo,
    /// Not one enabled repository is listed by its forge — renamed, deleted,
    /// or out of the token's reach — so there is nothing to draw from.
    NoListedRepos,
}

/// A turn that never started reports exactly this: nothing to back off from,
/// nothing spent, nothing to gate on, nothing cancelled.
#[derive(Default)]
pub struct Report {
    pub backoff: Backoff,
    /// Why the draw came up empty, when it did. `None` means a turn ran —
    /// whatever became of it.
    pub idle: Option<Idle>,
    /// The agent reported real forge changes with a `TASK COMPLETED` marker;
    /// only these turns count toward the hourly limit.
    pub completed: bool,
    /// The turn died on proxy authentication (a rejected client key or a
    /// proxy with no usable login); the main loop briefly gates further
    /// turns so a broken setup can't burn them back to back.
    pub auth_error: bool,
    /// The web UI cancelled the turn; the main loop skips the between-turn
    /// sleep so the next one starts right away.
    pub canceled: bool,
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
    /// The repository's persistent clone: the agent's working directory, and
    /// the only place the write sandbox opens up.
    dir: PathBuf,
    /// The turn's log under `<workspace>/logs`, which the page tails while
    /// the turn runs and serves in full once it is over.
    log_path: PathBuf,
    /// Epoch seconds; doubles as the turn's identifier in the web UI.
    started: u64,
    /// The same moment by the wall clock, which the token scan needs: it asks
    /// opencode for the messages recorded since.
    wall: SystemTime,
    /// ...and by a monotonic one, for the duration the record carries.
    clock: Instant,
}

impl Turn<'_> {
    fn forge_name(&self) -> &'static str {
        self.forge.kind.name()
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
            workspace: self.dir.display().to_string(),
            log_path: self.log_path.clone(),
            // 0 until the agent's process group exists; `run` fills it in the
            // moment it does, which is what arms the pause button.
            pgid: 0,
            started: self.started,
            tokens: None,
        }
    }

    /// Hand the finished turn to the status: the tail of its log becomes what
    /// an idle page shows, and the turn joins the list and the totals. The
    /// one place a turn is recorded, whether it ran to the end or never got
    /// past preparing its clone.
    fn finish(
        &self,
        status: &Shared,
        tail: &str,
        outcome: Outcome,
        elapsed: u64,
        tokens: Option<TokenUsage>,
    ) {
        let record = TurnRecord {
            task: self.task.to_owned(),
            repo: self.repo.to_owned(),
            forge: self.forge_name().into(),
            started: self.started,
            duration_secs: elapsed,
            outcome,
            tokens,
            log_path: self.log_path.clone(),
        };
        Status::update(status, |s| {
            s.paused = false;
            s.log_tail = last_lines(tail, 100).to_owned();
            s.record_turn(record);
        });
    }
}

/// Run one opencode turn against the repository's persistent workspace clone
/// and report how it went. The workspace survives across turns, and so does
/// the turn's log under `<workspace>/logs`, for as long as the process runs.
pub fn run(
    cfg: &Config,
    status: &Shared,
    pending: &crate::events::Pending,
    cache: &cache::Cache,
) -> Result<Report> {
    let pool = pool(cfg)?;
    if pool.is_empty() {
        // An empty enabled set is a settings problem and never reaches here;
        // what does is an enabled repository the forge no longer lists —
        // renamed, deleted, or out of the token's reach. The caller reports
        // it, since it is the one that knows how to show a blocker.
        return Ok(Report {
            idle: Some(Idle::NoListedRepos),
            ..Default::default()
        });
    }
    // The precondition probes plus the skip cache, sharing one set of forge
    // lookups across everything this draw asks about.
    let probe = cache::Probe::new(cache);
    let Some((task, forge, repo)) = pick(cfg, &pool, pending, &probe) else {
        return Ok(Report {
            idle: Some(Idle::NothingToDo),
            ..Default::default()
        });
    };

    let started = epoch_now();
    let (log_path, mut log) = open_log(&cfg.workspace.join("logs"), started)?;
    let turn = Turn {
        task,
        forge,
        repo,
        dir: workspace::repo_dir(&cfg.workspace, forge.kind.name(), repo),
        log_path,
        started,
        wall: SystemTime::now(),
        clock: Instant::now(),
    };
    info!(
        "new task: {} ({task} on {} {repo})",
        turn.dir.display(),
        turn.forge_name()
    );
    Status::update(status, |s| {
        s.paused = false;
        // A leftover request from between turns must not fell this one.
        s.cancel_requested = false;
        s.activity = turn.activity();
    });

    // A failed preparation (deleted repository, network blip, ...) fails the
    // turn, not the runner.
    if let Err(err) = workspace::prepare(cfg, forge, repo, &log) {
        let elapsed = turn.elapsed();
        let tail = read_tail(&mut log, 64 * 1024)?;
        warn!(
            "failed to prepare {repo} in {elapsed}s: {err:#}\n{}",
            last_lines(&tail, 20)
        );
        turn.finish(status, &tail, Outcome::Failed, elapsed, None);
        return Ok(Report::default());
    }

    let state = answered_state(cfg, &turn, &probe);
    let mut child = spawn_agent(cfg, &turn, &log)?;
    // Now that the group exists, let the web UI pause and resume it.
    Status::update(status, |s| {
        if let Activity::Running { pgid, .. } = &mut s.activity {
            *pgid = child.id() as i32;
        }
    });
    let (exit, canceled, sampled) = supervise(cfg, status, &turn, &mut child)?;
    let elapsed = turn.elapsed();

    let tail = read_tail(&mut log, 64 * 1024)?;
    let verdict = Verdict::read(&tail);
    let outcome = outcome_of(exit, canceled, &verdict, elapsed, &tail);

    // A task that came up empty is held back until the state it answered for
    // moves; any other outcome retires whatever was remembered, since a
    // completed turn changed something and a turn that failed, timed out, or
    // was cancelled never got to answer. A turn that died preparing returned
    // above without touching the memory: it never read `state`, so it has
    // nothing to say about whether the old mark still stands.
    if cache::basis(task).is_some() {
        match (outcome, &state) {
            (Outcome::Skipped, Some(state)) => cache.remember(task, forge.kind, repo, state),
            _ => cache.forget(task, forge.kind, repo),
        }
    }

    // The last reading can fail like any other (opencode mid-write, or its
    // database gone with the session); the samples taken while the turn ran
    // are the next best figure, and better than a row with no cost on it.
    let tokens = crate::usage::collect_since(turn.wall).or(sampled);
    turn.finish(status, &tail, outcome, elapsed, tokens);

    Ok(Report {
        backoff: match verdict.usage_limit {
            Some(epoch) => Backoff::UsageLimit(epoch),
            None => Backoff::Normal,
        },
        // This pass took its turn, whatever came of it.
        idle: None,
        completed: verdict.completed,
        auth_error: verdict.auth_error,
        canceled,
    })
}

/// Everything the draw may choose from: every enabled repository its forge
/// still lists, paired with that forge.
fn pool(cfg: &Config) -> Result<Vec<(&Forge, String)>> {
    let mut pool = Vec::new();
    for forge in &cfg.forges {
        pool.extend(
            list_repos(forge)?
                .into_iter()
                .filter(|repo| forge.enabled_repos.contains(repo))
                .map(|repo| (forge, repo)),
        );
    }
    Ok(pool)
}

/// The (task, forge, repository) triple this turn runs on, or None when
/// nothing in the pool is worth one.
///
/// Fresh forge activity jumps the queue: the watcher saw a comment land, so
/// that repository gets a feedback turn ahead of the random draw. Entries
/// that no longer check out (repository disabled or gone, the feedback
/// already handled, the task since disabled) are dropped rather than
/// requeued — the ordinary draw probes feedback anyway.
fn pick<'a>(
    cfg: &'a Config,
    pool: &'a [(&'a Forge, String)],
    pending: &crate::events::Pending,
    probe: &cache::Probe,
) -> Option<(&'a str, &'a Forge, &'a str)> {
    let feedback = cfg.tasks.iter().any(|task| task == "feedback");
    while let Some((kind, repo)) = pending.pop() {
        if !feedback {
            continue;
        }
        let Some((forge, repo)) = pool
            .iter()
            .find(|(forge, name)| forge.kind == kind && *name == repo)
            .map(|(forge, name)| (*forge, name.as_str()))
        else {
            continue;
        };
        if probe.actionable("feedback", forge, repo) {
            info!("fresh activity on {repo}; drawing it first");
            return Some(("feedback", forge, repo));
        }
    }
    precheck::draw(&cfg.tasks, pool, |task, forge, repo| {
        probe.actionable(task, forge, repo)
    })
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
/// startup, so a name can never be reused.
fn open_log(logs_dir: &Path, started: u64) -> Result<(PathBuf, File)> {
    workspace::private_dir(logs_dir)?;
    let path = logs_dir.join(format!("{started}.log"));
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

/// Whatever this task's answer depends on, read before the agent runs: the
/// commit it is about to read, taken while the clone is still on a clean
/// default branch — after the turn it could be sitting on whatever branch the
/// agent left behind — the notification stamp it is about to read, taken
/// before the agent marks any of the feed read, or the conflicting PRs it is
/// about to look at, taken before any force-push of its own moves them (the
/// probe memoized them when it answered the draw). A task that skips is
/// remembered against it. Best-effort: without it the task is simply drawn
/// again next time.
fn answered_state(cfg: &Config, turn: &Turn, probe: &cache::Probe) -> Option<String> {
    match cache::basis(turn.task)? {
        cache::Basis::Head => match workspace::head_sha(&cfg.workspace, &turn.dir) {
            Ok(head) => Some(head),
            Err(err) => {
                warn!(
                    "failed to read {}'s head commit: {err:#}",
                    turn.dir.display()
                );
                None
            }
        },
        cache::Basis::Feed | cache::Basis::Prs => probe.state(turn.task, turn.forge, turn.repo),
    }
}

/// Start the agent on the turn, inside the Landlock write sandbox so it
/// cannot work from any checkout other than the assigned clone.
fn spawn_agent(cfg: &Config, turn: &Turn, log: &File) -> Result<Child> {
    let mut argv = crate::sandbox::wrap(&turn.dir, &turn.log_path);
    argv.extend(workspace::throttle_argv(cfg));
    argv.extend(
        [
            "opencode",
            "run",
            "--command",
            crate::prompts::SWEEP_COMMAND,
            "--model",
            &cfg.model,
            turn.task,
            turn.repo,
            turn.forge_name(),
        ]
        .map(String::from),
    );
    // $4 in the sweep command: where the agent must work.
    argv.push(turn.dir.display().to_string());
    Command::new(&argv[0])
        .args(&argv[1..])
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
        // agent needs has to be pre-approved. The Landlock sandbox is the
        // real boundary, and legitimate work (cargo's registry, tool caches)
        // lives outside the clone, so opencode's own external-directory gate
        // stays open too.
        .env(
            "OPENCODE_PERMISSION",
            r#"{"edit":"allow","bash":"allow","webfetch":"allow","external_directory":"allow"}"#,
        )
        // The agent's skills run gh/glab inside the session; every configured
        // forge's auth has to reach them since nothing is in the process env.
        .envs(cfg.forges.iter().flat_map(|forge| forge.env()))
        .env("GIT_AUTHOR_NAME", &cfg.author_name)
        .env("GIT_AUTHOR_EMAIL", &turn.forge.email)
        .env("GIT_COMMITTER_NAME", &cfg.author_name)
        .env("GIT_COMMITTER_EMAIL", &turn.forge.email)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?))
        .spawn()
        .with_context(|| format!("failed to spawn {}", argv[0]))
}

/// Wait for the agent, killing its process group if the timeout runs out or
/// the web UI asks for the turn to be cut short. How it ended comes back as
/// the exit status (None when it was killed), whether the kill was the cancel
/// button's, and the last token sample taken while it ran.
///
/// Waited in short hops rather than one long one, resampling what the turn
/// has spent between them: opencode records each message as it goes, so the
/// page can watch the cost climb instead of learning it at the end.
fn supervise(
    cfg: &Config,
    status: &Shared,
    turn: &Turn,
    child: &mut Child,
) -> Result<(Option<ExitStatus>, bool, Option<TokenUsage>)> {
    let run_start = Instant::now();
    let mut sampled = None;
    let mut canceled = false;
    let exit = loop {
        // Checked first so a cancel clicked during preparation lands too.
        if status.lock().cancel_requested {
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
        // A failed read (opencode mid-write, say) leaves the last good
        // sample up rather than blinking the figure away.
        let tokens = crate::usage::collect_since(turn.wall);
        if tokens.is_some() && tokens != sampled {
            sampled = tokens.clone();
            Status::update(status, |s| {
                if let Activity::Running { tokens: live, .. } = &mut s.activity {
                    *live = tokens;
                }
            });
        }
    };
    if exit.is_none() {
        workspace::kill_group(child)?;
    }
    Ok((exit, canceled, sampled))
}

/// What the turn's log says about how it went. opencode exits 0 even when the
/// turn died on an error, so its exit status settles almost nothing and the
/// log is the witness for all four of these — read once, here, rather than
/// scanned again wherever one of the answers is wanted. Three of the four
/// were read in two different places apiece, and two of those re-sliced the
/// same closing lines to do it.
struct Verdict {
    /// The agent reported real forge changes with a `TASK COMPLETED` marker;
    /// only these turns count toward the hourly limit.
    completed: bool,
    /// opencode printed an error report, which it does while still exiting 0.
    errored: bool,
    /// The epoch an exhausted usage window reopens at, if the log named one.
    usage_limit: Option<u64>,
    /// The turn died on proxy authentication.
    auth_error: bool,
}

impl Verdict {
    fn read(tail: &str) -> Verdict {
        // The markers and the error report are the agent's last word, so they
        // are looked for in the closing lines; the usage notice and the auth
        // failure are whatever went wrong mid-turn and can sit anywhere in
        // the tail.
        let closing = last_lines(tail, 50);
        Verdict {
            completed: scan::reported_completed(closing),
            errored: scan::has_error_report(closing),
            usage_limit: scan::usage_limit_epoch(tail),
            auth_error: scan::auth_error(tail),
        }
    }
}

/// What the finished turn amounts to, and the one line the runner says about
/// it. How it ended outranks what it said: a turn killed by the cancel button
/// or the timeout is that, whatever markers its log carries, and a nonzero
/// exit (or an error report behind a zero one) is a failure before it is
/// anything else.
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
        Some(exit) if !exit.success() || verdict.errored => {
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

/// The repositories the bot can reach on a forge, as `<owner>/<repo>`.
/// Queried fresh each turn so new repositories join the pool without a
/// restart. Names the forge reports that aren't usable as a path are dropped
/// here, which is the one chokepoint every consumer draws from.
pub fn list_repos(forge: &Forge) -> Result<Vec<String>> {
    let listed = match forge.kind {
        ForgeKind::Gitea => list_gitea_repos()?,
        ForgeKind::Github => list_api_repos(forge, "gh", "user/repos", "full_name")?,
        ForgeKind::Gitlab => list_api_repos(
            forge,
            "glab",
            "projects?membership=true",
            "path_with_namespace",
        )?,
    };
    let mut repos = Vec::with_capacity(listed.len());
    for repo in listed {
        match safe_repo_path(&repo) {
            true => repos.push(repo),
            false => warn!(
                "ignoring unusable repository name from {}: {repo:?}",
                forge.kind.name()
            ),
        }
    }
    Ok(repos)
}

/// Whether a forge-reported name is safe to use as the path it is treated as
/// everywhere downstream: a directory under the workspace root (which the
/// Landlock ruleset then opens for writing, and which a reclone wipes first),
/// a segment of a forge API URL, and a segment of the clone URL. The name
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

/// `tea` prints the requested fields whitespace-separated with no header, so
/// owner and name come back as two columns and are rejoined into the
/// `<owner>/<repo>` path the rest of the runner expects.
fn list_gitea_repos() -> Result<Vec<String>> {
    let lines = paginate(|page| {
        let output = Command::new("tea")
            .args([
                "repos",
                "ls",
                "--output",
                "simple",
                "--fields",
                "owner,name",
            ])
            .args([
                "--limit",
                &PAGE_SIZE.to_string(),
                "--page",
                &page.to_string(),
            ])
            .stdin(Stdio::null())
            .output()
            .context("failed to run tea")?;
        if !output.status.success() {
            bail!(
                "tea repos ls exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        // Blank lines are tea's own padding rather than listed repositories,
        // so they don't count toward the page either way.
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect())
    })?;
    Ok(lines
        .iter()
        .filter_map(|line| gitea_repo_path(line))
        .collect())
}

/// The `<owner>/<repo>` path from one `tea repos ls` line; None for the blank
/// and malformed lines tea can emit, since neither field can contain spaces.
fn gitea_repo_path(line: &str) -> Option<String> {
    let mut fields = line.split_whitespace();
    let (owner, name) = (fields.next()?, fields.next()?);
    fields.next().is_none().then(|| format!("{owner}/{name}"))
}

/// Page through a REST listing and pluck one field per repository. gh and
/// glab expose the same `api` subcommand shape, which `precheck::api_json`
/// already drives — including authenticating from the forge's environment
/// variables — so only the paging and the field belong here.
fn list_api_repos(forge: &Forge, program: &str, path: &str, field: &str) -> Result<Vec<String>> {
    let separator = if path.contains('?') { '&' } else { '?' };
    let rows = paginate(|page| {
        let listing = precheck::api_json(
            forge,
            program,
            &format!("{path}{separator}per_page={PAGE_SIZE}&page={page}"),
        )?;
        match listing {
            serde_json::Value::Array(rows) => Ok(rows),
            _ => bail!("{program} api returned unexpected output"),
        }
    })?;
    Ok(rows
        .iter()
        .filter_map(|row| row[field].as_str().map(String::from))
        .collect())
}

/// The last `limit` bytes of the file, lossily decoded.
pub fn read_tail(file: &mut std::fs::File, limit: u64) -> Result<String> {
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(limit)))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn last_lines(text: &str, count: usize) -> &str {
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    match trimmed.rmatch_indices('\n').nth(count.saturating_sub(1)) {
        Some((at, _)) => &text[at + 1..],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use anyhow::{Result, bail};

    use super::{
        ExitStatus, Outcome, PAGE_SIZE, Verdict, gitea_repo_path, last_lines, open_log, outcome_of,
        paginate, safe_repo_path,
    };

    fn verdict(completed: bool, errored: bool) -> Verdict {
        Verdict {
            completed,
            errored,
            usage_limit: None,
            auth_error: false,
        }
    }

    /// How a turn ended outranks what its log says, and the log only gets the
    /// last word once the agent exited cleanly and quietly. Every one of
    /// these arms decides something: `Completed` is the only outcome that
    /// spends from the hourly limit, `Skipped` the only one a skip is
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

        // A nonzero exit is a failure, and so is a clean exit that reported
        // an error: opencode exits 0 on an unknown command or a missing
        // model, which is the whole reason the log is scanned at all.
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

    /// The log is read once, and where each thing is looked for matters. The
    /// agent's markers are its last word, so a `TASK COMPLETED` left far
    /// above the end — by a turn that reported and then ran on for pages — is
    /// not its answer. A usage notice or an auth failure is whatever went
    /// wrong mid-turn, so those are looked for in the whole tail.
    #[test]
    fn the_log_is_read_for_markers_at_the_end_and_failures_anywhere() {
        let filler = "filler\n".repeat(60);

        let verdict = Verdict::read(&format!(
            "TASK COMPLETED\nusage limit reached|1757000000\nauthentication_error\n{filler}"
        ));
        assert_eq!(verdict.usage_limit, Some(1_757_000_000));
        assert!(verdict.auth_error);
        assert!(!verdict.completed, "a buried marker is not the answer");

        let verdict = Verdict::read(&format!("{filler}TASK COMPLETED\n"));
        assert!(verdict.completed);
        assert!(!verdict.errored);
        assert_eq!(verdict.usage_limit, None);
        assert!(!verdict.auth_error);

        let verdict = Verdict::read(&format!("{filler}Error: unknown command\n"));
        assert!(verdict.errored);
        assert!(!verdict.completed);
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

    /// A repository name is a path component, a URL segment, and a Landlock
    /// write root, so anything that could escape the workspace has to be
    /// rejected before it gets that far.
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

    #[test]
    fn gitea_lines_become_owner_repo_paths() {
        assert_eq!(
            gitea_repo_path("cilki turbine"),
            Some("cilki/turbine".into())
        );
        assert_eq!(
            gitea_repo_path("  cilki	turbine  "),
            Some("cilki/turbine".into())
        );
        assert_eq!(gitea_repo_path("turbine"), None);
        assert_eq!(gitea_repo_path(""), None);
    }

    #[test]
    fn last_lines_counts_like_tail() {
        assert_eq!(last_lines("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(last_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(last_lines("a\nb\nc\n", 1), "c\n");
        assert_eq!(last_lines("a\nb", 50), "a\nb");
        assert_eq!(last_lines("", 50), "");
    }
}
