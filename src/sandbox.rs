//! Kernel-enforced confinement for the agent. The opencode process tree is
//! spawned through a hidden re-invocation of this binary, which applies a
//! Landlock ruleset and then execs the real command; everything opencode
//! spawns inherits the restrictions. Writes are only allowed in the assigned
//! clone, the turn log, and the tool state directories, so the agent cannot
//! work from some other checkout it finds on the machine. Reads stay
//! unrestricted: the agent needs toolchains and configs from all over, and
//! the damage vector is writing where it shouldn't.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use landlock::{
    ABI, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, path_beneath_rules,
};

/// The hidden first argument that turns this binary into the sandbox wrapper.
pub const MARKER: &str = "__sandbox-exec";

/// The argv prefix that reruns this binary as the sandbox wrapper around a
/// command: `codemine __sandbox-exec <repo> <log> -- <command...>`.
pub fn wrap(repo: &Path, log: &Path) -> Vec<String> {
    let exe = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "codemine".into());
    vec![
        exe,
        MARKER.into(),
        repo.display().to_string(),
        log.display().to_string(),
        "--".into(),
    ]
}

/// The wrapper entry point, dispatched from main before normal CLI parsing;
/// `args` is everything after the marker. Only returns on failure — the
/// message lands in the turn log and fails the turn, because running the
/// agent unconfined is worse than not running it.
pub fn exec(args: &[String]) -> Result<()> {
    let (repo, log, command) = match args {
        [repo, log, separator, command @ ..] if separator == "--" && !command.is_empty() => {
            (Path::new(repo), Path::new(log), command)
        }
        _ => bail!("usage: codemine {MARKER} <repo> <log> -- <command...>"),
    };
    restrict_writes(repo, log)?;
    let err = Command::new(&command[0]).args(&command[1..]).exec();
    Err(err).with_context(|| format!("failed to exec {}", command[0]))
}

/// Deny writes everywhere but the given paths and the tool state directories.
fn restrict_writes(repo: &Path, log: &Path) -> Result<()> {
    // Below ABI 2 the kernel cannot allow cross-directory renames at all,
    // which git needs constantly; a sandbox that breaks every commit is not
    // usable, so require a kernel that can do this properly.
    let abi = probe_abi();
    if abi < 2 {
        bail!(
            "Landlock is unusable on this kernel (ABI {abi}, need >= 2); \
             refusing to run the agent unconfined"
        );
    }
    // Everything V3 offers, degraded by best-effort on ABI 2 kernels (which
    // merely leaves truncation unrestricted).
    let access = AccessFs::from_write(ABI::V3);
    let status = Ruleset::default()
        .handle_access(access)?
        .create()?
        .add_rules(path_beneath_rules(writable_paths(repo, log), access))?
        .restrict_self()?;
    if status.ruleset == RulesetStatus::NotEnforced {
        bail!("Landlock ruleset was not enforced; refusing to run the agent unconfined");
    }
    Ok(())
}

/// Where the agent may write: the assigned clone, the turn log, and the
/// dotfile state of the tools it runs (opencode sessions, OAuth refreshes,
/// forge CLI state, package manager caches). Nonexistent paths are skipped —
/// with no rule there is nothing to allow.
fn writable_paths(repo: &Path, log: &Path) -> Vec<PathBuf> {
    let home = crate::config::home();
    let xdg = |var: &str, default: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(default))
    };
    let mut paths = vec![
        repo.to_path_buf(),
        log.to_path_buf(),
        home.join(".claude"),
        home.join(".npm"),
        home.join(".bun"),
        // Rust builds unpack registry crates and fetch toolchains here.
        home.join(".cargo"),
        home.join(".rustup"),
        xdg("XDG_CONFIG_HOME", ".config"),
        xdg("XDG_CACHE_HOME", ".cache"),
        xdg("XDG_DATA_HOME", ".local/share"),
        xdg("XDG_STATE_HOME", ".local/state"),
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
        PathBuf::from("/dev"),
        // A single-user Nix writes the store, its database, and its locks
        // directly (no daemon), and the prompts steer the agent into each
        // repo's nix shell; the store is tool space, not checkout space.
        PathBuf::from("/nix"),
    ];
    // The state directories may not exist until a tool first writes there,
    // which the sandbox would then deny; make the home-based ones real now.
    for path in &paths {
        if path.starts_with(&home) {
            std::fs::create_dir_all(path).ok();
        }
    }
    paths.retain(|path| path.exists());
    paths
}

/// The kernel's Landlock ABI version, or a negative errno-ish value when the
/// syscall is unavailable altogether.
fn probe_abi() -> i64 {
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
    unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Landlock restrictions apply per thread, so each test confines only
    /// itself and the rest of the suite runs unrestricted.
    #[test]
    fn writes_outside_the_allowlist_are_denied() {
        if probe_abi() < 2 {
            eprintln!("kernel has no usable Landlock; skipping");
            return;
        }
        let repo = tempfile::tempdir().unwrap();
        let log = repo.path().join("turn.log");
        std::fs::write(&log, "").unwrap();
        // Somewhere real that is not on the allowlist; target/ is already
        // build scratch space.
        let outside = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/landlock-probe");
        std::fs::remove_file(&outside).ok();

        restrict_writes(repo.path(), &log).unwrap();
        assert!(std::fs::write(repo.path().join("inside"), "ok").is_ok());
        assert!(std::fs::write(&log, "appended").is_ok());
        let denied = std::fs::write(&outside, "nope").unwrap_err();
        assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn wrap_builds_the_wrapper_argv() {
        let argv = wrap(Path::new("/w/repo"), Path::new("/w/logs/1.log"));
        assert_eq!(&argv[1..], [MARKER, "/w/repo", "/w/logs/1.log", "--"]);
    }

    #[test]
    fn exec_rejects_malformed_invocations() {
        assert!(exec(&[]).is_err());
        assert!(exec(&["repo".into(), "log".into(), "--".into()]).is_err());
        assert!(exec(&["repo".into(), "log".into(), "true".into()]).is_err());
    }
}
