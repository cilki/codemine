use std::io::{Read, Seek, SeekFrom};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use wait_timeout::ChildExt;

use crate::config::Config;
use crate::scan;

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
pub fn run(cfg: &Config) -> Result<Report> {
    let task = &cfg.tasks[fastrand::usize(..cfg.tasks.len())];
    let repos = list_repos()?;
    let repo = &repos[fastrand::usize(..repos.len())];

    let workspace = tempfile::Builder::new()
        .prefix("codemine.")
        .tempdir_in("/root")?;
    let mut log = tempfile::NamedTempFile::new()?;
    println!("new task: {} ({task} on {repo})", workspace.path().display());

    let start = Instant::now();
    let mut child = Command::new("opencode")
        .args(["run", "--command", &cfg.command, "--model", &cfg.model, task, repo])
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
        .context("failed to spawn opencode")?;

    let status = child.wait_timeout(cfg.turn_timeout)?;
    if status.is_none() {
        // SIGTERM with a grace period, then SIGKILL.
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        if child.wait_timeout(Duration::from_secs(10))?.is_none() {
            child.kill().ok();
            child.wait().ok();
        }
    }
    let elapsed = start.elapsed().as_secs();

    let tail = read_tail(log.as_file_mut(), 64 * 1024)?;
    let completed = !scan::reported_skipped(last_lines(&tail, 50));
    match status {
        None => eprintln!("timed out after {elapsed}s"),
        Some(status) if !status.success() || scan::has_error_report(last_lines(&tail, 50)) => {
            eprintln!(
                "failed with status {} in {elapsed}s:",
                status.code().unwrap_or(-1)
            );
            eprint!("{}", last_lines(&tail, 20));
        }
        Some(_) if completed => println!("ok in {elapsed}s"),
        Some(_) => println!("skipped in {elapsed}s"),
    }

    Ok(Report {
        backoff: match scan::usage_limit_epoch(&tail) {
            Some(epoch) => Backoff::UsageLimit(epoch),
            None => Backoff::Normal,
        },
        completed,
    })
}

/// The repositories the bot can reach, as `<owner>/<repo>`. Queried fresh each
/// turn so new repositories join the pool without a restart.
fn list_repos() -> Result<Vec<String>> {
    const PAGE_SIZE: usize = 50;
    let mut repos = Vec::new();
    for page in 1.. {
        let output = Command::new("tea")
            .args(["repos", "ls", "--output", "simple", "--fields", "name"])
            .args(["--limit", &PAGE_SIZE.to_string(), "--page", &page.to_string()])
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
    if repos.is_empty() {
        bail!("no repositories reachable on Gitea");
    }
    Ok(repos)
}

/// The last `limit` bytes of the file, lossily decoded.
fn read_tail(file: &mut std::fs::File, limit: u64) -> Result<String> {
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
