//! The persistent workspace: one clone per repository, kept up to date and
//! indexed with codegraph across turns instead of recloned each time.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write;
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

/// Where the credential helper keeps forge logins. It lives in the workspace
/// rather than at the helper's default `~/.git-credentials`, because the home
/// directory isn't necessarily writable.
pub fn git_credentials(workspace: &Path) -> PathBuf {
    workspace.join("git-credentials")
}

/// A `git` the runner runs itself, in `cwd`, with the clone's own `.git`
/// metadata disarmed.
///
/// The assigned clone is the one place the Landlock ruleset lets the agent
/// write, so by the time the next turn comes to update it, everything under
/// its `.git` is whatever the last turn left behind — and the runner, unlike
/// the agent, is not inside the sandbox. git treats a repository's hooks and
/// local config as code, which makes a plain `git` of the runner's own in
/// that directory the shortest way out of the confinement:
///
/// * `.git/hooks/post-checkout` runs on the `checkout` below, and
///   `.git/hooks/reference-transaction` on every ref the `fetch` moves.
/// * `core.fsmonitor` is a command git runs for any operation that reads the
///   index — `checkout`, `reset`, and `clean` all do.
/// * a `credential.helper` is handed the credentials git just used, so one
///   planted here collects the forge token on the next authenticated fetch.
///   The empty value resets the accumulated list (dropping the planted one
///   wherever it was configured); the runner's own helper is re-added after
///   it, so authentication still works.
///
/// `-c` outranks every config file, including the repository's, which is what
/// makes pinning these here enough. Only the runner's own commands are
/// affected: the agent's git still sees the clone it configured.
fn git_in(workspace: &Path, cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(cwd);
    for setting in [
        "core.hooksPath=/dev/null".to_owned(),
        "core.fsmonitor=false".to_owned(),
        "credential.helper=".to_owned(),
        format!(
            "credential.helper=store --file={}",
            git_credentials(workspace).display()
        ),
    ] {
        command.args(["-c", &setting]);
    }
    command
}

/// Atomically write `value` as pretty JSON: the state the runner keeps at the
/// workspace root (the settings and the skip cache) and the opencode config it
/// maintains all go through here. The temp file is created next to the target
/// so the rename can't cross filesystems, and it carries `tempfile`'s
/// owner-only mode — which is what keeps the forge tokens in `config.json`
/// and the proxy key in `opencode.json` out of reach of other local users.
pub fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.write_all(b"\n")?;
    file.persist(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Make the repository's clone exist, current, and indexed, and return its
/// directory. A broken clone is thrown away and recloned; codegraph is
/// best-effort and never fails the turn.
pub fn prepare(cfg: &Config, forge: &Forge, repo: &str, log: &File) -> Result<PathBuf> {
    let dir = repo_dir(&cfg.workspace, forge.kind.name(), repo);
    if dir.join(".git").exists() {
        if let Err(err) = update(&cfg.workspace, &dir, log) {
            warn!("update of {} failed ({err:#}); recloning", dir.display());
            reclone(&cfg.workspace, forge, repo, &dir, log)?;
        }
    } else {
        reclone(&cfg.workspace, forge, repo, &dir, log)?;
    }
    codegraph(cfg, &dir, log);
    Ok(dir)
}

/// Clone into `dir`, first clearing any half-created or broken leftovers.
fn reclone(workspace: &Path, forge: &Forge, repo: &str, dir: &Path, log: &File) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)
            .with_context(|| format!("failed to remove {}", dir.display()))?;
    }
    let parent = dir.parent().expect("repo dirs have parents");
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    let url = format!("{}/{repo}", forge.url.trim_end_matches('/'));
    run_logged(
        git_in(workspace, parent).args(["clone", &url]).arg(dir),
        GIT_TIMEOUT,
        log,
    )
}

/// Bring an existing clone back to a current default branch, whatever state
/// the previous turn left it in. Ignored files (build caches) survive.
fn update(workspace: &Path, dir: &Path, log: &File) -> Result<()> {
    // A turn killed mid-operation can leave the index locked; nothing else
    // touches the repository between turns, so the lock is always stale.
    let lock = dir.join(".git/index.lock");
    if lock.exists() {
        std::fs::remove_file(&lock)?;
    }
    let git = |args: &[&str]| run_logged(git_in(workspace, dir).args(args), GIT_TIMEOUT, log);
    // Every branch, spelled out rather than left to the clone's configured
    // refspec, so the agent sees all of them at their latest and a clone
    // made narrow can't quietly limit what a turn can reach.
    git(&[
        "fetch",
        "origin",
        "--prune",
        "+refs/heads/*:refs/remotes/origin/*",
    ])?;
    // Track upstream default-branch changes.
    git(&["remote", "set-head", "origin", "--auto"])?;
    let head = git_stdout(
        workspace,
        dir,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )?;
    let branch = head
        .trim()
        .strip_prefix("origin/")
        .with_context(|| format!("unexpected origin HEAD: {}", head.trim()))?;
    git(&["checkout", "--force", branch])?;
    git(&["reset", "--hard", &format!("origin/{branch}")])?;
    git(&["clean", "-fd", "-e", ".codegraph"])?;
    // The branch the turn starts on is current now; the rest are brought
    // forward separately. A failure there is not worth a reclone — the
    // turn's own branch is already correct.
    if let Err(err) = fast_forward_branches(workspace, dir, branch, log) {
        warn!("failed to refresh branches in {}: {err:#}", dir.display());
    }
    Ok(())
}

/// Force every other local branch onto its origin counterpart, so a branch
/// left behind by an earlier turn can't hide work pushed since. Branches
/// with no counterpart — never pushed, or merged and deleted upstream — are
/// left alone rather than silently thrown away.
fn fast_forward_branches(workspace: &Path, dir: &Path, current: &str, log: &File) -> Result<()> {
    let refs = |namespace| {
        git_stdout(
            workspace,
            dir,
            &["for-each-ref", "--format=%(refname:short)", namespace],
        )
    };
    let remote: BTreeSet<String> = refs("refs/remotes/origin")?
        .lines()
        .filter_map(|name| name.strip_prefix("origin/"))
        .map(str::to_owned)
        .collect();
    for local in refs("refs/heads")?.lines() {
        if local == current || !remote.contains(local) {
            continue;
        }
        run_logged(
            git_in(workspace, dir).args(["branch", "--force", local, &format!("origin/{local}")]),
            GIT_TIMEOUT,
            log,
        )?;
    }
    Ok(())
}

/// The commit the clone is checked out at, which the skip cache remembers an
/// empty turn against for the tasks that read the tree.
pub fn head_sha(workspace: &Path, dir: &Path) -> Result<String> {
    Ok(git_stdout(workspace, dir, &["rev-parse", "HEAD"])?
        .trim()
        .to_owned())
}

/// Stdout of a git command that is run for its answer rather than its
/// effect; anything but a clean exit is an error.
fn git_stdout(workspace: &Path, dir: &Path, args: &[&str]) -> Result<String> {
    let output = git_in(workspace, dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!("git {} exited with {}", args.join(" "), output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// git with a fixed identity, so the test doesn't depend on whatever
    /// the machine has configured.
    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git must be on PATH to run this test");
        assert!(status.success(), "git {args:?} failed");
    }

    fn commit(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), name).unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", name]);
    }

    /// `rev-parse` neither authenticates nor reads the index, so the
    /// workspace root it would take the credential helper from is irrelevant
    /// here and the clone stands in for it.
    fn head_of(dir: &Path, branch: &str) -> String {
        git_stdout(dir, dir, &["rev-parse", branch])
            .unwrap()
            .trim()
            .to_owned()
    }

    /// An origin with one commit on `main` plus a clone of it, which is the
    /// state `update` is asked to bring forward.
    fn origin_and_clone(root: &Path) -> (PathBuf, PathBuf) {
        let origin = root.join("origin");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-b", "main"]);
        commit(&origin, "first");
        let clone = root.join("clone");
        git(
            root,
            &[
                "clone",
                &origin.display().to_string(),
                &clone.display().to_string(),
            ],
        );
        (origin, clone)
    }

    /// Every branch the turn might touch starts at what origin has now, not
    /// at what a previous turn left in the clone.
    #[test]
    fn update_brings_every_branch_forward() {
        let root = tempfile::tempdir().unwrap();
        let origin = root.path().join("origin");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-b", "main"]);
        commit(&origin, "first");
        git(&origin, &["checkout", "-b", "feature"]);
        commit(&origin, "feature-one");
        git(&origin, &["checkout", "main"]);

        let clone = root.path().join("clone");
        git(
            root.path(),
            &[
                "clone",
                &origin.display().to_string(),
                &clone.display().to_string(),
            ],
        );
        // A branch an earlier turn checked out, pinned to the old commit.
        git(&clone, &["checkout", "-b", "feature", "origin/feature"]);
        git(&clone, &["checkout", "main"]);
        // A branch that only ever existed locally, which has nothing to
        // catch up to and must survive.
        git(&clone, &["branch", "local-only"]);
        let local_only = head_of(&clone, "local-only");

        // Upstream moves on both branches while the clone sits idle.
        git(&origin, &["checkout", "feature"]);
        commit(&origin, "feature-two");
        git(&origin, &["checkout", "main"]);
        commit(&origin, "second");

        let log = tempfile::tempfile().unwrap();
        update(root.path(), &clone, &log).unwrap();

        assert_eq!(head_of(&clone, "main"), head_of(&origin, "main"));
        assert_eq!(head_of(&clone, "feature"), head_of(&origin, "feature"));
        assert_eq!(
            head_of(&clone, "origin/feature"),
            head_of(&origin, "feature")
        );
        assert_eq!(head_of(&clone, "local-only"), local_only);
    }

    /// The clone is the agent's to write, so its `.git` is whatever the last
    /// turn left there — and the runner updating it next turn is outside the
    /// sandbox. None of the three things git would otherwise run out of a
    /// repository may run: the hooks `update`'s own commands fire, the
    /// `core.fsmonitor` command every index read consults, and a
    /// `credential.helper` that would be handed the forge token.
    #[test]
    fn update_runs_nothing_the_clone_planted() {
        let root = tempfile::tempdir().unwrap();
        let (origin, clone) = origin_and_clone(root.path());
        commit(&origin, "second");

        // Each plant writes a file named after itself if it ever runs.
        let fired = root.path().join("fired");
        std::fs::create_dir(&fired).unwrap();
        let plant = |name: &str| {
            let script = format!("#!/bin/sh\necho ran > {}\n", fired.join(name).display());
            let path = clone.join(".git/hooks").join(name);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        // post-checkout fires on the `checkout`, reference-transaction on
        // every ref the `fetch` and the `reset` move.
        plant("post-checkout");
        plant("reference-transaction");
        let fsmonitor = plant("fsmonitor");
        git(
            &clone,
            &["config", "core.fsmonitor", &fsmonitor.display().to_string()],
        );
        let helper = plant("credential-thief");
        git(
            &clone,
            &[
                "config",
                "credential.helper",
                &format!("!{}", helper.display()),
            ],
        );

        let log = tempfile::tempfile().unwrap();
        update(root.path(), &clone, &log).unwrap();

        // The update did its job...
        assert_eq!(head_of(&clone, "main"), head_of(&origin, "main"));
        // ...without running any of it.
        let ran: Vec<String> = std::fs::read_dir(&fired)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(ran.is_empty(), "the clone's own git metadata ran: {ran:?}");

        // The helper is the one plant `update` can't reach on a local origin,
        // since nothing authenticates; ask git directly whether it would be
        // consulted for the credentials a fetch over https would use.
        let mut child = git_in(root.path(), &clone)
            .args(["credential", "approve"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"protocol=https\nhost=forge.example\nusername=bot\npassword=token\n")
            .unwrap();
        child.wait().unwrap();
        assert!(
            !fired.join("credential-thief").exists(),
            "a credential helper in the clone was handed the forge token"
        );
        // The runner's own helper is still the one in effect.
        let stored = std::fs::read_to_string(git_credentials(root.path())).unwrap();
        assert!(stored.contains("bot:token@forge.example"), "{stored}");
    }
}
