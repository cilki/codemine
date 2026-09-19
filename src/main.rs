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
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::config::{Cli, Config, ForgeKind, USAGE};
use crate::settings::SettingsStore;
use crate::status::{Activity, Status};
use crate::turn::Backoff;

const CLAUDE_CREDENTIALS: &str = "/root/.claude/.credentials.json";

fn main() -> Result<()> {
    let Some(cli) = Cli::parse(std::env::args())? else {
        println!("{USAGE}");
        return Ok(());
    };
    setup(&cli)?;

    let store = Arc::new(SettingsStore::load(cli.workspace.join("config.json"))?);
    let status = Status::new();
    let addr = webui::spawn(cli.listen, status.clone(), store.clone())?;
    println!("webui listening on http://{addr}");

    let mut day = local_day()?;
    let mut completed_today = 0u32;
    let mut applied_generation = None;
    loop {
        let (settings, generation) = store.snapshot();
        let mut problems = settings.problems();
        if !claude_oauth_usable() {
            problems.push(format!("no usable Claude OAuth login at {CLAUDE_CREDENTIALS}"));
        }
        if !problems.is_empty() {
            Status::update(&status, |s| s.activity = Activity::Unconfigured { problems });
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        let cfg = settings
            .to_config(&cli)
            .expect("settings without problems are runnable");

        if applied_generation != Some(generation) {
            if let Err(err) = apply_forge_auth(&cfg) {
                eprintln!("failed to apply forge auth: {err:#}");
                Status::update(&status, |s| {
                    s.activity = Activity::Unconfigured {
                        problems: vec![format!("failed to apply forge auth: {err:#}")],
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
                        println!("completed {completed_today}/{limit} tasks today");
                    }
                }
                sleep(&cli, report.backoff, &status)?;
            }
            // A failing turn (bad token, unreachable forge, ...) must not
            // kill the runner now that config is editable at runtime.
            Err(err) => {
                eprintln!("turn failed: {err:#}");
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

    // ~/.gitconfig is mounted read-only from the host, so the credential
    // helper goes in the system config instead, where every process in the
    // container picks it up.
    run(Command::new("git").args(["config", "--system", "credential.helper", "store"]))?;
    Ok(())
}

/// Let git and tea authenticate to every configured forge; re-run whenever
/// the settings change so new tokens and URLs take effect on the next turn.
fn apply_forge_auth(cfg: &Config) -> Result<()> {
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
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open("/root/.git-credentials")?
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

/// Whether opencode can authenticate: the opencode-claude-auth plugin reads
/// (and refreshes) the same Claude OAuth credentials file that Claude Code
/// maintains. Checked every loop iteration so a fixed mount recovers without
/// a restart.
fn claude_oauth_usable() -> bool {
    std::fs::File::open(CLAUDE_CREDENTIALS)
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
            println!(
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
