//! Skip memory for the tasks whose answer is a function of something the
//! runner can observe for itself. `docs`, `simplify`, `benchmark`,
//! `coverage`, `mutation`, and `audit` all read the code and nothing else, so
//! a turn that found nothing to do will find nothing again until the code
//! moves. (`audit` also asks the ecosystem's advisory database, which moves
//! on its own; but it moves over days, and the alternative was the same
//! repository audited twice at the same commit twelve minutes apart.)
//! `respond` reads the forge's notification feed, so its answer holds until
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
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::config::{Forge, ForgeKind};
use crate::precheck::{conflicting_prs, review_requests};
use crate::workspace::JsonStore;

/// What a skipped turn on this task is remembered against, or `None` when the
/// skip isn't worth remembering at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Basis {
    /// The repository's default-branch commit: the task reads the tree and
    /// nothing else, so its answer holds until the code moves.
    Head,
    /// The newest unread thread on the repository's notification feed. This
    /// is what `respond` answers out of, and it needs remembering as much as
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

impl Basis {
    /// The basis a task section's `<!-- cache: name -->` line names.
    pub fn from_name(name: &str) -> Option<Basis> {
        match name {
            "head" => Some(Basis::Head),
            "feed" => Some(Basis::Feed),
            "prs" => Some(Basis::Prs),
            "reviews" => Some(Basis::Reviews),
            _ => None,
        }
    }
}

/// The state this task's answer depends on, as its section in the sweep
/// command (or the user's task file) declares it. `None` for the tasks that
/// can become actionable with nothing observable moving — `bump` watches the
/// outside world, `feature` invents its own work — where a remembered skip
/// would outlast the reason for it, and for a slug no section defines.
pub fn basis(task: &str) -> Option<Basis> {
    crate::prompts::task(task)?.basis
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
    marks: JsonStore<BTreeMap<String, Mark>>,
}

impl Cache {
    /// Load the remembered skips from `path`. Unlike the settings, a lost
    /// cache costs only a redundant turn, so an unreadable or malformed file
    /// is logged and dropped rather than failing startup.
    pub fn load(path: PathBuf) -> Cache {
        Cache {
            marks: JsonStore::load(path),
        }
    }

    /// Remember that the task found nothing to do on the repository in
    /// `state`, so it isn't drawn again until that state or the task moves.
    pub fn remember(&self, task: &str, forge: ForgeKind, repo: &str, state: &str) {
        let mark = Mark {
            state: state.to_owned(),
            prompt: digest(task),
        };
        let mut marks = self.marks.lock();
        if marks.insert(key(task, forge, repo), mark.clone()) == Some(mark) {
            return;
        }
        self.marks.save(&marks);
    }

    /// Drop whatever was remembered for the task on this repository: the turn
    /// either changed something or never got to answer, so the old mark no
    /// longer describes it.
    pub fn forget(&self, task: &str, forge: ForgeKind, repo: &str) {
        let mut marks = self.marks.lock();
        if marks.remove(&key(task, forge, repo)).is_some() {
            self.marks.save(&marks);
        }
    }

    fn marked(&self, key: &str) -> Option<Mark> {
        self.marks.lock().get(key).cloned()
    }
}

/// One task's entry for one repository. None of the three parts can contain a
/// colon — forge and task are fixed slugs, and forge paths are
/// `<owner>/<repo>` — so a flat key stays unambiguous and readable in the
/// file. The draw ledger (`ledger.rs`) keys its entries the same way.
pub fn key(task: &str, forge: ForgeKind, repo: &str) -> String {
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

/// One page of a repository's open PRs, as `precheck::open_prs` fetched it,
/// shared rather than copied out to each set of keys read off it.
type OpenPrs = Rc<Vec<serde_json::Value>>;

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
    /// The most open PRs of the bot's a repository may have before the tasks
    /// that open more are held back there; `None` is no budget.
    pr_budget: Option<usize>,
    /// The repositories already logged as at the budget this draw, so the
    /// hold is reported once per repository rather than once per task.
    held: RefCell<BTreeSet<String>>,
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
            pr_budget: None,
            held: RefCell::default(),
        }
    }

    /// The same probe, holding back the PR-opening tasks on any repository
    /// where the bot already has `budget` open PRs.
    pub fn with_pr_budget(self, budget: Option<usize>) -> Probe<'a> {
        Probe {
            pr_budget: budget,
            ..self
        }
    }

    /// Whether the task is worth a turn on this repository: it must have
    /// something to act on, must not already have come up empty at exactly
    /// this state, and must not be one that opens PRs on a repository already
    /// at its open-PR budget. The memory is checked first, so a task held
    /// back by it never reaches the precondition probe.
    pub fn actionable(&self, task: &str, repo: &str) -> bool {
        !self.remembered(task, repo) && !self.over_budget(task, repo) && self.has_work(task, repo)
    }

    /// Whether the task has anything to act on here, whatever the memory
    /// or the open-PR budget says: the precondition alone, which is all a
    /// turn the user asked for by name is held to.
    pub fn has_work(&self, task: &str, repo: &str) -> bool {
        match basis(task) {
            // A task that answers out of the feed has nothing to answer when
            // the repository has nothing unread, so its basis doubles as its
            // precondition. The feed is read once per forge for the memory's
            // sake anyway, so probing `respond` across the whole pool costs
            // the one request it already made rather than one per repository.
            // Fail-open: an unreadable feed draws the task and at worst
            // skips again.
            Some(Basis::Feed) => self.feed().is_none_or(|feed| feed.contains_key(repo)),
            // `rebase` has work only when the bot has an open conflicting PR
            // here, and `review` only when someone has asked the bot for a
            // review it hasn't given yet. Fail-closed, unlike the feed: an
            // unreadable listing leaves nothing to remember a skip against,
            // so drawing on it doesn't risk one wasted turn but one per draw
            // until the probe heals — the task is held back instead, logged
            // by the getter.
            Some(basis @ (Basis::Prs | Basis::Reviews)) => {
                self.pr_keys(basis, repo).is_some_and(|prs| !prs.is_empty())
            }
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
            basis @ (Basis::Prs | Basis::Reviews) => {
                self.pr_keys(basis, repo).map(|prs| prs.join(" "))
            }
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

    /// The PR keys a `Prs` or `Reviews` basis reads off the repository's
    /// open-PR page — the bot's own conflicting PRs, or the ones it is asked
    /// to review — answered once per repository and remembered for the rest
    /// of the draw. `None` when either the page or the keys couldn't be read.
    ///
    /// The page is read once per repository too, and shared by both sets,
    /// since the gated pass probes both. An unreadable page is logged where
    /// it is fetched, and only there.
    fn pr_keys(&self, basis: Basis, repo: &str) -> Option<PrKeys> {
        let (memo, what, read): (_, _, fn(&Forge, &str, &_) -> _) = match basis {
            Basis::Prs => (&self.conflicts, "conflicting PRs", conflicting_prs),
            Basis::Reviews => (&self.reviews, "review requests", review_requests),
            Basis::Head | Basis::Feed => unreachable!("{basis:?} is not read off the PR page"),
        };
        if let Some(keys) = memo.borrow().get(repo) {
            return keys.clone();
        }
        let open = self.open_prs(repo)?;
        once(memo, repo, || {
            read(self.forge, repo, &open).with_context(|| format!("failed to read {repo}'s {what}"))
        })
    }

    /// The repository's page of open PRs, fetched once per draw and shared
    /// by everything read off it.
    fn open_prs(&self, repo: &str) -> Option<OpenPrs> {
        once(&self.listings, repo, || {
            crate::precheck::open_prs(self.forge, repo)
                .map(Rc::new)
                .with_context(|| format!("failed to list {repo}'s open PRs"))
        })
    }

    /// Whether the task would open yet another PR on a repository where the
    /// bot already has its budget's worth open. The tasks that service the
    /// queue (the gated ones) and the ones that only file issues are never
    /// held back, and nothing is without a budget. Fail-open: an unreadable
    /// listing is no reason to stand down every ungated task, and it is
    /// logged by the getter.
    fn over_budget(&self, task: &str, repo: &str) -> bool {
        let Some(budget) = self.pr_budget else {
            return false;
        };
        if gated(task) || !crate::prompts::task(task).is_some_and(|t| t.opens_prs()) {
            return false;
        }
        let Some(open) = self.open_prs(repo) else {
            return false;
        };
        let own = crate::precheck::own_prs(self.forge, &open);
        if own < budget {
            return false;
        }
        if self.held.borrow_mut().insert(repo.to_owned()) {
            info!("{repo}: {own} open PRs of the bot's, holding back the tasks that open more");
        }
        true
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
        Forge::test(kind, "https://github.com")
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

    /// And for the PR keys `rebase` and `review` are probed out of — the
    /// bot's conflicting PRs for the one, its review requests for the other.
    /// `None` is a listing that couldn't be read.
    fn pr_probe<'a>(cache: &'a Cache, task: &str, prs: Option<&[&str]>) -> Probe<'a> {
        let probe = Probe::new(cache, &GITHUB);
        let keys = prs.map(|prs| prs.iter().map(|pr| (*pr).to_owned()).collect());
        match task {
            "rebase" => &probe.conflicts,
            "review" => &probe.reviews,
            other => panic!("{other} is not probed out of the PR page"),
        }
        .borrow_mut()
        .insert("me/repo".into(), keys);
        probe
    }

    /// Each task is remembered against whatever it reads: the tree-only ones
    /// against the tree, `respond` against the notification feed, `rebase`
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
        let head = ["docs", "simplify", "benchmark", "coverage", "mutation", "audit"];
        let unremembered = ["bump", "todo", "roleplay", "feature"];
        for task in head {
            assert_eq!(basis(task), Some(Basis::Head), "{task}");
        }
        assert_eq!(basis("respond"), Some(Basis::Feed));
        assert_eq!(basis("rebase"), Some(Basis::Prs));
        assert_eq!(basis("review"), Some(Basis::Reviews));
        for task in unremembered {
            assert_eq!(basis(task), None, "{task}");
        }

        let declared: BTreeSet<&str> = head
            .into_iter()
            .chain(unremembered)
            .chain(["respond", "rebase", "review"])
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

    /// A `respond` skip is keyed on the feed, not the tree: it stands while
    /// the feed sits still, and a thread newer than the one it answered for
    /// draws the task again. Without this one stale notification keeps the
    /// precheck true and `respond` — probed first, so first to win the draw
    /// — is drawn every single turn until somebody marks it read.
    #[test]
    fn a_feedback_skip_stands_until_a_newer_thread_arrives() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember(
            "respond",
            ForgeKind::Github,
            "me/repo",
            "2026-10-01T02:00:00Z",
        );

        let unchanged = [("me/repo", "2026-10-01T02:00:00Z")];
        assert!(feed_probe(&cache, &unchanged).remembered("respond", "me/repo"));
        let newer = [("me/repo", "2026-10-01T03:00:00Z")];
        assert!(!feed_probe(&cache, &newer).remembered("respond", "me/repo"));
    }

    /// A stamp moving backward is a thread being read or cleared rather than
    /// news, so the skip survives it — the same reading `events::fresh` takes.
    #[test]
    fn a_cleared_thread_does_not_redraw_feedback() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember(
            "respond",
            ForgeKind::Github,
            "me/repo",
            "2026-10-01T02:00:00Z",
        );
        let older = [("me/repo", "2026-10-01T01:00:00Z")];
        assert!(feed_probe(&cache, &older).remembered("respond", "me/repo"));
    }

    /// A feed with nothing on the repository fails open like an unreadable
    /// head. It costs no turn either way: the precheck reads the same empty
    /// feed and says there is nothing to do.
    #[test]
    fn an_empty_feed_draws_feedback() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember(
            "respond",
            ForgeKind::Github,
            "me/repo",
            "2026-10-01T02:00:00Z",
        );
        let elsewhere = [("me/other", "2026-10-01T03:00:00Z")];
        assert!(!feed_probe(&cache, &elsewhere).remembered("respond", "me/repo"));
        assert!(!feed_probe(&cache, &[]).remembered("respond", "me/repo"));
    }

    /// `respond`'s precondition is answered out of the same feed its memory
    /// is keyed on, with no mark involved: a repository with something unread
    /// is drawn, one with nothing on the feed is passed over, and a feed that
    /// couldn't be read fails open.
    #[test]
    fn feedback_is_drawn_only_for_a_repository_with_unread_activity() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        let feed = [("me/repo", "2026-10-01T02:00:00Z")];
        assert!(feed_probe(&cache, &feed).actionable("respond", "me/repo"));
        assert!(!feed_probe(&cache, &feed).actionable("respond", "me/other"));
        assert!(!feed_probe(&cache, &[]).actionable("respond", "me/repo"));

        let unreadable = Probe::new(&cache, &GITHUB);
        unreadable.unread.set(None).unwrap();
        assert!(unreadable.actionable("respond", "me/repo"));
    }

    /// The memory outranks the precondition: a thread `respond` has already
    /// answered for keeps it out of the draw even though the feed still names
    /// the repository.
    #[test]
    fn a_remembered_skip_beats_the_feed_precondition() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        let stamp = "2026-10-01T02:00:00Z";
        cache.remember("respond", ForgeKind::Github, "me/repo", stamp);
        let probe = feed_probe(&cache, &[("me/repo", stamp)]);
        assert!(!probe.actionable("respond", "me/repo"));
    }

    /// `rebase` is drawn only when the bot has an open conflicting PR, and
    /// `review` only when someone has asked it for a review it hasn't given.
    /// Unlike the feed, an unreadable listing holds both back rather than
    /// drawing them: with nothing to remember a skip against, a probe that
    /// stayed broken used to cost an agent session per draw, forever.
    #[test]
    fn a_pr_task_is_drawn_only_when_the_listing_names_a_pr() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        for task in ["rebase", "review"] {
            let probe = |prs| pr_probe(&cache, task, prs);
            assert!(
                probe(Some(&["12@abc"])).actionable(task, "me/repo"),
                "{task}"
            );
            assert!(!probe(Some(&[])).actionable(task, "me/repo"), "{task}");
            assert!(!probe(None).actionable(task, "me/repo"), "{task}");
        }
    }

    /// A `rebase` skip is keyed on the conflict set and a `review` skip on
    /// the request set: each stands while its set sits still, and any move —
    /// a force-push, a PR joining the set, the set emptying — redraws the
    /// task. The memory outranks the precondition, so one conflict the agent
    /// declines to resolve, or one request it answered without submitting,
    /// doesn't win every draw for as long as the forge reports it. An
    /// unreadable listing neither stands the mark up nor draws the task:
    /// `remembered` fails open as usual, and the fail-closed precondition
    /// holds the task back on the same answer.
    #[test]
    fn a_pr_skip_stands_until_its_pr_set_moves() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        for task in ["rebase", "review"] {
            cache.remember(task, ForgeKind::Github, "me/repo", "12@abc");
            let probe = |prs| pr_probe(&cache, task, prs);

            let same = probe(Some(&["12@abc"]));
            assert!(same.remembered(task, "me/repo"), "{task}");
            assert!(!same.actionable(task, "me/repo"), "{task}");
            // A force-push moves the head.
            assert!(
                !probe(Some(&["12@def"])).remembered(task, "me/repo"),
                "{task}"
            );
            // A second PR joins the set.
            assert!(
                !probe(Some(&["12@abc", "15@eee"])).remembered(task, "me/repo"),
                "{task}"
            );
            // The set empties: the mark no longer stands, and the
            // precondition holds the task back anyway.
            assert!(!probe(Some(&[])).remembered(task, "me/repo"), "{task}");

            let unreadable = probe(None);
            assert!(!unreadable.remembered(task, "me/repo"), "{task}");
            assert!(!unreadable.actionable(task, "me/repo"), "{task}");
        }
    }

    /// The forge the open-PR budget tests run on: it needs a login for the
    /// bot's own PRs to be told apart from anyone else's.
    static BOT: std::sync::LazyLock<Forge> = std::sync::LazyLock::new(|| Forge {
        login: "bot".into(),
        ..forge(ForgeKind::Github)
    });

    /// A probe with a budget of `budget` PRs and `me/repo`'s open-PR page
    /// already answered: `authors` are the open PRs' authors, `None` a page
    /// that couldn't be read.
    fn budget_probe<'a>(cache: &'a Cache, budget: usize, authors: Option<&[&str]>) -> Probe<'a> {
        let probe = Probe::new(cache, &BOT).with_pr_budget(Some(budget));
        let page = authors.map(|authors| {
            Rc::new(
                authors
                    .iter()
                    .map(|login| serde_json::json!({ "user": { "login": login } }))
                    .collect(),
            )
        });
        probe.listings.borrow_mut().insert("me/repo".into(), page);
        // `rebase` and `review` have work, so only the budget can hold them.
        for memo in [&probe.conflicts, &probe.reviews] {
            memo.borrow_mut()
                .insert("me/repo".into(), Some(vec!["12@abc".into()]));
        }
        probe
    }

    /// At the budget, the tasks that would open another PR are held back,
    /// while the ones that work on the open PRs and the ones that only file
    /// issues still draw. One below it, everything draws.
    #[test]
    fn a_repository_at_its_pr_budget_holds_back_the_tasks_that_open_prs() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        let full = budget_probe(&cache, 2, Some(&["bot", "bot"]));
        for task in ["docs", "bump", "simplify"] {
            assert!(!full.actionable(task, "me/repo"), "{task}");
        }
        for task in ["rebase", "review", "feature"] {
            assert!(full.actionable(task, "me/repo"), "{task}");
        }
        let room = budget_probe(&cache, 3, Some(&["bot", "bot"]));
        assert!(room.actionable("docs", "me/repo"));
    }

    /// Only the bot's own PRs count against its budget.
    #[test]
    fn other_authors_prs_do_not_count_against_the_budget() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        let probe = budget_probe(&cache, 2, Some(&["bot", "alice", "bob"]));
        assert!(probe.actionable("docs", "me/repo"));
    }

    /// An unreadable listing fails open: one bad page shouldn't stand down
    /// every ungated task. Without a budget the listing isn't read at all.
    #[test]
    fn the_budget_fails_open_and_costs_nothing_unset() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        assert!(budget_probe(&cache, 1, None).actionable("docs", "me/repo"));

        let unbudgeted = Probe::new(&cache, &BOT);
        assert!(unbudgeted.actionable("docs", "me/repo"));
        assert!(unbudgeted.listings.borrow().is_empty());
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
        cache.marks.lock().get_mut(&key).unwrap().prompt = "stale".into();
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
        assert!(Cache::load(path).marks.lock().is_empty());
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
