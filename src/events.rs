//! Forge activity watching: a background thread polls the configured
//! forge's account-wide feed and queues repositories with fresh activity, so
//! a comment on a PR or issue gets a respond turn, and a review request a
//! review turn, ahead of the random draw — immediately when the runner is
//! between turns (the sleep cuts itself short), or as the very next turn
//! when one is already running.
//!
//! Polling rather than webhooks on purpose: webhooks would need the runner
//! to be reachable from the forge and a hook registered on every
//! repository, while the feed is readable with the token the runner
//! already holds. The feed is the same signal the respond precheck reads,
//! and a queued repository is still put through each task's own precheck
//! before it is drawn, so an event can never queue work a turn wouldn't
//! have found.
//!
//! With forge commands on, the same feed is also read for comments that ask
//! the bot for a task by name; see `commands`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::Result;
use tracing::{info, warn};

use crate::config::{Forge, ForgeKind};
use crate::settings::SharedSettings;

/// How often the forge's feed is polled. GitHub asks notification pollers
/// for at most one request a minute, and the others are in no hurry either.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// The tasks fresh activity can lead to, in the order a queued repository is
/// offered to them: a review request is `review`'s, a comment on the bot's
/// own PR or an assignment is `respond`'s. Both arrive
/// on the same feed, so the watcher can't tell which until the tasks' own
/// prechecks do — and `review` asks first because its precheck is exact
/// (the request set), where `respond`'s is satisfied by any unread thread
/// and would claim a review request only to skip on it.
pub const QUEUED_TASKS: [&str; 2] = ["review", "respond"];

/// Work queued to jump the next draw: repositories with fresh forge
/// activity, and the (task, repository) pairs the user asked to run now.
/// The watcher and the web UI push, the turn runner pops, and the
/// between-turn sleep checks `is_empty` so either ends it early. Each entry
/// names its forge: the settings can switch forge between the push and the
/// pop, and a repository queued under the old one must not be drawn on the
/// new one just because the name is the same.
pub struct Pending {
    inner: Mutex<Queues>,
}

#[derive(Default)]
struct Queues {
    activity: VecDeque<(ForgeKind, String)>,
    requested: VecDeque<Request>,
}

/// A turn the user asked for by name: this task on this repository, next.
/// The task is the slug of one the runner defines, which whoever queues the
/// request has looked up; the pool is fixed once read, so it stays defined.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Request {
    #[serde(skip)]
    pub kind: ForgeKind,
    pub repo: String,
    pub task: &'static str,
    /// What the user wants the turn to look at, when the request came from a
    /// comment on the forge rather than from the web UI. Part of the
    /// request's identity, so two different asks for the same pair both run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<Focus>,
}

/// The comment a forge command came from: who wrote it, where, and what it
/// asked the turn to look at.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Focus {
    pub author: String,
    pub url: String,
    pub text: String,
}

pub type SharedPending = Arc<Pending>;

impl Pending {
    pub fn new() -> SharedPending {
        Arc::new(Pending {
            inner: Mutex::new(Queues::default()),
        })
    }

    /// Queue a repository, once: a second comment on a repository already
    /// waiting changes nothing.
    pub fn push(&self, kind: ForgeKind, repo: String) {
        let queue = &mut self.lock().activity;
        if !queue.iter().any(|(k, r)| *k == kind && *r == repo) {
            queue.push_back((kind, repo));
        }
    }

    pub fn pop(&self) -> Option<(ForgeKind, String)> {
        self.lock().activity.pop_front()
    }

    /// Queue a run-now request, once: asking twice for the same pair while
    /// it waits changes nothing.
    pub fn request(&self, request: Request) {
        let queue = &mut self.lock().requested;
        if !queue.contains(&request) {
            queue.push_back(request);
        }
    }

    pub fn pop_request(&self) -> Option<Request> {
        self.lock().requested.pop_front()
    }

    /// The run-now requests still waiting, oldest first, for the page.
    pub fn requests(&self) -> Vec<Request> {
        self.lock().requested.iter().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        let queues = self.lock();
        queues.activity.is_empty() && queues.requested.is_empty()
    }

    fn lock(&self) -> MutexGuard<'_, Queues> {
        crate::sync::lock(&self.inner)
    }
}

/// Start the watcher thread. It reads the settings fresh every poll, so the
/// forge comes and goes with them; a forge that stops being configured
/// simply stops being polled.
pub fn spawn(store: SharedSettings, pending: SharedPending) -> Result<()> {
    std::thread::Builder::new()
        .name("events".into())
        .spawn(move || watch(store, pending))?;
    Ok(())
}

fn watch(store: SharedSettings, pending: SharedPending) -> ! {
    // What the previous poll saw, so only movement queues work; and whether
    // the poll is currently failing, so a persistent failure logs once
    // instead of every minute.
    let mut seen: BTreeMap<(ForgeKind, String), String> = BTreeMap::new();
    // The same, per notification thread, for forge commands.
    let mut threads: BTreeMap<String, String> = BTreeMap::new();
    let mut failing = false;
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let (settings, _) = store.snapshot();
        let Some(mut forge) = settings.runtime_forge() else {
            continue;
        };
        // Activity only ever leads to the queued tasks' turns; with none of
        // them on for any repository there is nothing to queue. The turn
        // checks the repository's own picks again when it pops the entry.
        let queue_activity = QUEUED_TASKS.iter().any(|task| forge.runs_anywhere(task));
        if !queue_activity && !settings.forge_commands {
            continue;
        }
        let kind = forge.kind;
        let mut feed = match feed(&forge) {
            Ok(feed) => {
                if std::mem::replace(&mut failing, false) {
                    info!("polling {} activity works again", kind.name());
                }
                feed
            }
            Err(err) => {
                if !std::mem::replace(&mut failing, true) {
                    warn!("failed to poll {} activity: {err:#}", kind.name());
                }
                // Keep the last poll, so old notifications don't read as
                // fresh when the forge comes back.
                continue;
            }
        };
        // Commands first: a thread that carried one is cleared and taken off
        // the feed, so it doesn't also queue its repository for feedback.
        if settings.forge_commands {
            crate::commands::take(&mut forge, &mut feed, &mut threads, &pending);
        }
        if !queue_activity {
            continue;
        }
        let current: BTreeMap<(ForgeKind, String), String> = digest(kind, &feed)
            .into_iter()
            .filter(|(repo, _)| forge.repos.contains_key(repo))
            .map(|(repo, stamp)| ((kind, repo), stamp))
            .collect();
        for (kind, repo) in fresh(&seen, &current) {
            info!(
                "activity on {} {repo}; queueing it for the next draw",
                kind.name()
            );
            pending.push(kind, repo);
        }
        seen = current;
    }
}

/// The feed entries that are news since the last poll: repositories (or, for
/// `commands`, threads) not seen before, and seen ones whose newest stamp
/// moved forward. A stamp moving backward is a notification being read or
/// cleared, not news.
pub fn fresh<K: Ord + Clone>(seen: &BTreeMap<K, String>, current: &BTreeMap<K, String>) -> Vec<K> {
    current
        .iter()
        .filter(|(key, stamp)| seen.get(*key).is_none_or(|old| old < *stamp))
        .map(|(key, _)| key.clone())
        .collect()
}

/// The repositories with unread activity on the forge, each with the newest
/// stamp on its feed. One request — every feed is account-scoped — over
/// notification threads on Gitea and GitHub, todos on GitLab (which fire
/// on mentions and assignments rather than every comment; that is all GitLab
/// offers across projects).
///
/// The single reading of "this repository has feedback waiting" in the
/// runner: the watcher polls it for repositories to queue, and `cache::Probe`
/// both gates `respond` on it and remembers a skip against the stamp it
/// answered for. A review request lands on the same feed, which is how
/// `review` gets queued too; its own precheck then confirms the request.
pub fn unread(forge: &Forge) -> Result<BTreeMap<String, String>> {
    Ok(digest(forge.kind, &feed(forge)?))
}

/// The forge's unread feed as it answers it: notification threads on Gitea
/// and GitHub, pending todos on GitLab.
fn feed(forge: &Forge) -> Result<serde_json::Value> {
    crate::precheck::forge_json(forge, "notifications", "notifications", "todos")
}

/// Fold a feed into repository → newest stamp. The stamps are RFC 3339
/// strings from one server and only ever compared to each other, so string
/// order is time order; they are never parsed.
fn digest(kind: ForgeKind, feed: &serde_json::Value) -> BTreeMap<String, String> {
    let mut repos = BTreeMap::new();
    for item in feed.as_array().map(Vec::as_slice).unwrap_or_default() {
        let Some(repo) = repo_of(kind, item) else {
            continue;
        };
        let stamp = stamp(kind, item);
        let newest = repos.entry(repo.to_owned()).or_insert_with(String::new);
        if stamp > newest.as_str() {
            *newest = stamp.to_owned();
        }
    }
    repos
}

/// A feed entry's repository, under its forge's spelling.
pub fn repo_of(kind: ForgeKind, entry: &serde_json::Value) -> Option<&str> {
    match kind {
        ForgeKind::Gitlab => entry["project"]["path_with_namespace"].as_str(),
        _ => entry["repository"]["full_name"].as_str(),
    }
}

/// A feed entry's stamp, under its forge's spelling.
pub fn stamp(kind: ForgeKind, entry: &serde_json::Value) -> &str {
    let key = match kind {
        ForgeKind::Gitlab => "created_at",
        _ => "updated_at",
    };
    entry[key].as_str().unwrap_or_default()
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

    /// Requests queue apart from activity, deduplicated per pair, and wake
    /// the sleep the same way.
    #[test]
    fn requests_dedup_and_count_as_pending() {
        let pending = Pending::new();
        let request = |task: &'static str| Request {
            kind: ForgeKind::Github,
            repo: "me/a".into(),
            task,
            focus: None,
        };
        pending.request(request("docs"));
        pending.request(request("docs"));
        pending.request(request("bump"));
        assert!(!pending.is_empty());
        assert_eq!(pending.pop(), None);
        assert_eq!(pending.requests(), [request("docs"), request("bump")]);
        assert_eq!(pending.pop_request(), Some(request("docs")));
        assert_eq!(pending.pop_request(), Some(request("bump")));
        assert!(pending.is_empty());

        // Two different comments asking for the same pair both run.
        let asked = |url: &str| Request {
            focus: Some(Focus {
                author: "me".into(),
                url: url.into(),
                text: String::new(),
            }),
            ..request("docs")
        };
        pending.request(asked("#1"));
        pending.request(asked("#2"));
        pending.request(asked("#1"));
        assert_eq!(pending.requests(), [asked("#1"), asked("#2")]);
    }

    #[test]
    fn digest_takes_the_newest_stamp_per_repo() {
        let feed = json!([
            {"repository": {"full_name": "me/a"}, "updated_at": "2026-09-27T10:00:00Z"},
            {"repository": {"full_name": "me/a"}, "updated_at": "2026-09-27T12:00:00Z"},
            {"repository": {"full_name": "me/b"}, "updated_at": "2026-09-26T00:00:00Z"},
            {"unrelated": true},
        ]);
        let repos = digest(ForgeKind::Gitea, &feed);
        assert_eq!(repos.len(), 2);
        assert_eq!(repos["me/a"], "2026-09-27T12:00:00Z");
        assert_eq!(repos["me/b"], "2026-09-26T00:00:00Z");

        let todos = json!([{"project": {"path_with_namespace": "me/c"}, "created_at": "2026-09-27T09:00:00Z"}]);
        let repos = digest(ForgeKind::Gitlab, &todos);
        assert_eq!(repos["me/c"], "2026-09-27T09:00:00Z");

        // An error payload instead of an array digests to nothing.
        let bad = json!({"message": "bad credentials"});
        assert!(digest(ForgeKind::Github, &bad).is_empty());
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
