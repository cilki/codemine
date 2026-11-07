//! Who the bot is on the forge, resolved from the forge's own "who am I"
//! endpoint instead of configured by hand: the account a token authenticates
//! as already knows its own login and email, so turns commit as that
//! account.

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

/// How long a reading stands before the forge is asked again, whichever side
/// took it. A failure held this long keeps the main loop's five-second
/// unconfigured-retry cadence from polling a forge whose token is bad at that
/// same cadence; a success expires too, because the web UI's dot is how a
/// revoked token or a forge gone down shows up while turns are running.
const TTL: Duration = Duration::from_secs(60);

type Key = (ForgeKind, String, String);

/// What is known about one forge configuration. `resolve` and `health` ask
/// the forge the same question, so they share the answer: whichever asks
/// first spares the other the request.
#[derive(Default)]
struct Entry {
    /// The account, once any lookup has named it. Kept when a later reading
    /// fails, so a forge that goes down after startup doesn't stop turns
    /// that already know who they commit as.
    identity: Option<Identity>,
    /// The latest reading and when it was taken; None until the first lands.
    reading: Option<(Instant, ForgeHealth)>,
    /// Whether `health` has a probe out for this entry.
    probing: bool,
}

/// Entries keyed by everything an answer depends on: a different token or
/// host is a different account, so a just-saved token is asked about at once
/// instead of waiting out the old one.
static ENTRIES: Mutex<BTreeMap<Key, Entry>> = Mutex::new(BTreeMap::new());

fn entries() -> MutexGuard<'static, BTreeMap<Key, Entry>> {
    ENTRIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn key(forge: &Forge) -> Key {
    (forge.kind, forge.url.clone(), forge.token.clone())
}

/// Record a reading, and the account it named if it named one. Clears
/// `probing` whoever took it: the reading is fresh either way, so no probe
/// is wanted until it goes stale.
fn record(forge: &Forge, (health, identity): (ForgeHealth, Option<Identity>)) {
    let mut held = entries();
    let entry = held.entry(key(forge)).or_default();
    if let Some(identity) = identity
        && entry.identity.as_ref() != Some(&identity)
    {
        tracing::info!(
            "{} commits as {} <{}>",
            forge.kind.name(),
            identity.login,
            identity.email
        );
        entry.identity = Some(identity);
    }
    entry.reading = Some((Instant::now(), health));
    entry.probing = false;
}

/// Fill the forge's author email — and Gitea's credential-line user — from
/// its whoami endpoint, through the shared entry. Fails when the forge can't
/// answer: a turn there would commit as nobody, so the caller holds turns
/// like any other unrunnable configuration.
pub fn resolve(forge: &mut Forge) -> Result<()> {
    // A `let`, not a `match` scrutinee, so the guard is gone before `record`
    // takes the lock again: this mutex is not reentrant.
    let known =
        entries()
            .get(&key(forge))
            .and_then(|entry| match (&entry.identity, &entry.reading) {
                (Some(identity), _) => Some(Ok(identity.clone())),
                (None, Some((at, health))) if at.elapsed() < TTL => Some(Err(health.error.clone())),
                _ => None,
            });
    let answer = known.unwrap_or_else(|| {
        let (health, identity) = probe(forge);
        let answer = identity.clone().ok_or_else(|| health.error.clone());
        record(forge, (health, identity));
        answer
    });
    let identity = answer.map_err(|why| {
        anyhow!(
            "could not identify the {} account: {}",
            forge.kind.name(),
            why.unwrap_or_default()
        )
    })?;
    forge.email = identity.email;
    forge.login = identity.login;
    Ok(())
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
    /// Nothing to probe: the forge lacks what it needs.
    NoToken,
    NoUrl,
}

/// The forge's reading for the web UI.
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

/// The forge's reachability for the web UI. Never blocks: a stale or missing
/// reading is refreshed on a thread of its own and the last one (or
/// `Checking`) is handed back meanwhile. The caller is the event stream's
/// tick, and a forge that never answers would otherwise stall it for the
/// whole of Gitea's request timeout — or forever, for gh and glab, which
/// have none.
pub fn health(settings: &Settings) -> ForgeHealth {
    match settings.runtime_forge() {
        Some(forge) => reading(forge),
        None if settings.forge.token.is_empty() => ForgeHealth::bare(Reach::NoToken),
        None => ForgeHealth::bare(Reach::NoUrl),
    }
}

/// The standing reading for a forge, sending a probe out when it is stale.
fn reading(forge: Forge) -> ForgeHealth {
    let mut held = entries();
    let entry = held.entry(key(&forge)).or_default();
    let stale = entry
        .reading
        .as_ref()
        .is_none_or(|(at, _)| at.elapsed() >= TTL);
    if stale && !entry.probing {
        let spawned = std::thread::Builder::new()
            .name("forge-health".into())
            .spawn(move || record(&forge, probe(&forge)));
        // A thread that couldn't start is retried on the next tick.
        entry.probing = spawned.is_ok();
    }
    match &entry.reading {
        Some((_, health)) => health.clone(),
        None => ForgeHealth::bare(Reach::Checking),
    }
}

/// Ask the forge who the token is: the reading the answer amounts to, and the
/// account when it named a usable one.
fn probe(forge: &Forge) -> (ForgeHealth, Option<Identity>) {
    // Every forge spells its whoami endpoint the same way.
    let answer = match forge_json(forge, "user", "user", "user") {
        Ok(answer) => answer,
        Err(err) => {
            let error = format!("{err:#}");
            let health = ForgeHealth {
                state: classify(&error),
                account: None,
                error: Some(error),
            };
            return (health, None);
        }
    };
    match parse_identity(forge.kind, &answer) {
        Ok(identity) => {
            let health = ForgeHealth {
                state: Reach::Ok,
                account: Some(format!("{} <{}>", identity.login, identity.email)),
                error: None,
            };
            (health, Some(identity))
        }
        Err(err) => (
            ForgeHealth {
                state: Reach::Incomplete,
                account: None,
                error: Some(format!("{err:#}")),
            },
            None,
        ),
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
                    value["id"]
                        .as_u64()
                        .context("the user response has no id")?
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

    fn forge(kind: ForgeKind, token: &str) -> Forge {
        Forge {
            kind,
            token: token.into(),
            login: String::new(),
            email: String::new(),
            url: "https://forge.example.com".into(),
            repos: BTreeMap::new(),
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
        assert!(
            parse_identity(ForgeKind::Github, &json!({"login": "bot", "email": null})).is_err()
        );
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
        let mut gitea = forge(ForgeKind::Gitea, "cache-test-gitea");
        let mut github = forge(ForgeKind::Github, "cache-test-github");
        for f in [&gitea, &github] {
            entries().insert(
                key(f),
                Entry {
                    identity: Some(Identity {
                        login: format!("{}-bot", f.kind.name()),
                        email: format!("bot@{}.example.com", f.kind.name()),
                    }),
                    ..Entry::default()
                },
            );
        }
        resolve(&mut gitea).unwrap();
        resolve(&mut github).unwrap();
        // Gitea's credential-line user is the resolved login; the other
        // forges keep their fixed pseudo-users. The login lands on every
        // forge either way.
        assert_eq!(gitea.credential_user(), "gitea-bot");
        assert_eq!(gitea.login, "gitea-bot");
        assert_eq!(gitea.email, "bot@gitea.example.com");
        assert_eq!(github.credential_user(), "x-access-token");
        assert_eq!(github.login, "github-bot");
        assert_eq!(github.email, "bot@github.example.com");
    }

    /// The cache-miss path: `resolve` asks the forge, records the answer, and
    /// returns. It has to take the lock twice to do that — once to find
    /// nothing, once to write what it learned — so a guard that survived the
    /// lookup would hang here rather than fail. The forge is unreachable, so
    /// the recorded answer is a failure, which the next call hands back
    /// without asking again.
    #[test]
    fn a_fresh_lookup_records_its_answer() {
        let mut gitea = forge(ForgeKind::Gitea, "fresh-lookup-test");
        // Nothing listens on port 1, so whoami fails as fast as the request
        // can be refused.
        gitea.url = "http://127.0.0.1:1".into();

        let err = resolve(&mut gitea).unwrap_err();
        let first = format!("{err:#}");
        assert!(
            first.contains("could not identify the gitea account"),
            "{first}"
        );

        // Held, rather than re-asked: the same message comes back verbatim,
        // which it only can if the failing lookup made it to the cache.
        let err = resolve(&mut gitea).unwrap_err();
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
    fn a_forge_with_nothing_to_probe_says_why() {
        let mut settings = Settings::default();
        settings.forge.kind = ForgeKind::Gitlab;
        assert_eq!(health(&settings).state, Reach::NoToken);
        settings.forge.kind = ForgeKind::Gitea;
        settings.forge.token = "unused".into();
        assert_eq!(health(&settings).state, Reach::NoUrl);
    }

    /// The first reading is `Checking`, handed back without waiting; the
    /// probe lands on its own thread and a later call reads its answer.
    #[test]
    fn a_probe_lands_without_blocking_the_caller() {
        let mut settings = Settings::default();
        settings.forge.kind = ForgeKind::Gitea;
        settings.forge.token = "health-probe-test".into();
        // Nothing listens on port 1, so the probe fails fast.
        settings.forge.url = "http://127.0.0.1:1".into();
        assert_eq!(health(&settings).state, Reach::Checking);
        let deadline = Instant::now() + Duration::from_secs(10);
        let landed = loop {
            let gitea = health(&settings);
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
        let mut github = forge(ForgeKind::Github, "failed-test");
        let mut health = ForgeHealth::bare(Reach::Unauthorized);
        health.error = Some("the token is bad".into());
        entries().insert(
            key(&github),
            Entry {
                reading: Some((Instant::now(), health)),
                ..Entry::default()
            },
        );
        let err = resolve(&mut github).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "could not identify the github account: the token is bad"
        );
    }

    /// A reading the web UI's probe took is the one `resolve` answers from,
    /// and the other way round: the two ask the forge the same question, so
    /// whichever asked first spares the other the request.
    #[test]
    fn resolve_and_health_share_one_reading() {
        let mut settings = Settings::default();
        settings.forge.kind = ForgeKind::Gitea;
        settings.forge.token = "shared-reading-test".into();
        // Nothing listens on port 1, so the lookup fails fast.
        settings.forge.url = "http://127.0.0.1:1".into();
        let mut gitea = settings.runtime_forge().unwrap();
        let err = format!("{:#}", resolve(&mut gitea).unwrap_err());
        // Not `Checking`: resolve's failed lookup is already the reading.
        let health = health(&settings);
        assert_eq!(health.state, Reach::Unreachable);
        assert!(err.ends_with(health.error.as_deref().unwrap()), "{err}");
    }
}
