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
    // Creating a device node is handled — so the kernel denies it — but
    // granted nowhere, not even in the clone. It is no part of a build or a
    // commit, and a node the agent makes for itself is a write channel no
    // path rule can see: `mknod repo/disk b 259 2` inside the clone names
    // the partition the host's filesystem lives on, and writing it goes
    // around the ruleset entirely. The agent runs as whatever user the
    // runner does, which is root under the container image, and root there
    // holds CAP_MKNOD.
    let nodes = AccessFs::MakeChar | AccessFs::MakeBlock;
    let status = Ruleset::default()
        .handle_access(access)?
        .create()?
        .add_rules(path_beneath_rules(
            writable_paths(repo, log),
            access & !nodes,
        ))?
        .restrict_self()?;
    if status.ruleset == RulesetStatus::NotEnforced {
        bail!("Landlock ruleset was not enforced; refusing to run the agent unconfined");
    }
    Ok(())
}

/// The device files a build or a test legitimately writes to, named one by
/// one. `/dev` as a whole used to stand here, which handed the agent every
/// node the host exposes under it: `/dev/mem`, and the raw partition the
/// filesystem the ruleset is protecting lives on. Reads are unrestricted by
/// design, so nothing here is about hiding a device — only about not being
/// able to write one.
///
/// `/dev/pts` and `/dev/shm` are directories rather than devices, for the
/// pseudo-terminals a tool may allocate and the shared memory one may map;
/// neither is a channel to the disk. `/dev/stdout` and friends are symlinks
/// into `/proc/self/fd`, which the kernel resolves to the file the
/// descriptor already names, so they need no rule of their own.
const WRITABLE_DEVICES: [&str; 9] = [
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
    "/dev/ptmx",
    "/dev/pts",
    "/dev/shm",
];

/// Where the agent may write: the assigned clone, the turn log, the dotfile
/// state of the tools it runs (opencode sessions, forge CLI state, package
/// manager caches), and the handful of device files above. Nonexistent paths
/// are skipped — with no rule there is nothing to allow.
fn writable_paths(repo: &Path, log: &Path) -> Vec<PathBuf> {
    let home = crate::config::home();
    let xdg = crate::config::xdg_dir;
    let mut paths = vec![
        repo.to_path_buf(),
        log.to_path_buf(),
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
        // A single-user Nix writes the store, its database, and its locks
        // directly (no daemon), and the prompts steer the agent into each
        // repo's nix shell; the store is tool space, not checkout space.
        PathBuf::from("/nix"),
    ];
    paths.extend(WRITABLE_DEVICES.iter().map(PathBuf::from));
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

    /// A path the test could really write to before the ruleset was applied
    /// and that no rule covers, so a later denial is Landlock's doing rather
    /// than the filesystem's own permissions. The build tree is the natural
    /// pick, but a Nix build with `sandbox = false` unpacks into $TMPDIR, and
    /// /tmp is allowlisted, so fall back to somewhere else that is not.
    fn uncovered_writable_path(repo: &Path, log: &Path) -> Option<PathBuf> {
        let allowed = writable_paths(repo, log);
        let candidates = [
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
            PathBuf::from("/run"),
            PathBuf::from("/var/log"),
            PathBuf::from("/home"),
            PathBuf::from("/"),
        ];
        candidates.into_iter().find_map(|dir| {
            if allowed.iter().any(|root| dir.starts_with(root)) {
                return None;
            }
            let probe = dir.join("codemine-landlock-probe");
            let writable = std::fs::write(&probe, "probe").is_ok();
            std::fs::remove_file(&probe).ok();
            writable.then_some(probe)
        })
    }

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
        let Some(outside) = uncovered_writable_path(repo.path(), &log) else {
            eprintln!("nowhere writable outside the allowlist; skipping");
            return;
        };

        restrict_writes(repo.path(), &log).unwrap();
        assert!(std::fs::write(repo.path().join("inside"), "ok").is_ok());
        assert!(std::fs::write(&log, "appended").is_ok());
        let denied = std::fs::write(&outside, "nope").unwrap_err();
        assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
    }

    /// Some block device under `/dev`, whichever one — a raw disk is a raw
    /// disk. None on a host whose `/dev` has none (a container given only
    /// the character devices).
    fn some_block_device() -> Option<PathBuf> {
        use std::os::unix::fs::FileTypeExt;
        std::fs::read_dir("/dev").ok()?.flatten().find_map(|entry| {
            let block = entry.file_type().is_ok_and(|kind| kind.is_block_device());
            block.then(|| entry.path())
        })
    }

    /// Create a character device node at `path`, returning the raw errno
    /// outcome. `/dev/null`'s own numbers, so a node that does get created is
    /// harmless.
    fn mknod_char(path: &Path) -> std::io::Result<()> {
        let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        match unsafe { libc::mknod(path.as_ptr(), libc::S_IFCHR | 0o600, libc::makedev(1, 3)) } {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        }
    }

    /// The devices tools really do write to stay writable, and nothing else
    /// under `/dev` does. The whole directory used to be allowlisted, which
    /// covered every node on it: writing the host's raw partition went
    /// around the ruleset completely.
    #[test]
    fn only_the_listed_devices_are_writable() {
        if probe_abi() < 2 {
            eprintln!("kernel has no usable Landlock; skipping");
            return;
        }
        let repo = tempfile::tempdir().unwrap();
        let log = repo.path().join("turn.log");
        std::fs::write(&log, "").unwrap();
        let disk = some_block_device();

        restrict_writes(repo.path(), &log).unwrap();

        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/null")
                .is_ok(),
            "the agent still has to be able to discard output"
        );
        // /dev is no longer a directory the agent may add to either.
        let denied = std::fs::write("/dev/codemine-landlock-probe", "nope").unwrap_err();
        assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
        if let Some(disk) = disk {
            let denied = std::fs::OpenOptions::new()
                .write(true)
                .open(&disk)
                .expect_err(&format!("{} must not be writable", disk.display()));
            assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
        }
    }

    /// A node the agent makes for itself would be a write channel no path
    /// rule can see, so creating one is denied everywhere — the assigned
    /// clone included, which is otherwise entirely its own.
    #[test]
    fn no_device_node_can_be_created() {
        if probe_abi() < 2 {
            eprintln!("kernel has no usable Landlock; skipping");
            return;
        }
        let repo = tempfile::tempdir().unwrap();
        let log = repo.path().join("turn.log");
        std::fs::write(&log, "").unwrap();
        // Without CAP_MKNOD the kernel refuses for reasons of its own and a
        // later refusal would prove nothing, so establish it first.
        let before = repo.path().join("before");
        if let Err(err) = mknod_char(&before) {
            eprintln!("mknod is not permitted here at all ({err}); skipping");
            return;
        }
        std::fs::remove_file(&before).unwrap();

        restrict_writes(repo.path(), &log).unwrap();

        let denied = mknod_char(&repo.path().join("disk"))
            .expect_err("a device node in the clone must be denied");
        assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
        // Ordinary writes in the clone are untouched by the narrowing.
        assert!(std::fs::write(repo.path().join("inside"), "ok").is_ok());
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
