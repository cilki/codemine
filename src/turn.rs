use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};
use wait_timeout::ChildExt;

use crate::cache;
use crate::config::{Config, Forge, ForgeKind};
use crate::precheck;
use crate::scan;
use crate::status::{Activity, Outcome, Status, TurnRecord, epoch_now};
use crate::workspace;

#[derive(Default)]
pub enum Backoff {
    /// The usage window is exhausted until this epoch.
    UsageLimit(u64),
    #[default]
    Normal,
}

/// A turn that never started reports exactly this: nothing to back off from,
/// nothing spent, nothing to gate on, nothing cancelled.
#[derive(Default)]
pub struct Report {
    pub backoff: Backoff,
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

/// Run one opencode turn against the repository's persistent workspace clone
/// and report how it went. The workspace survives across turns, and so does
/// the turn's log under `<workspace>/logs`, for as long as the process runs.
pub fn run(
    cfg: &Config,
    status: &crate::status::Shared,
    pending: &crate::events::Pending,
    cache: &cache::Cache,
) -> Result<Report> {
    let mut pool = Vec::new();
    for forge in &cfg.forges {
        pool.extend(
            list_repos(forge)?
                .into_iter()
                .filter(|repo| forge.enabled_repos.contains(repo))
                .map(|repo| (forge, repo)),
        );
    }
    if pool.is_empty() {
        // An empty enabled set is a settings problem and never reaches here;
        // what does is an enabled repository the forge no longer lists —
        // renamed, deleted, or out of the token's reach.
        warn!("none of the enabled repositories are listed by their forge");
        return Ok(Report::default());
    }
    // Fresh forge activity jumps the queue: the watcher saw a comment land,
    // so that repository gets a feedback turn ahead of the random draw.
    // Entries that no longer check out (repository disabled or gone, the
    // feedback already handled, the task since disabled) are dropped rather
    // than requeued — the ordinary draw probes feedback anyway.
    let feedback = cfg.tasks.iter().any(|task| task == "feedback");
    // The precondition probes plus the skip cache, sharing one set of forge
    // lookups across everything this draw asks about.
    let probe = cache::Probe::new(cache);
    let mut urgent = None;
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
            urgent = Some(("feedback", forge, repo));
            break;
        }
    }
    let Some((task, forge, repo)) = urgent.or_else(|| {
        precheck::draw(&cfg.tasks, &pool, |task, forge, repo| {
            probe.actionable(task, forge, repo)
        })
    }) else {
        warn!("every enabled task has nothing to do on any enabled repository");
        return Ok(Report::default());
    };

    let dir = workspace::repo_dir(&cfg.workspace, forge.kind.name(), repo);
    let started_epoch = epoch_now();
    let started_wall = SystemTime::now();
    let logs_dir = cfg.workspace.join("logs");
    std::fs::create_dir_all(&logs_dir)
        .with_context(|| format!("failed to create {}", logs_dir.display()))?;
    let log_path = logs_dir.join(format!("{started_epoch}.log"));
    let mut log = std::fs::File::options()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&log_path)
        .with_context(|| format!("failed to create {}", log_path.display()))?;
    info!(
        "new task: {} ({task} on {} {repo})",
        dir.display(),
        forge.kind.name()
    );

    Status::update(status, |s| {
        s.paused = false;
        // A leftover request from between turns must not fell this one.
        s.cancel_requested = false;
        s.activity = Activity::Running {
            task: task.to_owned(),
            repo: repo.to_owned(),
            forge: forge.kind.name().into(),
            workspace: dir.display().to_string(),
            log_path: log_path.clone(),
            pgid: 0,
            started: started_epoch,
            tokens: None,
        };
    });

    let start = Instant::now();
    // A failed preparation (deleted repository, network blip, ...) fails the
    // turn, not the runner.
    if let Err(err) = workspace::prepare(cfg, forge, repo, &log) {
        let elapsed = start.elapsed().as_secs();
        let tail = read_tail(&mut log, 64 * 1024)?;
        warn!(
            "failed to prepare {repo} in {elapsed}s: {err:#}\n{}",
            last_lines(&tail, 20)
        );
        Status::update(status, |s| {
            s.log_tail = last_lines(&tail, 100).to_owned();
            s.record_turn(TurnRecord {
                task: task.to_owned(),
                repo: repo.to_owned(),
                forge: forge.kind.name().into(),
                started: started_epoch,
                duration_secs: elapsed,
                outcome: Outcome::Failed,
                tokens: None,
                log_path: log_path.clone(),
            });
        });
        return Ok(Report::default());
    }

    // Whatever this task's answer depends on, read before the agent runs: the
    // commit it is about to read, taken while the clone is still on a clean
    // default branch — after the turn it could be sitting on whatever branch
    // the agent left behind — or the notification stamp it is about to read,
    // taken before the agent marks any of the feed read. A task that skips is
    // remembered against it. Best-effort: without it the task is simply drawn
    // again next time.
    let state = match cache::basis(task) {
        Some(cache::Basis::Head) => match workspace::head_sha(&dir) {
            Ok(head) => Some(head),
            Err(err) => {
                warn!("failed to read {}'s head commit: {err:#}", dir.display());
                None
            }
        },
        Some(cache::Basis::Feed) => probe.state(task, forge, repo),
        None => None,
    };

    // The agent runs inside the Landlock write sandbox, so it cannot work
    // from any checkout other than the assigned clone.
    let mut argv = crate::sandbox::wrap(&dir, &log_path);
    argv.extend(workspace::throttle_argv(cfg));
    argv.extend(
        [
            "opencode",
            "run",
            "--command",
            crate::prompts::SWEEP_COMMAND,
            "--model",
            &cfg.model,
            task,
            repo,
            forge.kind.name(),
        ]
        .map(String::from),
    );
    // $4 in the sweep command: where the agent must work.
    argv.push(dir.display().to_string());
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        // Own process group, so the timeout can take down the whole tree.
        .process_group(0)
        .current_dir(&dir)
        // current_dir() changes the real working directory but not the
        // inherited $PWD, and anything trusting the variable over getcwd
        // would resolve the runner's own launch directory instead.
        .env("PWD", &dir)
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
        .env("GIT_AUTHOR_EMAIL", &forge.email)
        .env("GIT_COMMITTER_NAME", &cfg.author_name)
        .env("GIT_COMMITTER_EMAIL", &forge.email)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?))
        .spawn()
        .with_context(|| format!("failed to spawn {}", argv[0]))?;

    // Now that the group exists, let the web UI pause and resume it.
    Status::update(status, |s| {
        if let Activity::Running { pgid, .. } = &mut s.activity {
            *pgid = child.id() as i32;
        }
    });

    // Wait in short hops rather than one long one, resampling what the turn
    // has spent between them: opencode records each message as it goes, so
    // the page can watch the cost climb instead of learning it at the end.
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
        let tokens = crate::usage::collect_since(started_wall);
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
        workspace::kill_group(&mut child)?;
    }
    let elapsed = start.elapsed().as_secs();

    let tail = read_tail(&mut log, 64 * 1024)?;
    let completed = scan::reported_completed(last_lines(&tail, 50));
    let errored = scan::has_error_report(last_lines(&tail, 50));
    let outcome = match exit {
        None if canceled => {
            info!("canceled after {elapsed}s");
            Outcome::Canceled
        }
        None => {
            warn!("timed out after {elapsed}s");
            Outcome::Timeout
        }
        Some(exit) if !exit.success() || errored => {
            warn!(
                "failed with status {} in {elapsed}s:\n{}",
                exit.code().unwrap_or(-1),
                last_lines(&tail, 20)
            );
            Outcome::Failed
        }
        Some(_) if completed => {
            info!("ok in {elapsed}s");
            Outcome::Completed
        }
        Some(_) => {
            info!("skipped in {elapsed}s");
            Outcome::Skipped
        }
    };

    // A task that came up empty is held back until the state it answered for
    // moves; any other outcome retires whatever was remembered, since a
    // completed turn changed something and a turn that failed, timed out, or
    // was cancelled never got to answer.
    if cache::basis(task).is_some() {
        match (outcome, &state) {
            (Outcome::Skipped, Some(state)) => cache.remember(task, forge.kind, repo, state),
            _ => cache.forget(task, forge.kind, repo),
        }
    }

    // The last reading can fail like any other (opencode mid-write, or its
    // database gone with the session); the samples taken while the turn ran
    // are the next best figure, and better than a row with no cost on it.
    let tokens = crate::usage::collect_since(started_wall).or(sampled);
    Status::update(status, |s| {
        s.paused = false;
        s.log_tail = last_lines(&tail, 100).to_owned();
        s.record_turn(TurnRecord {
            task: task.to_owned(),
            repo: repo.to_owned(),
            forge: forge.kind.name().into(),
            started: started_epoch,
            duration_secs: elapsed,
            outcome,
            tokens,
            log_path,
        });
    });

    Ok(Report {
        backoff: match scan::usage_limit_epoch(&tail) {
            Some(epoch) => Backoff::UsageLimit(epoch),
            None => Backoff::Normal,
        },
        completed,
        auth_error: scan::auth_error(&tail),
        canceled,
    })
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
    use anyhow::{Result, bail};

    use super::{PAGE_SIZE, gitea_repo_path, last_lines, paginate, safe_repo_path};

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
