//! Skip memory for the tasks whose answer is a function of something the
//! runner can observe for itself. `docs`, `simplify`, `benchmark`,
//! `coverage`, and `mutation` all read the code and nothing else, so a turn
//! that found nothing to do will find nothing again until the code moves.
//! `feedback` reads the forge's notification feed, so its answer holds until
//! a thread newer than the ones it already read arrives. `rebase` reads the
//! forge's view of the bot's own open PRs, so its answer holds until one of
//! those branches or the conflict set moves. Drawing any of them meanwhile
//! buys a session's worth of tokens and another `TASK SKIPPED`.
//! Each skip is remembered against that state and the task's own
//! instructions, and the task isn't drawn for that repository again until one
//! of the two changes. The memory is persisted next to the rest of the
//! workspace, since the clones outlive the process too.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::config::{Forge, ForgeKind};

/// What a skipped turn on this task is remembered against, or `None` when the
/// skip isn't worth remembering at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Basis {
    /// The repository's default-branch commit: the task reads the tree and
    /// nothing else, so its answer holds until the code moves.
    Head,
    /// The newest unread thread on the repository's notification feed. This
    /// is what `feedback` answers out of, and it needs remembering as much as
    /// the tree does: the feed is also the task's precondition probe, so one
    /// stale thread nobody marks read keeps the probe true forever. Gated
    /// tasks are probed first and the first with work wins the draw, so that
    /// is not one wasted turn but every turn until the thread is cleared.
    Feed,
    /// The bot's own open conflicting PRs, as `rebase` sees them. Like the
    /// feed, the set is both the task's precondition and its memory: a
    /// conflict the agent declined to resolve (or a draft PR Gitea reports
    /// as unmergeable) keeps the probe true forever, and without memory
    /// `rebase` — probed first, like every gated task — wins every draw
    /// until the branch moves.
    Prs,
}

/// The state this task's answer depends on. `None` for the tasks that can
/// become actionable with nothing observable moving — `bump` watches the
/// outside world, `feature` invents its own work — where a remembered skip
/// would outlast the reason for it.
pub fn basis(task: &str) -> Option<Basis> {
    match task {
        "docs" | "simplify" | "benchmark" | "coverage" | "mutation" => Some(Basis::Head),
        "feedback" => Some(Basis::Feed),
        "rebase" => Some(Basis::Prs),
        _ => None,
    }
}

/// The state a skip was remembered against. Both halves have to still match
/// for the skip to stand.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
struct Mark {
    /// The task's `Basis` as the agent saw it: a commit for `Basis::Head`, a
    /// notification stamp for `Basis::Feed`.
    state: String,
    /// Digest of the task's instructions, so a reworded task section gets
    /// drawn again instead of staying gated on the old wording.
    prompt: String,
}

/// The remembered skips, keyed by forge, repository, and task.
pub struct Cache {
    path: PathBuf,
    marks: Mutex<BTreeMap<String, Mark>>,
}

impl Cache {
    /// Load the remembered skips from `path`. Unlike the settings, a lost
    /// cache costs only a redundant turn, so an unreadable or malformed file
    /// is logged and dropped rather than failing startup.
    pub fn load(path: PathBuf) -> Cache {
        let marks = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                warn!("ignoring {}: {err}", path.display());
                BTreeMap::new()
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(err) => {
                warn!("failed to read {}: {err}", path.display());
                BTreeMap::new()
            }
        };
        Cache {
            path,
            marks: Mutex::new(marks),
        }
    }

    /// Remember that the task found nothing to do on the repository in
    /// `state`, so it isn't drawn again until that state or the task moves.
    pub fn remember(&self, task: &str, forge: ForgeKind, repo: &str, state: &str) {
        let mark = Mark {
            state: state.to_owned(),
            prompt: digest(task),
        };
        let mut marks = self.lock();
        if marks.insert(key(task, forge, repo), mark.clone()) == Some(mark) {
            return;
        }
        self.save(&marks);
    }

    /// Drop whatever was remembered for the task on this repository: the turn
    /// either changed something or never got to answer, so the old mark no
    /// longer describes it.
    pub fn forget(&self, task: &str, forge: ForgeKind, repo: &str) {
        let mut marks = self.lock();
        if marks.remove(&key(task, forge, repo)).is_some() {
            self.save(&marks);
        }
    }

    fn marked(&self, key: &str) -> Option<Mark> {
        self.lock().get(key).cloned()
    }

    /// Write the memory back out. A failure costs a redundant turn after the
    /// next restart and nothing else, so it is logged rather than propagated.
    fn save(&self, marks: &BTreeMap<String, Mark>) {
        if let Err(err) = persist(&self.path, marks) {
            warn!("failed to save the skip cache: {err:#}");
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Mark>> {
        self.marks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Atomic write, like the settings store: the temp file is created next to
/// the target so the rename can't cross filesystems.
fn persist(path: &Path, marks: &BTreeMap<String, Mark>) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec_pretty(marks)?)?;
    file.write_all(b"\n")?;
    file.persist(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// One task's entry for one repository. None of the three parts can contain a
/// colon — forge and task are fixed slugs, and forge paths are
/// `<owner>/<repo>` — so a flat key stays unambiguous and readable in the
/// file.
fn key(task: &str, forge: ForgeKind, repo: &str) -> String {
    format!("{}:{repo}:{task}", forge.name())
}

/// A short digest of the task's instructions as this build spells them, so
/// the skips remembered under one wording don't gate the next one.
fn digest(task: &str) -> String {
    let instructions = crate::prompts::tasks()
        .into_iter()
        .find(|candidate| candidate.slug == task)
        .map(|candidate| candidate.description)
        .unwrap_or_default();
    Sha256::digest(instructions.as_bytes())[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// One repository's conflicting-PR keys, as `precheck::conflicting_prs`
/// spells them.
type Conflicts = Vec<String>;

/// The draw's precondition check with the skip memory folded in, plus the
/// forge state read along the way: default-branch commits per repository,
/// notification feeds per forge, conflicting PRs per repository. Each is
/// fetched at most once and answers for every candidate the rest of the draw
/// asks about.
pub struct Probe<'a> {
    cache: &'a Cache,
    heads: RefCell<BTreeMap<(ForgeKind, String), Option<String>>>,
    feeds: RefCell<BTreeMap<ForgeKind, Option<BTreeMap<String, String>>>>,
    conflicts: RefCell<BTreeMap<(ForgeKind, String), Option<Conflicts>>>,
}

impl<'a> Probe<'a> {
    pub fn new(cache: &'a Cache) -> Probe<'a> {
        Probe {
            cache,
            heads: RefCell::new(BTreeMap::new()),
            feeds: RefCell::new(BTreeMap::new()),
            conflicts: RefCell::new(BTreeMap::new()),
        }
    }

    /// Whether the task is worth a turn on this repository: it must have
    /// something to act on, and must not already have come up empty at
    /// exactly this state. The memory is checked first, so a task held back
    /// by it never reaches the precondition probe.
    pub fn actionable(&self, task: &str, forge: &Forge, repo: &str) -> bool {
        if self.remembered(task, forge, repo) {
            return false;
        }
        match basis(task) {
            // A task that answers out of the feed has nothing to answer when
            // the repository has nothing unread, so its basis doubles as its
            // precondition. The feed is read once per forge for the memory's
            // sake anyway, so probing `feedback` across the whole pool costs
            // the one request it already made rather than one per repository.
            // Fail-open: an unreadable feed draws the task and at worst
            // skips again.
            Some(Basis::Feed) => self.feed(forge).is_none_or(|feed| feed.contains_key(repo)),
            // `rebase` has work only when the bot has an open conflicting PR
            // here. Fail-closed, unlike the feed: an unreadable listing
            // leaves nothing to remember a skip against, so drawing on it
            // doesn't risk one wasted turn but one per draw until the probe
            // heals — the task is held back instead, logged by the getter.
            Some(Basis::Prs) => self
                .conflicting(forge, repo)
                .is_some_and(|prs| !prs.is_empty()),
            // Every other task is always worth a turn; only the memory above
            // holds one back.
            _ => true,
        }
    }

    /// The state the task's answer currently depends on, for remembering a
    /// skip against. `None` when the task keeps no memory, or when the state
    /// couldn't be read.
    pub fn state(&self, task: &str, forge: &Forge, repo: &str) -> Option<String> {
        match basis(task)? {
            Basis::Head => self.head(forge, repo),
            Basis::Feed => self.stamp(forge, repo),
            Basis::Prs => self.conflicting(forge, repo).map(|prs| prs.join(" ")),
        }
    }

    /// Whether a remembered skip still describes the repository.
    fn remembered(&self, task: &str, forge: &Forge, repo: &str) -> bool {
        let Some(basis) = basis(task) else {
            return false;
        };
        let Some(mark) = self.cache.marked(&key(task, forge.kind, repo)) else {
            return false;
        };
        // Checked before the state is read, so a reworded task costs no
        // round trip on its way to being drawn again.
        if mark.prompt != digest(task) {
            return false;
        }
        // Fail-open: with no state to compare against, the mark can't be
        // said to still stand, so it doesn't hold the task back. (`rebase`
        // is held back regardless — its precondition fails closed on the
        // same unreadable listing.)
        let Some(state) = self.state(task, forge, repo) else {
            return false;
        };
        match basis {
            // One commit is neither newer nor older than another here: any
            // move off the remembered one is news. The same goes for the
            // conflict set: a force-push, a close, a conflict appearing or
            // resolving under base movement all change the string.
            Basis::Head | Basis::Prs => state == mark.state,
            // Stamps from one server, only ever compared to each other, so
            // string order is time order — the same test `events::fresh`
            // makes. A stamp moving backward is a thread being read or
            // cleared rather than news, so the skip still stands.
            Basis::Feed => state <= mark.state,
        }
    }

    /// The repository's default-branch commit, read from the forge once per
    /// repository and remembered for the rest of the draw.
    fn head(&self, forge: &Forge, repo: &str) -> Option<String> {
        let id = (forge.kind, repo.to_owned());
        if let Some(head) = self.heads.borrow().get(&id) {
            return head.clone();
        }
        let head = crate::precheck::head_sha(forge, repo)
            .inspect_err(|err| warn!("failed to read {repo}'s head commit: {err:#}"))
            .ok();
        self.heads.borrow_mut().insert(id, head.clone());
        head
    }

    /// The newest unread notification on the repository's feed. `None` for a
    /// repository with nothing unread on it, or a feed that couldn't be read.
    fn stamp(&self, forge: &Forge, repo: &str) -> Option<String> {
        self.feed(forge)?.get(repo).cloned()
    }

    /// The bot's open conflicting PRs on the repository, read from the forge
    /// once per repository and remembered for the rest of the draw. `None`
    /// when the listing couldn't be read.
    fn conflicting(&self, forge: &Forge, repo: &str) -> Option<Vec<String>> {
        let id = (forge.kind, repo.to_owned());
        if let Some(prs) = self.conflicts.borrow().get(&id) {
            return prs.clone();
        }
        let prs = crate::precheck::conflicting_prs(forge, repo)
            .inspect_err(|err| warn!("failed to read {repo}'s conflicting PRs: {err:#}"))
            .ok();
        self.conflicts.borrow_mut().insert(id, prs.clone());
        prs
    }

    /// The forge's unread notifications as repository → newest stamp. The
    /// feed is account-wide, so one request per forge answers for every
    /// repository in the draw. `None` when it couldn't be read.
    fn feed(&self, forge: &Forge) -> Option<BTreeMap<String, String>> {
        if let Some(feed) = self.feeds.borrow().get(&forge.kind) {
            return feed.clone();
        }
        let feed = crate::events::unread(forge)
            .inspect_err(|err| {
                warn!(
                    "failed to read {}'s activity feed: {err:#}",
                    forge.kind.name()
                )
            })
            .ok();
        self.feeds.borrow_mut().insert(forge.kind, feed.clone());
        feed
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn cache(root: &tempfile::TempDir) -> Cache {
        Cache::load(root.path().join("skips.json"))
    }

    fn forge(kind: ForgeKind) -> Forge {
        Forge {
            kind,
            token: "tok".into(),
            user: String::new(),
            login: String::new(),
            email: String::new(),
            url: "https://github.com".into(),
            enabled_repos: BTreeSet::new(),
        }
    }

    /// A probe with `me/repo`'s head already answered, so the tests never
    /// reach for a forge.
    fn probe<'a>(cache: &'a Cache, head: &str) -> Probe<'a> {
        let probe = Probe::new(cache);
        probe
            .heads
            .borrow_mut()
            .insert((ForgeKind::Github, "me/repo".into()), Some(head.into()));
        probe
    }

    /// The same for the notification feed, which the probe reads once per
    /// forge rather than per repository.
    fn feed_probe<'a>(cache: &'a Cache, feed: &[(&str, &str)]) -> Probe<'a> {
        let probe = Probe::new(cache);
        probe.feeds.borrow_mut().insert(
            ForgeKind::Github,
            Some(
                feed.iter()
                    .map(|(repo, stamp)| ((*repo).to_owned(), (*stamp).to_owned()))
                    .collect(),
            ),
        );
        probe
    }

    /// And for the conflicting-PR listing `rebase` is probed out of; `None`
    /// is a listing that couldn't be read.
    fn conflict_probe<'a>(cache: &'a Cache, prs: Option<&[&str]>) -> Probe<'a> {
        let probe = Probe::new(cache);
        probe.conflicts.borrow_mut().insert(
            (ForgeKind::Github, "me/repo".into()),
            prs.map(|prs| prs.iter().map(|pr| (*pr).to_owned()).collect()),
        );
        probe
    }

    /// Each task is remembered against whatever it reads: the tree-only ones
    /// against the tree, `feedback` against the notification feed, `rebase`
    /// against the bot's conflicting PRs. The tasks that can become
    /// actionable with nothing observable moving keep no memory at all.
    #[test]
    fn basis_matches_what_each_task_reads() {
        for task in ["docs", "simplify", "benchmark", "coverage", "mutation"] {
            assert_eq!(basis(task), Some(Basis::Head), "{task}");
        }
        assert_eq!(basis("feedback"), Some(Basis::Feed));
        assert_eq!(basis("rebase"), Some(Basis::Prs));
        for task in ["bump", "todo", "roleplay", "audit", "feature"] {
            assert_eq!(basis(task), None, "{task}");
        }
    }

    /// Every remembered slug still names a task the sweep command defines, so
    /// a renamed section can't quietly stop being remembered.
    #[test]
    fn remembered_tasks_are_real_tasks() {
        let slugs = crate::prompts::default_tasks();
        for task in [
            "docs",
            "simplify",
            "benchmark",
            "coverage",
            "mutation",
            "feedback",
            "rebase",
        ] {
            assert!(slugs.iter().any(|slug| slug == task), "{task} is gone");
        }
    }

    /// A `feedback` skip is keyed on the feed, not the tree: it stands while
    /// the feed sits still, and a thread newer than the one it answered for
    /// draws the task again. Without this one stale notification keeps the
    /// precheck true and `feedback` — probed first, so first to win the draw
    /// — is drawn every single turn until somebody marks it read.
    #[test]
    fn a_feedback_skip_stands_until_a_newer_thread_arrives() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        let github = forge(ForgeKind::Github);
        cache.remember(
            "feedback",
            ForgeKind::Github,
            "me/repo",
            "2026-10-01T02:00:00Z",
        );

        let unchanged = [("me/repo", "2026-10-01T02:00:00Z")];
        assert!(feed_probe(&cache, &unchanged).remembered("feedback", &github, "me/repo"));
        let newer = [("me/repo", "2026-10-01T03:00:00Z")];
        assert!(!feed_probe(&cache, &newer).remembered("feedback", &github, "me/repo"));
    }

    /// A stamp moving backward is a thread being read or cleared rather than
    /// news, so the skip survives it — the same reading `events::fresh` takes.
    #[test]
    fn a_cleared_thread_does_not_redraw_feedback() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember(
            "feedback",
            ForgeKind::Github,
            "me/repo",
            "2026-10-01T02:00:00Z",
        );
        let older = [("me/repo", "2026-10-01T01:00:00Z")];
        assert!(feed_probe(&cache, &older).remembered(
            "feedback",
            &forge(ForgeKind::Github),
            "me/repo"
        ));
    }

    /// A feed with nothing on the repository fails open like an unreadable
    /// head. It costs no turn either way: the precheck reads the same empty
    /// feed and says there is nothing to do.
    #[test]
    fn an_empty_feed_draws_feedback() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember(
            "feedback",
            ForgeKind::Github,
            "me/repo",
            "2026-10-01T02:00:00Z",
        );
        let elsewhere = [("me/other", "2026-10-01T03:00:00Z")];
        assert!(!feed_probe(&cache, &elsewhere).remembered(
            "feedback",
            &forge(ForgeKind::Github),
            "me/repo"
        ));
        assert!(!feed_probe(&cache, &[]).remembered(
            "feedback",
            &forge(ForgeKind::Github),
            "me/repo"
        ));
    }

    /// `feedback`'s precondition is answered out of the same feed its memory
    /// is keyed on, with no mark involved: a repository with something unread
    /// is drawn, one with nothing on the feed is passed over, and a feed that
    /// couldn't be read fails open.
    #[test]
    fn feedback_is_drawn_only_for_a_repository_with_unread_activity() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        let github = forge(ForgeKind::Github);
        let feed = [("me/repo", "2026-10-01T02:00:00Z")];
        assert!(feed_probe(&cache, &feed).actionable("feedback", &github, "me/repo"));
        assert!(!feed_probe(&cache, &feed).actionable("feedback", &github, "me/other"));
        assert!(!feed_probe(&cache, &[]).actionable("feedback", &github, "me/repo"));

        let unreadable = Probe::new(&cache);
        unreadable
            .feeds
            .borrow_mut()
            .insert(ForgeKind::Github, None);
        assert!(unreadable.actionable("feedback", &github, "me/repo"));
    }

    /// The memory outranks the precondition: a thread `feedback` has already
    /// answered for keeps it out of the draw even though the feed still names
    /// the repository.
    #[test]
    fn a_remembered_skip_beats_the_feed_precondition() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        let stamp = "2026-10-01T02:00:00Z";
        cache.remember("feedback", ForgeKind::Github, "me/repo", stamp);
        let probe = feed_probe(&cache, &[("me/repo", stamp)]);
        assert!(!probe.actionable("feedback", &forge(ForgeKind::Github), "me/repo"));
    }

    /// `rebase` is drawn only when the bot has an open conflicting PR, and —
    /// unlike the feed — an unreadable listing holds it back rather than
    /// drawing it: with nothing to remember a skip against, a probe that
    /// stayed broken used to cost an agent session per draw, forever.
    #[test]
    fn rebase_is_drawn_only_when_an_owned_pr_conflicts() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        let github = forge(ForgeKind::Github);
        assert!(conflict_probe(&cache, Some(&["12@abc"])).actionable("rebase", &github, "me/repo"));
        assert!(!conflict_probe(&cache, Some(&[])).actionable("rebase", &github, "me/repo"));
        assert!(!conflict_probe(&cache, None).actionable("rebase", &github, "me/repo"));
    }

    /// A `rebase` skip is keyed on the conflict set: it stands while the set
    /// sits still, and any move — a force-push, a new conflict, the set
    /// emptying — redraws the task. Without this, one conflict the agent
    /// declines to resolve keeps the precheck true and `rebase` — gated, so
    /// probed ahead of the blind draw — is drawn every single turn until the
    /// branch moves.
    #[test]
    fn a_rebase_skip_stands_until_the_conflict_set_moves() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        let github = forge(ForgeKind::Github);
        cache.remember("rebase", ForgeKind::Github, "me/repo", "12@abc");

        assert!(conflict_probe(&cache, Some(&["12@abc"])).remembered("rebase", &github, "me/repo"));
        // A force-push moves the head.
        assert!(
            !conflict_probe(&cache, Some(&["12@def"])).remembered("rebase", &github, "me/repo")
        );
        // A second conflict appears.
        assert!(
            !conflict_probe(&cache, Some(&["12@abc", "15@eee"]))
                .remembered("rebase", &github, "me/repo")
        );
        // The set empties: the mark no longer stands, and the precondition
        // holds the task back anyway.
        assert!(!conflict_probe(&cache, Some(&[])).remembered("rebase", &github, "me/repo"));
    }

    /// The memory outranks the precondition here too: a conflict `rebase`
    /// has already answered for keeps it out of the draw even though the
    /// forge still reports it.
    #[test]
    fn a_remembered_rebase_skip_beats_the_precondition() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("rebase", ForgeKind::Github, "me/repo", "12@abc");
        let probe = conflict_probe(&cache, Some(&["12@abc"]));
        assert!(!probe.actionable("rebase", &forge(ForgeKind::Github), "me/repo"));
    }

    /// An unreadable listing neither stands a mark up nor draws the task:
    /// `remembered` fails open as usual, and the fail-closed precondition
    /// holds the task back on the same answer.
    #[test]
    fn an_unreadable_pr_listing_holds_rebase_back() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("rebase", ForgeKind::Github, "me/repo", "12@abc");
        let probe = conflict_probe(&cache, None);
        assert!(!probe.remembered("rebase", &forge(ForgeKind::Github), "me/repo"));
        assert!(!probe.actionable("rebase", &forge(ForgeKind::Github), "me/repo"));
    }

    /// A task with no precondition of its own is drawn without any forge
    /// lookup once the memory has let it through.
    #[test]
    fn an_unprobed_task_needs_no_forge_lookup() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        let probe = Probe::new(&cache);
        assert!(probe.actionable("docs", &forge(ForgeKind::Github), "me/repo"));
        assert!(probe.heads.borrow().is_empty());
        assert!(probe.feeds.borrow().is_empty());
        assert!(probe.conflicts.borrow().is_empty());
    }

    #[test]
    fn a_remembered_skip_stands_until_the_head_moves() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("docs", ForgeKind::Github, "me/repo", "abc");
        let github = forge(ForgeKind::Github);
        assert!(probe(&cache, "abc").remembered("docs", &github, "me/repo"));
        assert!(!probe(&cache, "def").remembered("docs", &github, "me/repo"));
    }

    /// Nothing is remembered for another task, another repository, or a forge
    /// that merely shares the path.
    #[test]
    fn marks_are_scoped_to_one_task_and_repository() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("docs", ForgeKind::Github, "me/repo", "abc");

        let probe = probe(&cache, "abc");
        assert!(!probe.remembered("coverage", &forge(ForgeKind::Github), "me/repo"));
        assert!(!probe.remembered("docs", &forge(ForgeKind::Github), "me/other"));
        assert!(!probe.remembered("docs", &forge(ForgeKind::Gitea), "me/repo"));
    }

    #[test]
    fn forgetting_draws_the_task_again() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("simplify", ForgeKind::Github, "me/repo", "abc");
        cache.forget("simplify", ForgeKind::Github, "me/repo");
        assert!(!probe(&cache, "abc").remembered("simplify", &forge(ForgeKind::Github), "me/repo"));
    }

    /// A task with no basis is never held back, whatever the file happens to
    /// hold, and costs no forge lookup either.
    #[test]
    fn unremembered_tasks_are_never_held_back() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("bump", ForgeKind::Github, "me/repo", "abc");
        let probe = Probe::new(&cache);
        assert!(!probe.remembered("bump", &forge(ForgeKind::Github), "me/repo"));
        assert!(probe.heads.borrow().is_empty());
        assert!(probe.feeds.borrow().is_empty());
        assert!(probe.conflicts.borrow().is_empty());
    }

    /// A skip remembered under one wording of the task doesn't gate the next.
    #[test]
    fn reworded_instructions_invalidate_a_skip() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("docs", ForgeKind::Github, "me/repo", "abc");
        let key = key("docs", ForgeKind::Github, "me/repo");
        cache.lock().get_mut(&key).unwrap().prompt = "stale".into();
        assert!(!probe(&cache, "abc").remembered("docs", &forge(ForgeKind::Github), "me/repo"));
    }

    /// An unreadable head fails open: better a redundant turn than a task
    /// gated on a comparison that couldn't be made.
    #[test]
    fn an_unknown_head_draws_the_task() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("docs", ForgeKind::Github, "me/repo", "abc");
        let probe = Probe::new(&cache);
        probe
            .heads
            .borrow_mut()
            .insert((ForgeKind::Github, "me/repo".into()), None);
        assert!(!probe.remembered("docs", &forge(ForgeKind::Github), "me/repo"));
    }

    #[test]
    fn marks_survive_a_restart() {
        let root = tempfile::tempdir().unwrap();
        cache(&root).remember("mutation", ForgeKind::Gitlab, "me/repo", "abc");
        assert_eq!(
            cache(&root).marked(&key("mutation", ForgeKind::Gitlab, "me/repo")),
            Some(Mark {
                state: "abc".into(),
                prompt: digest("mutation"),
            })
        );
    }

    /// A cache file from a future version, or a truncated write, starts the
    /// runner over rather than stopping it.
    #[test]
    fn a_malformed_file_loads_empty() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("skips.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        assert!(Cache::load(path).lock().is_empty());
    }

    /// The digest is stable across calls, so a mark can't invalidate itself,
    /// and specific enough to tell the task sections apart.
    #[test]
    fn digests_are_stable_and_task_specific() {
        assert_eq!(digest("docs"), digest("docs"));
        assert_ne!(digest("docs"), digest("coverage"));
        assert_eq!(digest("docs").len(), 16);
    }
}
