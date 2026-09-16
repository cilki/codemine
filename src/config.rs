use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

#[derive(Clone, Copy, PartialEq)]
pub enum ForgeKind {
    Gitea,
    Github,
    Gitlab,
}

impl ForgeKind {
    /// The slug passed to the sweep prompt; it doubles as the skill name.
    pub fn name(self) -> &'static str {
        match self {
            ForgeKind::Gitea => "gitea",
            ForgeKind::Github => "github",
            ForgeKind::Gitlab => "gitlab",
        }
    }
}

/// I/O scheduling class for the agent process tree, passed to ionice.
#[derive(Clone, Copy)]
pub enum IoClass {
    /// Best-effort class at the lowest level (ionice -c 2 -n 7).
    BestEffort,
    /// Idle class: only gets disk time nobody else wants (ionice -c 3).
    Idle,
}

/// One code forge the bot sweeps, enabled by its token variable.
pub struct Forge {
    pub kind: ForgeKind,
    pub token: String,
    /// Username for the git credential line; GitHub and GitLab accept any
    /// token-bearing pseudo-user over HTTPS.
    pub user: String,
    pub url: String,
}

pub struct Config {
    pub forges: Vec<Forge>,
    pub model: String,
    pub command: String,
    /// The task pool the runner draws from each turn; the slugs name the task
    /// sections in the sweep command template.
    pub tasks: Vec<String>,
    /// Maximum completed (not skipped) tasks per local day; None is unlimited.
    pub daily_limit: Option<u32>,
    pub author_name: String,
    pub author_email: String,
    pub turn_timeout: Duration,
    /// CPU niceness applied to the agent process tree (CODEMINE_NICE, 1-19).
    pub nice: Option<u8>,
    /// I/O scheduling class applied to the agent process tree (CODEMINE_IONICE).
    pub ionice: Option<IoClass>,
    /// Run a single turn and exit (--once).
    pub once: bool,
    /// Bind address for the read-only status web UI; None disables it.
    pub webui: Option<SocketAddr>,
    /// Root of the persistent workspace where repositories stay cloned across
    /// turns (CODEMINE_WORKSPACE, default ~/.codemine).
    pub workspace: PathBuf,
}

fn required(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| anyhow!("{name} is not set"))
}

impl Config {
    pub fn from_env(args: impl Iterator<Item = String>) -> Result<Self> {
        let timeout = match std::env::var("CODEMINE_TIMEOUT") {
            Ok(s) => s
                .parse()
                .with_context(|| format!("CODEMINE_TIMEOUT is not a number of seconds: {s}"))?,
            Err(_) => 21600,
        };

        let tasks = match std::env::var("CODEMINE_TASKS") {
            Ok(s) => {
                let tasks: Vec<String> = s.split_whitespace().map(String::from).collect();
                if tasks.is_empty() {
                    bail!("CODEMINE_TASKS is set but empty");
                }
                tasks
            }
            Err(_) => crate::prompts::default_tasks(),
        };

        let daily_limit = match std::env::var("CODEMINE_DAILY_LIMIT") {
            Ok(s) => Some(
                s.parse()
                    .with_context(|| format!("CODEMINE_DAILY_LIMIT is not a number: {s}"))?,
            ),
            Err(_) => None,
        };

        let mut forges = Vec::new();
        if std::env::var("GITEA_TOKEN").is_ok() {
            forges.push(Forge {
                kind: ForgeKind::Gitea,
                token: required("GITEA_TOKEN")?,
                user: required("GITEA_USER")?,
                url: required("GITEA_URL")?,
            });
        }
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            forges.push(Forge {
                kind: ForgeKind::Github,
                token,
                user: "x-access-token".into(),
                url: std::env::var("GITHUB_URL").unwrap_or_else(|_| "https://github.com".into()),
            });
        }
        if let Ok(token) = std::env::var("GITLAB_TOKEN") {
            forges.push(Forge {
                kind: ForgeKind::Gitlab,
                token,
                user: "oauth2".into(),
                url: std::env::var("GITLAB_URL").unwrap_or_else(|_| "https://gitlab.com".into()),
            });
        }
        if forges.is_empty() {
            bail!("no forge configured; set GITEA_TOKEN, GITHUB_TOKEN, or GITLAB_TOKEN");
        }

        let nice = match std::env::var("CODEMINE_NICE") {
            Ok(s) => {
                let nice = s
                    .parse()
                    .with_context(|| format!("CODEMINE_NICE is not a number: {s}"))?;
                if !(1..=19).contains(&nice) {
                    bail!("CODEMINE_NICE must be between 1 and 19: {s}");
                }
                Some(nice)
            }
            Err(_) => None,
        };

        let ionice = match std::env::var("CODEMINE_IONICE") {
            Ok(s) => Some(match s.as_str() {
                "best-effort" => IoClass::BestEffort,
                "idle" => IoClass::Idle,
                _ => bail!("CODEMINE_IONICE must be \"best-effort\" or \"idle\": {s}"),
            }),
            Err(_) => None,
        };

        let webui = match std::env::var("CODEMINE_WEBUI") {
            Ok(s) => Some(
                s.parse()
                    .with_context(|| format!("CODEMINE_WEBUI is not a socket address: {s}"))?,
            ),
            Err(_) => None,
        };

        let workspace = match std::env::var_os("CODEMINE_WORKSPACE") {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/root".into()))
                .join(".codemine"),
        };

        Ok(Self {
            forges,
            model: required("CODEMINE_MODEL")?,
            command: std::env::var("CODEMINE_COMMAND").unwrap_or_else(|_| "sweep".into()),
            tasks,
            author_name: required("GIT_AUTHOR_NAME")?,
            author_email: required("GIT_AUTHOR_EMAIL")?,
            daily_limit,
            turn_timeout: Duration::from_secs(timeout),
            nice,
            ionice,
            once: args.skip(1).any(|arg| arg == "--once"),
            webui,
            workspace,
        })
    }
}
