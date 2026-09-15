use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

pub struct Config {
    pub gitea_token: String,
    pub gitea_user: String,
    pub gitea_url: String,
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
    /// Run a single turn and exit (--once).
    pub once: bool,
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
            Err(_) => ["feedback", "rebase", "bump-deps", "simplify", "todo", "roleplay"]
                .map(String::from)
                .into(),
        };

        let daily_limit = match std::env::var("CODEMINE_DAILY_LIMIT") {
            Ok(s) => Some(
                s.parse()
                    .with_context(|| format!("CODEMINE_DAILY_LIMIT is not a number: {s}"))?,
            ),
            Err(_) => None,
        };

        Ok(Self {
            gitea_token: required("GITEA_TOKEN")?,
            gitea_user: required("GITEA_USER")?,
            gitea_url: required("GITEA_URL")?,
            model: required("CODEMINE_MODEL")?,
            command: std::env::var("CODEMINE_COMMAND").unwrap_or_else(|_| "sweep".into()),
            tasks,
            author_name: required("GIT_AUTHOR_NAME")?,
            author_email: required("GIT_AUTHOR_EMAIL")?,
            daily_limit,
            turn_timeout: Duration::from_secs(timeout),
            once: args.skip(1).any(|arg| arg == "--once"),
        })
    }
}
