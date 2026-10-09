//! Starter prompts baked into the binary and installed into opencode's config
//! directory at startup, so the binary works without the image copying them.

use std::collections::BTreeSet;
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

/// opencode's config file. Three separate pieces of startup work edit it and
/// a fourth reads it, so the name lives here rather than in each of them.
fn opencode_json(dir: &Path) -> PathBuf {
    dir.join("opencode.json")
}

/// A JSON file as a `Value`. A file that isn't there — or is there and
/// holds nothing but whitespace, which is what a `touch` or an interrupted
/// write leaves — reads as an empty object: every caller only ever adds to
/// or subtracts from what it finds, so there is nothing to tell apart. Any
/// other unparseable file is an error naming it, because somebody wrote that
/// file and the alternative is `edit_json` answering a typo in it by
/// replacing the lot.
fn read_json(path: &Path) -> Result<serde_json::Value> {
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(serde_json::json!({}));
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_slice(&bytes).with_context(|| format!("{} is not valid JSON", path.display()))
}

/// Read a JSON file, let `edit` change what it found, and write it back when
/// — and only when — the edit changed something.
///
/// Nothing here owns a whole file: opencode's config carries the user's own
/// settings alongside the provider options and MCP entry codemine maintains,
/// and its auth.json is entirely theirs but for one dead entry. So an
/// already-correct file is left as it is, which is what lets it be mounted
/// read-only — the shape every deployment that configures opencode itself
/// has, and the reason `install_mcp` gives for not touching the file at all
/// when codegraph is missing.
///
/// The write goes through the same writer the settings and the skip cache
/// use: a temp file beside the target, created owner-only and renamed into
/// place. Writing in place left the client key in a file the umask had made
/// world-readable, and a half-written config behind if the write failed
/// partway; neither window exists here, for any of the callers rather than
/// just the one that happens to carry the key.
fn edit_json(path: &Path, edit: impl FnOnce(&mut serde_json::Value)) -> Result<()> {
    let before = read_json(path)?;
    let mut after = before.clone();
    edit(&mut after);
    if after == before {
        return Ok(());
    }
    crate::workspace::write_json(path, &after)
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
    let path = opencode_json(dir);
    if !codegraph {
        if configures_codegraph(&path) {
            tracing::warn!(
                "{} still configures the codegraph MCP server; remove the entry by hand",
                path.display()
            );
        }
        return Ok(());
    }
    edit_json(&path, |config| {
        // opencode's config maps server names directly under `mcp`; an
        // earlier version nested them under `mcp.servers`, which opencode
        // reads as a server named "servers" and rejects the whole config
        // over, so drop the leftover on the way through. Accessed via
        // get_mut, not indexing: IndexMut on Value inserts null for missing
        // keys, and a written-out `"servers": null` (which one buggy version
        // did leave behind, hence the Null arm) fails opencode's schema just
        // the same.
        let drop_servers = match config.get_mut("mcp").and_then(|mcp| mcp.get_mut("servers")) {
            Some(serde_json::Value::Object(servers)) => {
                servers.remove("codegraph");
                servers.is_empty()
            }
            Some(serde_json::Value::Null) => true,
            _ => false,
        };
        if drop_servers && let Some(mcp) = config.get_mut("mcp").and_then(|mcp| mcp.as_object_mut())
        {
            mcp.remove("servers");
        }
        config["mcp"]["codegraph"] = serde_json::json!({
            "type": "local",
            "command": ["codegraph", "serve", "--mcp"],
            "enabled": true,
        });
    })
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
/// with no auth.json entry and no plugin. The file is written owner-only
/// since it carries the key — see [`edit_json`].
pub fn install_provider(dir: &Path, proxy: &crate::settings::ProxySettings) -> Result<()> {
    edit_json(&opencode_json(dir), |config| {
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
    })
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
    edit_json(auth_json, |auth| {
        if let Some(auth) = auth.as_object_mut() {
            auth.remove("anthropic");
        }
    })
}

/// Where opencode stores provider credentials; only touched to scrub the
/// legacy anthropic entry.
pub fn opencode_auth_json() -> PathBuf {
    crate::config::xdg_dir("XDG_DATA_HOME", ".local/share").join("opencode/auth.json")
}

/// Drop npm references to the retired plugin (under its current or previous
/// name) from opencode's config.
fn scrub_plugin_config(dir: &Path) -> Result<()> {
    edit_json(&opencode_json(dir), |config| {
        // get_mut rather than indexing: IndexMut would plant a null `plugin`
        // key in a config that never had one, and that alone counts as a
        // change worth writing back.
        if let Some(plugins) = config
            .get_mut("plugin")
            .and_then(serde_json::Value::as_array_mut)
        {
            plugins.retain(|entry| {
                !entry.as_str().is_some_and(|entry| {
                    entry.contains("opencode-claude-auth") || entry.contains("opencode-auth-plugin")
                })
            });
        }
    })
}

/// Whether opencode's config already names the codegraph MCP server. This is
/// the one arm that runs when the config may be untouchable, so an
/// unparseable file is treated as not configuring it rather than becoming a
/// startup failure.
fn configures_codegraph(path: &Path) -> bool {
    let config = read_json(path).unwrap_or_default();
    !config["mcp"]["codegraph"].is_null() || !config["mcp"]["servers"]["codegraph"].is_null()
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

        // An operator's own config arrives at whatever their umask allowed;
        // the key must not inherit that.
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

    /// A rename gives the file a new inode, so this tells a file that was
    /// rewritten with identical bytes from one that was never written at all.
    fn inode(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).unwrap().ino()
    }

    /// A config that already says everything codemine needs is left alone
    /// rather than rewritten. The arm that runs without codegraph refuses to
    /// touch the file at all on the grounds that it is often mounted
    /// read-only; the arms that maintain the entries used to rewrite exactly
    /// such a file on every startup and every settings save, which is a
    /// startup failure on a config nobody can write.
    #[test]
    fn an_unchanged_config_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = opencode_json(dir.path());
        let auth = dir.path().join("auth.json");
        let proxy = crate::settings::ProxySettings::default();

        install_mcp(dir.path(), true).unwrap();
        install_provider(dir.path(), &proxy).unwrap();
        let before = (std::fs::read(&path).unwrap(), inode(&path));

        install_mcp(dir.path(), true).unwrap();
        install_provider(dir.path(), &proxy).unwrap();
        remove_claude_plugin(dir.path(), &auth).unwrap();
        assert_eq!((std::fs::read(&path).unwrap(), inode(&path)), before);
        // There was no stale entry to subtract, so auth.json isn't conjured
        // up to hold the absence of one.
        assert!(!auth.exists());

        // A real change still lands.
        install_provider(
            dir.path(),
            &crate::settings::ProxySettings {
                api_key: "k2".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_ne!(inode(&path), before.1);
    }

    /// Every write of the config is owner-only, not just the one that
    /// happens to carry the client key: `install_mcp` runs first and creates
    /// the file, and the key lands in it moments later.
    #[test]
    fn the_config_is_owner_only_whichever_write_created_it() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;

        let dir = tempfile::tempdir().unwrap();
        install_mcp(dir.path(), true).unwrap();
        assert_eq!(mode(&opencode_json(dir.path())), 0o600);

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            opencode_json(dir.path()),
            r#"{"plugin":["opencode-claude-auth"]}"#,
        )
        .unwrap();
        remove_claude_plugin(dir.path(), &dir.path().join("auth.json")).unwrap();
        assert_eq!(mode(&opencode_json(dir.path())), 0o600);
    }

    /// An empty config is what a `touch` or an interrupted write leaves, and
    /// there is nothing in it to preserve — reading it as an empty object
    /// beats failing startup over it. A file with something in it that isn't
    /// JSON is a different thing: somebody wrote that, and the alternative
    /// to an error is replacing their typo.
    #[test]
    fn an_empty_config_is_not_a_malformed_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = opencode_json(dir.path());
        let proxy = crate::settings::ProxySettings::default();

        std::fs::write(&path, "\n  \n").unwrap();
        install_provider(dir.path(), &proxy).unwrap();
        install_mcp(dir.path(), true).unwrap();
        let config = read_json(&path).unwrap();
        assert_eq!(
            config["provider"]["anthropic"]["options"]["baseURL"],
            format!("{}/v1", crate::proxy::DEFAULT_BASE_URL)
        );
        assert_eq!(config["mcp"]["codegraph"]["type"], "local");

        std::fs::write(&path, "{ not json").unwrap();
        let err = install_provider(dir.path(), &proxy).unwrap_err();
        assert!(format!("{err:#}").contains("is not valid JSON"), "{err:#}");
        assert!(install_mcp(dir.path(), true).is_err());
        // Left exactly as it was found, rather than replaced.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
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
