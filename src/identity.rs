//! Who the bot is on each forge, resolved from the forge's own "who am I"
//! endpoint instead of configured by hand: the account a token authenticates
//! as already knows its own login and email, so turns on a forge commit as
//! that account.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use crate::config::{Forge, ForgeKind};
use crate::precheck::forge_json;
use crate::settings::Settings;

/// The account a forge token authenticates as.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Identity {
    /// Login name; Gitea's git credential line is written with it.
    pub login: String,
    /// The email commits on this forge are authored (and committed) as.
    pub email: String,
}

/// One cache slot: the answer, or a recent failure held so the main loop's
/// five-second unconfigured-retry cadence doesn't poll a forge whose token
/// is bad at that same cadence.
#[derive(Clone)]
enum Cached {
    Known(Identity),
    Failed(Instant, String),
}

/// How long a failed lookup is held before the forge is asked again.
const RETRY: Duration = Duration::from_secs(60);

type Key = (ForgeKind, String, String);

/// Answers for the life of the process, keyed by everything they depend on:
/// a different token or host is a different account, and the same token
/// answers the same way until a human edits the account on the forge — which
/// a restart or a token re-save picks up.
static CACHE: Mutex<BTreeMap<Key, Cached>> = Mutex::new(BTreeMap::new());

fn lock() -> MutexGuard<'static, BTreeMap<Key, Cached>> {
    CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The cache's answer for a key, if it still stands: a resolved identity, or
/// the reason a recent lookup failed. None means the forge has to be asked.
///
/// A function of its own so the guard cannot outlive the lookup. `resolve`
/// records what it learns under the same lock, and a guard borrowed into a
/// `match` scrutinee lives until the end of the match — long enough to meet
/// that second lock and park the thread forever on this non-reentrant mutex.
fn cached(key: &Key) -> Option<Result<Identity, String>> {
    let answer = lock().get(key).cloned();
    match answer {
        Some(Cached::Known(identity)) => Some(Ok(identity)),
        Some(Cached::Failed(when, why)) if when.elapsed() < RETRY => Some(Err(why)),
        _ => None,
    }
}

/// Record an answer for a key, holding the lock only for the write.
fn remember(key: Key, answer: Cached) {
    lock().insert(key, answer);
}

/// Fill each forge's author email — and Gitea's credential-line user — from
/// its whoami endpoint, through the cache. Fails on the first forge that
/// can't answer, naming it: a turn there would commit as nobody, so the
/// caller holds turns like any other unrunnable configuration. The web UI's
/// view of the same endpoint is [`health`], which keeps asking.
pub fn resolve(forges: &mut [Forge]) -> Result<(), (ForgeKind, anyhow::Error)> {
    for forge in forges.iter_mut() {
        let key = (forge.kind, forge.url.clone(), forge.token.clone());
        let identity = match cached(&key) {
            Some(Ok(identity)) => identity,
            Some(Err(why)) => return Err((forge.kind, anyhow!("{why}"))),
            None => match whoami(forge) {
                Ok(identity) => {
                    tracing::info!(
                        "{} commits as {} <{}>",
                        forge.kind.name(),
                        identity.login,
                        identity.email
                    );
                    remember(key, Cached::Known(identity.clone()));
                    identity
                }
                Err(err) => {
                    let err = err.context(format!(
                        "could not identify the {} account",
                        forge.kind.name()
                    ));
                    remember(key, Cached::Failed(Instant::now(), format!("{err:#}")));
                    return Err((forge.kind, err));
                }
            },
        };
        forge.email = identity.email.clone();
        forge.login = identity.login.clone();
        if forge.kind == ForgeKind::Gitea {
            forge.user = identity.login.clone();
        }
    }
    Ok(())
}

fn whoami(forge: &Forge) -> Result<Identity> {
    parse_identity(forge.kind, &ask_whoami(forge)?)
}

/// The raw whoami answer. Every forge spells its whoami endpoint the same way.
fn ask_whoami(forge: &Forge) -> Result<serde_json::Value> {
    forge_json(forge, "user", "user", "user")
}

/// Whether the web UI's forge card can reach its forge, as the dot beside
/// the URL shows it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reach {
    /// No answer yet for these settings: the first probe is still out.
    Checking,
    /// The forge answered with the account the token authenticates as.
    Ok,
    /// The forge answered, but refused the token.
    Unauthorized,
    /// No usable answer: refused connection, DNS, timeout, a wrong URL.
    Unreachable,
    /// The token works, but the account came back without a login or email
    /// to commit as.
    Incomplete,
    /// Nothing to probe: the forge is switched off, or lacks what it needs.
    Disabled,
    NoToken,
    NoUrl,
}

/// One forge's reading for the web UI.
#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
pub struct ForgeHealth {
    pub state: Reach,
    /// "login <email>", once the forge has named the account.
    pub account: Option<String>,
    /// Why the probe failed, for the dot's tooltip.
    pub error: Option<String>,
}

impl ForgeHealth {
    fn bare(state: Reach) -> Self {
        ForgeHealth {
            state,
            account: None,
            error: None,
        }
    }
}

/// How long a reading stands before the forge is asked again. Unlike
/// `resolve`'s cache, a success expires too: the dot is how a revoked token
/// or a forge gone down shows up while turns are running.
const HEALTH_TTL: Duration = Duration::from_secs(60);

/// The last reading for one forge configuration, and whether a probe for it
/// is still out.
struct Reading {
    at: Option<Instant>,
    health: ForgeHealth,
    probing: bool,
}

/// Readings keyed like `CACHE`: a new token or URL is a new reading, so a
/// just-saved token is probed at once instead of waiting out the old one.
static READINGS: Mutex<BTreeMap<Key, Reading>> = Mutex::new(BTreeMap::new());

fn readings() -> MutexGuard<'static, BTreeMap<Key, Reading>> {
    READINGS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Each forge's reachability for the web UI, keyed by forge slug. Never
/// blocks: a stale or missing reading is refreshed on a thread of its own and
/// the last one (or `Checking`) is handed back meanwhile. The caller is the
/// event stream's tick, and a forge that never answers would otherwise stall
/// it for the whole of Gitea's request timeout — or forever, for gh and glab,
/// which have none.
pub fn health(settings: &Settings) -> BTreeMap<String, ForgeHealth> {
    ForgeKind::ALL
        .into_iter()
        .map(|kind| {
            let stored = settings.forge(kind);
            let health = match settings.runtime_forge(kind) {
                Some(forge) => reading(forge),
                None if !stored.enabled => ForgeHealth::bare(Reach::Disabled),
                None if stored.token.is_empty() => ForgeHealth::bare(Reach::NoToken),
                None => ForgeHealth::bare(Reach::NoUrl),
            };
            (kind.name().to_owned(), health)
        })
        .collect()
}

/// The standing reading for a forge, sending a probe out when it is stale.
fn reading(forge: Forge) -> ForgeHealth {
    let key = (forge.kind, forge.url.clone(), forge.token.clone());
    let mut held = readings();
    let entry = held.entry(key.clone()).or_insert_with(|| Reading {
        at: None,
        health: ForgeHealth::bare(Reach::Checking),
        probing: false,
    });
    let stale = entry.at.is_none_or(|at| at.elapsed() >= HEALTH_TTL);
    if stale && !entry.probing {
        let spawned = std::thread::Builder::new()
            .name("forge-health".into())
            .spawn(move || {
                let health = probe(&forge);
                if let Some(entry) = readings().get_mut(&key) {
                    entry.health = health;
                    entry.at = Some(Instant::now());
                    entry.probing = false;
                }
            });
        // A thread that couldn't start is retried on the next tick.
        entry.probing = spawned.is_ok();
    }
    entry.health.clone()
}

/// Ask the forge who the token is, and read the answer as reachability.
fn probe(forge: &Forge) -> ForgeHealth {
    let answer = match ask_whoami(forge) {
        Ok(answer) => answer,
        Err(err) => {
            let error = format!("{err:#}");
            return ForgeHealth {
                state: classify(&error),
                account: None,
                error: Some(error),
            };
        }
    };
    match parse_identity(forge.kind, &answer) {
        Ok(identity) => ForgeHealth {
            state: Reach::Ok,
            account: Some(format!("{} <{}>", identity.login, identity.email)),
            error: None,
        },
        Err(err) => ForgeHealth {
            state: Reach::Incomplete,
            account: None,
            error: Some(format!("{err:#}")),
        },
    }
}

/// Whether a failed whoami was the forge refusing the token, or no answer at
/// all. Read off the error text, which is the only place the three ways of
/// asking agree: our own Gitea request reports `HTTP {status}`, and gh and
/// glab both end their complaint with `(HTTP {status})`. A 403 that is
/// GitHub's rate limit is the forge being busy, not the token being bad. A
/// token that can't even be sent as a header is a bad token too.
fn classify(error: &str) -> Reach {
    let error = error.to_lowercase();
    let refused = error.contains("http 401")
        || error.contains("401 unauthorized")
        || error.contains("bad credentials")
        || (error.contains("http 403") && !error.contains("rate limit"))
        || error.contains("not a usable header value");
    if refused {
        Reach::Unauthorized
    } else {
        Reach::Unreachable
    }
}

/// The login and email out of a whoami response. A present-but-empty field
/// reads as missing: an instance that hides the email would otherwise hand
/// back an identity that authors commits as nobody.
fn parse_identity(kind: ForgeKind, value: &serde_json::Value) -> Result<Identity> {
    let field = |name: &str| {
        value[name]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    match kind {
        ForgeKind::Gitea => Ok(Identity {
            login: field("login").context("the user response has no login")?,
            email: field("email").context("the user response has no email")?,
        }),
        ForgeKind::Github => {
            let login = field("login").context("the user response has no login")?;
            let email = match field("email") {
                Some(email) => email,
                // A private primary email comes back null; GitHub's own
                // noreply address is what its web editor commits as.
                None => format!(
                    "{}+{login}@users.noreply.github.com",
                    value["id"].as_u64().context("the user response has no id")?
                ),
            };
            Ok(Identity { login, email })
        }
        ForgeKind::Gitlab => Ok(Identity {
            login: field("username").context("the user response has no username")?,
            // commit_email is the account's commit-email preference,
            // including GitLab's noreply option; email is the primary.
            email: field("commit_email")
                .or_else(|| field("email"))
                .context("the user response has no email")?,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;

    fn forge(kind: ForgeKind, user: &str, token: &str) -> Forge {
        Forge {
            kind,
            token: token.into(),
            user: user.into(),
            login: String::new(),
            email: String::new(),
            url: "https://forge.example.com".into(),
            enabled_repos: BTreeSet::new(),
        }
    }

    #[test]
    fn gitea_identity_needs_login_and_email() {
        let identity = parse_identity(
            ForgeKind::Gitea,
            &json!({"login": "bot", "email": "bot@example.com"}),
        )
        .unwrap();
        assert_eq!(identity.login, "bot");
        assert_eq!(identity.email, "bot@example.com");
        // A hidden email comes back empty, which is no email at all.
        assert!(parse_identity(ForgeKind::Gitea, &json!({"login": "bot", "email": ""})).is_err());
        assert!(parse_identity(ForgeKind::Gitea, &json!({"email": "bot@example.com"})).is_err());
    }

    #[test]
    fn github_private_email_becomes_the_noreply_address() {
        let public = parse_identity(
            ForgeKind::Github,
            &json!({"login": "bot", "id": 123, "email": "bot@example.com"}),
        )
        .unwrap();
        assert_eq!(public.email, "bot@example.com");
        let private = parse_identity(
            ForgeKind::Github,
            &json!({"login": "bot", "id": 123, "email": null}),
        )
        .unwrap();
        assert_eq!(private.email, "123+bot@users.noreply.github.com");
        // Without the id the noreply address can't be built.
        assert!(parse_identity(ForgeKind::Github, &json!({"login": "bot", "email": null})).is_err());
        assert!(parse_identity(ForgeKind::Github, &json!({"id": 123})).is_err());
    }

    #[test]
    fn gitlab_prefers_the_commit_email() {
        let preferred = parse_identity(
            ForgeKind::Gitlab,
            &json!({
                "username": "bot",
                "email": "primary@example.com",
                "commit_email": "commit@example.com",
            }),
        )
        .unwrap();
        assert_eq!(preferred.login, "bot");
        assert_eq!(preferred.email, "commit@example.com");
        let fallback = parse_identity(
            ForgeKind::Gitlab,
            &json!({"username": "bot", "email": "primary@example.com"}),
        )
        .unwrap();
        assert_eq!(fallback.email, "primary@example.com");
    }

    #[test]
    fn resolve_fills_from_the_cache() {
        // Tokens are unique per test: the cache is one static shared across
        // the test binary's threads.
        let mut forges = vec![
            forge(ForgeKind::Gitea, "", "cache-test-gitea"),
            forge(ForgeKind::Github, "x-access-token", "cache-test-github"),
        ];
        for f in &forges {
            lock().insert(
                (f.kind, f.url.clone(), f.token.clone()),
                Cached::Known(Identity {
                    login: format!("{}-bot", f.kind.name()),
                    email: format!("bot@{}.example.com", f.kind.name()),
                }),
            );
        }
        resolve(&mut forges).unwrap();
        // Gitea's credential-line user is the resolved login; the other
        // forges keep their fixed pseudo-users. The login lands on every
        // forge either way.
        assert_eq!(forges[0].user, "gitea-bot");
        assert_eq!(forges[0].login, "gitea-bot");
        assert_eq!(forges[0].email, "bot@gitea.example.com");
        assert_eq!(forges[1].user, "x-access-token");
        assert_eq!(forges[1].login, "github-bot");
        assert_eq!(forges[1].email, "bot@github.example.com");
    }

    /// The cache-miss path: `resolve` asks the forge, records the answer, and
    /// returns. It has to take the lock twice to do that — once to find
    /// nothing, once to write what it learned — so a guard that survived the
    /// lookup would hang here rather than fail. The forge is unreachable, so
    /// the recorded answer is a failure, which the next call hands back
    /// without asking again.
    #[test]
    fn a_fresh_lookup_records_its_answer() {
        let mut forges = vec![forge(ForgeKind::Gitea, "", "fresh-lookup-test")];
        // Nothing listens on port 1, so whoami fails as fast as the request
        // can be refused.
        forges[0].url = "http://127.0.0.1:1".into();

        let (kind, err) = resolve(&mut forges).unwrap_err();
        assert_eq!(kind, ForgeKind::Gitea);
        let first = format!("{err:#}");
        assert!(
            first.contains("could not identify the gitea account"),
            "{first}"
        );

        // Held, rather than re-asked: the same message comes back verbatim,
        // which it only can if the failing lookup made it to the cache.
        let (_, err) = resolve(&mut forges).unwrap_err();
        assert_eq!(format!("{err:#}"), first);
    }

    #[test]
    fn a_refused_token_reads_apart_from_no_answer() {
        for refused in [
            "could not identify the gitea account: the gitea api answered HTTP 401 for https://g/api/v1/user",
            "gh exited with exit status: 1: gh: Bad credentials (HTTP 401)",
            "glab exited with exit status: 1: glab: 401 Unauthorized (HTTP 401)",
            "the gitea api answered HTTP 403 for https://g/api/v1/user",
            "the Authorization value is not a usable header value",
        ] {
            assert_eq!(classify(refused), Reach::Unauthorized, "{refused}");
        }
        for unreachable in [
            "failed to GET http://127.0.0.1:1/api/v1/user: connection refused",
            "the gitea api answered HTTP 404 for https://g/api/v1/user",
            "gh exited with exit status: 1: error connecting to ghe.example.com",
            "gh: API rate limit exceeded for user ID 1. (HTTP 403)",
            "failed to run glab",
        ] {
            assert_eq!(classify(unreachable), Reach::Unreachable, "{unreachable}");
        }
    }

    #[test]
    fn forges_with_nothing_to_probe_say_why() {
        let mut settings = Settings::default();
        settings.github.token = "unused".into();
        settings.gitea.enabled = true;
        settings.gitea.token = "unused".into();
        settings.gitlab.enabled = true;
        let health = health(&settings);
        // Only enabled forges are probed, token or not.
        assert_eq!(health["github"].state, Reach::Disabled);
        assert_eq!(health["gitea"].state, Reach::NoUrl);
        assert_eq!(health["gitlab"].state, Reach::NoToken);
    }

    /// The first reading is `Checking`, handed back without waiting; the
    /// probe lands on its own thread and a later call reads its answer.
    #[test]
    fn a_probe_lands_without_blocking_the_caller() {
        let mut settings = Settings::default();
        settings.gitea.enabled = true;
        settings.gitea.token = "health-probe-test".into();
        // Nothing listens on port 1, so the probe fails fast.
        settings.gitea.url = "http://127.0.0.1:1".into();
        assert_eq!(health(&settings)["gitea"].state, Reach::Checking);
        let deadline = Instant::now() + Duration::from_secs(10);
        let landed = loop {
            let gitea = health(&settings).remove("gitea").unwrap();
            if gitea.state != Reach::Checking || Instant::now() > deadline {
                break gitea;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(landed.state, Reach::Unreachable);
        assert!(landed.error.is_some());
    }

    #[test]
    fn a_recent_failure_is_held_without_asking_again() {
        let mut forges = vec![forge(ForgeKind::Github, "x-access-token", "failed-test")];
        lock().insert(
            (ForgeKind::Github, forges[0].url.clone(), "failed-test".into()),
            Cached::Failed(Instant::now(), "the token is bad".into()),
        );
        let (kind, err) = resolve(&mut forges).unwrap_err();
        assert_eq!(kind, ForgeKind::Github);
        assert_eq!(format!("{err:#}"), "the token is bad");
    }
}
