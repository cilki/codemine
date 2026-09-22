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
    crate::config::xdg_dir("XDG_CONFIG_HOME", ".config").join("opencode")
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
    // opencode's config maps server names directly under `mcp`; an earlier
    // version nested them under `mcp.servers`, which opencode reads as a
    // server named "servers" and rejects the whole config over, so drop the
    // leftover on the way through. Accessed via get_mut, not indexing:
    // IndexMut on Value inserts null for missing keys, and a written-out
    // `"servers": null` (which one buggy version did leave behind, hence the
    // Null arm) fails opencode's schema just the same.
    let drop_servers = match config.get_mut("mcp").and_then(|mcp| mcp.get_mut("servers")) {
        Some(serde_json::Value::Object(servers)) => {
            servers.remove("codegraph");
            servers.is_empty()
        }
        Some(serde_json::Value::Null) => true,
        _ => false,
    };
    if drop_servers
        && let Some(mcp) = config.get_mut("mcp").and_then(|mcp| mcp.as_object_mut()) {
            mcp.remove("servers");
        }
    config["mcp"]["codegraph"] = serde_json::json!({
        "type": "local",
        "command": ["codegraph", "serve", "--mcp"],
        "enabled": true,
    });
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Symlink the opencode-claude-auth plugin into opencode's plugin directory,
/// so any image that ships the package gets the anthropic provider without
/// image-specific wiring. The package must be recent enough to present
/// requests as a Claude Code session, or Anthropic bills them as a
/// third-party app drawing extra usage instead of the subscription
/// (opencode-claude-auth#145); nix/nixpkgs.nix overlays the pin accordingly.
/// A missing package is only a warning: opencode still runs, just without
/// Claude models.
pub fn install_plugin(dir: &Path) -> Result<()> {
    scrub_plugin_config(dir)?;
    match claude_auth_entrypoint() {
        Some(entrypoint) => link_plugin(dir, &entrypoint),
        // The host may provision the plugin straight into opencode's plugin
        // directory instead of a node_modules root (the NixOS module does);
        // only warn when it's nowhere at all. metadata() follows symlinks,
        // so a dangling link left by an uninstall doesn't count.
        None if ["plugin", "plugins"].iter().any(|sub| {
            std::fs::metadata(dir.join(sub).join("opencode-claude-auth.js")).is_ok()
        }) =>
        {
            Ok(())
        }
        None => {
            tracing::warn!(
                "opencode-claude-auth is not installed; opencode will have no anthropic provider"
            );
            Ok(())
        }
    }
}

/// Drop npm references to the plugin (under its current or previous name)
/// from opencode's config: alongside the symlink they'd load a second,
/// differently-versioned copy from the npm registry. The config is only
/// rewritten when something was actually dropped, because it is often
/// mounted read-only.
fn scrub_plugin_config(dir: &Path) -> Result<()> {
    let path = dir.join("opencode.json");
    let Some(mut config) = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    else {
        return Ok(());
    };
    let Some(plugins) = config["plugin"].as_array() else {
        return Ok(());
    };
    let kept: Vec<serde_json::Value> = plugins
        .iter()
        .filter(|entry| {
            !entry.as_str().is_some_and(|entry| {
                entry.contains("opencode-claude-auth") || entry.contains("opencode-auth-plugin")
            })
        })
        .cloned()
        .collect();
    if kept.len() == plugins.len() {
        return Ok(());
    }
    config["plugin"] = kept.into();
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// The plugin's entrypoint under the usual global node_modules roots.
fn claude_auth_entrypoint() -> Option<PathBuf> {
    [
        crate::config::home().join(".nix-profile/lib/node_modules"),
        PathBuf::from("/usr/local/lib/node_modules"),
        PathBuf::from("/usr/lib/node_modules"),
    ]
    .iter()
    .map(|root| root.join("opencode-claude-auth"))
    .find_map(|pkg| package_entrypoint(&pkg))
}

/// The file package.json's `main` names, defaulting to index.js like node;
/// None when the package or the file isn't there.
fn package_entrypoint(pkg: &Path) -> Option<PathBuf> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(pkg.join("package.json")).ok()?).ok()?;
    let entrypoint = pkg.join(manifest["main"].as_str().unwrap_or("index.js"));
    entrypoint.is_file().then_some(entrypoint)
}

fn link_plugin(dir: &Path, entrypoint: &Path) -> Result<()> {
    let plugin_dir = dir.join("plugin");
    std::fs::create_dir_all(&plugin_dir)
        .with_context(|| format!("failed to create {}", plugin_dir.display()))?;
    let link = plugin_dir.join("opencode-claude-auth.js");
    if let Err(err) = std::fs::remove_file(&link)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        return Err(err).with_context(|| format!("failed to remove {}", link.display()));
    }
    std::os::unix::fs::symlink(entrypoint, &link)
        .with_context(|| format!("failed to link {}", link.display()))
}

/// Where opencode stores provider credentials.
pub fn opencode_auth_json() -> PathBuf {
    crate::config::xdg_dir("XDG_DATA_HOME", ".local/share").join("opencode/auth.json")
}

/// Drop the anthropic entry from opencode's stored credentials so the plugin
/// re-derives it from the Claude Code credentials file: a stale or hand-added
/// entry makes opencode call Anthropic as a plain third-party app, which
/// bills extra usage instead of the subscription. A missing or malformed
/// file is left for opencode to sort out.
pub fn scrub_anthropic_auth(path: &Path) -> Result<()> {
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(());
    };
    let Ok(mut auth) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        tracing::warn!("{} is not valid JSON; leaving it alone", path.display());
        return Ok(());
    };
    if auth
        .as_object_mut()
        .is_some_and(|auth| auth.remove("anthropic").is_some())
    {
        std::fs::write(path, serde_json::to_vec_pretty(&auth)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

/// Whether opencode's config already names the codegraph MCP server; an
/// unreadable or malformed file is treated as not configuring it, so this
/// never turns into a startup failure.
fn configures_codegraph(path: &Path) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|config| {
            !config["mcp"]["codegraph"].is_null()
                || !config["mcp"]["servers"]["codegraph"].is_null()
        })
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
                "mutation",
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
            config["mcp"]["codegraph"]["command"],
            serde_json::json!(["codegraph", "serve", "--mcp"])
        );
        assert_eq!(config["mcp"]["codegraph"]["type"], "local");
        // A null `servers` key must not appear either: opencode rejects the
        // whole config over `mcp.servers: null`, and is_null() can't tell a
        // missing key from a literal null.
        assert!(!config["mcp"].as_object().unwrap().contains_key("servers"));

        std::fs::write(
            &path,
            r#"{"theme":"dark","mcp":{"other":{"type":"remote","url":"http://x"}}}"#,
        )
        .unwrap();
        install_mcp(dir.path(), true).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(config["theme"], "dark");
        assert_eq!(config["mcp"]["other"]["type"], "remote");
        assert_eq!(
            config["mcp"]["codegraph"]["enabled"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn install_mcp_drops_stale_servers_nesting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");

        // A previous version nested the entry under mcp.servers, which
        // opencode rejects wholesale; install_mcp migrates it away.
        std::fs::write(
            &path,
            r#"{"mcp":{"servers":{"codegraph":{"type":"stdio","command":"codegraph"}}}}"#,
        )
        .unwrap();
        install_mcp(dir.path(), true).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(!config["mcp"].as_object().unwrap().contains_key("servers"));
        assert_eq!(config["mcp"]["codegraph"]["type"], "local");

        // A literal `"servers": null` (left behind by a buggy version that
        // auto-vivified it while migrating) is dropped too.
        std::fs::write(&path, r#"{"mcp":{"servers":null}}"#).unwrap();
        install_mcp(dir.path(), true).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(!config["mcp"].as_object().unwrap().contains_key("servers"));

        // Entries codemine didn't write stay put, even under mcp.servers.
        std::fs::write(&path, r#"{"mcp":{"servers":{"codegraph":{},"other":{}}}}"#).unwrap();
        install_mcp(dir.path(), true).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(!config["mcp"]["servers"]["other"].is_null());
        assert!(config["mcp"]["servers"]["codegraph"].is_null());
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
