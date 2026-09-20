use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

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

    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "gitea" => Some(ForgeKind::Gitea),
            "github" => Some(ForgeKind::Github),
            "gitlab" => Some(ForgeKind::Gitlab),
            _ => None,
        }
    }
}

/// A model the runner can be pointed at, as an opencode provider/model ID.
#[derive(Serialize)]
pub struct Model {
    pub id: &'static str,
    pub label: &'static str,
}

/// The models offered in the web UI. Only Anthropic's are listed, because the
/// runner authenticates through the Claude OAuth credentials Claude Code
/// maintains and no other provider can log in.
pub const MODELS: [Model; 8] = [
    Model {
        id: "anthropic/claude-fable-5",
        label: "Claude Fable 5",
    },
    Model {
        id: "anthropic/claude-opus-5",
        label: "Claude Opus 5",
    },
    Model {
        id: "anthropic/claude-opus-4-8",
        label: "Claude Opus 4.8",
    },
    Model {
        id: "anthropic/claude-opus-4-7",
        label: "Claude Opus 4.7",
    },
    Model {
        id: "anthropic/claude-opus-4-6",
        label: "Claude Opus 4.6",
    },
    Model {
        id: "anthropic/claude-sonnet-5",
        label: "Claude Sonnet 5",
    },
    Model {
        id: "anthropic/claude-sonnet-4-6",
        label: "Claude Sonnet 4.6",
    },
    Model {
        id: "anthropic/claude-haiku-4-5",
        label: "Claude Haiku 4.5",
    },
];

/// I/O scheduling class for the agent process tree, passed to ionice.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IoClass {
    /// Best-effort class at the lowest level (ionice -c 2 -n 7).
    BestEffort,
    /// Idle class: only gets disk time nobody else wants (ionice -c 3).
    Idle,
}

/// One code forge the bot sweeps.
pub struct Forge {
    pub kind: ForgeKind,
    pub token: String,
    /// Username for the git credential line; GitHub and GitLab accept any
    /// token-bearing pseudo-user over HTTPS.
    pub user: String,
    pub url: String,
    /// Repositories excluded from sweeping; everything else the account can
    /// reach is fair game, so new repositories join the pool automatically.
    pub disabled_repos: BTreeSet<String>,
}

impl Forge {
    /// Environment for child processes that talk to this forge: `gh` and
    /// `glab` (run both directly and inside the agent session) authenticate
    /// from these variables. Gitea needs none; `tea` reads its login file.
    pub fn env(&self) -> Vec<(String, String)> {
        match self.kind {
            ForgeKind::Gitea => Vec::new(),
            ForgeKind::Github => {
                let mut env = vec![("GITHUB_TOKEN".to_owned(), self.token.clone())];
                if self.url != "https://github.com" {
                    let host = self
                        .url
                        .split_once("://")
                        .map_or(self.url.as_str(), |(_, rest)| rest);
                    env.push(("GH_HOST".to_owned(), host.trim_end_matches('/').to_owned()));
                }
                env
            }
            ForgeKind::Gitlab => {
                let mut env = vec![("GITLAB_TOKEN".to_owned(), self.token.clone())];
                if self.url != "https://gitlab.com" {
                    env.push((
                        "GITLAB_HOST".to_owned(),
                        self.url.trim_end_matches('/').to_owned(),
                    ));
                }
                env
            }
        }
    }
}

/// A runnable snapshot of the settings, rebuilt from the store before each
/// turn so UI changes apply at the next turn boundary.
pub struct Config {
    pub forges: Vec<Forge>,
    pub model: String,
    /// The task pool the runner draws from each turn; the slugs name the task
    /// sections in the sweep command template.
    pub tasks: Vec<String>,
    /// Completed (not skipped) tasks allowed per hour; None is unlimited.
    /// Spent from a bucket holding an hour's worth, so the rate can be
    /// fractional: 0.5 is one turn every two hours.
    pub hourly_limit: Option<f64>,
    pub author_name: String,
    pub author_email: String,
    pub turn_timeout: Duration,
    /// CPU niceness applied to the agent process tree (1-19).
    pub nice: Option<u8>,
    /// I/O scheduling class applied to the agent process tree.
    pub ionice: Option<IoClass>,
    /// Root of the persistent workspace where repositories stay cloned across
    /// turns.
    pub workspace: PathBuf,
}

pub const USAGE: &str = "usage: codemine [--listen ADDR] [--workspace DIR] [--once]

  --listen ADDR     bind address for the web UI (default 0.0.0.0:8080)
  --workspace DIR   persistent workspace root (default ~/.codemine)
  --once            run a single turn and exit
  --help            show this help";

/// Command-line options; everything else is configured through the web UI
/// and persisted in the workspace.
pub struct Cli {
    pub listen: SocketAddr,
    pub workspace: PathBuf,
    pub once: bool,
}

impl Cli {
    /// Parse argv; None means --help was requested and the caller should
    /// print `USAGE` instead of running.
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Option<Self>> {
        let mut cli = Cli {
            listen: SocketAddr::from(([0, 0, 0, 0], 8080)),
            workspace: default_workspace(),
            once: false,
        };
        let mut args = args.skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--listen" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--listen needs an address\n{USAGE}"))?;
                    cli.listen = value
                        .parse()
                        .with_context(|| format!("--listen is not a socket address: {value}"))?;
                }
                "--workspace" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--workspace needs a directory\n{USAGE}"))?;
                    cli.workspace = PathBuf::from(value);
                }
                "--once" => cli.once = true,
                "--help" | "-h" => return Ok(None),
                other => bail!("unknown argument: {other}\n{USAGE}"),
            }
        }
        Ok(Some(cli))
    }
}

/// The current user's home directory, falling back to the container's when
/// `$HOME` is unset (a bare `docker run` with no user).
pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/root".into()))
}

fn default_workspace() -> PathBuf {
    home().join(".codemine")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Cli>> {
        Cli::parse(std::iter::once("codemine".to_owned()).chain(args.iter().map(|s| s.to_string())))
    }

    #[test]
    fn parse_defaults() {
        let cli = parse(&[]).unwrap().unwrap();
        assert_eq!(cli.listen, "0.0.0.0:8080".parse().unwrap());
        assert!(cli.workspace.ends_with(".codemine"));
        assert!(!cli.once);
    }

    #[test]
    fn parse_flags() {
        let cli = parse(&[
            "--listen",
            "127.0.0.1:9000",
            "--workspace",
            "/tmp/ws",
            "--once",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(cli.listen, "127.0.0.1:9000".parse().unwrap());
        assert_eq!(cli.workspace, PathBuf::from("/tmp/ws"));
        assert!(cli.once);
    }

    #[test]
    fn parse_errors_and_help() {
        assert!(parse(&["--help"]).unwrap().is_none());
        assert!(parse(&["--listen"]).is_err());
        assert!(parse(&["--listen", "nonsense"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
    }

    #[test]
    fn forge_env_hosts() {
        let forge = |kind, url: &str| Forge {
            kind,
            token: "tok".into(),
            user: String::new(),
            url: url.into(),
            disabled_repos: BTreeSet::new(),
        };
        assert!(
            forge(ForgeKind::Gitea, "https://git.example.com")
                .env()
                .is_empty()
        );
        assert_eq!(
            forge(ForgeKind::Github, "https://github.com").env(),
            [("GITHUB_TOKEN".to_owned(), "tok".to_owned())]
        );
        assert_eq!(
            forge(ForgeKind::Github, "https://github.example.com/").env(),
            [
                ("GITHUB_TOKEN".to_owned(), "tok".to_owned()),
                ("GH_HOST".to_owned(), "github.example.com".to_owned()),
            ]
        );
        assert_eq!(
            forge(ForgeKind::Gitlab, "https://gitlab.example.com").env(),
            [
                ("GITLAB_TOKEN".to_owned(), "tok".to_owned()),
                (
                    "GITLAB_HOST".to_owned(),
                    "https://gitlab.example.com".to_owned()
                ),
            ]
        );
    }
}
