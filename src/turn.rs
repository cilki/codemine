use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use wait_timeout::ChildExt;

use crate::config::{Config, Forge, ForgeKind, IoClass};
use crate::scan;
use crate::status::{Activity, Outcome, Status, TurnRecord, epoch_now};

pub enum Backoff {
    /// The usage window is exhausted until this epoch.
    UsageLimit(u64),
    Normal,
}

pub struct Report {
    pub backoff: Backoff,
    /// The agent did real work rather than reporting the turn as skipped;
    /// only these turns count toward the daily limit.
    pub completed: bool,
}

/// Run one opencode turn in a fresh workspace and report how it went. The
/// workspace and log are cleaned up when they drop.
pub fn run(cfg: &Config, status: &crate::status::Shared) -> Result<Report> {
    let task = &cfg.tasks[fastrand::usize(..cfg.tasks.len())];
    let mut pool = Vec::new();
    for forge in &cfg.forges {
        pool.extend(list_repos(forge)?.into_iter().map(|repo| (forge, repo)));
    }
    if pool.is_empty() {
        bail!("no repositories reachable on any forge");
    }
    let (forge, repo) = &pool[fastrand::usize(..pool.len())];

    let workspace = tempfile::Builder::new()
        .prefix("codemine.")
        .tempdir_in("/root")?;
    let mut log = tempfile::NamedTempFile::new()?;
    println!(
        "new task: {} ({task} on {} {repo})",
        workspace.path().display(),
        forge.kind.name()
    );

    let started_epoch = epoch_now();
    let started_wall = SystemTime::now();
    Status::update(status, |s| {
        s.activity = Activity::Running {
            task: task.clone(),
            repo: repo.clone(),
            forge: forge.kind.name().into(),
            workspace: workspace.path().display().to_string(),
            log_path: log.path().to_path_buf(),
            started: started_epoch,
        };
    });

    let start = Instant::now();
    // Throttling wraps the agent in nice/ionice; every descendant inherits
    // both priorities across fork and exec.
    let nice_arg = cfg.nice.map(|n| n.to_string());
    let mut argv: Vec<&str> = Vec::new();
    if let Some(nice) = &nice_arg {
        argv.extend(["nice", "-n", nice]);
    }
    match cfg.ionice {
        Some(IoClass::BestEffort) => argv.extend(["ionice", "-c", "2", "-n", "7"]),
        Some(IoClass::Idle) => argv.extend(["ionice", "-c", "3"]),
        None => {}
    }
    argv.extend([
        "opencode",
        "run",
        "--command",
        &cfg.command,
        "--model",
        &cfg.model,
        task,
        repo,
        forge.kind.name(),
    ]);
    let mut child = Command::new(argv[0])
        .args(&argv[1..])
        // Own process group, so the timeout can take down the whole tree.
        .process_group(0)
        .current_dir(workspace.path())
        .env("NO_COLOR", "1")
        // The bash runner exported these for everything it ran; only opencode's
        // subprocesses ever used them.
        .env("GIT_COMMITTER_NAME", &cfg.author_name)
        .env("GIT_COMMITTER_EMAIL", &cfg.author_email)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.as_file().try_clone()?))
        .stderr(Stdio::from(log.as_file().try_clone()?))
        .spawn()
        .with_context(|| format!("failed to spawn {}", argv[0]))?;

    let exit = child.wait_timeout(cfg.turn_timeout)?;
    if exit.is_none() {
        // SIGTERM the whole process group with a grace period, then SIGKILL;
        // signalling only the child would orphan cargo/rustc grandchildren.
        let pgid = -(child.id() as i32);
        unsafe { libc::kill(pgid, libc::SIGTERM) };
        if child.wait_timeout(Duration::from_secs(10))?.is_none() {
            unsafe { libc::kill(pgid, libc::SIGKILL) };
            child.wait().ok();
        }
    }
    let elapsed = start.elapsed().as_secs();

    let tail = read_tail(log.as_file_mut(), 64 * 1024)?;
    let completed = !scan::reported_skipped(last_lines(&tail, 50));
    let errored = scan::has_error_report(last_lines(&tail, 50));
    let outcome = match exit {
        None => {
            eprintln!("timed out after {elapsed}s");
            Outcome::Timeout
        }
        Some(exit) if !exit.success() || errored => {
            eprintln!(
                "failed with status {} in {elapsed}s:",
                exit.code().unwrap_or(-1)
            );
            eprint!("{}", last_lines(&tail, 20));
            Outcome::Failed
        }
        Some(_) if completed => {
            println!("ok in {elapsed}s");
            Outcome::Completed
        }
        Some(_) => {
            println!("skipped in {elapsed}s");
            Outcome::Skipped
        }
    };

    let tokens = crate::usage::collect_since(started_wall);
    Status::update(status, |s| {
        s.log_tail = last_lines(&tail, 100).to_owned();
        s.record_turn(TurnRecord {
            task: task.clone(),
            repo: repo.clone(),
            forge: forge.kind.name().into(),
            started: started_epoch,
            duration_secs: elapsed,
            outcome,
            tokens,
        });
    });

    Ok(Report {
        backoff: match scan::usage_limit_epoch(&tail) {
            Some(epoch) => Backoff::UsageLimit(epoch),
            None => Backoff::Normal,
        },
        completed,
    })
}

const PAGE_SIZE: usize = 50;

/// The repositories the bot can reach on a forge, as `<owner>/<repo>`.
/// Queried fresh each turn so new repositories join the pool without a
/// restart.
fn list_repos(forge: &Forge) -> Result<Vec<String>> {
    match forge.kind {
        ForgeKind::Gitea => list_gitea_repos(),
        ForgeKind::Github => list_api_repos("gh", "user/repos", "full_name"),
        ForgeKind::Gitlab => {
            list_api_repos("glab", "projects?membership=true", "path_with_namespace")
        }
    }
}

fn list_gitea_repos() -> Result<Vec<String>> {
    let mut repos = Vec::new();
    for page in 1.. {
        let output = Command::new("tea")
            .args(["repos", "ls", "--output", "simple", "--fields", "name"])
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
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(String::from),
        );
        if repos.len() - before < PAGE_SIZE {
            break;
        }
    }
    Ok(repos)
}

/// Page through a REST listing and pluck one field per repository; gh and
/// glab expose the same `api` subcommand shape and authenticate from the
/// environment.
fn list_api_repos(program: &str, path: &str, field: &str) -> Result<Vec<String>> {
    let separator = if path.contains('?') { '&' } else { '?' };
    let mut repos = Vec::new();
    for page in 1.. {
        let output = Command::new(program)
            .args([
                "api",
                &format!("{path}{separator}per_page={PAGE_SIZE}&page={page}"),
            ])
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
    use super::last_lines;

    #[test]
    fn last_lines_counts_like_tail() {
        assert_eq!(last_lines("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(last_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(last_lines("a\nb\nc\n", 1), "c\n");
        assert_eq!(last_lines("a\nb", 50), "a\nb");
        assert_eq!(last_lines("", 50), "");
    }
}
