//! Mutable configuration edited through the web UI and persisted as JSON at
//! the workspace root. The main loop takes a snapshot before each turn, so
//! changes apply at the next turn boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Cli, Config, Forge, ForgeKind};
use crate::schedule::Schedule;

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    /// Where the model proxy lives. Named `claude` in configs written before
    /// the section stopped being Claude-specific.
    #[serde(alias = "claude")]
    pub proxy: ProxySettings,
    /// The one forge the runner sweeps.
    pub forge: ForgeSettings,
    /// Model to run turns with, as provider/model; empty means unconfigured.
    pub model: String,
    /// At most this many completed (not skipped) tasks in any rolling
    /// window; None is unlimited. Configs written while this was a
    /// fractional `hourly_limit` still load: the old key is ignored, so the
    /// limit starts out unset and the next save drops it.
    pub rate_limit: Option<RateLimit>,
    /// Seconds before a turn is cut off.
    pub turn_timeout_secs: u64,
    /// The daily window automatic turns may start in; off by default. Turns
    /// the forge asks for start outside it too.
    pub schedule: Schedule,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            proxy: ProxySettings::default(),
            forge: ForgeSettings::default(),
            model: String::new(),
            rate_limit: None,
            turn_timeout_secs: 21600,
            schedule: Schedule::default(),
        }
    }
}

/// The longest window a rate limit may span. The runner keeps completion
/// times this far back, so a window edited up to the cap still sees exactly
/// what happened inside it.
pub const MAX_WINDOW_HOURS: u32 = 24;

/// "At most `tasks` completed tasks in any `hours` hours", counted over a
/// sliding window: a turn may start once fewer than `tasks` completions fall
/// inside the last `hours` hours. Whole numbers on both sides, so a slow rate
/// is a wider window rather than a fraction — one every two hours is
/// `{ tasks: 1, hours: 2 }`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimit {
    pub tasks: u32,
    #[serde(default = "RateLimit::default_hours")]
    pub hours: u32,
}

impl RateLimit {
    fn default_hours() -> u32 {
        1
    }

    /// The window's length in seconds.
    pub fn window_secs(&self) -> u64 {
        u64::from(self.hours) * 3600
    }

    fn validate(&self) -> Result<()> {
        if self.tasks == 0 {
            bail!("rate limit must allow at least one task");
        }
        if !(1..=MAX_WINDOW_HOURS).contains(&self.hours) {
            bail!("rate limit window must be between 1 and {MAX_WINDOW_HOURS} hours");
        }
        Ok(())
    }
}

/// The CLIProxyAPI section. Every model provider reaches the models through
/// the proxy, which owns the subscription OAuth login and refresh; these
/// settings say where it listens and how to authenticate to it. There is no
/// enable switch: without the proxy nothing can run a turn at all.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct ProxySettings {
    /// Where CLIProxyAPI listens; the Anthropic-compatible API and the
    /// management API share the port.
    pub base_url: String,
    /// A client key from the proxy's `api-keys` list; opencode presents it
    /// in place of a real Anthropic credential. Optional, since a proxy with
    /// no `api-keys` configured takes any key — see
    /// [`crate::prompts::install_provider`]. Write-only in the UI, like forge
    /// tokens.
    pub api_key: String,
    /// The proxy's management key; optional. With it the proxy card shows
    /// live login and refresh state, without it only reachability.
    pub management_key: String,
}

impl Default for ProxySettings {
    fn default() -> Self {
        ProxySettings {
            base_url: crate::proxy::DEFAULT_BASE_URL.into(),
            api_key: String::new(),
            management_key: String::new(),
        }
    }
}

/// The forge section: which forge, how to reach it, and what to sweep there.
/// There is no enable switch — the dropdown picks the forge, and a forge
/// without a token simply isn't active yet.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct ForgeSettings {
    pub kind: ForgeKind,
    /// Personal access token; empty means no token stored. Never serialized
    /// to the UI — see [`Settings::redacted`].
    pub token: String,
    /// Base URL; empty means the forge's default (github.com / gitlab.com).
    pub url: String,
    /// The repositories the picker has touched, by full name. Everything
    /// starts disabled — a fresh login sweeps nothing, and new repositories
    /// stay out of the pool until they're enabled by hand. A disabled
    /// repository keeps its task picks, so turning it back on restores them.
    pub repos: BTreeMap<String, RepoSettings>,
}

/// One repository's place in the sweep: whether it is in, and which tasks
/// are drawn on it.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct RepoSettings {
    pub enabled: bool,
    pub tasks: BTreeSet<String>,
}

impl ForgeSettings {
    /// Whether the forge takes part in sweeps: it has a token, plus a URL
    /// for Gitea, which has no default instance.
    pub fn active(&self) -> bool {
        !self.token.is_empty() && (self.kind != ForgeKind::Gitea || !self.url.is_empty())
    }

    fn url(&self) -> String {
        if !self.url.is_empty() {
            return self.url.clone();
        }
        match self.kind {
            ForgeKind::Gitea => String::new(),
            ForgeKind::Github => "https://github.com".into(),
            ForgeKind::Gitlab => "https://gitlab.com".into(),
        }
    }

    /// The enabled repositories, each with the tasks on it that the sweep
    /// command still defines. The turn passes the slug to the command and
    /// the command looks up the section named by it, so a slug with no
    /// section — one an older sweep command defined — reaches the agent as
    /// a bare word with nothing to do; dropped here rather than drawn, while
    /// the stored pick stays so a hand-edited config.json isn't rewritten
    /// behind the user's back.
    fn runnable_repos(&self) -> BTreeMap<String, BTreeSet<String>> {
        self.repos
            .iter()
            .filter(|(_, repo)| repo.enabled)
            .map(|(name, repo)| {
                let tasks = repo
                    .tasks
                    .iter()
                    .filter(|task| crate::prompts::defines(task))
                    .cloned()
                    .collect();
                (name.clone(), tasks)
            })
            .collect()
    }
}

/// A URL as the settings keep it: no surrounding space, no trailing slash, so
/// every caller can append a path unconditionally — which they all do, from
/// `crate::http::get` to the clone URL `workspace::reclone` builds.
fn normalize_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_owned()
}

/// Whether a URL names something the runner can actually address. A bare host
/// is the likeliest way to mistype one, and neither the forge CLIs nor
/// `crate::http` can make a request without the scheme.
fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// A secret arriving from the UI: trimmed, with the stored one kept when it
/// comes back empty. Every secret here is write-only — [`Settings::redacted`]
/// sends a `*_set` flag in place of the value — so the browser has nothing to
/// send back, and blank means "unchanged" rather than "cleared".
fn keep_stored(incoming: &mut String, stored: &str) {
    *incoming = incoming.trim().to_owned();
    if incoming.is_empty() {
        *incoming = stored.to_owned();
    }
}

/// The other half of write-only: replace a secret in one serialized settings
/// section with the `*_set` flag the UI reads in its place, so the value
/// itself never reaches the browser.
fn redact(section: &mut serde_json::Value, field: &str) {
    let section = section
        .as_object_mut()
        .expect("settings sections are objects");
    let set = section[field]
        .as_str()
        .is_some_and(|secret| !secret.is_empty());
    section.remove(field);
    section.insert(format!("{field}_set"), set.into());
}

/// Something that blocks turns from running, tied to the web UI element that
/// fixes it so the page can mark that field instead of listing the message.
/// An empty `field` means nothing in the settings can fix it.
#[derive(Serialize, Clone)]
pub struct Problem {
    pub field: String,
    pub message: String,
}

impl Problem {
    pub fn new(field: &str, message: impl Into<String>) -> Self {
        Problem {
            field: field.to_owned(),
            message: message.into(),
        }
    }
}

impl Settings {
    /// The runtime forge, with the fixed pseudo-user and default URL filled
    /// in; None while the forge is not active.
    pub fn runtime_forge(&self) -> Option<Forge> {
        let forge = &self.forge;
        if !forge.active() {
            return None;
        }
        Some(Forge {
            kind: forge.kind,
            token: forge.token.clone(),
            login: String::new(),
            email: String::new(),
            url: forge.url(),
            repos: forge.runnable_repos(),
        })
    }

    /// What blocks turns from running, each tied to the field that fixes it.
    /// Empty means the settings are runnable.
    pub fn problems(&self) -> Vec<Problem> {
        let mut problems = Vec::new();
        let forge = &self.forge;
        let name = forge.kind.name();
        if forge.token.is_empty() {
            problems.push(Problem::new("f-token", format!("{name} has no token")));
        }
        if forge.kind == ForgeKind::Gitea && forge.url.is_empty() {
            problems.push(Problem::new("f-url", "gitea needs a URL"));
        }
        if forge.active() {
            let repos = forge.runnable_repos();
            if repos.is_empty() {
                // Every repository starts disabled, so this is where a fresh
                // install sits once the forge is set up. Without it the
                // runner looked healthy — "sleeping" between empty draws —
                // while relisting the forge every minute to rediscover that
                // it has nothing to sweep.
                problems.push(Problem::new(
                    "repos",
                    "no repositories are enabled for sweeping",
                ));
            } else if repos.values().all(BTreeSet::is_empty) {
                // Reachable from stored picks alone: a task the sweep command
                // used to define stays in config.json after it is dropped
                // from the command. Without this the runner drew it anyway
                // and spent a turn on a task with no instructions.
                problems.push(Problem::new(
                    "repos",
                    "no enabled repository has a task the sweep command defines",
                ));
            }
        }
        if self.model.is_empty() {
            problems.push(Problem::new("s-model", "model is not set"));
        }
        problems
    }

    /// The runnable snapshot for a turn; None while `problems` is nonempty.
    pub fn to_config(&self, cli: &Cli) -> Option<Config> {
        if !self.problems().is_empty() {
            return None;
        }
        Some(Config {
            forge: self
                .runtime_forge()
                .expect("a forge without problems is active"),
            model: self.model.clone(),
            rate_limit: self.rate_limit,
            // Commits are attributed to the model that authored them; the
            // email comes from each forge's account via identity::resolve.
            author_name: model_author_name(&self.model),
            turn_timeout: Duration::from_secs(self.turn_timeout_secs),
            schedule: self.schedule,
            workspace: cli.workspace.clone(),
            sandbox: cli.sandbox,
        })
    }

    /// The settings as JSON for the UI, with each secret replaced by a
    /// `*_set` flag so secrets are write-only.
    pub fn redacted(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).expect("settings serialize to JSON");
        redact(&mut value["forge"], "token");
        for key in ["api_key", "management_key"] {
            redact(&mut value["proxy"], key);
        }
        value
    }

    /// Replace these settings with `incoming` from the UI. An empty incoming
    /// secret keeps the stored one (the UI never sees secrets back), a
    /// nonempty one overwrites it — unless the forge itself changed, when
    /// the stored token belongs to the old forge and would only ever be
    /// rejected by the new one.
    pub fn apply_update(&mut self, mut incoming: Settings) -> Result<()> {
        incoming.model = incoming.model.trim().to_owned();
        let forge = &mut incoming.forge;
        forge.url = normalize_url(&forge.url);
        let stored = if forge.kind == self.forge.kind {
            self.forge.token.as_str()
        } else {
            ""
        };
        keep_stored(&mut forge.token, stored);
        let proxy = &mut incoming.proxy;
        proxy.base_url = normalize_url(&proxy.base_url);
        if proxy.base_url.is_empty() {
            // The URL is no secret, so an emptied one has no stored value to
            // fall back on — only the address the proxy normally listens at.
            proxy.base_url = ProxySettings::default().base_url;
        }
        keep_stored(&mut proxy.api_key, &self.proxy.api_key);
        keep_stored(&mut proxy.management_key, &self.proxy.management_key);
        incoming.validate()?;
        *self = incoming;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if let Some(limit) = &self.rate_limit {
            limit.validate()?;
        }
        if self.turn_timeout_secs == 0 {
            bail!("turn timeout must be positive");
        }
        self.schedule.validate()?;
        // Empty is how a forge asks for its default instance; Gitea has
        // none, which `problems` reports instead.
        let url = &self.forge.url;
        if !url.is_empty() && !is_http_url(url) {
            bail!(
                "{} URL must start with http:// or https://",
                self.forge.kind.name()
            );
        }
        if !is_http_url(&self.proxy.base_url) {
            bail!("the CLIProxyAPI URL must start with http:// or https://");
        }
        Ok(())
    }
}

/// The git author a model ID reads as: "anthropic/claude-sonnet-5" becomes
/// "Claude Sonnet 5". The provider prefix goes, words are capitalized,
/// adjacent version numbers join with dots ("4-8" → "4.8"), and a snapshot
/// date stamp (8+ digits) is dropped — it pins a build, it isn't a name.
pub fn model_author_name(model: &str) -> String {
    let id = model.rsplit('/').next().unwrap_or(model);
    let mut words: Vec<String> = Vec::new();
    for part in id.split('-').filter(|part| !part.is_empty()) {
        let numeric = part.bytes().all(|b| b.is_ascii_digit());
        match words.last_mut() {
            _ if numeric && part.len() >= 8 => {}
            Some(last) if numeric && last.bytes().all(|b| b.is_ascii_digit() || b == b'.') => {
                last.push('.');
                last.push_str(part);
            }
            _ if numeric => words.push(part.to_owned()),
            _ => {
                let mut chars = part.chars();
                let first = chars.next().expect("empty parts are filtered out");
                words.push(format!("{}{}", first.to_uppercase(), chars.as_str()));
            }
        }
    }
    words.join(" ")
}

/// The settings plus their persistence, shared between the main loop and the
/// web UI. The generation counter bumps on every successful update so the
/// main loop knows when to reapply side effects (git credentials, tea login).
pub struct SettingsStore {
    path: PathBuf,
    inner: Mutex<(Settings, u64)>,
}

pub type SharedSettings = Arc<SettingsStore>;

impl SettingsStore {
    /// Load from `path`; a missing file yields defaults (first run), a
    /// malformed one fails startup rather than silently discarding config.
    pub fn load(path: PathBuf) -> Result<Self> {
        let settings = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not valid settings JSON", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        Ok(SettingsStore {
            path,
            inner: Mutex::new((settings, 0)),
        })
    }

    pub fn snapshot(&self) -> (Settings, u64) {
        let inner = self.lock();
        (inner.0.clone(), inner.1)
    }

    /// The generation alone, for polling a wait against an edit without
    /// cloning the settings to find out.
    pub fn generation(&self) -> u64 {
        self.lock().1
    }

    /// Mutate the settings, persisting on success; nothing changes in memory
    /// when the mutation or the write fails.
    pub fn update(&self, f: impl FnOnce(&mut Settings) -> Result<()>) -> Result<()> {
        let mut inner = self.lock();
        let mut candidate = inner.0.clone();
        f(&mut candidate)?;
        crate::workspace::write_json(&self.path, &candidate)?;
        inner.0 = candidate;
        inner.1 += 1;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, (Settings, u64)> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn repo(enabled: bool, tasks: &[&str]) -> RepoSettings {
        RepoSettings {
            enabled,
            tasks: tasks.iter().map(|task| (*task).to_owned()).collect(),
        }
    }

    fn configured() -> Settings {
        Settings {
            proxy: ProxySettings {
                api_key: "proxy-key".into(),
                ..Default::default()
            },
            forge: ForgeSettings {
                kind: ForgeKind::Github,
                token: "tok".into(),
                repos: [("owner/repo".to_owned(), repo(true, &["docs", "review"]))].into(),
                ..Default::default()
            },
            model: "anthropic/claude".into(),
            ..Default::default()
        }
    }

    fn cli() -> Cli {
        Cli::parse(
            ["codemine", "--workspace", "/tmp/ws"]
                .map(String::from)
                .into_iter(),
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn defaults_are_unconfigured() {
        let settings = Settings::default();
        assert_eq!(settings.turn_timeout_secs, 21600);
        assert_eq!(settings.forge.kind, ForgeKind::Github);
        assert!(settings.forge.repos.is_empty());
        let fields: Vec<String> = settings.problems().into_iter().map(|p| p.field).collect();
        assert_eq!(fields, ["f-token", "s-model"]);
        assert!(settings.to_config(&cli()).is_none());
    }

    #[test]
    fn the_schedule_is_off_until_it_is_turned_on() {
        let settings = configured();
        assert!(!settings.schedule.enabled);
        assert_eq!(settings.to_config(&cli()).unwrap().schedule.hold(0), None);

        let mut scheduled = configured();
        scheduled.schedule.enabled = true;
        scheduled.schedule.start_minute = 22 * 60;
        scheduled.schedule.end_minute = 6 * 60;
        let config = scheduled.to_config(&cli()).unwrap();
        assert_eq!(config.schedule.hold(23 * 3600), None);
        assert_eq!(config.schedule.hold(12 * 3600), Some(10 * 3600));
    }

    #[test]
    fn configured_settings_build_a_config() {
        let settings = configured();
        assert!(settings.problems().is_empty());
        let config = settings.to_config(&cli()).unwrap();
        assert_eq!(config.forge.kind, ForgeKind::Github);
        assert_eq!(config.forge.url, "https://github.com");
        assert_eq!(config.forge.credential_user(), "x-access-token");
        // The author email is resolved from the forge account before any
        // turn runs, never from the settings.
        assert_eq!(config.forge.email, "");
        assert!(config.forge.runs("docs", "owner/repo"));
        assert!(!config.forge.runs("bump", "owner/repo"));
        // Commits are authored as the model, prettified.
        assert_eq!(config.author_name, "Claude");
        assert_eq!(config.workspace, PathBuf::from("/tmp/ws"));
    }

    /// A stored pick outlives the sweep command that defined it: dropping a
    /// task from the command leaves its slug in config.json, and a turn drawn
    /// on a slug with no section reaches the agent with no instructions. The
    /// snapshot the runner draws from carries only slugs the command still
    /// defines, and a sweep with nothing left says so on the field that fixes
    /// it instead of running empty turns.
    #[test]
    fn a_task_slug_the_sweep_command_dropped_is_not_drawn() {
        let mut settings = configured();
        settings
            .forge
            .repos
            .insert("owner/repo".into(), repo(true, &["lint", "docs"]));
        assert!(settings.problems().is_empty());
        let config = settings.to_config(&cli()).unwrap();
        assert_eq!(config.forge.repos["owner/repo"], ["docs".to_owned()].into());
        // The stored picks are left alone, so a hand-edited config.json isn't
        // rewritten behind the user's back.
        assert_eq!(
            settings.forge.repos["owner/repo"],
            repo(true, &["lint", "docs"])
        );

        settings
            .forge
            .repos
            .insert("owner/repo".into(), repo(true, &["lint"]));
        let fields: Vec<String> = settings.problems().into_iter().map(|p| p.field).collect();
        assert_eq!(fields, ["repos"]);
        assert!(settings.to_config(&cli()).is_none());
    }

    /// A disabled repository is out of the sweep whatever tasks it has
    /// picked, and the picks survive so turning it back on restores them.
    #[test]
    fn a_disabled_repository_keeps_its_picks_but_is_not_swept() {
        let mut settings = configured();
        settings
            .forge
            .repos
            .insert("owner/other".into(), repo(false, &["docs"]));
        let config = settings.to_config(&cli()).unwrap();
        assert!(!config.forge.repos.contains_key("owner/other"));
        assert!(!config.forge.runs("docs", "owner/other"));

        settings.forge.repos.get_mut("owner/repo").unwrap().enabled = false;
        let fields: Vec<String> = settings.problems().into_iter().map(|p| p.field).collect();
        assert_eq!(fields, ["repos"]);
        assert_eq!(settings.forge.repos["owner/repo"].tasks.len(), 2);
    }

    #[test]
    fn the_author_is_the_prettified_model_name() {
        for (model, name) in [
            ("anthropic/claude-sonnet-5", "Claude Sonnet 5"),
            ("anthropic/claude-opus-4-8", "Claude Opus 4.8"),
            ("anthropic/claude-haiku-4-5-20251001", "Claude Haiku 4.5"),
            ("anthropic/claude-3-5-sonnet-20241022", "Claude 3.5 Sonnet"),
            ("anthropic/claude-fable-5", "Claude Fable 5"),
            ("claude", "Claude"),
        ] {
            assert_eq!(model_author_name(model), name, "{model}");
        }
    }

    #[test]
    fn proxy_keys_are_optional() {
        // Neither key is required: a stock CLIProxyAPI needs no client key,
        // and the management key only buys account status.
        let mut settings = configured();
        settings.proxy.api_key = String::new();
        settings.proxy.management_key = String::new();
        let fields: Vec<String> = settings.problems().into_iter().map(|p| p.field).collect();
        assert!(fields.is_empty(), "{fields:?}");
        assert!(settings.to_config(&cli()).is_some());
    }

    #[test]
    fn proxy_keys_are_write_only_and_kept_on_empty_updates() {
        let value = configured().redacted();
        assert!(value["proxy"].get("api_key").is_none());
        assert!(value["proxy"].get("management_key").is_none());
        assert_eq!(value["proxy"]["api_key_set"], true);
        assert_eq!(value["proxy"]["management_key_set"], false);

        let mut settings = configured();
        let mut incoming = configured();
        incoming.proxy.api_key = String::new();
        incoming.proxy.base_url = String::new();
        incoming.proxy.management_key = " mgmt ".into();
        settings.apply_update(incoming).unwrap();
        assert_eq!(settings.proxy.api_key, "proxy-key");
        // An emptied URL falls back to the default rather than breaking.
        assert_eq!(settings.proxy.base_url, crate::proxy::DEFAULT_BASE_URL);
        assert_eq!(settings.proxy.management_key, "mgmt");

        let mut bad = configured();
        bad.proxy.base_url = "127.0.0.1:8317".into();
        assert!(settings.apply_update(bad).is_err());
    }

    #[test]
    fn a_config_from_the_claude_era_still_loads_its_proxy_section() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "claude": { "enabled": true, "base_url": "http://box:9000", "api_key": "k" },
        }))
        .unwrap();
        assert_eq!(settings.proxy.base_url, "http://box:9000");
        assert_eq!(settings.proxy.api_key, "k");
    }

    #[test]
    fn gitea_without_a_url_is_a_problem() {
        let mut settings = configured();
        settings.forge.kind = ForgeKind::Gitea;
        let fields: Vec<String> = settings.problems().into_iter().map(|p| p.field).collect();
        assert_eq!(fields, ["f-url"]);
        assert!(settings.to_config(&cli()).is_none());

        settings.forge.url = "https://git.example.com".into();
        assert!(settings.problems().is_empty());
        let config = settings.to_config(&cli()).unwrap();
        assert_eq!(config.forge.url, "https://git.example.com");
        // Gitea's credential user is the login, read off the forge later.
        assert_eq!(config.forge.credential_user(), "");
    }

    #[test]
    fn a_forge_with_no_enabled_repositories_is_a_problem() {
        // Repositories start disabled, so a forge that is otherwise fully
        // set up still cannot sweep anything.
        let mut settings = configured();
        settings.forge.repos.clear();
        let fields: Vec<String> = settings.problems().into_iter().map(|p| p.field).collect();
        assert_eq!(fields, ["repos"], "{fields:?}");
        assert!(settings.to_config(&cli()).is_none());

        // The problem is the forge's, not the repositories', while there is
        // no token: nothing can be listed to enable yet.
        settings.forge.token.clear();
        let fields: Vec<String> = settings.problems().into_iter().map(|p| p.field).collect();
        assert_eq!(fields, ["f-token"], "{fields:?}");
    }

    /// Every URL the settings hold is stored the same way, so a caller can
    /// append a path without having to know whose URL it has. The forge URL
    /// used to be normalized by one copy of that rule and the proxy's by
    /// another, and neither copy was covered here.
    #[test]
    fn every_url_is_stored_without_its_trailing_slash() {
        let mut settings = configured();
        let mut incoming = configured();
        incoming.forge.url = "  https://github.example.com//  ".into();
        incoming.proxy.base_url = " http://box:9000/ ".into();
        settings.apply_update(incoming).unwrap();
        assert_eq!(settings.forge.url, "https://github.example.com");
        assert_eq!(settings.proxy.base_url, "http://box:9000");

        // The scheme is required of both too, and the message names which
        // one is wrong.
        let mut bad = configured();
        bad.forge.kind = ForgeKind::Gitlab;
        bad.forge.url = "gitlab.example.com".into();
        let err = settings.apply_update(bad).unwrap_err();
        assert!(err.to_string().contains("gitlab"), "{err}");
    }

    #[test]
    fn redaction_strips_tokens() {
        let value = configured().redacted();
        assert!(value["forge"].get("token").is_none(), "leaks token");
        assert_eq!(value["forge"]["token_set"], true);
        assert_eq!(value["forge"]["kind"], "github");
        assert_eq!(Settings::default().redacted()["forge"]["token_set"], false);
    }

    #[test]
    fn empty_incoming_token_keeps_stored_one() {
        let mut settings = configured();
        let mut incoming = configured();
        incoming.forge.token = String::new();
        incoming.model = "anthropic/other".into();
        settings.apply_update(incoming).unwrap();
        assert_eq!(settings.forge.token, "tok");
        assert_eq!(settings.model, "anthropic/other");

        let mut overwrite = configured();
        overwrite.forge.token = " newtok ".into();
        settings.apply_update(overwrite).unwrap();
        assert_eq!(settings.forge.token, "newtok");
    }

    /// Switching forge is a clean slate: the stored token was issued by the
    /// old forge, so keeping it behind an empty field would only have the
    /// new forge reject it — and the health dot would say "token rejected"
    /// about a token the page shows as set.
    #[test]
    fn switching_forge_drops_the_stored_token() {
        let mut settings = configured();
        let mut incoming = configured();
        incoming.forge.kind = ForgeKind::Gitlab;
        incoming.forge.token = String::new();
        incoming.forge.repos.clear();
        settings.apply_update(incoming).unwrap();
        assert_eq!(settings.forge.kind, ForgeKind::Gitlab);
        assert_eq!(settings.forge.token, "");
        let fields: Vec<String> = settings.problems().into_iter().map(|p| p.field).collect();
        assert_eq!(fields, ["f-token"]);
    }

    #[test]
    fn repositories_round_trip_through_json_with_their_tasks() {
        let settings = configured();
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["forge"]["kind"], "github");
        assert_eq!(value["forge"]["repos"]["owner/repo"]["enabled"], true);
        assert_eq!(
            value["forge"]["repos"]["owner/repo"]["tasks"],
            serde_json::json!(["docs", "review"])
        );
        let back: Settings = serde_json::from_value(value).unwrap();
        assert_eq!(back.forge.repos, settings.forge.repos);
    }

    #[test]
    fn validation_rejects_bad_values() {
        let mut settings = configured();
        for (tasks, hours) in [(0, 1), (1, 0), (1, MAX_WINDOW_HOURS + 1)] {
            let mut bad = configured();
            bad.rate_limit = Some(RateLimit { tasks, hours });
            assert!(settings.apply_update(bad).is_err(), "{tasks} per {hours}h");
        }
        let mut good = configured();
        good.rate_limit = Some(RateLimit {
            tasks: 1,
            hours: MAX_WINDOW_HOURS,
        });
        assert!(settings.clone().apply_update(good).is_ok());
        let mut bad = configured();
        bad.turn_timeout_secs = 0;
        assert!(settings.apply_update(bad).is_err());
        let mut bad = configured();
        bad.forge.url = "github.example.com".into();
        assert!(settings.apply_update(bad).is_err());
        let mut bad = configured();
        bad.schedule.enabled = true;
        bad.schedule.end_minute = bad.schedule.start_minute;
        assert!(settings.apply_update(bad).is_err());
        // The failed updates changed nothing.
        assert_eq!(settings.turn_timeout_secs, 21600);
        assert_eq!(settings.rate_limit, None);
    }

    /// The limit used to be a fractional `hourly_limit` spent from a token
    /// bucket. A config.json written then still loads, with the limit unset
    /// rather than guessed at, and the next save drops the old key.
    #[test]
    fn a_config_with_the_retired_hourly_limit_still_loads() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "model": "anthropic/claude",
            "hourly_limit": 0.5,
        }))
        .unwrap();
        assert_eq!(settings.model, "anthropic/claude");
        assert_eq!(settings.rate_limit, None);
        let value = serde_json::to_value(&settings).unwrap();
        assert!(value.get("hourly_limit").is_none());
    }

    /// A limit without a window is a limit per hour.
    #[test]
    fn a_rate_limit_window_defaults_to_an_hour() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "rate_limit": { "tasks": 3 },
        }))
        .unwrap();
        assert_eq!(settings.rate_limit, Some(RateLimit { tasks: 3, hours: 1 }));
    }

    /// CPU and I/O priority used to be settings; they belong to the service
    /// now. A config.json written while they were still settings must keep
    /// loading, and the next save drops them.
    #[test]
    fn a_config_with_the_retired_priority_settings_still_loads() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "model": "anthropic/claude",
            "nice": 10,
            "ionice": "idle",
        }))
        .unwrap();
        assert_eq!(settings.model, "anthropic/claude");
        let value = serde_json::to_value(&settings).unwrap();
        assert!(value.get("nice").is_none());
        assert!(value.get("ionice").is_none());
    }

    #[test]
    fn store_round_trips_and_restricts_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        let store = SettingsStore::load(path.clone()).unwrap();
        assert!(!path.exists()); // defaults are not persisted until an update
        store.update(|s| s.apply_update(configured())).unwrap();
        assert_eq!(store.snapshot().1, 1);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");

        let reloaded = SettingsStore::load(path.clone()).unwrap();
        let (settings, generation) = reloaded.snapshot();
        assert_eq!(generation, 0);
        assert_eq!(settings.forge.token, "tok");
        assert_eq!(settings.forge.repos, configured().forge.repos);
        assert_eq!(settings.model, "anthropic/claude");

        std::fs::write(&path, "not json").unwrap();
        assert!(SettingsStore::load(path).is_err());
    }
}
