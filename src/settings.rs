//! Mutable configuration edited through the web UI and persisted as JSON at
//! the workspace root. The main loop takes a snapshot before each turn, so
//! changes apply at the next turn boundary.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Cli, Config, Forge, ForgeKind, IoClass};
use crate::schedule::Schedule;

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    /// Where the model proxy lives. Named `claude` in configs written before
    /// the section stopped being Claude-specific.
    #[serde(alias = "claude")]
    pub proxy: ProxySettings,
    pub gitea: ForgeSettings,
    pub github: ForgeSettings,
    pub gitlab: ForgeSettings,
    /// Model to run turns with, as provider/model; empty means unconfigured.
    pub model: String,
    /// The task pool the runner draws from each turn.
    pub tasks: Vec<String>,
    /// Completed (not skipped) tasks allowed per hour; None is unlimited.
    /// Fractional rates are the point of the unit: 0.5 is one task every
    /// two hours. The runner spends them from a bucket, so a whole hour's
    /// worth can run back to back.
    pub hourly_limit: Option<f64>,
    /// Seconds before a turn is cut off.
    pub turn_timeout_secs: u64,
    /// CPU niceness for the agent process tree, 1-19.
    pub nice: Option<u8>,
    /// I/O scheduling class for the agent process tree.
    pub ionice: Option<IoClass>,
    /// The daily window turns may start in; off by default.
    pub schedule: Schedule,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            proxy: ProxySettings::default(),
            gitea: ForgeSettings::default(),
            github: ForgeSettings::default(),
            gitlab: ForgeSettings::default(),
            model: String::new(),
            tasks: crate::prompts::default_tasks(),
            hourly_limit: None,
            turn_timeout_secs: 21600,
            nice: None,
            ionice: None,
            schedule: Schedule::default(),
        }
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

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct ForgeSettings {
    pub enabled: bool,
    /// Personal access token; empty means no token stored. Never serialized
    /// to the UI — see [`Settings::redacted`].
    pub token: String,
    /// Base URL; empty means the forge's default (github.com / gitlab.com).
    pub url: String,
    /// Repositories included in sweeping. Everything starts disabled — a
    /// fresh login sweeps nothing, and new repositories stay out of the pool
    /// until they're enabled by hand.
    pub enabled_repos: BTreeSet<String>,
}

impl ForgeSettings {
    /// Whether the forge takes part in sweeps: enabled with a token, plus a
    /// URL for Gitea, which has no default instance.
    pub fn active(&self, kind: ForgeKind) -> bool {
        self.enabled
            && !self.token.is_empty()
            && (kind != ForgeKind::Gitea || !self.url.is_empty())
    }

    fn url(&self, kind: ForgeKind) -> String {
        if !self.url.is_empty() {
            return self.url.clone();
        }
        match kind {
            ForgeKind::Gitea => String::new(),
            ForgeKind::Github => "https://github.com".into(),
            ForgeKind::Gitlab => "https://gitlab.com".into(),
        }
    }

    fn trim(&mut self) {
        self.token = self.token.trim().to_owned();
        self.url = self.url.trim().trim_end_matches('/').to_owned();
    }
}

const FORGE_KINDS: [ForgeKind; 3] = [ForgeKind::Gitea, ForgeKind::Github, ForgeKind::Gitlab];

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
    pub fn forge(&self, kind: ForgeKind) -> &ForgeSettings {
        match kind {
            ForgeKind::Gitea => &self.gitea,
            ForgeKind::Github => &self.github,
            ForgeKind::Gitlab => &self.gitlab,
        }
    }

    fn forge_mut(&mut self, kind: ForgeKind) -> &mut ForgeSettings {
        match kind {
            ForgeKind::Gitea => &mut self.gitea,
            ForgeKind::Github => &mut self.github,
            ForgeKind::Gitlab => &mut self.gitlab,
        }
    }

    /// The runtime forge for an active forge kind, with the fixed pseudo-user
    /// and default URL filled in; None when the forge is not active.
    pub fn runtime_forge(&self, kind: ForgeKind) -> Option<Forge> {
        let forge = self.forge(kind);
        if !forge.active(kind) {
            return None;
        }
        Some(Forge {
            kind,
            token: forge.token.clone(),
            user: match kind {
                // Filled with the account login by `identity::resolve`
                // before the credential line is written.
                ForgeKind::Gitea => String::new(),
                ForgeKind::Github => "x-access-token".into(),
                ForgeKind::Gitlab => "oauth2".into(),
            },
            login: String::new(),
            email: String::new(),
            url: forge.url(kind),
            enabled_repos: forge.enabled_repos.clone(),
        })
    }

    /// What blocks turns from running, each tied to the field that fixes it.
    /// Empty means the settings are runnable.
    pub fn problems(&self) -> Vec<Problem> {
        let mut problems = Vec::new();
        for kind in FORGE_KINDS {
            let forge = self.forge(kind);
            if !forge.enabled {
                continue;
            }
            let name = kind.name();
            if forge.token.is_empty() {
                problems.push(Problem::new(
                    &format!("f-{name}-token"),
                    format!("{name} is enabled but has no token"),
                ));
            }
            if kind == ForgeKind::Gitea && forge.url.is_empty() {
                problems.push(Problem::new("f-gitea-url", "gitea needs a URL"));
            }
        }
        let active: Vec<&ForgeSettings> = FORGE_KINDS
            .iter()
            .filter(|&&kind| self.forge(kind).active(kind))
            .map(|&kind| self.forge(kind))
            .collect();
        if active.is_empty() {
            problems.push(Problem::new("forges", "no forge is enabled with a token"));
        } else if active.iter().all(|forge| forge.enabled_repos.is_empty()) {
            // Every repository starts disabled, so this is where a fresh
            // install sits once the forge is set up. Without it the runner
            // looked healthy — "sleeping" between empty draws — while relisting
            // the forge every minute to rediscover that it has nothing to
            // sweep.
            problems.push(Problem::new(
                "forges",
                "no repositories are enabled for sweeping",
            ));
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
            forges: FORGE_KINDS
                .iter()
                .filter_map(|&kind| self.runtime_forge(kind))
                .collect(),
            model: self.model.clone(),
            tasks: self.tasks.clone(),
            hourly_limit: self.hourly_limit,
            // Commits are attributed to the model that authored them; the
            // email comes from each forge's account via identity::resolve.
            author_name: model_author_name(&self.model),
            turn_timeout: Duration::from_secs(self.turn_timeout_secs),
            nice: self.nice,
            ionice: self.ionice,
            schedule: self.schedule,
            workspace: cli.workspace.clone(),
        })
    }

    /// The settings as JSON for the UI, with each secret replaced by a
    /// `*_set` flag so secrets are write-only.
    pub fn redacted(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).expect("settings serialize to JSON");
        for kind in FORGE_KINDS {
            let forge = value[kind.name()]
                .as_object_mut()
                .expect("forge sections are objects");
            let set = forge["token"]
                .as_str()
                .is_some_and(|token| !token.is_empty());
            forge.remove("token");
            forge.insert("token_set".into(), set.into());
        }
        let proxy = value["proxy"]
            .as_object_mut()
            .expect("the proxy section is an object");
        for key in ["api_key", "management_key"] {
            let set = proxy[key].as_str().is_some_and(|key| !key.is_empty());
            proxy.remove(key);
            proxy.insert(format!("{key}_set"), set.into());
        }
        value
    }

    /// Replace these settings with `incoming` from the UI. An empty incoming
    /// secret keeps the stored one (the UI never sees secrets back), a
    /// nonempty one overwrites it.
    pub fn apply_update(&mut self, mut incoming: Settings) -> Result<()> {
        incoming.model = incoming.model.trim().to_owned();
        for kind in FORGE_KINDS {
            let forge = incoming.forge_mut(kind);
            forge.trim();
            if forge.token.is_empty() {
                forge.token = self.forge(kind).token.clone();
            }
        }
        let proxy = &mut incoming.proxy;
        proxy.base_url = proxy.base_url.trim().trim_end_matches('/').to_owned();
        if proxy.base_url.is_empty() {
            proxy.base_url = ProxySettings::default().base_url;
        }
        proxy.api_key = proxy.api_key.trim().to_owned();
        if proxy.api_key.is_empty() {
            proxy.api_key = self.proxy.api_key.clone();
        }
        proxy.management_key = proxy.management_key.trim().to_owned();
        if proxy.management_key.is_empty() {
            proxy.management_key = self.proxy.management_key.clone();
        }
        incoming.validate()?;
        *self = incoming;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if let Some(nice) = self.nice
            && !(1..=19).contains(&nice)
        {
            bail!("nice must be between 1 and 19");
        }
        if let Some(limit) = self.hourly_limit
            && !(limit.is_finite() && limit > 0.0)
        {
            bail!("hourly limit must be a positive number");
        }
        if self.turn_timeout_secs == 0 {
            bail!("turn timeout must be positive");
        }
        if self.tasks.iter().all(|task| task.trim().is_empty()) {
            bail!("task pool must not be empty");
        }
        self.schedule.validate()?;
        for kind in FORGE_KINDS {
            let url = &self.forge(kind).url;
            if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
                bail!("{} URL must start with http:// or https://", kind.name());
            }
        }
        let base = &self.proxy.base_url;
        if !base.starts_with("http://") && !base.starts_with("https://") {
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

    fn configured() -> Settings {
        Settings {
            proxy: ProxySettings {
                api_key: "proxy-key".into(),
                ..Default::default()
            },
            github: ForgeSettings {
                enabled: true,
                token: "tok".into(),
                enabled_repos: ["owner/repo".to_owned()].into(),
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
        assert!(!settings.tasks.is_empty());
        assert!(!settings.problems().is_empty());
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
        assert_eq!(config.forges.len(), 1);
        assert_eq!(config.forges[0].url, "https://github.com");
        assert_eq!(config.forges[0].user, "x-access-token");
        // The author email is resolved from the forge account before any
        // turn runs, never from the settings.
        assert_eq!(config.forges[0].email, "");
        // Commits are authored as the model, prettified.
        assert_eq!(config.author_name, "Claude");
        assert_eq!(config.workspace, PathBuf::from("/tmp/ws"));
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
    fn incomplete_enabled_forge_is_a_problem() {
        let mut settings = configured();
        settings.gitea.enabled = true;
        settings.gitea.token = "tok".into();
        let fields: Vec<String> = settings.problems().into_iter().map(|p| p.field).collect();
        assert!(fields.iter().any(|f| f == "f-gitea-url"), "{fields:?}");
        assert!(settings.to_config(&cli()).is_none());
    }

    #[test]
    fn a_forge_with_no_enabled_repositories_is_a_problem() {
        // Repositories start disabled, so a forge that is otherwise fully
        // set up still cannot sweep anything.
        let mut settings = configured();
        settings.github.enabled_repos.clear();
        let fields: Vec<String> = settings.problems().into_iter().map(|p| p.field).collect();
        assert_eq!(fields, ["forges"], "{fields:?}");
        assert!(settings.to_config(&cli()).is_none());

        // One enabled repository anywhere is enough; the other forges are
        // allowed to sit empty.
        settings.gitea = ForgeSettings {
            enabled: true,
            token: "tok".into(),
            url: "https://git.example.com".into(),
            enabled_repos: ["owner/repo".to_owned()].into(),
        };
        assert!(settings.problems().is_empty());
    }

    #[test]
    fn redaction_strips_tokens() {
        let value = configured().redacted();
        for forge in ["gitea", "github", "gitlab"] {
            assert!(value[forge].get("token").is_none(), "{forge} leaks token");
        }
        assert_eq!(value["github"]["token_set"], true);
        assert_eq!(value["gitea"]["token_set"], false);
    }

    #[test]
    fn empty_incoming_token_keeps_stored_one() {
        let mut settings = configured();
        let mut incoming = configured();
        incoming.github.token = String::new();
        incoming.model = "anthropic/other".into();
        settings.apply_update(incoming).unwrap();
        assert_eq!(settings.github.token, "tok");
        assert_eq!(settings.model, "anthropic/other");

        let mut overwrite = configured();
        overwrite.github.token = " newtok ".into();
        settings.apply_update(overwrite).unwrap();
        assert_eq!(settings.github.token, "newtok");
    }

    #[test]
    fn validation_rejects_bad_values() {
        let mut settings = configured();
        let mut bad = configured();
        bad.nice = Some(40);
        assert!(settings.apply_update(bad).is_err());
        let mut bad = configured();
        bad.hourly_limit = Some(0.0);
        assert!(settings.apply_update(bad).is_err());
        let mut bad = configured();
        bad.turn_timeout_secs = 0;
        assert!(settings.apply_update(bad).is_err());
        let mut bad = configured();
        bad.tasks = vec![];
        assert!(settings.apply_update(bad).is_err());
        let mut bad = configured();
        bad.github.url = "github.example.com".into();
        assert!(settings.apply_update(bad).is_err());
        let mut bad = configured();
        bad.schedule.enabled = true;
        bad.schedule.end_minute = bad.schedule.start_minute;
        assert!(settings.apply_update(bad).is_err());
        // The failed updates changed nothing.
        assert_eq!(settings.nice, None);
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
        assert_eq!(settings.github.token, "tok");
        assert_eq!(settings.model, "anthropic/claude");

        std::fs::write(&path, "not json").unwrap();
        assert!(SettingsStore::load(path).is_err());
    }
}
