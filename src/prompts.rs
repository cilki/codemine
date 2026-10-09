//! Starter prompts baked into the binary and installed into opencode's config
//! directory at startup, so the binary works without the image copying them.

use std::collections::BTreeSet;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

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
    if drop_servers && let Some(mcp) = config.get_mut("mcp").and_then(|mcp| mcp.as_object_mut()) {
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

/// Hand the opencode plugin install to rtk itself: `rtk init -g --opencode`
/// writes `~/.config/opencode/plugins/rtk.ts` and nothing else, only when the
/// contents changed, so repeating it every startup costs nothing. The plugin
/// pipes each shell command the agent runs through `rtk rewrite`, swapping in
/// the rtk equivalent so the agent reads compressed build and test output
/// instead of the raw firehose. Without rtk on the PATH there is nothing to
/// install and the agent reads raw output; a plugin left behind by an earlier
/// install disables itself when it can't find the binary. Nothing here is
/// allowed to fail startup, which is why it reports instead of returning.
pub fn install_rtk() {
    let output = match Command::new("rtk")
        .args(["init", "-g", "--opencode"])
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(_) => {
            tracing::info!("rtk is not installed; the agent's commands run unrewritten");
            return;
        }
    };
    if !output.status.success() {
        tracing::warn!(
            "rtk init exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return;
    }
    if let Err(err) = install_rtk_config(&rtk_config_path()) {
        tracing::warn!("{err:#}");
    }
}

/// rtk's config, written only when the deployment hasn't supplied one — it is
/// rtk's file, not ours. `gh` and `glab` are kept out of the rewrite because
/// the forge skills have the agent read review comments and notification
/// thread ids straight out of those commands' output, and rtk compresses
/// exactly that away. The savings rtk is here for are in build and test
/// output, which these don't touch.
const RTK_CONFIG: &str = "\
# Written by codemine because no rtk config was present; edit freely.
[hooks]
exclude_commands = [\"gh\", \"glab\"]
";

/// Where rtk reads its config, resolved the way rtk resolves it: its
/// `dirs::config_dir` honors `XDG_CONFIG_HOME` exactly as `xdg_dir` does.
pub fn rtk_config_path() -> PathBuf {
    crate::config::xdg_dir("XDG_CONFIG_HOME", ".config").join("rtk/config.toml")
}

/// Write the config if there isn't one, leaving an operator's own settings
/// alone. The path is a parameter rather than read from the environment so the
/// test can point it at a tempdir without touching env the rest of the suite
/// shares.
fn install_rtk_config(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let parent = path.parent().expect("the config path has a parent");
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    std::fs::write(path, RTK_CONFIG).with_context(|| format!("failed to write {}", path.display()))
}

/// Stands in for the client key when none is configured. A CLIProxyAPI with
/// an empty `api-keys` list — the default — accepts any key, but the
/// anthropic provider refuses to send a request without one, so the field
/// can't just be left out.
const UNUSED_API_KEY: &str = "cliproxyapi";

/// Point opencode's anthropic provider at CLIProxyAPI: base URL and client
/// key merged into `opencode.json`, everything else preserved. The proxy
/// speaks Anthropic's own API shape, so the stock provider works against it
/// with no auth.json entry and no plugin. The file is made owner-only since
/// it may carry the key.
pub fn install_provider(dir: &Path, proxy: &crate::settings::ProxySettings) -> Result<()> {
    let path = dir.join("opencode.json");
    let mut config: serde_json::Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not valid JSON", path.display()))?,
        Err(_) => serde_json::json!({}),
    };
    config["provider"]["anthropic"]["options"] = serde_json::json!({
        // The provider's default is https://api.anthropic.com/v1, so the
        // version segment belongs to the base URL.
        "baseURL": format!("{}/v1", proxy.base_url),
        "apiKey": if proxy.api_key.is_empty() {
            UNUSED_API_KEY
        } else {
            &proxy.api_key
        },
    });
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    // Written through a temp file beside the target and renamed into place,
    // the way the settings store writes config.json. Writing the file and
    // narrowing it afterwards left the key in a file the umask had made
    // world-readable until the chmod landed, and left a half-written config
    // behind if the write failed partway; a temp file is created 0600 from
    // the start and the rename is atomic, so neither window exists.
    let mut file = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("failed to create a temp file in {}", dir.display()))?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(&serde_json::to_vec_pretty(&config)?)?;
    file.persist(&path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// One-time migration from the opencode-claude-auth era: drop the plugin
/// links a previous version (or the NixOS module) installed, its npm
/// references in `opencode.json`, and the stale `anthropic` entry in
/// opencode's auth.json — any of which would fight the provider config for
/// the account. The tokens in those files are dead weight now; the login
/// lives in CLIProxyAPI.
pub fn remove_claude_plugin(dir: &Path, auth_json: &Path) -> Result<()> {
    for sub in ["plugin", "plugins"] {
        let link = dir.join(sub).join("opencode-claude-auth.js");
        if let Err(err) = std::fs::remove_file(&link)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(err).with_context(|| format!("failed to remove {}", link.display()));
        }
    }
    scrub_plugin_config(dir)?;
    if let Some(mut auth) = std::fs::read(auth_json)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        && auth
            .as_object_mut()
            .is_some_and(|auth| auth.remove("anthropic").is_some())
    {
        std::fs::write(auth_json, serde_json::to_vec_pretty(&auth)?)
            .with_context(|| format!("failed to write {}", auth_json.display()))?;
    }
    Ok(())
}

/// Where opencode stores provider credentials; only touched to scrub the
/// legacy anthropic entry.
pub fn opencode_auth_json() -> PathBuf {
    crate::config::xdg_dir("XDG_DATA_HOME", ".local/share").join("opencode/auth.json")
}

/// Drop npm references to the retired plugin (under its current or previous
/// name) from opencode's config. The config is only rewritten when something
/// was actually dropped, because it is often mounted read-only.
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

/// Whether the sweep command defines this task. A pool slug it doesn't define
/// — a task that has since been removed, or a typo in a hand-edited
/// config.json — carries no instructions, so drawing it would spend a whole
/// agent session on a bare word. Parsed once: this is asked on the status
/// poll, not just at the turn boundary.
pub fn defines(slug: &str) -> bool {
    static SLUGS: OnceLock<BTreeSet<String>> = OnceLock::new();
    SLUGS
        .get_or_init(|| default_tasks().into_iter().collect())
        .contains(slug)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// What the stored task pool is filtered through, so it has to answer for
    /// exactly the sections the command has.
    #[test]
    fn defines_answers_for_the_sweep_sections_only() {
        for slug in default_tasks() {
            assert!(defines(&slug), "{slug}");
        }
        assert!(!defines("no-such-task"));
        assert!(!defines(""));
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
    fn install_provider_merges_and_restricts() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        let proxy = crate::settings::ProxySettings {
            api_key: "k1".into(),
            ..Default::default()
        };

        // No key configured: the provider is still wired to the proxy, with
        // the placeholder key a keyless proxy ignores.
        install_provider(
            dir.path(),
            &crate::settings::ProxySettings {
                api_key: String::new(),
                ..Default::default()
            },
        )
        .unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            config["provider"]["anthropic"]["options"]["apiKey"],
            UNUSED_API_KEY
        );

        install_provider(dir.path(), &proxy).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            config["provider"]["anthropic"]["options"]["baseURL"],
            format!("{}/v1", crate::proxy::DEFAULT_BASE_URL)
        );
        assert_eq!(config["provider"]["anthropic"]["options"]["apiKey"], "k1");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");

        // install_mcp writes the same file first and has no key to protect,
        // so the real sequence hands install_provider a config at whatever
        // the umask allowed; the key must not inherit that.
        std::fs::write(&path, r#"{"theme":"dark"}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        install_provider(dir.path(), &proxy).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");

        // A rewrite replaces the options wholesale (a changed key must not
        // leave the old one behind) but preserves the rest of the file.
        std::fs::write(
            &path,
            r#"{"theme":"dark","provider":{"anthropic":{"options":{"apiKey":"old","stale":true}}}}"#,
        )
        .unwrap();
        install_provider(dir.path(), &proxy).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(config["theme"], "dark");
        assert_eq!(config["provider"]["anthropic"]["options"]["apiKey"], "k1");
        assert!(config["provider"]["anthropic"]["options"]["stale"].is_null());
    }

    #[test]
    fn remove_claude_plugin_scrubs_the_legacy_era() {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth.json");
        for sub in ["plugin", "plugins"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
            std::fs::write(dir.path().join(sub).join("opencode-claude-auth.js"), "x").unwrap();
        }
        std::fs::write(
            dir.path().join("opencode.json"),
            r#"{"theme":"dark","plugin":["opencode-claude-auth@2.2.0","other-plugin"]}"#,
        )
        .unwrap();
        std::fs::write(
            &auth,
            r#"{"anthropic":{"type":"oauth","access":"a","refresh":"r"},"other":{"key":"k"}}"#,
        )
        .unwrap();

        remove_claude_plugin(dir.path(), &auth).unwrap();

        for sub in ["plugin", "plugins"] {
            assert!(
                !dir.path()
                    .join(sub)
                    .join("opencode-claude-auth.js")
                    .exists()
            );
        }
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("opencode.json")).unwrap())
                .unwrap();
        assert_eq!(config["theme"], "dark");
        assert_eq!(config["plugin"], serde_json::json!(["other-plugin"]));
        let auth_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&auth).unwrap()).unwrap();
        assert!(auth_json.get("anthropic").is_none());
        assert_eq!(auth_json["other"]["key"], "k");

        // Nothing to scrub: a second run is a clean no-op, and missing files
        // are fine.
        remove_claude_plugin(dir.path(), &auth).unwrap();
        remove_claude_plugin(tempfile::tempdir().unwrap().path(), &dir.path().join("no")).unwrap();
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

    /// rtk's own config is ours to create but not to own: the exclusions land
    /// when there is nothing there, and an operator's file survives untouched.
    #[test]
    fn install_rtk_config_only_writes_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtk/config.toml");

        install_rtk_config(&path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains(r#"exclude_commands = ["gh", "glab"]"#));

        let original = "[hooks]\nexclude_commands = [\"curl\"]\n";
        std::fs::write(&path, original).unwrap();
        install_rtk_config(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}
