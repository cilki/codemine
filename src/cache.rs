//! Skip memory for the tasks whose answer is a function of something the
//! runner can observe for itself. `docs`, `simplify`, `benchmark`,
//! `coverage`, and `mutation` all read the code and nothing else, so a turn
//! that found nothing to do will find nothing again until the code moves.
//! `feedback` reads the forge's notification feed, so its answer holds until
//! a thread newer than the ones it already read arrives. `rebase` reads the
//! forge's view of the bot's own open PRs, so its answer holds until one of
//! those branches or the conflict set moves. `review` reads the forge's view
//! of the PRs the bot has been asked to review, so its answer holds until a
//! request is made, withdrawn, or the branch under it moves. Drawing any of
//! them meanwhile buys a session's worth of tokens and another
//! `TASK SKIPPED`.
//! Each skip is remembered against that state and the task's own
//! instructions, and the task isn't drawn for that repository again until one
//! of the two changes. The memory is persisted at the workspace root so it
//! outlives the process; it is keyed by commit and forge state, not by the
//! clone, so the fresh clones a restart brings leave it valid.

use std::cell::{OnceCell, RefCell};
use std::collections::BTreeMap;
use std::path::PathBuf;
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
    /// The open PRs the bot is a pending requested reviewer on, as `review`
    /// sees them. Same duality as `Prs`: a request the agent found nothing
    /// to say about and didn't submit on (or couldn't — a draft, a branch
    /// that won't build) would otherwise keep the probe true forever.
    Reviews,
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
        "review" => Some(Basis::Reviews),
        _ => None,
    }
}

/// Whether the task has a precondition probe, which is exactly the tasks
/// whose basis is forge state rather than the tree: an empty feed, an empty
/// conflict set, or an empty request set is both "nothing has moved" and
/// "there is nothing to act on", so the same read answers either question. A
/// `Basis::Head` task reads the tree the agent is handed and can always find
/// something there, so only its memory holds it back. Every other task is
/// always drawable.
pub fn gated(task: &str) -> bool {
    matches!(basis(task), Some(Basis::Feed | Basis::Prs | Basis::Reviews))
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
        if let Err(err) = crate::workspace::write_json(&self.path, marks) {
            warn!("failed to save the skip cache: {err:#}");
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Mark>> {
        self.marks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
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
    let instructions = crate::prompts::task(task).map_or("", |task| &task.description);
    Sha256::digest(instructions.as_bytes())[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// One repository's PR keys, as `precheck::conflicting_prs` and
/// `precheck::review_requests` spell them.
type PrKeys = Vec<String>;

/// One page of a repository's open PRs, as `precheck::open_prs` fetched it.
type OpenPrs = Vec<serde_json::Value>;

/// The draw's precondition check with the skip memory folded in, plus the
/// forge state read along the way: default-branch commits per repository,
/// the forge's notification feed, and the open-PR listing per repository with
/// the conflicting PRs and pending review requests read off it. Each is
/// fetched at most once and answers for every candidate the rest of the draw
/// asks about. A probe lives for one draw, and a draw is over one forge.
pub struct Probe<'a> {
    cache: &'a Cache,
    forge: &'a Forge,
    heads: RefCell<BTreeMap<String, Option<String>>>,
    /// The feed is account-wide, so one read answers for every repository.
    unread: OnceCell<Option<BTreeMap<String, String>>>,
    /// The page both PR answers below are read from.
    listings: RefCell<BTreeMap<String, Option<OpenPrs>>>,
    conflicts: RefCell<BTreeMap<String, Option<PrKeys>>>,
    reviews: RefCell<BTreeMap<String, Option<PrKeys>>>,
}

impl<'a> Probe<'a> {
    pub fn new(cache: &'a Cache, forge: &'a Forge) -> Probe<'a> {
        Probe {
            cache,
            forge,
            heads: RefCell::default(),
            unread: OnceCell::new(),
            listings: RefCell::default(),
            conflicts: RefCell::default(),
            reviews: RefCell::default(),
        }
    }

    /// Whether the task is worth a turn on this repository: it must have
    /// something to act on, and must not already have come up empty at
    /// exactly this state. The memory is checked first, so a task held back
    /// by it never reaches the precondition probe.
    pub fn actionable(&self, task: &str, repo: &str) -> bool {
        if self.remembered(task, repo) {
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
            Some(Basis::Feed) => self.feed().is_none_or(|feed| feed.contains_key(repo)),
            // `rebase` has work only when the bot has an open conflicting PR
            // here. Fail-closed, unlike the feed: an unreadable listing
            // leaves nothing to remember a skip against, so drawing on it
            // doesn't risk one wasted turn but one per draw until the probe
            // heals — the task is held back instead, logged by the getter.
            Some(Basis::Prs) => self.conflicting(repo).is_some_and(|prs| !prs.is_empty()),
            // `review` likewise: work only when someone has asked the bot
            // for a review it hasn't given yet, and held back on an
            // unreadable listing.
            Some(Basis::Reviews) => self.requested(repo).is_some_and(|prs| !prs.is_empty()),
            // Every other task is always worth a turn; only the memory above
            // holds one back.
            _ => true,
        }
    }

    /// The state the task's answer currently depends on, for remembering a
    /// skip against. `None` when the task keeps no memory, or when the state
    /// couldn't be read.
    pub fn state(&self, task: &str, repo: &str) -> Option<String> {
        match basis(task)? {
            Basis::Head => self.head(repo),
            Basis::Feed => self.feed()?.get(repo).cloned(),
            Basis::Prs => self.conflicting(repo).map(|prs| prs.join(" ")),
            Basis::Reviews => self.requested(repo).map(|prs| prs.join(" ")),
        }
    }

    /// Whether a remembered skip still describes the repository.
    fn remembered(&self, task: &str, repo: &str) -> bool {
        let Some(basis) = basis(task) else {
            return false;
        };
        let Some(mark) = self.cache.marked(&key(task, self.forge.kind, repo)) else {
            return false;
        };
        // Checked before the state is read, so a reworded task costs no
        // round trip on its way to being drawn again.
        if mark.prompt != digest(task) {
            return false;
        }
        // Fail-open: with no state to compare against, the mark can't be
        // said to still stand, so it doesn't hold the task back. (`rebase`
        // and `review` are held back regardless — their preconditions fail
        // closed on the same unreadable listing.)
        let Some(state) = self.state(task, repo) else {
            return false;
        };
        match basis {
            // One commit is neither newer nor older than another here: any
            // move off the remembered one is news. The same goes for the
            // conflict set: a force-push, a close, a conflict appearing or
            // resolving under base movement all change the string. And for
            // the request set: a push, a re-request, a review submitted.
            Basis::Head | Basis::Prs | Basis::Reviews => state == mark.state,
            // Stamps from one server, only ever compared to each other, so
            // string order is time order — the same test `events::fresh`
            // makes. A stamp moving backward is a thread being read or
            // cleared rather than news, so the skip still stands.
            Basis::Feed => state <= mark.state,
        }
    }

    /// The repository's default-branch commit, read from the forge once per
    /// repository and remembered for the rest of the draw.
    fn head(&self, repo: &str) -> Option<String> {
        once(&self.heads, repo, || {
            crate::precheck::head_sha(self.forge, repo)
                .with_context(|| format!("failed to read {repo}'s head commit"))
        })
    }

    /// The bot's open conflicting PRs on the repository, answered once per
    /// repository and remembered for the rest of the draw. `None` when the
    /// listing couldn't be read.
    fn conflicting(&self, repo: &str) -> Option<PrKeys> {
        once(&self.conflicts, repo, || {
            let open = self.open_prs(repo)?;
            crate::precheck::conflicting_prs(self.forge, repo, &open)
                .with_context(|| format!("failed to read {repo}'s conflicting PRs"))
        })
    }

    /// The open PRs the bot is asked to review on the repository, answered
    /// once per repository and remembered for the rest of the draw. `None`
    /// when the listing couldn't be read.
    fn requested(&self, repo: &str) -> Option<PrKeys> {
        once(&self.reviews, repo, || {
            let open = self.open_prs(repo)?;
            crate::precheck::review_requests(self.forge, repo, &open)
                .with_context(|| format!("failed to read {repo}'s review requests"))
        })
    }

    /// The repository's open PRs as its forge lists them, read once per
    /// repository and shared by both answers above: `rebase` wants the bot's
    /// own conflicting ones out of that page and `review` the ones it has
    /// been asked about, and the gated pass probes both. Each used to fetch
    /// the page for itself, which on Gitea and GitHub is the identical
    /// request made twice per repository per draw.
    fn open_prs(&self, repo: &str) -> Result<OpenPrs> {
        once(&self.listings, repo, || {
            crate::precheck::open_prs(self.forge, repo)
                .with_context(|| format!("failed to list {repo}'s open PRs"))
        })
        .context("the open PR listing is unreadable")
    }

    /// The forge's unread notifications as repository → newest stamp, read
    /// once per draw. `None` when it couldn't be read.
    fn feed(&self) -> Option<&BTreeMap<String, String>> {
        self.unread
            .get_or_init(|| {
                logged(crate::events::unread(self.forge).with_context(|| {
                    format!("failed to read {}'s activity feed", self.forge.kind.name())
                }))
            })
            .as_ref()
    }
}

/// Ask the forge for one repository's worth of state at most once per draw,
/// handing every later caller the same answer — a failure included, since a
/// listing the forge just refused is not worth re-asking for the next
/// candidate. One draw asks the same question of the same repository for
/// several tasks.
fn once<V: Clone>(
    memo: &RefCell<BTreeMap<String, Option<V>>>,
    repo: &str,
    read: impl FnOnce() -> Result<V>,
) -> Option<V> {
    if let Some(answer) = memo.borrow().get(repo) {
        return answer.clone();
    }
    let answer = logged(read());
    memo.borrow_mut().insert(repo.to_owned(), answer.clone());
    answer
}

/// A forge read's answer, with a failure logged and dropped.
fn logged<V>(answer: Result<V>) -> Option<V> {
    answer.inspect_err(|err| warn!("{err:#}")).ok()
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
            login: String::new(),
            email: String::new(),
            url: "https://github.com".into(),
            repos: BTreeMap::new(),
        }
    }

    /// The forge every probe below draws on.
    static GITHUB: std::sync::LazyLock<Forge> =
        std::sync::LazyLock::new(|| forge(ForgeKind::Github));

    /// A probe with `me/repo`'s head already answered, so the tests never
    /// reach for a forge.
    fn probe<'a>(cache: &'a Cache, head: &str) -> Probe<'a> {
        let probe = Probe::new(cache, &GITHUB);
        probe
            .heads
            .borrow_mut()
            .insert("me/repo".into(), Some(head.into()));
        probe
    }

    /// The same for the notification feed, which the probe reads once per
    /// forge rather than per repository.
    fn feed_probe<'a>(cache: &'a Cache, feed: &[(&str, &str)]) -> Probe<'a> {
        let probe = Probe::new(cache, &GITHUB);
        let feed = feed
            .iter()
            .map(|(repo, stamp)| ((*repo).to_owned(), (*stamp).to_owned()))
            .collect();
        probe.unread.set(Some(feed)).unwrap();
        probe
    }

    /// And for the conflicting-PR listing `rebase` is probed out of; `None`
    /// is a listing that couldn't be read.
    fn conflict_probe<'a>(cache: &'a Cache, prs: Option<&[&str]>) -> Probe<'a> {
        let probe = Probe::new(cache, &GITHUB);
        probe.conflicts.borrow_mut().insert(
            "me/repo".into(),
            prs.map(|prs| prs.iter().map(|pr| (*pr).to_owned()).collect()),
        );
        probe
    }

    /// And for the review-request listing `review` is probed out of.
    fn review_probe<'a>(cache: &'a Cache, prs: Option<&[&str]>) -> Probe<'a> {
        let probe = Probe::new(cache, &GITHUB);
        probe.reviews.borrow_mut().insert(
            "me/repo".into(),
            prs.map(|prs| prs.iter().map(|pr| (*pr).to_owned()).collect()),
        );
        probe
    }

    /// Each task is remembered against whatever it reads: the tree-only ones
    /// against the tree, `feedback` against the notification feed, `rebase`
    /// against the bot's conflicting PRs, `review` against the PRs it is
    /// asked to review. The tasks that can become actionable with nothing
    /// observable moving keep no memory at all.
    ///
    /// The check runs both ways over the whole task pool. A slug that no
    /// longer names a section has stopped being remembered, and a section
    /// nobody listed here has silently defaulted to keeping no memory, which
    /// is what a new task gets whether or not that is the right answer for
    /// it.
    #[test]
    fn basis_matches_what_each_task_reads() {
        let head = ["docs", "simplify", "benchmark", "coverage", "mutation"];
        let unremembered = ["bump", "todo", "roleplay", "audit", "feature"];
        for task in head {
            assert_eq!(basis(task), Some(Basis::Head), "{task}");
        }
        assert_eq!(basis("feedback"), Some(Basis::Feed));
        assert_eq!(basis("rebase"), Some(Basis::Prs));
        assert_eq!(basis("review"), Some(Basis::Reviews));
        for task in unremembered {
            assert_eq!(basis(task), None, "{task}");
        }

        let declared: BTreeSet<&str> = head
            .into_iter()
            .chain(unremembered)
            .chain(["feedback", "rebase", "review"])
            .collect();
        for task in &declared {
            assert!(crate::prompts::defines(task), "{task} is gone");
        }
        for task in crate::prompts::tasks() {
            assert!(
                declared.contains(task.slug.as_str()),
                "{} declares nothing",
                task.slug
            );
        }
    }

    /// The probed tasks are exactly the ones whose basis is forge state: that
    /// read answers both "has anything moved" and "is there anything to act
    /// on", so there is one list of them rather than two to keep in step.
    #[test]
    fn the_gated_tasks_are_the_ones_probed_out_of_forge_state() {
        assert!(gated("feedback"));
        assert!(gated("rebase"));
        assert!(gated("review"));
        for task in ["docs", "simplify", "bump", "todo", "feature"] {
            assert!(!gated(task), "{task}");
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
        cache.remember(
            "feedback",
            ForgeKind::Github,
            "me/repo",
            "2026-10-01T02:00:00Z",
        );

        let unchanged = [("me/repo", "2026-10-01T02:00:00Z")];
        assert!(feed_probe(&cache, &unchanged).remembered("feedback", "me/repo"));
        let newer = [("me/repo", "2026-10-01T03:00:00Z")];
        assert!(!feed_probe(&cache, &newer).remembered("feedback", "me/repo"));
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
        assert!(feed_probe(&cache, &older).remembered("feedback", "me/repo"));
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
        assert!(!feed_probe(&cache, &elsewhere).remembered("feedback", "me/repo"));
        assert!(!feed_probe(&cache, &[]).remembered("feedback", "me/repo"));
    }

    /// `feedback`'s precondition is answered out of the same feed its memory
    /// is keyed on, with no mark involved: a repository with something unread
    /// is drawn, one with nothing on the feed is passed over, and a feed that
    /// couldn't be read fails open.
    #[test]
    fn feedback_is_drawn_only_for_a_repository_with_unread_activity() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        let feed = [("me/repo", "2026-10-01T02:00:00Z")];
        assert!(feed_probe(&cache, &feed).actionable("feedback", "me/repo"));
        assert!(!feed_probe(&cache, &feed).actionable("feedback", "me/other"));
        assert!(!feed_probe(&cache, &[]).actionable("feedback", "me/repo"));

        let unreadable = Probe::new(&cache, &GITHUB);
        unreadable.unread.set(None).unwrap();
        assert!(unreadable.actionable("feedback", "me/repo"));
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
        assert!(!probe.actionable("feedback", "me/repo"));
    }

    /// `rebase` is drawn only when the bot has an open conflicting PR, and —
    /// unlike the feed — an unreadable listing holds it back rather than
    /// drawing it: with nothing to remember a skip against, a probe that
    /// stayed broken used to cost an agent session per draw, forever.
    #[test]
    fn rebase_is_drawn_only_when_an_owned_pr_conflicts() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        assert!(conflict_probe(&cache, Some(&["12@abc"])).actionable("rebase", "me/repo"));
        assert!(!conflict_probe(&cache, Some(&[])).actionable("rebase", "me/repo"));
        assert!(!conflict_probe(&cache, None).actionable("rebase", "me/repo"));
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
        cache.remember("rebase", ForgeKind::Github, "me/repo", "12@abc");

        assert!(conflict_probe(&cache, Some(&["12@abc"])).remembered("rebase", "me/repo"));
        // A force-push moves the head.
        assert!(!conflict_probe(&cache, Some(&["12@def"])).remembered("rebase", "me/repo"));
        // A second conflict appears.
        assert!(
            !conflict_probe(&cache, Some(&["12@abc", "15@eee"])).remembered("rebase", "me/repo")
        );
        // The set empties: the mark no longer stands, and the precondition
        // holds the task back anyway.
        assert!(!conflict_probe(&cache, Some(&[])).remembered("rebase", "me/repo"));
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
        assert!(!probe.actionable("rebase", "me/repo"));
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
        assert!(!probe.remembered("rebase", "me/repo"));
        assert!(!probe.actionable("rebase", "me/repo"));
    }

    /// `review` is drawn only when someone has asked the bot for a review it
    /// hasn't given, and like `rebase` an unreadable listing holds it back.
    #[test]
    fn review_is_drawn_only_when_a_review_is_requested() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        assert!(review_probe(&cache, Some(&["12@abc"])).actionable("review", "me/repo"));
        assert!(!review_probe(&cache, Some(&[])).actionable("review", "me/repo"));
        assert!(!review_probe(&cache, None).actionable("review", "me/repo"));
    }

    /// A `review` skip is keyed on the request set: it stands while the set
    /// sits still, and any move — a push to the branch, a new request, the
    /// review being submitted — redraws the task. The memory outranks the
    /// precondition, so a request the agent already answered for without
    /// submitting doesn't win every draw.
    #[test]
    fn a_review_skip_stands_until_the_request_set_moves() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("review", ForgeKind::Github, "me/repo", "12@abc");

        let same = review_probe(&cache, Some(&["12@abc"]));
        assert!(same.remembered("review", "me/repo"));
        assert!(!same.actionable("review", "me/repo"));
        assert!(!review_probe(&cache, Some(&["12@def"])).remembered("review", "me/repo"));
        assert!(!review_probe(&cache, Some(&["12@abc", "15@eee"])).remembered("review", "me/repo"));
        assert!(!review_probe(&cache, Some(&[])).remembered("review", "me/repo"));

        let unreadable = review_probe(&cache, None);
        assert!(!unreadable.remembered("review", "me/repo"));
        assert!(!unreadable.actionable("review", "me/repo"));
    }

    /// A task with no precondition of its own is drawn without any forge
    /// lookup once the memory has let it through.
    #[test]
    fn an_unprobed_task_needs_no_forge_lookup() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        let probe = Probe::new(&cache, &GITHUB);
        assert!(probe.actionable("docs", "me/repo"));
        assert!(probe.heads.borrow().is_empty());
        assert!(probe.unread.get().is_none());
        assert!(probe.conflicts.borrow().is_empty());
        assert!(probe.reviews.borrow().is_empty());
    }

    #[test]
    fn a_remembered_skip_stands_until_the_head_moves() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("docs", ForgeKind::Github, "me/repo", "abc");
        assert!(probe(&cache, "abc").remembered("docs", "me/repo"));
        assert!(!probe(&cache, "def").remembered("docs", "me/repo"));
    }

    /// Nothing is remembered for another task, another repository, or a forge
    /// that merely shares the path.
    #[test]
    fn marks_are_scoped_to_one_task_and_repository() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("docs", ForgeKind::Github, "me/repo", "abc");

        let probe = probe(&cache, "abc");
        assert!(!probe.remembered("coverage", "me/repo"));
        assert!(!probe.remembered("docs", "me/other"));

        let gitea = forge(ForgeKind::Gitea);
        let probe = Probe::new(&cache, &gitea);
        probe
            .heads
            .borrow_mut()
            .insert("me/repo".into(), Some("abc".into()));
        assert!(!probe.remembered("docs", "me/repo"));
    }

    #[test]
    fn forgetting_draws_the_task_again() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("simplify", ForgeKind::Github, "me/repo", "abc");
        cache.forget("simplify", ForgeKind::Github, "me/repo");
        assert!(!probe(&cache, "abc").remembered("simplify", "me/repo"));
    }

    /// A task with no basis is never held back, whatever the file happens to
    /// hold, and costs no forge lookup either.
    #[test]
    fn unremembered_tasks_are_never_held_back() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("bump", ForgeKind::Github, "me/repo", "abc");
        let probe = Probe::new(&cache, &GITHUB);
        assert!(!probe.remembered("bump", "me/repo"));
        assert!(probe.heads.borrow().is_empty());
        assert!(probe.unread.get().is_none());
        assert!(probe.conflicts.borrow().is_empty());
        assert!(probe.reviews.borrow().is_empty());
    }

    /// A skip remembered under one wording of the task doesn't gate the next.
    #[test]
    fn reworded_instructions_invalidate_a_skip() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("docs", ForgeKind::Github, "me/repo", "abc");
        let key = key("docs", ForgeKind::Github, "me/repo");
        cache.lock().get_mut(&key).unwrap().prompt = "stale".into();
        assert!(!probe(&cache, "abc").remembered("docs", "me/repo"));
    }

    /// An unreadable head fails open: better a redundant turn than a task
    /// gated on a comparison that couldn't be made.
    #[test]
    fn an_unknown_head_draws_the_task() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("docs", ForgeKind::Github, "me/repo", "abc");
        let probe = Probe::new(&cache, &GITHUB);
        probe.heads.borrow_mut().insert("me/repo".into(), None);
        assert!(!probe.remembered("docs", "me/repo"));
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
