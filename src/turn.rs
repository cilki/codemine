use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};
use wait_timeout::ChildExt;

use crate::config::{Config, Forge, ForgeKind};
use crate::scan;
use crate::status::{Activity, Outcome, Status, TurnRecord, epoch_now};
use crate::workspace;

pub enum Backoff {
    /// The usage window is exhausted until this epoch.
    UsageLimit(u64),
    Normal,
}

pub struct Report {
    pub backoff: Backoff,
    /// The agent reported real forge changes with a `TASK COMPLETED` marker;
    /// only these turns count toward the hourly limit.
    pub completed: bool,
    /// The turn died on revoked Claude OAuth credentials; the main loop
    /// gates further turns until a fresh login replaces them.
    pub oauth_revoked: bool,
}

/// Run one opencode turn against the repository's persistent workspace clone
/// and report how it went. The workspace survives across turns, and so does
/// the turn's log under `<workspace>/logs`, for as long as the process runs.
pub fn run(cfg: &Config, status: &crate::status::Shared) -> Result<Report> {
    let task = &cfg.tasks[fastrand::usize(..cfg.tasks.len())];
    let mut pool = Vec::new();
    for forge in &cfg.forges {
        pool.extend(
            list_repos(forge)?
                .into_iter()
                .filter(|repo| !forge.disabled_repos.contains(repo))
                .map(|repo| (forge, repo)),
        );
    }
    if pool.is_empty() {
        warn!("no repositories enabled on any forge");
        return Ok(Report {
            backoff: Backoff::Normal,
            completed: false,
            oauth_revoked: false,
        });
    }
    let (forge, repo) = &pool[fastrand::usize(..pool.len())];

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
        s.activity = Activity::Running {
            task: task.clone(),
            repo: repo.clone(),
            forge: forge.kind.name().into(),
            workspace: dir.display().to_string(),
            log_path: log_path.clone(),
            pgid: 0,
            started: started_epoch,
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
                task: task.clone(),
                repo: repo.clone(),
                forge: forge.kind.name().into(),
                started: started_epoch,
                duration_secs: elapsed,
                outcome: Outcome::Failed,
                tokens: None,
                log_path: log_path.clone(),
            });
        });
        return Ok(Report {
            backoff: Backoff::Normal,
            completed: false,
            oauth_revoked: false,
        });
    }

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
        .env("GIT_AUTHOR_EMAIL", &cfg.author_email)
        .env("GIT_COMMITTER_NAME", &cfg.author_name)
        .env("GIT_COMMITTER_EMAIL", &cfg.author_email)
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

    let exit = child.wait_timeout(cfg.turn_timeout)?;
    if exit.is_none() {
        workspace::kill_group(&mut child)?;
    }
    let elapsed = start.elapsed().as_secs();

    let tail = read_tail(&mut log, 64 * 1024)?;
    let completed = scan::reported_completed(last_lines(&tail, 50));
    let errored = scan::has_error_report(last_lines(&tail, 50));
    let outcome = match exit {
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

    let tokens = crate::usage::collect_since(started_wall);
    Status::update(status, |s| {
        s.paused = false;
        s.log_tail = last_lines(&tail, 100).to_owned();
        s.record_turn(TurnRecord {
            task: task.clone(),
            repo: repo.clone(),
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
        oauth_revoked: scan::oauth_revoked(&tail),
    })
}

const PAGE_SIZE: usize = 50;

/// The repositories the bot can reach on a forge, as `<owner>/<repo>`.
/// Queried fresh each turn so new repositories join the pool without a
/// restart.
pub fn list_repos(forge: &Forge) -> Result<Vec<String>> {
    match forge.kind {
        ForgeKind::Gitea => list_gitea_repos(),
        ForgeKind::Github => list_api_repos(forge, "gh", "user/repos", "full_name"),
        ForgeKind::Gitlab => list_api_repos(
            forge,
            "glab",
            "projects?membership=true",
            "path_with_namespace",
        ),
    }
}

/// `tea` prints the requested fields whitespace-separated with no header, so
/// owner and name come back as two columns and are rejoined into the
/// `<owner>/<repo>` path the rest of the runner expects.
fn list_gitea_repos() -> Result<Vec<String>> {
    let mut repos = Vec::new();
    for page in 1.. {
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
        let before = repos.len();
        repos.extend(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(gitea_repo_path),
        );
        if repos.len() - before < PAGE_SIZE {
            break;
        }
    }
    Ok(repos)
}

/// The `<owner>/<repo>` path from one `tea repos ls` line; None for the blank
/// and malformed lines tea can emit, since neither field can contain spaces.
fn gitea_repo_path(line: &str) -> Option<String> {
    let mut fields = line.split_whitespace();
    let (owner, name) = (fields.next()?, fields.next()?);
    fields.next().is_none().then(|| format!("{owner}/{name}"))
}

/// Page through a REST listing and pluck one field per repository; gh and
/// glab expose the same `api` subcommand shape and authenticate from the
/// forge's environment variables.
fn list_api_repos(forge: &Forge, program: &str, path: &str, field: &str) -> Result<Vec<String>> {
    let separator = if path.contains('?') { '&' } else { '?' };
    let mut repos = Vec::new();
    for page in 1.. {
        let output = Command::new(program)
            .args([
                "api",
                &format!("{path}{separator}per_page={PAGE_SIZE}&page={page}"),
            ])
            .envs(forge.env())
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to run {program}"))?;
        if !output.status.success() {
            bail!(
                "{program} api exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let values: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("{program} api returned unexpected output"))?;
        let count = values.len();
        repos.extend(
            values
                .iter()
                .filter_map(|value| value[field].as_str().map(String::from)),
        );
        if count < PAGE_SIZE {
            break;
        }
    }
    Ok(repos)
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
    use super::{gitea_repo_path, last_lines};

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
