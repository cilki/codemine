//! Prompts baked into the binary: the sweep prompt every turn sends, and the
//! forge skills installed into opencode's config directory at startup, so
//! the binary works without the image copying them.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::Serialize;

/// The prompt every turn runs, rendered by `sweep_prompt`. It is written as
/// an opencode command (front matter, `$1`–`$4`) but no longer installed as
/// one: `opencode run` dropped `--command`, so the runner fills it in itself.
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
/// settings alongside the provider options and MCP entry codemine maintains.
/// So an already-correct file is left as it is, which is what lets it be
/// mounted read-only — the shape every deployment that configures opencode
/// itself has, and the reason `install_mcp` gives for not touching the file at all
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

/// The sweep prompt for one turn: `SWEEP` without its front matter, with
/// `$1`–`$4` replaced by the task, the repository, the forge, and the clone
/// the agent must work in, and the paragraph between the `codegraph` markers
/// kept only when the tool it recommends is there to use — `install_mcp`
/// wires `codegraph_explore` in on the same condition, and a prompt that
/// prefers a tool the session doesn't have is a prompt the agent has to
/// read past.
///
/// Replaced in a single pass, so a value that itself contains `$2` and the
/// like is inserted as it is rather than expanded again.
pub fn sweep_prompt(task: &str, repo: &str, forge: &str, dir: &Path, codegraph: bool) -> String {
    let source = str::from_utf8(SWEEP).expect("sweep.md is UTF-8");
    let dir = dir.display().to_string();
    let args = [task, repo, forge, dir.as_str()];
    let mut prompt = String::with_capacity(source.len());
    let source = without_codegraph_markers(strip_front_matter(source), codegraph);
    let mut rest = source.as_str();
    while let Some(at) = rest.find('$') {
        prompt.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let arg = after
            .chars()
            .next()
            .and_then(|c| c.to_digit(10))
            .and_then(|n| args.get((n as usize).checked_sub(1)?));
        match arg {
            Some(value) => {
                prompt.push_str(value);
                rest = &after[1..];
            }
            None => {
                prompt.push('$');
                rest = after;
            }
        }
    }
    prompt.push_str(rest);
    prompt
}

/// The sweep command with its `<!-- codegraph -->` … `<!-- /codegraph -->`
/// block either kept (without the marker lines) or dropped whole, blank line
/// after it included. A source with no block, or a half of one, is returned
/// as it is.
fn without_codegraph_markers(source: &str, keep: bool) -> String {
    const OPEN: &str = "<!-- codegraph -->\n";
    const CLOSE: &str = "<!-- /codegraph -->\n";
    let Some(start) = source.find(OPEN) else {
        return source.to_owned();
    };
    let Some(end) = source[start..].find(CLOSE).map(|at| start + at) else {
        return source.to_owned();
    };
    let body = &source[start + OPEN.len()..end];
    let mut after = &source[end + CLOSE.len()..];
    if !keep {
        after = after.strip_prefix('\n').unwrap_or(after);
    }
    let mut out = String::with_capacity(source.len());
    out.push_str(&source[..start]);
    if keep {
        out.push_str(body);
    }
    out.push_str(after);
    out
}

/// A markdown file without its leading `---` front-matter block, if it has
/// one, and without the blank lines that separated the two.
fn strip_front_matter(source: &str) -> &str {
    let Some(header) = source.strip_prefix("---\n") else {
        return source;
    };
    match header.find("\n---\n") {
        Some(end) => header[end + "\n---\n".len()..].trim_start_matches('\n'),
        None => source,
    }
}

/// Write the embedded skills under `dir`, overwriting whatever is there; the
/// binary is the source of truth.
pub fn install(dir: &Path) -> Result<()> {
    for (path, bytes) in [
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
    // `child::output` says "not found in PATH" for a missing rtk, and does
    // so under QEMU too, where the spawn alone can't tell.
    if let Err(err) = crate::child::output(Command::new("rtk").args(["init", "-g", "--opencode"]))
    {
        tracing::warn!("{err:#}; the agent's commands run unrewritten");
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
/// exactly that away. `grep`, `diff` and `pytest` are kept out because rtk
/// does not just compress their output, it changes what they answer: the
/// turn logs show `grep -v` reporting the lines it was told to leave out, a
/// `\|` alternation counted as matches across files that have none, and a
/// `pytest` that isn't installed summarized as "No tests collected" — each
/// of which the agent acted on. The savings rtk is here for are in build and
/// test output, which these don't touch.
const RTK_CONFIG: &str = "\
# Written by codemine because no rtk config was present; edit freely.
[hooks]
exclude_commands = [\"gh\", \"glab\", \"grep\", \"diff\", \"pytest\"]
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

/// The task pool, parsed from the sweep command's `## "slug"` sections once
/// per process: the command is baked into the binary, and the draw asks
/// after a task's instructions for every candidate it weighs.
pub fn tasks() -> &'static [Task] {
    static TASKS: OnceLock<Vec<Task>> = OnceLock::new();
    TASKS.get_or_init(parse_tasks)
}

fn parse_tasks() -> Vec<Task> {
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

/// The task the sweep command defines under this slug, if any.
pub fn task(slug: &str) -> Option<&'static Task> {
    tasks().iter().find(|task| task.slug == slug)
}

/// Whether the sweep command defines this task. A stored slug it doesn't
/// define — a task that has since been removed, or a typo in a hand-edited
/// config.json — carries no instructions, so drawing it would spend a whole
/// agent session on a bare word.
pub fn defines(slug: &str) -> bool {
    task(slug).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tasks_carry_their_sweep_instructions() {
        let by_slug = |slug: &str| {
            task(slug)
                .unwrap_or_else(|| panic!("no {slug} task"))
                .description
                .as_str()
        };

        // Wrapped lines are joined back into one bullet per instruction.
        assert_eq!(
            by_slug("todo"),
            "- Handle a TODO comment in the code or a TODO list item from the \
             project's AGENTS.md"
        );
        assert_eq!(by_slug("feedback").lines().count(), 3);
        // Nested bullets keep their indentation.
        assert!(
            by_slug("bump").contains("\n  - Only make a PR"),
            "{}",
            by_slug("bump")
        );
        // The `# General information` section is not a task and its prose
        // never lands on the task above it.
        assert!(!tasks().iter().any(|task| task.slug.starts_with('#')));
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
        for task in tasks() {
            assert!(defines(&task.slug), "{}", task.slug);
        }
        assert!(!defines("no-such-task"));
        assert!(!defines(""));
    }

    #[test]
    fn install_writes_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path()).unwrap();
        for (path, bytes) in [
            ("skills/gitea/SKILL.md", GITEA_SKILL),
            ("skills/github/SKILL.md", GITHUB_SKILL),
            ("skills/gitlab/SKILL.md", GITLAB_SKILL),
        ] {
            assert_eq!(std::fs::read(dir.path().join(path)).unwrap(), bytes);
        }

        let skill = dir.path().join("skills/github/SKILL.md");
        std::fs::write(&skill, "stale").unwrap();
        install(dir.path()).unwrap();
        assert_eq!(std::fs::read(&skill).unwrap(), GITHUB_SKILL);
        // The sweep prompt is sent with each turn, not installed as a command.
        assert!(!dir.path().join("commands").exists());
    }

    /// The GitLab skill's commands have to be ones `glab` actually runs as
    /// written. `glab api` has no `--jq`, and it sends a bracketed field name
    /// such as `-f "position[new_line]=42"` as a literal JSON key, which
    /// GitLab ignores. A review built that way lands as one plain comment
    /// with no line anchors.
    #[test]
    fn gitlab_skill_commands_are_ones_glab_runs() {
        let skill = std::str::from_utf8(GITLAB_SKILL).unwrap();
        let commands = skill
            .lines()
            .filter(|line| line.trim_start().starts_with("glab "));
        for line in commands {
            assert!(!line.contains("--jq"), "glab has no --jq: {line}");
        }
        let tokens: Vec<&str> = skill.split_whitespace().collect();
        for pair in tokens.windows(2) {
            if ["-f", "-F", "--field", "--raw-field"].contains(&pair[0]) {
                let name = pair[1].trim_matches(['"', '\'']);
                let name = name.split('=').next().unwrap_or(name);
                assert!(!name.contains('['), "bracketed glab field: {}", pair[1]);
            }
        }

        // Findings go out as anchored drafts, published by the verdict.
        let review = skill
            .split("## Reviewing a merge request")
            .nth(1)
            .and_then(|rest| rest.split("\n## ").next())
            .expect("the gitlab skill has a review section");
        for needle in [
            "draft_notes",
            "--input",
            "Content-Type: application/json",
            "old_line",
            "new_line",
            "/submit_review",
        ] {
            assert!(
                review.contains(needle),
                "{needle} missing from the review section"
            );
        }
    }

    #[test]
    fn sweep_prompt_fills_in_the_turn() {
        let prompt = sweep_prompt(
            "rebase",
            "cilki/$2-repo",
            "gitea",
            Path::new("/work/some dir/repo"),
            true,
        );
        // No front matter: opencode would take it as part of the message.
        assert!(
            prompt.starts_with("You are an assistant developer."),
            "{prompt}"
        );
        assert!(!prompt.contains("description:"));
        assert!(prompt.contains("Your assigned task for this turn: rebase\n"));
        // A value holding a placeholder goes in as it is.
        assert!(
            prompt.contains("Your assigned repository: cilki/$2-repo, hosted on our gitea forge")
        );
        assert!(prompt.contains("checked out on its\ndefault branch at `/work/some dir/repo`"));
        assert!(prompt.contains("Load the `gitea` skill"));
        assert!(prompt.contains("as the\n  `gitea` skill describes"));
        for placeholder in ["$1", "$3", "$4"] {
            assert!(
                !prompt.contains(placeholder),
                "{placeholder} left in the prompt"
            );
        }
        // Everything after the front matter survives, task sections included.
        assert!(prompt.contains("## \"rebase\""));
        assert!(prompt.ends_with("rediscovering this.\n"));
    }

    /// The codegraph paragraph is in the prompt exactly when the tool is:
    /// recommending `codegraph_explore` to a session that has no such tool
    /// is a paragraph the agent has to read past. The markers themselves
    /// never reach the agent, and dropping the block leaves no double blank
    /// line behind.
    #[test]
    fn sweep_prompt_recommends_codegraph_only_when_it_is_there() {
        let with = sweep_prompt("docs", "me/repo", "gitea", Path::new("/r"), true);
        let without = sweep_prompt("docs", "me/repo", "gitea", Path::new("/r"), false);
        assert!(with.contains("Prefer the `codegraph_explore` tool"), "{with}");
        assert!(!without.contains("codegraph"), "{without}");
        for prompt in [&with, &without] {
            assert!(!prompt.contains("<!--"), "{prompt}");
            assert!(!prompt.contains("\n\n\n"), "{prompt}");
        }
        // Everything else is the same prompt.
        assert_eq!(
            with.replace(
                "Prefer the `codegraph_explore` tool over grep/find/broad file reads when\n\
                 exploring the codebase — it answers structural questions in a single call and is\n\
                 much cheaper. Fall back to normal exploration only if the tool is unavailable or\n\
                 fails.\n\n",
                ""
            ),
            without
        );
    }

    #[test]
    fn front_matter_is_only_a_leading_block() {
        assert_eq!(strip_front_matter("---\na: b\n---\n\nbody\n"), "body\n");
        assert_eq!(strip_front_matter("body\n---\nmore\n"), "body\n---\nmore\n");
        // An unterminated block is not front matter.
        assert_eq!(strip_front_matter("---\na: b\n"), "---\na: b\n");
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

    /// The file is shared with whatever else runs under the same $HOME — a
    /// turn on codemine's own repository starts the binary it built, and that
    /// instance's startup writes the default proxy into it. Reasserting the
    /// configured proxy has to take the file back to the settings while
    /// leaving the MCP entry, and a file already in agreement is not
    /// rewritten, since the runner does this before every turn.
    #[test]
    fn install_provider_takes_back_a_clobbered_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        let proxy = crate::settings::ProxySettings {
            base_url: "https://proxy.example:8317".into(),
            api_key: "k1".into(),
            ..Default::default()
        };
        install_mcp(dir.path(), true).unwrap();
        install_provider(dir.path(), &proxy).unwrap();

        // What a default-settings instance leaves behind.
        install_provider(dir.path(), &crate::settings::ProxySettings::default()).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            config["provider"]["anthropic"]["options"]["baseURL"],
            format!("{}/v1", crate::proxy::DEFAULT_BASE_URL)
        );

        install_provider(dir.path(), &proxy).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            config["provider"]["anthropic"]["options"]["baseURL"],
            "https://proxy.example:8317/v1"
        );
        assert_eq!(config["provider"]["anthropic"]["options"]["apiKey"], "k1");
        assert_eq!(config["mcp"]["codegraph"]["type"], "local");

        // In agreement: compared, not written.
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        install_provider(dir.path(), &proxy).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
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
        let proxy = crate::settings::ProxySettings::default();

        install_mcp(dir.path(), true).unwrap();
        install_provider(dir.path(), &proxy).unwrap();
        let before = (std::fs::read(&path).unwrap(), inode(&path));

        install_mcp(dir.path(), true).unwrap();
        install_provider(dir.path(), &proxy).unwrap();
        assert_eq!((std::fs::read(&path).unwrap(), inode(&path)), before);

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

        // A config someone else created world-readable comes back
        // owner-only once codemine has a change to write into it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(opencode_json(dir.path()), r#"{"theme":"dark"}"#).unwrap();
        install_provider(dir.path(), &crate::settings::ProxySettings::default()).unwrap();
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
        assert!(written.contains(r#"exclude_commands = ["gh", "glab", "grep", "diff", "pytest"]"#));

        let original = "[hooks]\nexclude_commands = [\"curl\"]\n";
        std::fs::write(&path, original).unwrap();
        install_rtk_config(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}
