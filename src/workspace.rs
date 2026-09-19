//! The persistent workspace: one clone per repository, kept up to date and
//! indexed with codegraph across turns instead of recloned each time.

use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::warn;
use wait_timeout::ChildExt;

use crate::config::{Config, Forge, IoClass};

const GIT_TIMEOUT: Duration = Duration::from_secs(600);
const CODEGRAPH_SYNC_TIMEOUT: Duration = Duration::from_secs(600);
/// The first index of a big repository is slow.
const CODEGRAPH_INIT_TIMEOUT: Duration = Duration::from_secs(1800);

/// Where a repository lives in the workspace; `repo` is `<owner>/<repo>`.
pub fn repo_dir(root: &Path, forge_slug: &str, repo: &str) -> PathBuf {
    root.join(forge_slug).join(repo)
}

/// Make the repository's clone exist, current, and indexed, and return its
/// directory. A broken clone is thrown away and recloned; codegraph is
/// best-effort and never fails the turn.
pub fn prepare(cfg: &Config, forge: &Forge, repo: &str, log: &File) -> Result<PathBuf> {
    let dir = repo_dir(&cfg.workspace, forge.kind.name(), repo);
    if dir.join(".git").exists() {
        if let Err(err) = update(&dir, log) {
            warn!("update of {} failed ({err:#}); recloning", dir.display());
            reclone(forge, repo, &dir, log)?;
        }
    } else {
        reclone(forge, repo, &dir, log)?;
    }
    codegraph(cfg, &dir, log);
    Ok(dir)
}

/// Clone into `dir`, first clearing any half-created or broken leftovers.
fn reclone(forge: &Forge, repo: &str, dir: &Path, log: &File) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)
            .with_context(|| format!("failed to remove {}", dir.display()))?;
    }
    let parent = dir.parent().expect("repo dirs have parents");
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    let url = format!("{}/{repo}", forge.url.trim_end_matches('/'));
    run_logged(
        Command::new("git").args(["clone", &url]).arg(dir),
        GIT_TIMEOUT,
        log,
    )
}

/// Bring an existing clone back to a current default branch, whatever state
/// the previous turn left it in. Ignored files (build caches) survive.
fn update(dir: &Path, log: &File) -> Result<()> {
    // A turn killed mid-operation can leave the index locked; nothing else
    // touches the repository between turns, so the lock is always stale.
    let lock = dir.join(".git/index.lock");
    if lock.exists() {
        std::fs::remove_file(&lock)?;
    }
    let git = |args: &[&str]| {
        run_logged(
            Command::new("git").current_dir(dir).args(args),
            GIT_TIMEOUT,
            log,
        )
    };
    git(&["fetch", "origin", "--prune"])?;
    // Track upstream default-branch changes.
    git(&["remote", "set-head", "origin", "--auto"])?;
    let head = Command::new("git")
        .current_dir(dir)
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .stdin(Stdio::null())
        .output()
        .context("failed to run git symbolic-ref")?;
    if !head.status.success() {
        bail!("git symbolic-ref exited with {}", head.status);
    }
    let head = String::from_utf8_lossy(&head.stdout);
    let branch = head
        .trim()
        .strip_prefix("origin/")
        .with_context(|| format!("unexpected origin HEAD: {}", head.trim()))?;
    git(&["checkout", "--force", branch])?;
    git(&["reset", "--hard", &format!("origin/{branch}")])?;
    git(&["clean", "-fd", "-e", ".codegraph"])?;
    Ok(())
}

/// Whether the codegraph CLI is on the PATH, checked once at first use;
/// without it the runner skips indexing entirely and the agent explores the
/// tree normally.
pub fn codegraph_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let available = Command::new("codegraph")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !available {
            warn!("codegraph is not installed; running without indexes");
        }
        available
    })
}

/// Keep the repository's codegraph index current so the agent can explore it
/// through the `codegraph_explore` tool instead of reading files. On failure
/// the turn still runs; the agent falls back to normal exploration.
fn codegraph(cfg: &Config, dir: &Path, log: &File) {
    if !codegraph_available() {
        return;
    }
    let run = |args: &[&str], timeout| {
        let mut argv = throttle_argv(cfg);
        argv.push("codegraph".into());
        argv.extend(args.iter().map(|s| s.to_string()));
        run_logged(
            Command::new(&argv[0]).args(&argv[1..]).current_dir(dir),
            timeout,
            log,
        )
    };
    if dir.join(".codegraph").exists() {
        match run(&["sync"], CODEGRAPH_SYNC_TIMEOUT) {
            Ok(()) => return,
            // A failing sync usually means a corrupt or outdated index;
            // rebuild it from scratch.
            Err(err) => {
                warn!("codegraph sync failed ({err:#}); reindexing");
                if let Err(err) = std::fs::remove_dir_all(dir.join(".codegraph")) {
                    warn!("failed to remove stale index: {err:#}");
                    return;
                }
            }
        }
    }
    if let Err(err) = run(&["init", "--yes"], CODEGRAPH_INIT_TIMEOUT) {
        warn!("codegraph init failed ({err:#}); running the turn unindexed");
    }
}

/// The nice/ionice prefix that throttles a process tree; every descendant
/// inherits both priorities across fork and exec.
pub fn throttle_argv(cfg: &Config) -> Vec<String> {
    let mut argv = Vec::new();
    if let Some(nice) = cfg.nice {
        argv.extend(["nice".into(), "-n".into(), nice.to_string()]);
    }
    match cfg.ionice {
        Some(IoClass::BestEffort) => {
            argv.extend(["ionice", "-c", "2", "-n", "7"].map(String::from));
        }
        Some(IoClass::Idle) => argv.extend(["ionice", "-c", "3"].map(String::from)),
        None => {}
    }
    argv
}

/// SIGTERM the child's whole process group with a grace period, then SIGKILL;
/// signalling only the child would orphan its grandchildren. The SIGCONT
/// alongside the SIGTERM lets a paused (stopped) tree wake up and die
/// gracefully instead of eating the SIGKILL.
pub fn kill_group(child: &mut Child) -> Result<()> {
    let pgid = -(child.id() as i32);
    unsafe {
        libc::kill(pgid, libc::SIGTERM);
        libc::kill(pgid, libc::SIGCONT);
    }
    if child.wait_timeout(Duration::from_secs(10))?.is_none() {
        unsafe { libc::kill(pgid, libc::SIGKILL) };
        child.wait().ok();
    }
    Ok(())
}

/// Stop or continue a whole process group, for the web UI's pause button.
pub fn pause_group(pgid: i32, pause: bool) -> Result<()> {
    if pgid <= 0 {
        bail!("no process group to signal");
    }
    let signal = if pause { libc::SIGSTOP } else { libc::SIGCONT };
    match unsafe { libc::kill(-pgid, signal) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()).context("failed to signal the process group"),
    }
}

/// Run a command with its output appended to the turn log, killing its whole
/// process group if it outlives `timeout`.
fn run_logged(command: &mut Command, timeout: Duration, log: &File) -> Result<()> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?))
        .spawn()
        .with_context(|| format!("failed to spawn {program}"))?;
    match child.wait_timeout(timeout)? {
        Some(status) if status.success() => Ok(()),
        Some(status) => bail!("{program} exited with {status}"),
        None => {
            kill_group(&mut child)?;
            bail!("{program} timed out after {}s", timeout.as_secs());
        }
    }
}
