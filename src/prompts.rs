//! Starter prompts baked into the binary and installed into opencode's config
//! directory at startup, so the binary works without the image copying them.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

/// The only opencode command the runner drives; `SWEEP` is installed under
/// this name and every turn runs it.
pub const SWEEP_COMMAND: &str = "sweep";

pub const SWEEP: &[u8] = include_bytes!("../commands/sweep.md");
pub const GITEA_SKILL: &[u8] = include_bytes!("../skills/gitea/SKILL.md");
pub const GITHUB_SKILL: &[u8] = include_bytes!("../skills/github/SKILL.md");
pub const GITLAB_SKILL: &[u8] = include_bytes!("../skills/gitlab/SKILL.md");

/// Where opencode looks for commands and skills: `$XDG_CONFIG_HOME/opencode`,
/// else `~/.config/opencode`.
pub fn opencode_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return Path::new(&dir).join("opencode");
    }
    crate::config::home().join(".config/opencode")
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
/// Without codegraph the config is left alone, since it is often mounted
/// read-only; a leftover entry is reported instead of removed, because
/// opencode would try to spawn a binary that isn't there.
pub fn install_mcp(dir: &Path, codegraph: bool) -> Result<()> {
    let path = dir.join("opencode.json");
    if !codegraph {
        if configures_codegraph(&path) {
            tracing::warn!(
                "{} still configures the codegraph MCP server; remove the entry by hand",
                path.display()
            );
        }
        return Ok(());
    }
    let mut config: serde_json::Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not valid JSON", path.display()))?,
        Err(_) => serde_json::json!({}),
    };
    config["mcp"]["servers"]["codegraph"] = serde_json::json!({
        "type": "stdio",
        "command": "codegraph",
        "args": ["serve", "--mcp"],
        // Keep codegraph_explore on the native tool list.
        "codemode": false,
    });
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Whether opencode's config already names the codegraph MCP server; an
/// unreadable or malformed file is treated as not configuring it, so this
/// never turns into a startup failure.
fn configures_codegraph(path: &Path) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|config| !config["mcp"]["servers"]["codegraph"].is_null())
}

/// One task the runner can draw: the slug passed to the sweep prompt, plus
/// the instructions that slug selects, so the UI can explain a task without a
/// second copy of the text to keep in sync.
#[derive(Serialize)]
pub struct Task {
    pub slug: String,
    pub description: String,
}

/// The task pool, parsed from the sweep command's `## "slug"` sections.
pub fn tasks() -> Vec<Task> {
    let mut sections: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut inside = false;
    for line in str::from_utf8(SWEEP).expect("sweep.md is UTF-8").lines() {
        if let Some(slug) = line.strip_prefix("## \"").and_then(|s| s.strip_suffix('"')) {
            sections.push((slug, Vec::new()));
            inside = true;
        } else if line.starts_with('#') {
            // Any other heading ends the run of task sections.
            inside = false;
        } else if inside {
            sections
                .last_mut()
                .expect("inside a section implies there is one")
                .1
                .push(line);
        }
    }
    sections
        .into_iter()
        .map(|(slug, lines)| Task {
            slug: slug.to_owned(),
            description: describe(&lines),
        })
        .collect()
}

/// A task section's markdown bullets as tooltip text: one line per bullet,
/// with the soft-wrapped continuation lines joined back up.
fn describe(lines: &[&str]) -> String {
    let mut bullets: Vec<String> = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match trimmed.strip_prefix("- ") {
            Some(text) => {
                let indent = &line[..line.len() - line.trim_start().len()];
                bullets.push(format!("{indent}- {text}"));
            }
            None => match bullets.last_mut() {
                Some(bullet) => {
                    bullet.push(' ');
                    bullet.push_str(trimmed);
                }
                None => bullets.push(trimmed.to_owned()),
            },
        }
    }
    bullets.join("\n")
}

/// The default task pool: every task the sweep command defines.
pub fn default_tasks() -> Vec<String> {
    tasks().into_iter().map(|task| task.slug).collect()
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
                "bump",
                "simplify",
                "todo",
                "roleplay",
                "benchmark",
                "audit",
                "docs",
                "coverage",
                "feature"
            ]
        );
    }

    #[test]
    fn tasks_carry_their_sweep_instructions() {
        let tasks = tasks();
        let by_slug = |slug: &str| {
            tasks
                .iter()
                .find(|task| task.slug == slug)
                .unwrap_or_else(|| panic!("no {slug} task"))
                .description
                .clone()
        };

        // Wrapped lines are joined back into one bullet per instruction.
        assert_eq!(
            by_slug("todo"),
            "- Handle a TODO comment in the code or a TODO list item from the \
             project's AGENTS.md"
        );
        assert_eq!(by_slug("feedback").lines().count(), 2);
        // Nested bullets keep their indentation.
        assert!(
            by_slug("bump").contains("\n  - Only make a PR"),
            "{}",
            by_slug("bump")
        );
        // The `# General information` section is not a task and its prose
        // never lands on the task above it.
        assert!(!tasks.iter().any(|task| task.slug.starts_with('#')));
        assert!(
            !by_slug("roleplay").contains("nix"),
            "{}",
            by_slug("roleplay")
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
    fn install_mcp_leaves_config_alone_without_codegraph() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");

        // No config file: nothing created.
        install_mcp(dir.path(), false).unwrap();
        assert!(!path.exists());

        // An existing config is left byte-for-byte alone, entry or not, so a
        // read-only file can't fail the run.
        for original in [
            r#"{"theme":"dark"}"#,
            r#"{"mcp":{"servers":{"codegraph":{"type":"stdio"}}}}"#,
        ] {
            std::fs::write(&path, original).unwrap();
            install_mcp(dir.path(), false).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
    }
}
