//! Commit signing with an SSH key only the workspace holds, so the commits
//! turns make show as verified on the forge and can land on branches that
//! require signed commits.
//!
//! Signing is git config rather than an instruction in the prompt: the block
//! [`git_config`] returns goes into the global config every git the runner
//! spawns reads (see `install_git_config`), so the agent can't forget it, and
//! `git rebase`, `git am` and `git cherry-pick` sign what they rewrite too.
//! A clone's own config can't turn it back off: `workspace::reset_local_config`
//! keeps none of its `commit.*`, `gpg.*` or `user.*` keys.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::child;
use crate::workspace;

/// Where the private key lives, under the workspace; the public key is next
/// to it with `.pub` appended, as `ssh-keygen` writes it. On the volume, so
/// it survives restarts and reclones, and a key registered once on the forge
/// stays the one that signs.
pub fn key_path(workspace: &Path) -> PathBuf {
    workspace.join("signing").join("id_ed25519")
}

/// Held while a key is generated: the web UI and the main loop both ask for
/// the key, and two `ssh-keygen` runs racing would leave whichever renamed
/// last, so the key one of them showed would not be the one that signs.
static GENERATING: Mutex<()> = Mutex::new(());

/// The public key, as one `ssh-ed25519 AAAA… codemine` line, generating the
/// pair first when the workspace has none yet.
pub fn ensure_key(workspace: &Path) -> Result<String> {
    let _held = crate::sync::lock(&GENERATING);
    let key = key_path(workspace);
    let public = public_path(&key);
    if !key.exists() {
        generate(&key)?;
    }
    match std::fs::read_to_string(&public) {
        Ok(line) => Ok(line.trim().to_owned()),
        // The private key is the one that matters; a lost `.pub` is derived
        // from it again rather than replaced along with it, which would
        // invalidate a key already registered on the forge.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let line = child::output(Command::new("ssh-keygen").arg("-y").arg("-f").arg(&key))?;
            let line = String::from_utf8_lossy(&line).trim().to_owned();
            workspace::write_private(&public, format!("{line}\n").as_bytes())?;
            Ok(line)
        }
        Err(err) => Err(err).with_context(|| format!("failed to read {}", public.display())),
    }
}

fn public_path(key: &Path) -> PathBuf {
    let mut public = key.as_os_str().to_owned();
    public.push(".pub");
    PathBuf::from(public)
}

/// Generate a passphraseless ed25519 pair at `key`. Written into a private
/// scratch directory beside it and renamed into place, the private key last,
/// so a crash mid-generation leaves no key rather than half of one — and the
/// next call, which goes by the private key alone, starts over.
fn generate(key: &Path) -> Result<()> {
    let dir = key.parent().expect("the key path has a directory");
    workspace::private_dir(dir)?;
    let scratch = tempfile::tempdir_in(dir)
        .with_context(|| format!("failed to make a scratch directory in {}", dir.display()))?;
    let fresh = scratch.path().join("id_ed25519");
    child::output(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "codemine", "-f"])
            .arg(&fresh),
    )
    .context("failed to generate the commit signing key")?;
    std::fs::rename(public_path(&fresh), public_path(key))
        .with_context(|| format!("failed to move the public key into {}", dir.display()))?;
    std::fs::rename(&fresh, key)
        .with_context(|| format!("failed to move the signing key into {}", dir.display()))?;
    Ok(())
}

/// The git config that signs every commit and tag with `key`. It goes after
/// the `[include]` of the user's own config, so it wins over anything that
/// says otherwise there — `gpg.ssh.program` included, which would otherwise
/// hand the signing to whatever agent the host uses (1Password's
/// `op-ssh-sign`, say) instead of the key on disk.
pub fn git_config(key: &Path) -> String {
    format!(
        "[gpg]\n\tformat = ssh\n\
         [gpg \"ssh\"]\n\tprogram = ssh-keygen\n\
         [user]\n\tsigningkey = \"{}\"\n\
         [commit]\n\tgpgsign = true\n\
         [tag]\n\tgpgsign = true\n",
        key.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

/// Make one signed commit, in a scratch repository, through the git config
/// at `gitconfig`: what every commit in every turn would otherwise find out
/// on its own, as `error: gpg failed to sign the data`.
pub fn probe(gitconfig: &Path) -> Result<()> {
    let scratch = tempfile::tempdir().context("failed to make a scratch repository")?;
    let git = |args: &[&str]| {
        child::output(
            Command::new("git")
                .args(args)
                .current_dir(scratch.path())
                .env("GIT_CONFIG_GLOBAL", gitconfig)
                .env("GIT_AUTHOR_NAME", "codemine")
                .env("GIT_AUTHOR_EMAIL", "codemine@localhost")
                .env("GIT_COMMITTER_NAME", "codemine")
                .env("GIT_COMMITTER_EMAIL", "codemine@localhost"),
        )
    };
    git(&["init", "-q"])?;
    let tree = git(&["write-tree"])?;
    let tree = String::from_utf8_lossy(&tree).trim().to_owned();
    git(&["commit-tree", "-S", "-m", "probe", &tree]).context("git could not sign a commit")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The tests that sign need `ssh-keygen`, which git itself calls to sign;
    /// without it they have nothing to check and say so.
    fn have_ssh_keygen() -> bool {
        let found = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join("ssh-keygen").is_file())
        });
        if !found {
            eprintln!("ssh-keygen is not on the PATH; skipping");
        }
        found
    }

    fn git(dir: &Path, gitconfig: &Path, args: &[&str]) -> String {
        let out = child::output(
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", gitconfig)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "bot")
                .env("GIT_AUTHOR_EMAIL", "bot@example.com")
                .env("GIT_COMMITTER_NAME", "bot")
                .env("GIT_COMMITTER_EMAIL", "bot@example.com"),
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    /// The key is made once, owner-only, and then kept: a second ask, a
    /// restart, must hand back the same key, or the one the user registered
    /// on the forge stops matching what signs.
    #[test]
    fn the_key_is_generated_once_and_kept() {
        if !have_ssh_keygen() {
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let first = ensure_key(ws.path()).unwrap();
        assert!(first.starts_with("ssh-ed25519 "), "{first}");
        assert_eq!(ensure_key(ws.path()).unwrap(), first);

        let key = key_path(ws.path());
        let mode = std::fs::metadata(&key).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "key mode {mode:o}");
        let dir = std::fs::metadata(key.parent().unwrap()).unwrap();
        assert_eq!(dir.permissions().mode() & 0o077, 0);
        // Nothing of the scratch directory is left beside the key.
        let entries = std::fs::read_dir(key.parent().unwrap()).unwrap().count();
        assert_eq!(entries, 2);

        // A lost public half is derived from the private key again, not
        // replaced along with it.
        std::fs::remove_file(public_path(&key)).unwrap();
        assert_eq!(ensure_key(ws.path()).unwrap(), first);
    }

    /// A plain `git commit` made through the config, with nothing asking
    /// for a signature, is signed with the workspace key and verifies
    /// against its public half.
    #[test]
    fn commits_made_through_the_config_verify() {
        if !have_ssh_keygen() {
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let public = ensure_key(ws.path()).unwrap();
        let gitconfig = ws.path().join("gitconfig");
        let signers = ws.path().join("allowed_signers");
        std::fs::write(&signers, format!("bot@example.com {public}\n")).unwrap();
        std::fs::write(
            &gitconfig,
            format!(
                "{}[gpg \"ssh\"]\n\tallowedSignersFile = {}\n",
                git_config(&key_path(ws.path())),
                signers.display()
            ),
        )
        .unwrap();

        probe(&gitconfig).unwrap();

        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &gitconfig, &["init", "-q"]);
        std::fs::write(repo.path().join("f"), "x").unwrap();
        git(repo.path(), &gitconfig, &["add", "f"]);
        git(repo.path(), &gitconfig, &["commit", "-q", "-m", "signed"]);
        git(repo.path(), &gitconfig, &["verify-commit", "HEAD"]);
    }

    /// Without a usable key the probe fails, which is what holds turns
    /// instead of letting every commit in them fail.
    #[test]
    fn the_probe_fails_without_a_key() {
        let ws = tempfile::tempdir().unwrap();
        let gitconfig = ws.path().join("gitconfig");
        std::fs::write(&gitconfig, git_config(&key_path(ws.path()))).unwrap();
        assert!(probe(&gitconfig).is_err());
    }
}
