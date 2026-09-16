//! Starter prompts baked into the binary and installed into opencode's config
//! directory at startup, so the binary works without the image copying them.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const SWEEP: &[u8] = include_bytes!("../commands/sweep.md");
pub const GITEA_SKILL: &[u8] = include_bytes!("../skills/gitea/SKILL.md");
pub const GITHUB_SKILL: &[u8] = include_bytes!("../skills/github/SKILL.md");
pub const GITLAB_SKILL: &[u8] = include_bytes!("../skills/gitlab/SKILL.md");

/// Where opencode looks for commands and skills: `$XDG_CONFIG_HOME/opencode`,
/// else `$HOME/.config/opencode`, else the container home.
pub fn opencode_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return Path::new(&dir).join("opencode");
    }
    if let Ok(home) = std::env::var("HOME") {
        return Path::new(&home).join(".config/opencode");
    }
    PathBuf::from("/root/.config/opencode")
}

/// Write the embedded prompts under `dir`, overwriting whatever is there; the
/// binary is the source of truth.
pub fn install(dir: &Path) -> Result<()> {
    for (path, bytes) in [
        ("commands/sweep.md", SWEEP),
        ("skills/gitea/SKILL.md", GITEA_SKILL),
        ("skills/github/SKILL.md", GITHUB_SKILL),
        ("skills/gitlab/SKILL.md", GITLAB_SKILL),
    ] {
        let path = dir.join(path);
        let parent = path.parent().expect("prompt paths have parents");
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        std::fs::write(&path, bytes)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

/// The default task pool, taken from the sweep command's `## "slug"` section
/// headings so the two can't drift apart.
pub fn default_tasks() -> Vec<String> {
    str::from_utf8(SWEEP)
        .expect("sweep.md is UTF-8")
        .lines()
        .filter_map(|line| line.strip_prefix("## \"")?.strip_suffix('"'))
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tasks_match_sweep_sections() {
        assert_eq!(
            default_tasks(),
            [
                "feedback",
                "rebase",
                "bump-deps",
                "simplify",
                "todo",
                "roleplay"
            ]
        );
    }

    #[test]
    fn install_writes_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path()).unwrap();
        let sweep = dir.path().join("commands/sweep.md");
        assert_eq!(std::fs::read(&sweep).unwrap(), SWEEP);
        for (path, bytes) in [
            ("skills/gitea/SKILL.md", GITEA_SKILL),
            ("skills/github/SKILL.md", GITHUB_SKILL),
            ("skills/gitlab/SKILL.md", GITLAB_SKILL),
        ] {
            assert_eq!(std::fs::read(dir.path().join(path)).unwrap(), bytes);
        }

        std::fs::write(&sweep, "stale").unwrap();
        install(dir.path()).unwrap();
        assert_eq!(std::fs::read(&sweep).unwrap(), SWEEP);
    }
}
