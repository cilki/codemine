//! The persistent workspace: one clone per repository, kept up to date and
//! indexed with codegraph across turns instead of recloned each time.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
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
///
/// Pinning stops at the keys that can be named in advance, which is why the
/// clone's local config is also rebuilt from scratch before any of this runs
/// — see [`reset_local_config`].
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
    private_dir(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.write_all(b"\n")?;
    file.persist(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Create a directory of the workspace, owner-only, narrowing an existing one
/// to the same.
///
/// The files the runner writes itself are already owner-only (`config.json`,
/// `git-credentials`, `skips.json`), but most of what the workspace holds is
/// written by something else: `git clone` lays down the repository trees,
/// opencode writes the turn logs through an inherited descriptor, and in the
/// container image CLIProxyAPI keeps its subscription login under the same
/// root. None of those are ours to chmod one by one, and `create_dir_all`
/// leaves the directories above them at 0777 & ~umask — 0755 on a default
/// umask — so every local user can walk in and read the lot. Taking the
/// traversal bit off the directory covers everything beneath it at once.
///
/// Existing directories are narrowed too, not just newly created ones: a
/// workspace made before this was enforced keeps the mode it was made with,
/// and that is exactly the deployment with something worth reading in it.
pub fn private_dir(path: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to restrict {}", path.display()))
}

/// Make the repository's clone exist, current, and indexed, and return its
/// directory. A broken clone is thrown away and recloned; codegraph is
/// best-effort and never fails the turn.
pub fn prepare(cfg: &Config, forge: &Forge, repo: &str, log: &File) -> Result<PathBuf> {
    let dir = repo_dir(&cfg.workspace, forge.kind.name(), repo);
    let url = clone_url(forge, repo);
    if dir.join(".git").exists() {
        if let Err(err) = update(&cfg.workspace, &url, &dir, log) {
            warn!("update of {} failed ({err:#}); recloning", dir.display());
            reclone(&cfg.workspace, &url, &dir, log)?;
        }
    } else {
        reclone(&cfg.workspace, &url, &dir, log)?;
    }
    codegraph(cfg, &dir, log);
    Ok(dir)
}

/// Where a repository is cloned from. The runner builds it from the forge's
/// own URL and the (validated) repository path every time rather than reading
/// it back out of the clone, which is also what lets the clone's `origin`
/// be rebuilt from nothing — see [`reset_local_config`].
fn clone_url(forge: &Forge, repo: &str) -> String {
    format!("{}/{repo}", forge.url.trim_end_matches('/'))
}

/// Clone into `dir`, first clearing any half-created or broken leftovers.
fn reclone(workspace: &Path, url: &str, dir: &Path, log: &File) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)
            .with_context(|| format!("failed to remove {}", dir.display()))?;
    }
    let parent = dir.parent().expect("repo dirs have parents");
    private_dir(parent)?;
    run_logged(
        git_in(workspace, parent).args(["clone", url]).arg(dir),
        GIT_TIMEOUT,
        log,
    )
}

/// Bring an existing clone back to a current default branch, whatever state
/// the previous turn left it in. Ignored files (build caches) survive.
fn update(workspace: &Path, url: &str, dir: &Path, log: &File) -> Result<()> {
    // Before any git that would act on it: the config is the agent's to
    // write and git reads it as code.
    reset_local_config(workspace, dir, url)?;
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

/// The local config keys the runner carries over when it rebuilds the
/// clone's own: the ones that say how the repository is stored on disk, which
/// the runner cannot invent — a sha256 or reftable clone is unreadable
/// without them. `extensions.worktreeConfig` is deliberately not among them:
/// it is what makes git read a second config file, `.git/config.worktree`,
/// which this rebuild does not cover.
const KEPT_CONFIG: [&str; 8] = [
    "core.repositoryformatversion",
    "core.filemode",
    "core.symlinks",
    "core.ignorecase",
    "core.precomposeunicode",
    "extensions.objectformat",
    "extensions.compatobjectformat",
    "extensions.refstorage",
];

/// Whether a key survives the rebuild. `branch.<name>.remote` and `.merge`
/// join the list above because they are the agent's, not the runner's: a
/// `git push -u` during one turn is what makes the plain `git pull` and
/// `git push` of the next one work, and the subsequent branch name is the
/// only part of a config key that may itself contain dots.
fn kept_config(name: &str) -> bool {
    KEPT_CONFIG.contains(&name)
        || (name.starts_with("branch.") && (name.ends_with(".remote") || name.ends_with(".merge")))
}

/// Rebuild the clone's local config from what the runner itself knows,
/// keeping only [`KEPT_CONFIG`] of what was there.
///
/// `git_in` pins the three config keys that [`git_in`]'s own doc comment
/// names, and that is as far as naming keys can go: git runs a
/// `filter.<anything>.smudge` on every path the `checkout` writes and a
/// `filter.<anything>.clean` on every path it stats, and the driver's name is
/// the agent's to choose, so there is no `-c` that pre-empts it. `include.path`
/// closes the question — one directive pulls in a whole file of keys, so any
/// list of keys to override is a list the clone can add to. Confirmed to run
/// as the runner on git 2.54 during `update` alone: `filter.*.smudge` and
/// `filter.*.clean` (with `.git/info/attributes` assigning the driver, which
/// needs no commit), `core.sshCommand` against an `origin` pointed at an
/// `ssh://` URL, and `remote.origin.uploadpack` against the local one.
///
/// So the file is replaced rather than overridden. What the runner needs of
/// it, it writes: this clone is not bare, it keeps reflogs, and its one
/// remote is the forge URL the repository was cloned from — which is also
/// what stops the clone from redirecting the runner's own fetch. Everything
/// else is dropped, `include.path` included, which is what makes the
/// rebuilt file the whole of what git will read.
fn reset_local_config(workspace: &Path, dir: &Path, url: &str) -> Result<()> {
    // `--list` reads the file and resolves its includes but runs nothing,
    // and it is scoped to this one config, so the global config the runner
    // installed is untouched by any of this.
    let listing = git_stdout(workspace, dir, &["config", "--local", "--list", "-z"])?;
    let kept: Vec<(String, String)> = listing
        .split('\0')
        .filter(|record| !record.is_empty())
        .map(|record| match record.split_once('\n') {
            Some((name, value)) => (name.to_owned(), value.to_owned()),
            // A variable written with no value at all is git's spelling of
            // true, and `--list` prints it with no newline to match.
            None => (record.to_owned(), "true".to_owned()),
        })
        .filter(|(name, _)| kept_config(name))
        .collect();
    // Replaced whole, so a crash mid-rebuild can't leave a clone describing
    // itself as something it isn't.
    let git_dir = dir.join(".git");
    let mut file = tempfile::NamedTempFile::new_in(&git_dir)
        .with_context(|| format!("failed to write under {}", git_dir.display()))?;
    file.write_all(config_text(url, &kept).as_bytes())?;
    let path = git_dir.join("config");
    file.persist(&path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

/// The rebuilt config as git config text: the runner's own keys plus the
/// carried-over ones, grouped under one header per section.
fn config_text(url: &str, kept: &[(String, String)]) -> String {
    let own = [
        ("core.bare", "false"),
        ("core.logallrefupdates", "true"),
        ("remote.origin.url", url),
        ("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"),
    ];
    let mut sections: BTreeMap<String, String> = BTreeMap::new();
    let entries = own
        .into_iter()
        .chain(kept.iter().map(|(name, value)| (&**name, &**value)));
    for (name, value) in entries {
        let Some((header, key)) = split_config_name(name) else {
            continue;
        };
        sections
            .entry(header)
            .or_default()
            .push_str(&format!("\t{key} = \"{}\"\n", quote(value)));
    }
    sections
        .into_iter()
        .map(|(header, body)| format!("{header}\n{body}"))
        .collect()
}

/// A `git config --list` name split into the section header it belongs under
/// and the variable name itself: `core.filemode` is `[core]` and `filemode`,
/// and `branch.my.topic.merge` is `[branch "my.topic"]` and `merge`, since a
/// subsection is the only part that may contain dots.
fn split_config_name(name: &str) -> Option<(String, &str)> {
    let (first, last) = (name.find('.')?, name.rfind('.')?);
    let (section, key) = (&name[..first], &name[last + 1..]);
    let header = match first == last {
        true => format!("[{section}]"),
        false => format!("[{section} \"{}\"]", quote(&name[first + 1..last])),
    };
    Some((header, key))
}

/// Escape a value, or a subsection name, for the inside of the double quotes
/// git's own parser reads it back out of. Unquoted, a `;` or a `#` would
/// start a comment and swallow the rest of the line; quoted, only the quote
/// itself, the escape character, and the line ending need spelling out.
fn quote(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
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

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// A workspace directory is owner-only however it came to exist: the
    /// clones, the turn logs, and the proxy's login underneath it are only as
    /// private as the directories above them.
    #[test]
    fn workspace_directories_are_owner_only() {
        let root = tempfile::tempdir().unwrap();

        // Created from nothing, intermediate levels included.
        let nested = root.path().join("gitea/owner");
        private_dir(&nested).unwrap();
        assert_eq!(mode_of(&nested), 0o700);
        assert_eq!(mode_of(&root.path().join("gitea")), 0o700);

        // And an existing world-readable one, as a workspace from before this
        // was enforced would be.
        let old = root.path().join("logs");
        std::fs::create_dir(&old).unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&old).unwrap();
        assert_eq!(mode_of(&old), 0o700);
    }

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

    /// The `url` `update` is given for a local origin: the runner builds it
    /// from the forge's URL and never reads it back out of the clone, so a
    /// test has to supply it the same way.
    fn url_of(origin: &Path) -> String {
        origin.display().to_string()
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
        update(root.path(), &url_of(&origin), &clone, &log).unwrap();

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
        update(root.path(), &url_of(&origin), &clone, &log).unwrap();

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

    /// Which of `fired` the plants managed to touch.
    fn plants_that_ran(fired: &Path) -> Vec<String> {
        let mut ran: Vec<String> = std::fs::read_dir(fired)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        ran.sort();
        ran
    }

    /// The config plant no `-c` can pre-empt, because the clone names the
    /// key: a `filter` driver of its own invention, assigned by the
    /// `.git/info/attributes` that needs no commit to take effect. git runs
    /// the smudge side on every path the `checkout` writes and the clean side
    /// on every path it stats. The same driver again behind an `include.path`
    /// is why overriding the keys the first one used would not have helped
    /// either — one directive is a whole file of keys the clone chose.
    #[test]
    fn update_runs_no_filter_the_clone_defined() {
        let root = tempfile::tempdir().unwrap();
        let (origin, clone) = origin_and_clone(root.path());
        // A second commit, so the `checkout` and `reset` really do write a
        // path for a smudge filter to be applied to.
        commit(&origin, "second");

        let fired = root.path().join("fired");
        std::fs::create_dir(&fired).unwrap();
        // A filter has to pass the content through, or git fails the
        // checkout for reasons of its own and proves nothing.
        let plant = |name: &str| format!("sh -c 'touch {}; cat'", fired.join(name).display());
        git(&clone, &["config", "filter.evil.smudge", &plant("smudge")]);
        git(&clone, &["config", "filter.evil.clean", &plant("clean")]);
        let included = clone.join(".git/included");
        std::fs::write(
            &included,
            format!(
                "[filter \"included\"]\n\tsmudge = \"{}\"\n",
                plant("included")
            ),
        )
        .unwrap();
        git(
            &clone,
            &["config", "include.path", &included.display().to_string()],
        );
        std::fs::write(
            clone.join(".git/info/attributes"),
            "first filter=evil\nsecond filter=included\n",
        )
        .unwrap();
        // What the agent legitimately leaves behind, which has to survive.
        git(&clone, &["config", "branch.main.remote", "origin"]);

        let log = tempfile::tempfile().unwrap();
        update(root.path(), &url_of(&origin), &clone, &log).unwrap();

        assert_eq!(head_of(&clone, "main"), head_of(&origin, "main"));
        let ran = plants_that_ran(&fired);
        assert!(ran.is_empty(), "the clone's own config ran: {ran:?}");

        // None of it is left to run next time either, while the keys that
        // are the runner's and the agent's came through.
        let listed = git_stdout(root.path(), &clone, &["config", "--local", "--list"]).unwrap();
        for key in ["filter.", "include.path", "fsmonitor", "credential."] {
            assert!(
                !listed.contains(key),
                "{key} survived the rebuild: {listed}"
            );
        }
        assert!(listed.contains("core.repositoryformatversion="), "{listed}");
        assert!(listed.contains("branch.main.remote=origin"), "{listed}");
    }

    /// `origin` is the clone's to rewrite, and the runner's fetch is what
    /// would go out to wherever it now points — carrying `core.sshCommand`
    /// and `remote.origin.uploadpack`, which are commands, with it. The
    /// runner builds the URL from the forge instead, so the redirection goes
    /// nowhere and neither command is left to be run.
    #[test]
    fn update_fetches_the_forges_url_and_not_the_clones() {
        let root = tempfile::tempdir().unwrap();
        let (origin, clone) = origin_and_clone(root.path());
        commit(&origin, "second");

        let fired = root.path().join("fired");
        std::fs::create_dir(&fired).unwrap();
        // These stand in for a transport git is about to give up on, so they
        // must not sit on the test's stdin while it does.
        let plant = |name: &str| format!("sh -c 'touch {}; exit 1'", fired.join(name).display());
        git(
            &clone,
            &["config", "remote.origin.url", "ssh://nobody@127.0.0.1:1/x"],
        );
        git(&clone, &["config", "core.sshCommand", &plant("ssh")]);
        git(
            &clone,
            &["config", "remote.origin.uploadpack", &plant("uploadpack")],
        );

        let log = tempfile::tempfile().unwrap();
        update(root.path(), &url_of(&origin), &clone, &log).unwrap();

        let ran = plants_that_ran(&fired);
        assert!(ran.is_empty(), "the clone's own config ran: {ran:?}");
        assert_eq!(head_of(&clone, "main"), head_of(&origin, "main"));
        let listed = git_stdout(root.path(), &clone, &["config", "--local", "--list"]).unwrap();
        assert!(
            listed.contains(&format!("remote.origin.url={}", url_of(&origin))),
            "{listed}"
        );
        for key in ["core.sshcommand", "uploadpack"] {
            assert!(
                !listed.contains(key),
                "{key} survived the rebuild: {listed}"
            );
        }
    }

    /// The rebuilt file has to read back as the values that went into it,
    /// whatever they contain: a `;` would otherwise start a comment, and a
    /// subsection is the one part of a key that may hold dots.
    #[test]
    fn the_rebuilt_config_reads_back_as_itself() {
        let root = tempfile::tempdir().unwrap();
        let (origin, clone) = origin_and_clone(root.path());
        let branch = r#"feature/a"b\c;d"#;
        let kept = [
            ("core.repositoryformatversion".to_owned(), "0".to_owned()),
            (
                format!("branch.{branch}.merge"),
                format!("refs/heads/{branch}"),
            ),
        ];
        std::fs::write(
            clone.join(".git/config"),
            config_text(&url_of(&origin), &kept),
        )
        .unwrap();

        let get = |key: &str| {
            git_stdout(root.path(), &clone, &["config", "--local", "--get", key])
                .unwrap()
                .trim()
                .to_owned()
        };
        assert_eq!(get("remote.origin.url"), url_of(&origin));
        assert_eq!(get("core.bare"), "false");
        assert_eq!(
            get(&format!("branch.{branch}.merge")),
            format!("refs/heads/{branch}")
        );
    }

    /// Only a subsection may contain dots, so the split has to come from both
    /// ends of the name rather than from the first dot it finds.
    #[test]
    fn a_config_name_splits_around_its_subsection() {
        assert_eq!(
            split_config_name("core.filemode"),
            Some(("[core]".to_owned(), "filemode"))
        );
        assert_eq!(
            split_config_name("branch.my.topic.merge"),
            Some((r#"[branch "my.topic"]"#.to_owned(), "merge"))
        );
        assert_eq!(split_config_name("bare"), None);
    }

    /// The upstream a `push -u` left behind survives; nothing else about a
    /// remote does, since a second remote is a second URL the clone could
    /// send the runner's git to.
    #[test]
    fn only_the_format_keys_and_the_upstream_are_kept() {
        for name in [
            "core.repositoryformatversion",
            "extensions.refstorage",
            "branch.main.merge",
            "branch.my.topic.remote",
        ] {
            assert!(kept_config(name), "{name} should be kept");
        }
        for name in [
            "core.sshcommand",
            "core.fsmonitor",
            "core.hookspath",
            "core.pager",
            "credential.helper",
            "include.path",
            "includeif.gitdir:/.path",
            "filter.evil.smudge",
            "remote.origin.uploadpack",
            "remote.origin.url",
            "url.https://evil.example/.insteadof",
            "protocol.ext.allow",
            "extensions.worktreeconfig",
            "alias.fetch",
        ] {
            assert!(!kept_config(name), "{name} should be dropped");
        }
    }
}
