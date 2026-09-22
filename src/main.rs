//! Continuously invoke an opencode command to sweep repositories on our code
//! forges. Each turn runs in a fresh session against a persistent per-repo
//! workspace that stays cloned and codegraph-indexed across turns,
//! back-to-back with the previous one. Everything except the CLI flags is
//! configured through the always-on web UI and persisted in the workspace.

mod config;
mod emblem;
mod host;
mod prompts;
mod sandbox;
mod scan;
mod settings;
mod status;
mod turn;
mod usage;
mod webui;
mod workspace;

use std::collections::VecDeque;
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
    // The hidden sandbox wrapper mode the turn runner spawns opencode
    // through; on success it execs the wrapped command and never returns.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(sandbox::MARKER) {
        return sandbox::exec(&args[2..]);
    }

    let Some(mut cli) = Cli::parse(std::env::args())? else {
        println!("{USAGE}");
        return Ok(());
    };
    init_logging();
    setup(&mut cli)?;

    let store = Arc::new(SettingsStore::load(cli.workspace.join("config.json"))?);
    let status = Shared::new();
    let addr = webui::spawn(cli.listen, status.clone(), store.clone())?;
    info!("webui listening on http://{addr}");

    // Warm the model listing off the startup path: `opencode models` can
    // take the better part of a minute on small hosts, and the first
    // settings page load shouldn't stall for it.
    std::thread::Builder::new()
        .name("models".into())
        .spawn(|| drop(config::models()))?;

    // Epochs of completed turns, for the UI's trailing-hour count.
    let mut completions: VecDeque<u64> = VecDeque::new();
    // Turns available to spend right now. The bucket refills at the
    // configured rate and holds at most an hour's worth, so a limit of 2
    // runs two turns back to back and then one every half hour. It starts
    // full, whatever limit is configured later.
    let mut allowance = f64::INFINITY;
    let mut refilled = status::epoch_now();
    let mut applied_generation = None;
    // The credentials file's mtime when a turn last died on revoked OAuth;
    // turns stay gated until a fresh login rewrites the file.
    let mut revoked_stamp: Option<SystemTime> = None;
    loop {
        let (settings, generation) = store.snapshot();
        let mut problems = settings.problems();
        if let Some(stamp) = revoked_stamp {
            if credentials_stamp() == stamp {
                problems.push(Problem::new(
                    "",
                    "the Claude OAuth login was revoked; log in again with Claude Code",
                ));
            } else {
                info!("Claude credentials were replaced; resuming turns");
                revoked_stamp = None;
            }
        }
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

        let now = status::epoch_now();
        // The bucket's clock advances every pass, so the time it grew for is
        // counted once whether or not this pass gets to run a turn.
        let since = std::mem::replace(&mut refilled, now);
        let recent = prune_completions(&mut completions, now);
        Status::update(&status, |s| {
            s.completed_last_hour = recent;
            s.hourly_limit = cfg.hourly_limit;
        });
        if let Some(limit) = cfg.hourly_limit {
            allowance = refill(allowance, since, now, limit);
            if allowance < 1.0 {
                // Whole seconds rounded up, so the wait can't expire a hair
                // early and spin the loop.
                let wait = ((1.0 - allowance) * HOUR / limit).ceil() as u64;
                Status::update(&status, |s| {
                    s.activity = Activity::RateLimited { until: now + wait }
                });
                // Sleep in slices and fall back into the loop, so a raised
                // limit applies within a minute instead of at the end of
                // the wait.
                std::thread::sleep(Duration::from_secs(wait.min(60)));
                continue;
            }
        }

        match turn::run(&cfg, &status) {
            Ok(report) => {
                if report.oauth_revoked {
                    error!(
                        "the Claude OAuth login was revoked; holding turns until {} changes",
                        claude_credentials().display()
                    );
                    revoked_stamp = Some(credentials_stamp());
                }
                if report.completed {
                    let at = status::epoch_now();
                    completions.push_back(at);
                    let recent = prune_completions(&mut completions, at);
                    Status::update(&status, |s| s.completed_last_hour = recent);
                    allowance -= 1.0;
                    if let Some(limit) = cfg.hourly_limit {
                        info!(
                            "completed {recent} tasks in the last hour; {allowance:.1} of {} turns left at {limit}/hour",
                            capacity(limit)
                        );
                    }
                }
                match report.backoff {
                    // A cancelled turn heads straight into the next one —
                    // that is the button's promise — but a usage-limit
                    // backoff still holds, since retrying early just burns
                    // the next turn on the same limit.
                    Backoff::Normal if report.canceled => {}
                    backoff => sleep(&cli, backoff, &status)?,
                }
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

const HOUR: f64 = 3600.0;

/// How many turns the bucket holds when full: an hour's worth, but never
/// less than one, so a fractional limit still lets a turn through — 0.5 an
/// hour is one turn every two hours rather than none at all.
fn capacity(limit: f64) -> f64 {
    limit.floor().max(1.0)
}

/// The allowance grown for the time since it was last topped up, capped at
/// the bucket's capacity so an idle runner banks at most one hour.
fn refill(allowance: f64, since: u64, now: u64, limit: f64) -> f64 {
    let earned = now.saturating_sub(since) as f64 * limit / HOUR;
    (allowance + earned).min(capacity(limit))
}

/// Drop completions older than an hour and report how many are left, for the
/// UI's counter.
fn prune_completions(completions: &mut VecDeque<u64>, now: u64) -> u32 {
    while completions
        .front()
        .is_some_and(|&at| now.saturating_sub(at) >= HOUR as u64)
    {
        completions.pop_front();
    }
    completions.len() as u32
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
fn setup(cli: &mut Cli) -> Result<()> {
    // Install the embedded prompts where opencode resolves commands and
    // skills by name, so the binary works without the image copying them,
    // link the Claude OAuth plugin into opencode's plugin directory
    // (dropping any stale anthropic credential a previous version left
    // behind), and wire the codegraph MCP server into opencode's config when
    // the CLI is actually installed.
    prompts::install(&prompts::opencode_config_dir())?;
    prompts::install_plugin(&prompts::opencode_config_dir())?;
    prompts::scrub_anthropic_auth(&prompts::opencode_auth_json())?;
    prompts::install_mcp(
        &prompts::opencode_config_dir(),
        workspace::codegraph_available(),
    )?;

    std::fs::create_dir_all(&cli.workspace)
        .with_context(|| format!("failed to create {}", cli.workspace.display()))?;
    // Run from the workspace: everything the runner owns lives there, and
    // this way it doesn't pin whatever directory it was launched from. The
    // path is made absolute first so a relative --workspace still resolves
    // correctly everywhere after the change of directory.
    cli.workspace = cli
        .workspace
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", cli.workspace.display()))?;
    std::env::set_current_dir(&cli.workspace)
        .with_context(|| format!("failed to chdir to {}", cli.workspace.display()))?;
    // set_current_dir doesn't touch the $PWD convention variable, which
    // would otherwise keep naming the launch directory to every child that
    // trusts it over getcwd.
    // SAFETY: setup() runs before the web UI and agent threads exist, so no
    // other thread can be touching the environment.
    unsafe { std::env::set_var("PWD", &cli.workspace) };

    // Turn logs are only reachable through the in-memory turn list, which
    // starts empty, so whatever a previous process left behind is
    // unreachable garbage.
    let logs = cli.workspace.join("logs");
    if logs.exists() {
        std::fs::remove_dir_all(&logs)
            .with_context(|| format!("failed to clear {}", logs.display()))?;
    }

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
            format!(
                "{}\n",
                forge
                    .url
                    .replacen("://", &format!("://{}:{}@", forge.user, forge.token), 1)
            )
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

/// A fingerprint of the credentials file that changes when it's rewritten,
/// so a fresh login is detectable; a missing file maps to the epoch.
fn credentials_stamp() -> SystemTime {
    std::fs::metadata(claude_credentials())
        .and_then(|meta| meta.modified())
        .unwrap_or(UNIX_EPOCH)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bucket_allows_a_burst_then_the_rate() {
        // Two an hour: two turns in hand at once, one back every half hour.
        assert_eq!(capacity(2.0), 2.0);
        let full = refill(f64::INFINITY, 0, 0, 2.0);
        assert_eq!(full, 2.0);
        let spent = full - 2.0;
        assert_eq!(refill(spent, 100, 100, 2.0), 0.0);
        assert_eq!(refill(spent, 100, 100 + 1800, 2.0), 1.0);
        // An idle day banks an hour's worth and not a turn more.
        assert_eq!(refill(spent, 0, 86_400, 2.0), 2.0);
    }

    #[test]
    fn a_fractional_limit_holds_one_turn() {
        assert_eq!(capacity(0.5), 1.0);
        assert_eq!(refill(0.0, 0, 3600, 0.5), 0.5);
        assert_eq!(refill(0.0, 0, 7200, 0.5), 1.0);
        assert_eq!(refill(0.0, 0, 86_400, 0.5), 1.0);
    }

    #[test]
    fn only_the_last_hour_of_completions_counts() {
        let mut completions: VecDeque<u64> = VecDeque::from([1_000, 4_000, 4_500]);
        // 1_000 is still inside the hour here, and an hour old at 4_600.
        assert_eq!(prune_completions(&mut completions, 4_500), 3);
        assert_eq!(prune_completions(&mut completions, 4_600), 2);
        assert_eq!(prune_completions(&mut completions, 7_000), 2);

        assert_eq!(prune_completions(&mut completions, 100_000), 0);
    }
}
