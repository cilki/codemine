//! Who the bot is on each forge, resolved from the forge's own "who am I"
//! endpoint instead of configured by hand: the account a token authenticates
//! as already knows its own login and email, so turns on a forge commit as
//! that account.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use crate::config::{Forge, ForgeKind};
use crate::precheck::{api_json, gitea_json};

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
/// its whoami endpoint, through the cache, returning who the bot is on each
/// forge for the UI. Fails on the first forge that can't answer, naming it:
/// a turn there would commit as nobody, so the caller holds turns like any
/// other unrunnable configuration.
pub fn resolve(
    forges: &mut [Forge],
) -> Result<BTreeMap<String, Identity>, (ForgeKind, anyhow::Error)> {
    let mut resolved = BTreeMap::new();
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
        resolved.insert(forge.kind.name().to_owned(), identity);
    }
    Ok(resolved)
}

fn whoami(forge: &Forge) -> Result<Identity> {
    let value = match forge.kind {
        ForgeKind::Gitea => gitea_json(forge, "user")?,
        ForgeKind::Github => api_json(forge, "gh", "user")?,
        ForgeKind::Gitlab => api_json(forge, "glab", "user")?,
    };
    parse_identity(forge.kind, &value)
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
