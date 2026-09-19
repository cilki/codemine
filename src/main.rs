//! Continuously invoke an opencode command to sweep repositories on our code
//! forges. Each turn runs in a fresh session against a persistent per-repo
//! workspace that stays cloned and codegraph-indexed across turns,
//! back-to-back with the previous one. Everything except the CLI flags is
//! configured through the always-on web UI and persisted in the workspace.

mod config;
mod prompts;
mod scan;
mod settings;
mod status;
mod turn;
mod usage;
mod webui;
mod workspace;

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::config::{Cli, Config, ForgeKind, USAGE};
use crate::settings::{Problem, SettingsStore};
use crate::status::{Activity, Shared, Status};
use crate::turn::Backoff;

fn main() -> Result<()> {
    let Some(cli) = Cli::parse(std::env::args())? else {
        println!("{USAGE}");
        return Ok(());
    };
    init_logging();
    setup(&cli)?;

    let store = Arc::new(SettingsStore::load(cli.workspace.join("config.json"))?);
    let status = Shared::new();
    let addr = webui::spawn(cli.listen, status.clone(), store.clone())?;
    info!("webui listening on http://{addr}");

    let mut day = local_day()?;
    let mut completed_today = 0u32;
    let mut applied_generation = None;
    loop {
        let (settings, generation) = store.snapshot();
        let mut problems = settings.problems();
        if !claude_oauth_usable() {
            problems.push(Problem::new(
                "",
                format!(
                    "no usable Claude OAuth login at {}",
                    claude_credentials().display()
                ),
            ));
        }
        if !problems.is_empty() {
            Status::update(&status, |s| {
                s.activity = Activity::Unconfigured { problems }
            });
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        let cfg = settings
            .to_config(&cli)
            .expect("settings without problems are runnable");

        if applied_generation != Some(generation) {
            if let Err(err) = apply_forge_auth(&cli, &cfg) {
                error!("failed to apply forge auth: {err:#}");
                Status::update(&status, |s| {
                    s.activity = Activity::Unconfigured {
                        problems: vec![Problem::new(
                            "",
                            format!("failed to apply forge auth: {err:#}"),
                        )],
                    }
                });
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
            applied_generation = Some(generation);
        }

        let today = local_day()?;
        if today != day {
            day = today;
            completed_today = 0;
        }
        Status::update(&status, |s| {
            s.day = day.clone();
            s.completed_today = completed_today;
            s.daily_limit = cfg.daily_limit;
        });
        if cfg
            .daily_limit
            .is_some_and(|limit| completed_today >= limit)
        {
            Status::update(&status, |s| {
                s.activity = Activity::WaitingForTomorrow { day: day.clone() }
            });
            // Sleep in slices and fall back into the loop, so the date check
            // stays DST-safe and a raised limit applies within minutes.
            std::thread::sleep(Duration::from_secs(600));
            continue;
        }

        match turn::run(&cfg, &status) {
            Ok(report) => {
                if report.completed {
                    completed_today += 1;
                    Status::update(&status, |s| s.completed_today = completed_today);
                    if let Some(limit) = cfg.daily_limit {
                        info!("completed {completed_today}/{limit} tasks today");
                    }
                }
                sleep(&cli, report.backoff, &status)?;
            }
            // A failing turn (bad token, unreachable forge, ...) must not
            // kill the runner now that config is editable at runtime.
            Err(err) => {
                error!("turn failed: {err:#}");
                Status::update(&status, |s| s.log_tail = format!("turn failed: {err:#}"));
                sleep(&cli, Backoff::Normal, &status)?;
            }
        }
        if cli.once {
            return Ok(());
        }
    }
}

/// The current local date, e.g. "2026-09-14"; the daily task limit resets
/// when it changes.
fn local_day() -> Result<String> {
    let output = Command::new("date")
        .arg("+%F")
        .output()
        .context("failed to run date")?;
    if !output.status.success() {
        bail!("date exited with {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Send logs to stderr at info and above, overridable per-module with
/// `RUST_LOG`. Timestamps are left off because the runner's output is
/// expected to be stamped by whatever supervises it.
fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .without_time()
        .with_ansi(std::env::var_os("NO_COLOR").is_none())
        .with_writer(std::io::stderr)
        .init();
}

/// One-time startup work that doesn't depend on the mutable settings.
fn setup(cli: &Cli) -> Result<()> {
    // Install the embedded prompts where opencode resolves commands and
    // skills by name, so the binary works without the image copying them, and
    // wire the codegraph MCP server into opencode's config when the CLI is
    // actually installed.
    prompts::install(&prompts::opencode_config_dir())?;
    prompts::install_mcp(
        &prompts::opencode_config_dir(),
        workspace::codegraph_available(),
    )?;

    std::fs::create_dir_all(&cli.workspace)
        .with_context(|| format!("failed to create {}", cli.workspace.display()))?;

    let gitconfig = install_git_config(cli)?;
    // SAFETY: setup() runs before the web UI and agent threads exist, so no
    // other thread can be touching the environment.
    unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", &gitconfig) };
    Ok(())
}

/// Write the git config the runner owns and return its path, for
/// `GIT_CONFIG_GLOBAL`: neither `~/.gitconfig` (mounted read-only from the
/// host) nor `/etc/gitconfig` (root-only) can be counted on to take the
/// credential helper. Every git the runner spawns, opencode's included,
/// inherits the variable. The real global config is chained in with
/// `include.path`, so the user's identity and everything else they set still
/// applies; git ignores the include when the file isn't there.
fn install_git_config(cli: &Cli) -> Result<PathBuf> {
    let path = cli.workspace.join("gitconfig");
    let mut config = String::new();
    if let Ok(home) = std::env::var("HOME") {
        config.push_str(&format!("[include]\n\tpath = {home}/.gitconfig\n"));
    }
    config.push_str(&format!(
        "[credential]\n\thelper = store --file={}\n",
        git_credentials(cli).display()
    ));
    std::fs::write(&path, config).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

/// Where the credential helper keeps forge logins. It lives in the workspace
/// rather than at the helper's default `~/.git-credentials`, because the home
/// directory isn't necessarily writable.
fn git_credentials(cli: &Cli) -> PathBuf {
    cli.workspace.join("git-credentials")
}

/// Let git and tea authenticate to every configured forge; re-run whenever
/// the settings change so new tokens and URLs take effect on the next turn.
fn apply_forge_auth(cli: &Cli, cfg: &Config) -> Result<()> {
    let credentials: String = cfg
        .forges
        .iter()
        .map(|forge| {
            let line = forge
                .url
                .replacen("://", &format!("://{}:{}@", forge.user, forge.token), 1);
            format!("{line}\n")
        })
        .collect();
    let path = git_credentials(cli);
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("failed to write {}", path.display()))?
        .write_all(credentials.as_bytes())?;

    // Log tea in so its commands work without further setup; gh and glab need
    // no login because the runner passes GITHUB_TOKEN and GITLAB_TOKEN to
    // them directly. Drop any previous login first, since `tea login add`
    // refuses to update an existing name.
    if let Some(gitea) = cfg.forges.iter().find(|f| f.kind == ForgeKind::Gitea) {
        Command::new("tea")
            .args(["login", "delete", "gitea"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();
        run(Command::new("tea").args([
            "login",
            "add",
            "--name",
            "gitea",
            "--url",
            &gitea.url,
            "--token",
            &gitea.token,
        ]))?;
    }
    Ok(())
}

/// Where Claude Code keeps the OAuth credentials the opencode-claude-auth
/// plugin reads and refreshes.
fn claude_credentials() -> PathBuf {
    config::home().join(".claude/.credentials.json")
}

/// Whether opencode can authenticate: the opencode-claude-auth plugin reads
/// (and refreshes) the same Claude OAuth credentials file that Claude Code
/// maintains. Checked every loop iteration so a fixed mount recovers without
/// a restart.
fn claude_oauth_usable() -> bool {
    std::fs::File::open(claude_credentials())
        .ok()
        .and_then(|file| serde_json::from_reader::<_, serde_json::Value>(file).ok())
        .is_some_and(|credentials| {
            ["accessToken", "refreshToken"].iter().all(|key| {
                credentials["claudeAiOauth"][key]
                    .as_str()
                    .is_some_and(|token| !token.is_empty())
            })
        })
}

fn run(command: &mut Command) -> Result<()> {
    let program = command.get_program().to_string_lossy().into_owned();
    let status = command
        .status()
        .with_context(|| format!("failed to run {program}"))?;
    if !status.success() {
        bail!("{program} exited with {status}");
    }
    Ok(())
}

/// When the usage window is exhausted, Anthropic reports the epoch at which it
/// reopens; wait for that instead of burning turns until then. Otherwise pause
/// just long enough to keep a failing run from spinning the loop.
fn sleep(cli: &Cli, backoff: Backoff, status: &status::Shared) -> Result<()> {
    if cli.once {
        return Ok(());
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let (seconds, limited) = match backoff {
        Backoff::UsageLimit(epoch) if epoch > now => {
            info!(
                "usage: limit reached; sleeping until {}",
                iso8601(epoch).unwrap_or_else(|| epoch.to_string())
            );
            (epoch - now + 60, true)
        }
        _ => (60, false),
    };
    Status::update(status, |s| {
        s.activity = match limited {
            true => Activity::UsageLimit {
                until: now + seconds,
            },
            false => Activity::Sleeping {
                until: now + seconds,
            },
        };
    });
    std::thread::sleep(Duration::from_secs(seconds));
    Ok(())
}

fn iso8601(epoch: u64) -> Option<String> {
    let output = Command::new("date")
        .args(["-d", &format!("@{epoch}"), "-Iseconds"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
