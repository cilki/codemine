//! Continuously invoke an opencode command to sweep repositories on Gitea.
//! Each turn runs in a fresh workspace and a fresh session, back-to-back with
//! the previous one.

mod config;
mod prompts;
mod scan;
mod status;
mod turn;
mod usage;
mod webui;

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::config::{Config, ForgeKind};
use crate::status::{Activity, Status};
use crate::turn::Backoff;

const CLAUDE_CREDENTIALS: &str = "/root/.claude/.credentials.json";

fn main() -> Result<()> {
    let cfg = Config::from_env(std::env::args())?;
    setup(&cfg)?;

    let status = Status::new(cfg.daily_limit);
    if let Some(addr) = cfg.webui {
        let addr = webui::spawn(addr, status.clone())?;
        println!("webui listening on http://{addr}");
    }

    let mut day = local_day()?;
    let mut completed_today = 0u32;
    loop {
        let today = local_day()?;
        if today != day {
            day = today;
            completed_today = 0;
        }
        Status::update(&status, |s| {
            s.day = day.clone();
            s.completed_today = completed_today;
        });
        if cfg
            .daily_limit
            .is_some_and(|limit| completed_today >= limit)
        {
            Status::update(&status, |s| {
                s.activity = Activity::WaitingForTomorrow { day: day.clone() }
            });
            wait_for_tomorrow(&day)?;
            continue;
        }

        let report = turn::run(&cfg, &status)?;
        if report.completed {
            completed_today += 1;
            Status::update(&status, |s| s.completed_today = completed_today);
            if let Some(limit) = cfg.daily_limit {
                println!("completed {completed_today}/{limit} tasks today");
            }
        }
        sleep(&cfg, report.backoff, &status)?;
        if cfg.once {
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

/// Poll instead of sleeping to a computed midnight so DST shifts can't
/// oversleep or wake early.
fn wait_for_tomorrow(day: &str) -> Result<()> {
    println!("daily limit reached; sleeping until tomorrow");
    while local_day()? == day {
        std::thread::sleep(Duration::from_secs(600));
    }
    Ok(())
}

fn setup(cfg: &Config) -> Result<()> {
    // Install the embedded prompts where opencode resolves commands and
    // skills by name, so the binary works without the image copying them.
    prompts::install(&prompts::opencode_config_dir())?;

    // Let git authenticate to every configured forge over HTTPS. ~/.gitconfig
    // is mounted read-only from the host, so the credential helper goes in the
    // system config instead, where every process in the container picks it up.
    run(Command::new("git").args(["config", "--system", "credential.helper", "store"]))?;
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
    // no login because they read GITHUB_TOKEN and GITLAB_TOKEN from the
    // environment directly. The container's filesystem is fresh on every
    // start, so there is no existing login to collide with.
    if let Some(gitea) = cfg.forges.iter().find(|f| f.kind == ForgeKind::Gitea) {
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

    // Sanity check credentials. opencode authenticates through the
    // opencode-claude-auth plugin, which reads (and refreshes) the same Claude
    // OAuth credentials file that Claude Code maintains.
    let usable = std::fs::File::open(CLAUDE_CREDENTIALS)
        .ok()
        .and_then(|file| serde_json::from_reader::<_, serde_json::Value>(file).ok())
        .is_some_and(|credentials| {
            ["accessToken", "refreshToken"].iter().all(|key| {
                credentials["claudeAiOauth"][key]
                    .as_str()
                    .is_some_and(|token| !token.is_empty())
            })
        });
    if !usable {
        bail!("No usable Claude OAuth login at {CLAUDE_CREDENTIALS}.");
    }
    Ok(())
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
fn sleep(cfg: &Config, backoff: Backoff, status: &status::Shared) -> Result<()> {
    if cfg.once {
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
