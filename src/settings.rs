//! Mutable configuration edited through the web UI and persisted as JSON at
//! the workspace root. The main loop takes a snapshot before each turn, so
//! changes apply at the next turn boundary.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Cli, Config, Forge, ForgeKind, IoClass};

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    pub gitea: ForgeSettings,
    pub github: ForgeSettings,
    pub gitlab: ForgeSettings,
    /// Model to run turns with, as provider/model; empty means unconfigured.
    pub model: String,
    /// The opencode command to run each turn.
    pub command: String,
    /// The task pool the runner draws from each turn.
    pub tasks: Vec<String>,
    /// Maximum completed (not skipped) tasks per local day; None is unlimited.
    pub daily_limit: Option<u32>,
    /// The name and email commits are authored (and committed) as; empty
    /// means unconfigured.
    pub author_name: String,
    pub author_email: String,
    /// Seconds before a turn is cut off.
    pub turn_timeout_secs: u64,
    /// CPU niceness for the agent process tree, 1-19.
    pub nice: Option<u8>,
    /// I/O scheduling class for the agent process tree.
    pub ionice: Option<IoClass>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            gitea: ForgeSettings::default(),
            github: ForgeSettings::default(),
            gitlab: ForgeSettings::default(),
            model: String::new(),
            command: "sweep".into(),
            tasks: crate::prompts::default_tasks(),
            daily_limit: None,
            author_name: String::new(),
            author_email: String::new(),
            turn_timeout_secs: 21600,
            nice: None,
            ionice: None,
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
    /// Bot account username; only Gitea needs it, the other forges use fixed
    /// pseudo-users for HTTPS auth.
    pub user: String,
    /// Base URL; empty means the forge's default (github.com / gitlab.com).
    pub url: String,
    /// Repositories excluded from sweeping. Everything else is enabled, so
    /// new repositories join the pool automatically.
    pub disabled_repos: BTreeSet<String>,
}

impl ForgeSettings {
    /// Whether the forge takes part in sweeps: enabled with a token, plus
    /// user and URL for Gitea, which has no default instance.
    pub fn active(&self, kind: ForgeKind) -> bool {
        self.enabled
            && !self.token.is_empty()
            && (kind != ForgeKind::Gitea || (!self.user.is_empty() && !self.url.is_empty()))
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
        self.user = self.user.trim().to_owned();
        self.url = self.url.trim().trim_end_matches('/').to_owned();
    }
}

const FORGE_KINDS: [ForgeKind; 3] = [ForgeKind::Gitea, ForgeKind::Github, ForgeKind::Gitlab];

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
                ForgeKind::Gitea => forge.user.clone(),
                ForgeKind::Github => "x-access-token".into(),
                ForgeKind::Gitlab => "oauth2".into(),
            },
            url: forge.url(kind),
            disabled_repos: forge.disabled_repos.clone(),
        })
    }

    /// What blocks turns from running, as human-readable messages for the UI.
    /// Empty means the settings are runnable.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for kind in FORGE_KINDS {
            let forge = self.forge(kind);
            if !forge.enabled {
                continue;
            }
            if forge.token.is_empty() {
                problems.push(format!("{} is enabled but has no token", kind.name()));
            }
            if kind == ForgeKind::Gitea && (forge.user.is_empty() || forge.url.is_empty()) {
                problems.push("gitea needs a user and URL".into());
            }
        }
        if !FORGE_KINDS.iter().any(|&kind| self.forge(kind).active(kind)) {
            problems.push("no forge is enabled with a token".into());
        }
        if self.model.is_empty() {
            problems.push("model is not set".into());
        }
        if self.author_name.is_empty() {
            problems.push("git author name is not set".into());
        }
        if self.author_email.is_empty() {
            problems.push("git author email is not set".into());
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
            command: self.command.clone(),
            tasks: self.tasks.clone(),
            daily_limit: self.daily_limit,
            author_name: self.author_name.clone(),
            author_email: self.author_email.clone(),
            turn_timeout: Duration::from_secs(self.turn_timeout_secs),
            nice: self.nice,
            ionice: self.ionice,
            workspace: cli.workspace.clone(),
        })
    }

    /// The settings as JSON for the UI, with each forge's token replaced by a
    /// `token_set` flag so tokens are write-only.
    pub fn redacted(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).expect("settings serialize to JSON");
        for kind in FORGE_KINDS {
            let forge = value[kind.name()]
                .as_object_mut()
                .expect("forge sections are objects");
            let set = forge["token"].as_str().is_some_and(|token| !token.is_empty());
            forge.remove("token");
            forge.insert("token_set".into(), set.into());
        }
        value
    }

    /// Replace these settings with `incoming` from the UI. An empty incoming
    /// token keeps the stored one (the UI never sees tokens back), a nonempty
    /// one overwrites it.
    pub fn apply_update(&mut self, mut incoming: Settings) -> Result<()> {
        incoming.model = incoming.model.trim().to_owned();
        incoming.command = incoming.command.trim().to_owned();
        incoming.author_name = incoming.author_name.trim().to_owned();
        incoming.author_email = incoming.author_email.trim().to_owned();
        for kind in FORGE_KINDS {
            let forge = incoming.forge_mut(kind);
            forge.trim();
            if forge.token.is_empty() {
                forge.token = self.forge(kind).token.clone();
            }
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
        if self.turn_timeout_secs == 0 {
            bail!("turn timeout must be positive");
        }
        if self.command.is_empty() {
            bail!("command must not be empty");
        }
        if self.tasks.iter().all(|task| task.trim().is_empty()) {
            bail!("task pool must not be empty");
        }
        for kind in FORGE_KINDS {
            let url = &self.forge(kind).url;
            if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
                bail!("{} URL must start with http:// or https://", kind.name());
            }
        }
        Ok(())
    }
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

    /// Mutate the settings, persisting on success; nothing changes in memory
    /// when the mutation or the write fails.
    pub fn update(&self, f: impl FnOnce(&mut Settings) -> Result<()>) -> Result<()> {
        let mut inner = self.lock();
        let mut candidate = inner.0.clone();
        f(&mut candidate)?;
        self.persist(&candidate)?;
        inner.0 = candidate;
        inner.1 += 1;
        Ok(())
    }

    /// Atomic 0600 write: the temp file is created next to the target (same
    /// filesystem, so the rename is atomic) with owner-only permissions.
    fn persist(&self, settings: &Settings) -> Result<()> {
        let parent = self
            .path
            .parent()
            .with_context(|| format!("{} has no parent directory", self.path.display()))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&serde_json::to_vec_pretty(settings)?)?;
        file.write_all(b"\n")?;
        file.persist(&self.path)
            .with_context(|| format!("failed to write {}", self.path.display()))?;
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
            github: ForgeSettings {
                enabled: true,
                token: "tok".into(),
                ..Default::default()
            },
            model: "anthropic/claude".into(),
            author_name: "Bot".into(),
            author_email: "bot@example.com".into(),
            ..Default::default()
        }
    }

    fn cli() -> Cli {
        Cli::parse(["codemine", "--workspace", "/tmp/ws"].map(String::from).into_iter())
            .unwrap()
            .unwrap()
    }

    #[test]
    fn defaults_are_unconfigured() {
        let settings = Settings::default();
        assert_eq!(settings.command, "sweep");
        assert_eq!(settings.turn_timeout_secs, 21600);
        assert!(!settings.tasks.is_empty());
        assert!(!settings.problems().is_empty());
        assert!(settings.to_config(&cli()).is_none());
    }

    #[test]
    fn configured_settings_build_a_config() {
        let settings = configured();
        assert!(settings.problems().is_empty());
        let config = settings.to_config(&cli()).unwrap();
        assert_eq!(config.forges.len(), 1);
        assert_eq!(config.forges[0].url, "https://github.com");
        assert_eq!(config.forges[0].user, "x-access-token");
        assert_eq!(config.workspace, PathBuf::from("/tmp/ws"));
    }

    #[test]
    fn incomplete_enabled_forge_is_a_problem() {
        let mut settings = configured();
        settings.gitea.enabled = true;
        settings.gitea.token = "tok".into();
        let problems = settings.problems();
        assert!(problems.iter().any(|p| p.contains("gitea")), "{problems:?}");
        assert!(settings.to_config(&cli()).is_none());
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
        bad.turn_timeout_secs = 0;
        assert!(settings.apply_update(bad).is_err());
        let mut bad = configured();
        bad.tasks = vec![];
        assert!(settings.apply_update(bad).is_err());
        let mut bad = configured();
        bad.github.url = "github.example.com".into();
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
