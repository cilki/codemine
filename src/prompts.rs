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

/// Merge the codegraph MCP server into opencode's config, giving the agent
/// the `codegraph_explore` tool; everything else in the file is preserved.
/// When codegraph is not installed the entry is removed instead, so opencode
/// never tries to spawn a missing binary.
pub fn install_mcp(dir: &Path, codegraph: bool) -> Result<()> {
    let path = dir.join("opencode.json");
    let mut config: serde_json::Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not valid JSON", path.display()))?,
        Err(_) if !codegraph => return Ok(()),
        Err(_) => serde_json::json!({}),
    };
    if codegraph {
        config["mcp"]["servers"]["codegraph"] = serde_json::json!({
            "type": "stdio",
            "command": "codegraph",
            "args": ["serve", "--mcp"],
            // Keep codegraph_explore on the native tool list.
            "codemode": false,
        });
    } else if let Some(servers) = config
        .get_mut("mcp")
        .and_then(|mcp| mcp.get_mut("servers"))
        .and_then(|servers| servers.as_object_mut())
    {
        servers.remove("codegraph");
    }
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("failed to write {}", path.display()))?;
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

    #[test]
    fn install_mcp_merges_into_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");

        install_mcp(dir.path(), true).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            config["mcp"]["servers"]["codegraph"]["command"],
            "codegraph"
        );

        std::fs::write(
            &path,
            r#"{"theme":"dark","mcp":{"servers":{"other":{"type":"stdio"}}}}"#,
        )
        .unwrap();
        install_mcp(dir.path(), true).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(config["theme"], "dark");
        assert_eq!(config["mcp"]["servers"]["other"]["type"], "stdio");
        assert_eq!(
            config["mcp"]["servers"]["codegraph"]["codemode"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn install_mcp_removes_entry_without_codegraph() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");

        // No config file: nothing to remove, nothing created.
        install_mcp(dir.path(), false).unwrap();
        assert!(!path.exists());

        install_mcp(dir.path(), true).unwrap();
        install_mcp(dir.path(), false).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(config["mcp"]["servers"].get("codegraph").is_none());
    }
}
