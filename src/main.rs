//! Continuously invoke an opencode command to sweep repositories on our code
//! forges. Each turn runs in a fresh session against a persistent per-repo
//! workspace that stays cloned and codegraph-indexed across turns,
//! back-to-back with the previous one. Everything except the CLI flags is
//! configured through the always-on web UI and persisted in the workspace.

mod cache;
mod config;
mod emblem;
mod events;
#[cfg(feature = "hostinfo")]
mod host;
mod identity;
mod precheck;
mod prompts;
mod proxy;
mod sandbox;
mod scan;
mod schedule;
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
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::config::{Cli, Config, ForgeKind, USAGE};
use crate::settings::{Problem, SettingsStore};
use crate::status::{Activity, Allowance, Shared, Status};
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
    // Point opencode at the proxy before anything lists models; the web UI
    // re-runs this whenever the proxy settings change.
    prompts::install_provider(&prompts::opencode_config_dir(), &store.snapshot().0.proxy)?;
    // A task that came up empty remembers the state it answered for — the
    // tree's commit, or the notification feed for `feedback` — so it isn't
    // drawn again until that moves.
    let cache = cache::Cache::load(cli.workspace.join("skips.json"));
    let status = Shared::new();
    let addr = webui::spawn(
        cli.listen,
        status.clone(),
        store.clone(),
        cli.workspace.clone(),
    )?;
    info!("webui listening on http://{addr}");

    // Warm the model listing off the startup path: `opencode models` can
    // take the better part of a minute on small hosts, and the first
    // settings page load shouldn't stall for it.
    std::thread::Builder::new()
        .name("models".into())
        .spawn(|| drop(config::models()))?;

    // Watch the forges for activity: a comment on a PR or issue queues that
    // repository for a feedback turn ahead of the draw, and ends the
    // between-turn sleep early.
    let pending = events::Pending::new();
    events::spawn(store.clone(), pending.clone())?;

    // Turns available to spend right now. The bucket refills at the
    // configured rate and holds at most an hour's worth, so a limit of 2
    // runs two turns back to back and then one every half hour. It starts
    // full, whatever limit is configured later, and is resized in step with
    // the limit it was last sized against.
    let mut allowance = f64::INFINITY;
    let mut sized_for: Option<f64> = None;
    let mut refilled = status::epoch_now();
    let mut applied_generation = None;
    // The epoch until which turns are held after one died on proxy auth: a
    // short flat pause, since refreshing is CLIProxyAPI's continuous job and
    // anything it can't fix on its own needs a human either way — which the
    // health problem below surfaces independently.
    let mut auth_gated: Option<u64> = None;
    loop {
        let (settings, generation) = store.snapshot();
        let mut problems = settings.problems();
        if let Some(retry_at) = auth_gated {
            if status::epoch_now() >= retry_at {
                info!("retrying after the proxy auth gate expired");
                auth_gated = None;
                Status::update(&status, |s| s.oauth_gated_until = None);
            } else {
                problems.push(Problem::new(
                    "proxy-card",
                    "the last turn failed to authenticate with CLIProxyAPI; \
                     retrying shortly",
                ));
            }
        }
        if problems.is_empty()
            && let Some(message) = proxy::problem(&settings.proxy)
        {
            problems.push(Problem::new("proxy-card", message));
        }
        if !problems.is_empty() {
            Status::update(&status, |s| {
                s.activity = Activity::Unconfigured { problems }
            });
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        let mut cfg = settings
            .to_config(&cli)
            .expect("settings without problems are runnable");

        // Who the bot is on each forge: the author email and Gitea's
        // credential-line user come from the forge, not the settings.
        // Cached, so this is a map lookup on every pass but the first.
        match identity::resolve(&mut cfg.forges) {
            Ok(resolved) => {
                let identities: std::collections::BTreeMap<String, String> = resolved
                    .into_iter()
                    .map(|(name, id)| (name, format!("{} <{}>", id.login, id.email)))
                    .collect();
                // Only a change wakes the SSE stream; this runs every pass.
                if status.lock().identities != identities {
                    Status::update(&status, |s| s.identities = identities);
                }
            }
            Err((kind, err)) => {
                error!("{err:#}");
                Status::update(&status, |s| {
                    s.activity = Activity::Unconfigured {
                        problems: vec![Problem::new(
                            &format!("f-{}-token", kind.name()),
                            format!("{err:#}"),
                        )],
                    }
                });
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        }

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

        // Off-hours holds come before the bucket so the allowance keeps
        // growing across the closed stretch, the same as any idle time; a
        // turn already under way is left to finish, since the window gates
        // when turns start, not how long they may run.
        if let Some(wait) = cfg.schedule.hold(schedule::local_second_of_day()) {
            let until = status::epoch_now() + wait;
            Status::update(&status, |s| s.activity = Activity::OffHours { until });
            // Sliced like the rate-limit wait, and cut short by an edit, so
            // a widened window applies at once instead of at the end of the
            // hold.
            hold(&store, generation, wait.min(60));
            continue;
        }

        // An edited limit resizes the bucket instead of waiting for the old
        // one to run out: raising it hands the extra turns over now, and
        // lowering it spills what no longer fits. An unlimited stretch isn't
        // accounted for at all — the allowance only drains over one — so a
        // limit set afterwards starts from a full bucket.
        if cfg.hourly_limit != sized_for {
            allowance = match (sized_for, cfg.hourly_limit) {
                (Some(old), Some(new)) => resized(allowance, capacity(old), capacity(new)),
                (None, Some(new)) => capacity(new),
                (_, None) => allowance,
            };
            sized_for = cfg.hourly_limit;
        }

        let now = status::epoch_now();
        // The bucket's clock advances every pass, so the time it grew for is
        // counted once whether or not this pass gets to run a turn.
        let since = std::mem::replace(&mut refilled, now);
        if let Some(limit) = cfg.hourly_limit {
            allowance = refill(allowance, since, now, limit);
            Status::update(&status, |s| s.allowance = Some(spendable(allowance, limit)));
            if allowance < 1.0 {
                // Whole seconds rounded up, so the wait can't expire a hair
                // early and spin the loop.
                let wait = ((1.0 - allowance) * HOUR / limit).ceil() as u64;
                Status::update(&status, |s| {
                    s.activity = Activity::RateLimited { until: now + wait }
                });
                // Sleep in slices and fall back into the loop, cut short by
                // an edit, so a limit raised in the web UI applies at once
                // instead of at the end of the wait.
                hold(&store, generation, wait.min(60));
                continue;
            }
        } else {
            Status::update(&status, |s| s.allowance = None);
        }

        match turn::run(&cfg, &status, &pending, &cache) {
            Ok(report) => {
                if report.auth_error {
                    let until = status::epoch_now() + AUTH_RETRY;
                    error!(
                        "the turn died on proxy auth; holding turns for {} minutes",
                        AUTH_RETRY / 60
                    );
                    auth_gated = Some(until);
                    Status::update(&status, |s| s.oauth_gated_until = Some(until));
                }
                if report.completed {
                    allowance -= 1.0;
                    if let Some(limit) = cfg.hourly_limit {
                        let left = spendable(allowance, limit);
                        Status::update(&status, |s| s.allowance = Some(left));
                        info!(
                            "{:.1} of {} turns left at {limit}/hour",
                            left.available, left.capacity
                        );
                    }
                }
                match report.backoff {
                    // A cancelled turn heads straight into the next one —
                    // that is the button's promise — but a usage-limit
                    // backoff still holds, since retrying early just burns
                    // the next turn on the same limit.
                    Backoff::Normal if report.canceled => {}
                    backoff => sleep(&cli, backoff, &status, &pending),
                }
            }
            // A failing turn (bad token, unreachable forge, ...) must not
            // kill the runner now that config is editable at runtime.
            Err(err) => {
                error!("turn failed: {err:#}");
                Status::update(&status, |s| s.log_tail = format!("turn failed: {err:#}"));
                sleep(&cli, Backoff::Normal, &status, &pending);
            }
        }
        if cli.once {
            return Ok(());
        }
    }
}

const HOUR: f64 = 3600.0;

/// How long turns are held after one died on proxy auth, in seconds. Flat
/// and short: refreshing is the proxy's continuous job, so either the blip
/// passes on its own or the health check surfaces what a human must fix.
const AUTH_RETRY: u64 = 5 * 60;

/// How many turns the bucket holds when full: an hour's worth, but never
/// less than one, so a fractional limit still lets a turn through — 0.5 an
/// hour is one turn every two hours rather than none at all.
fn capacity(limit: f64) -> f64 {
    limit.floor().max(1.0)
}

/// The bucket as the page shows it. The allowance is clamped at zero: a turn
/// that just spent the last of it leaves a hair less than none behind, and
/// "-0.0 of 2" reads as a bug.
fn spendable(allowance: f64, limit: f64) -> Allowance {
    Allowance {
        available: allowance.max(0.0),
        capacity: capacity(limit),
    }
}

/// The allowance grown for the time since it was last topped up, capped at
/// the bucket's capacity so an idle runner banks at most one hour.
fn refill(allowance: f64, since: u64, now: u64, limit: f64) -> f64 {
    let earned = now.saturating_sub(since) as f64 * limit / HOUR;
    (allowance + earned).min(capacity(limit))
}

/// What the bucket holds after its capacity changes from `was` to `now`:
/// the growth is handed over at once, so a raised limit buys turns now
/// rather than an hour from now, and whatever a shrunken bucket can't hold
/// spills.
pub fn resized(allowance: f64, was: f64, now: f64) -> f64 {
    (allowance + (now - was).max(0.0)).min(now)
}

/// Sleep for `seconds`, returning early once the settings change: the wait
/// was computed from a limit or a window that no longer applies, so the loop
/// goes back around and works the hold out again.
fn hold(store: &SettingsStore, generation: u64, seconds: u64) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || store.generation() != generation {
            break;
        }
        std::thread::sleep(left.min(Duration::from_secs(1)));
    }
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
    // retire whatever the opencode-claude-auth era left behind (plugin
    // links, npm references, the stale anthropic credential), wire the
    // codegraph MCP server into opencode's config when the CLI is actually
    // installed, and let rtk install its own command-rewrite plugin when that
    // CLI is there.
    prompts::install(&prompts::opencode_config_dir())?;
    prompts::remove_claude_plugin(
        &prompts::opencode_config_dir(),
        &prompts::opencode_auth_json(),
    )?;
    prompts::install_mcp(
        &prompts::opencode_config_dir(),
        workspace::codegraph_available(),
    )?;
    prompts::install_rtk();

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

/// Run `command`, failing with whatever it complained about. The caller's
/// error reaches the web UI as the reason turns can't run, and a bare exit
/// status there says nothing about what the user has to fix — a mistyped
/// token is the likely cause and only the command knows it.
fn run(command: &mut Command) -> Result<()> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run {program}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        match said.trim() {
            "" => bail!("{program} exited with {}", output.status),
            said => bail!("{program} exited with {}: {said}", output.status),
        }
    }
    Ok(())
}

/// When the usage window is exhausted, Anthropic reports the epoch at which it
/// reopens; wait for that instead of burning turns until then. Otherwise pause
/// just long enough to keep a failing run from spinning the loop.
fn sleep(cli: &Cli, backoff: Backoff, status: &status::Shared, pending: &events::Pending) {
    if cli.once {
        return;
    }
    let now = status::epoch_now();
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
    // Sliced, so fresh forge activity starts the next turn right away
    // instead of waiting out the pause. The usage-limit hold is slept out in
    // full: nothing can run before the window reopens.
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if !limited && !pending.is_empty() {
            info!("forge activity; ending the sleep early");
            break;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(Duration::from_secs(5)));
    }
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
    fn a_changed_limit_resizes_the_bucket() {
        // Spent out at one an hour, then raised to four: the three turns the
        // new limit adds are in hand right away, not an hour from now.
        assert_eq!(resized(0.0, capacity(1.0), capacity(4.0)), 3.0);
        // A full bucket grows with the limit and stops at the new ceiling.
        assert_eq!(resized(1.0, capacity(1.0), capacity(4.0)), 4.0);
        // A raise too small to fit another turn hands over nothing; the wait
        // still shortens, since the bucket refills faster.
        assert_eq!(resized(0.0, capacity(1.0), capacity(1.5)), 0.0);
        // Lowered: what the bucket can no longer hold spills, and what fits
        // stays — slowing down mid-hour doesn't bank a debt.
        assert_eq!(resized(4.0, capacity(4.0), capacity(1.0)), 1.0);
        assert_eq!(resized(0.5, capacity(4.0), capacity(1.0)), 0.5);
    }

    #[test]
    fn the_bucket_never_shows_a_negative_allowance() {
        // The turn that spends the last whole turn leaves a sliver behind.
        let left = spendable(-0.000_001, 2.0);
        assert_eq!(left.available, 0.0);
        assert_eq!(left.capacity, 2.0);
        // A fractional limit still holds one turn, so the page reads "1 / 1".
        assert_eq!(spendable(1.0, 0.25).capacity, 1.0);
    }
}
