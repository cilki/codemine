//! Forge activity watching: a background thread polls each configured
//! forge's account-wide feed and queues repositories with fresh activity, so
//! a comment on a PR or issue gets a feedback turn ahead of the random draw
//! — immediately when the runner is between turns (the sleep cuts itself
//! short), or as the very next turn when one is already running.
//!
//! Polling rather than webhooks on purpose: webhooks would need the runner
//! to be reachable from every forge and a hook registered on every
//! repository, while the feeds are readable with the tokens the runner
//! already holds. The feeds are the same signals the feedback precheck
//! reads, so an event can never queue work the turn wouldn't have found.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::Result;
use tracing::{info, warn};

use crate::config::{Forge, ForgeKind};
use crate::settings::SharedSettings;

/// How often each forge's feed is polled. GitHub asks notification pollers
/// for at most one request a minute, and the others are in no hurry either.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Repositories with fresh forge activity, queued to jump the next draw.
/// The watcher pushes, the turn runner pops, and the between-turn sleep
/// checks `is_empty` so activity ends it early.
pub struct Pending {
    inner: Mutex<VecDeque<(ForgeKind, String)>>,
}

pub type SharedPending = Arc<Pending>;

impl Pending {
    pub fn new() -> SharedPending {
        Arc::new(Pending {
            inner: Mutex::new(VecDeque::new()),
        })
    }

    /// Queue a repository, once: a second comment on a repository already
    /// waiting changes nothing.
    pub fn push(&self, kind: ForgeKind, repo: String) {
        let mut queue = self.lock();
        if !queue.iter().any(|(k, r)| *k == kind && *r == repo) {
            queue.push_back((kind, repo));
        }
    }

    pub fn pop(&self) -> Option<(ForgeKind, String)> {
        self.lock().pop_front()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<(ForgeKind, String)>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Start the watcher thread. It reads the settings fresh every poll, so
/// forges come and go with them; a forge that stops being configured simply
/// stops being polled.
pub fn spawn(store: SharedSettings, pending: SharedPending) -> Result<()> {
    std::thread::Builder::new()
        .name("events".into())
        .spawn(move || watch(store, pending))?;
    Ok(())
}

fn watch(store: SharedSettings, pending: SharedPending) -> ! {
    // What the previous poll saw, so only movement queues work; and which
    // forges are currently failing to poll, so a persistent failure logs
    // once instead of every minute.
    let mut seen: BTreeMap<(ForgeKind, String), String> = BTreeMap::new();
    let mut failing: BTreeSet<ForgeKind> = BTreeSet::new();
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let (settings, _) = store.snapshot();
        // Activity only ever leads to feedback turns; without the task in
        // the pool there is nothing to queue.
        if !settings.tasks.iter().any(|task| task == "feedback") {
            continue;
        }
        let mut current: BTreeMap<(ForgeKind, String), String> = BTreeMap::new();
        for kind in [ForgeKind::Gitea, ForgeKind::Github, ForgeKind::Gitlab] {
            let Some(forge) = settings.runtime_forge(kind) else {
                continue;
            };
            match unread(&forge) {
                Ok(repos) => {
                    if failing.remove(&kind) {
                        info!("polling {} activity works again", kind.name());
                    }
                    current.extend(
                        repos
                            .into_iter()
                            .filter(|(repo, _)| forge.enabled_repos.contains(repo))
                            .map(|(repo, stamp)| ((kind, repo), stamp)),
                    );
                }
                Err(err) => {
                    if failing.insert(kind) {
                        warn!("failed to poll {} activity: {err:#}", kind.name());
                    }
                    // Carry the last poll forward, so old notifications
                    // don't read as fresh when the forge comes back.
                    current.extend(
                        seen.iter()
                            .filter(|((k, _), _)| *k == kind)
                            .map(|(key, stamp)| (key.clone(), stamp.clone())),
                    );
                }
            }
        }
        for (kind, repo) in fresh(&seen, &current) {
            info!(
                "activity on {} {repo}; queueing a feedback turn",
                kind.name()
            );
            pending.push(kind, repo);
        }
        seen = current;
    }
}

/// The feed entries that are news since the last poll: repositories not seen
/// before, and seen ones whose newest stamp moved forward. A stamp moving
/// backward is a notification being read or cleared, not news.
fn fresh(
    seen: &BTreeMap<(ForgeKind, String), String>,
    current: &BTreeMap<(ForgeKind, String), String>,
) -> Vec<(ForgeKind, String)> {
    current
        .iter()
        .filter(|(key, stamp)| seen.get(*key).is_none_or(|old| old < *stamp))
        .map(|(key, _)| key.clone())
        .collect()
}

/// The repositories with unread activity on one forge, each with the newest
/// stamp on its feed. One request per forge — every feed is account-scoped —
/// and the same signals the feedback precheck reads: notification threads on
/// Gitea and GitHub, todos on GitLab (which fire on mentions and assignments
/// rather than every comment; that is all GitLab offers across projects).
///
/// Shared with the skip memory, which remembers a `feedback` skip against the
/// stamp it answered for and so has to read the feed the same way the watcher
/// does.
pub fn unread(forge: &Forge) -> Result<BTreeMap<String, String>> {
    let feed = match forge.kind {
        ForgeKind::Gitea => crate::precheck::gitea_json(forge, "notifications")?,
        ForgeKind::Github => crate::precheck::api_json(forge, "gh", "notifications")?,
        ForgeKind::Gitlab => crate::precheck::api_json(forge, "glab", "todos")?,
    };
    Ok(match forge.kind {
        ForgeKind::Gitea | ForgeKind::Github => {
            digest(&feed, &["repository", "full_name"], "updated_at")
        }
        ForgeKind::Gitlab => digest(&feed, &["project", "path_with_namespace"], "created_at"),
    })
}

/// Fold a feed into repository → newest stamp. The stamps are RFC 3339
/// strings from one server and only ever compared to each other, so string
/// order is time order; they are never parsed.
fn digest(
    feed: &serde_json::Value,
    repo_path: &[&str; 2],
    stamp_key: &str,
) -> BTreeMap<String, String> {
    let mut repos = BTreeMap::new();
    for item in feed.as_array().map(Vec::as_slice).unwrap_or_default() {
        let Some(repo) = item[repo_path[0]][repo_path[1]].as_str() else {
            continue;
        };
        let stamp = item[stamp_key].as_str().unwrap_or_default();
        let newest = repos.entry(repo.to_owned()).or_insert_with(String::new);
        if stamp > newest.as_str() {
            *newest = stamp.to_owned();
        }
    }
    repos
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pending_dedups_and_pops_in_order() {
        let pending = Pending::new();
        assert!(pending.is_empty());
        pending.push(ForgeKind::Github, "me/a".into());
        pending.push(ForgeKind::Gitea, "me/a".into());
        pending.push(ForgeKind::Github, "me/a".into());
        assert!(!pending.is_empty());
        assert_eq!(pending.pop(), Some((ForgeKind::Github, "me/a".into())));
        assert_eq!(pending.pop(), Some((ForgeKind::Gitea, "me/a".into())));
        assert_eq!(pending.pop(), None);
    }

    #[test]
    fn digest_takes_the_newest_stamp_per_repo() {
        let feed = json!([
            {"repository": {"full_name": "me/a"}, "updated_at": "2026-09-27T10:00:00Z"},
            {"repository": {"full_name": "me/a"}, "updated_at": "2026-09-27T12:00:00Z"},
            {"repository": {"full_name": "me/b"}, "updated_at": "2026-09-26T00:00:00Z"},
            {"unrelated": true},
        ]);
        let repos = digest(&feed, &["repository", "full_name"], "updated_at");
        assert_eq!(repos.len(), 2);
        assert_eq!(repos["me/a"], "2026-09-27T12:00:00Z");
        assert_eq!(repos["me/b"], "2026-09-26T00:00:00Z");

        let todos = json!([{"project": {"path_with_namespace": "me/c"}, "created_at": "2026-09-27T09:00:00Z"}]);
        let repos = digest(&todos, &["project", "path_with_namespace"], "created_at");
        assert_eq!(repos["me/c"], "2026-09-27T09:00:00Z");

        // An error payload instead of an array digests to nothing.
        let bad = json!({"message": "bad credentials"});
        assert!(digest(&bad, &["repository", "full_name"], "updated_at").is_empty());
    }

    #[test]
    fn fresh_flags_new_repos_and_advanced_stamps() {
        let kind = ForgeKind::Github;
        let entry = |repo: &str, stamp: &str| ((kind, repo.to_owned()), stamp.to_owned());
        let seen = BTreeMap::from([entry("me/a", "2"), entry("me/b", "5")]);
        let current = BTreeMap::from([
            entry("me/a", "3"), // advanced: a new notification
            entry("me/b", "4"), // moved back: one was read, nothing new
            entry("me/c", "1"), // not seen before
        ]);
        assert_eq!(
            fresh(&seen, &current),
            [(kind, "me/a".to_owned()), (kind, "me/c".to_owned())]
        );
        assert!(fresh(&current, &current).is_empty());
    }
}
