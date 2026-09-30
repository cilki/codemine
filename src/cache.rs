//! Skip memory for the tasks that answer out of the tree alone. `docs`,
//! `simplify`, `benchmark`, `coverage`, and `mutation` all read the code and
//! nothing else, so a turn that found nothing to do will find nothing again
//! until the code moves — drawing them meanwhile buys a session's worth of
//! tokens and another `TASK SKIPPED`. Each skip is remembered against the
//! repository's default-branch commit and the task's own instructions, and
//! the task isn't drawn for that repository again until one of the two
//! changes. The memory is persisted next to the rest of the workspace, since
//! the clones outlive the process too.

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

/// Whether a skipped turn on this task is worth remembering. Only tasks whose
/// answer is a function of the tree qualify: one that also depends on the
/// forge (`feedback`), on the outside world (`bump`), or on the agent's own
/// imagination (`feature`) could become actionable without a single commit
/// landing, and a remembered skip would outlast the reason for it.
pub fn cachable(task: &str) -> bool {
    matches!(
        task,
        "docs" | "simplify" | "benchmark" | "coverage" | "mutation"
    )
}

/// The state a skip was remembered against. Both halves have to still match
/// for the skip to stand.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
struct Mark {
    /// The default branch's commit as the agent saw it.
    head: String,
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

    /// Remember that the task found nothing to do on the repository at
    /// `head`, so it isn't drawn again until the code or the task moves.
    pub fn remember(&self, task: &str, forge: ForgeKind, repo: &str, head: &str) {
        let mark = Mark {
            head: head.to_owned(),
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

/// The draw's precondition check with the skip memory folded in, plus the
/// default-branch commits read along the way. The same repository is probed
/// once per cachable task, and one round trip answers for all of them.
pub struct Probe<'a> {
    cache: &'a Cache,
    heads: RefCell<BTreeMap<(ForgeKind, String), Option<String>>>,
}

impl<'a> Probe<'a> {
    pub fn new(cache: &'a Cache) -> Probe<'a> {
        Probe {
            cache,
            heads: RefCell::new(BTreeMap::new()),
        }
    }

    /// Whether the task is worth a turn on this repository: it must have
    /// something to act on, and must not already have come up empty at
    /// exactly this state. The two are disjoint in practice — no cachable
    /// task is a gated one — so each candidate costs one probe or the other,
    /// never both.
    pub fn actionable(&self, task: &str, forge: &Forge, repo: &str) -> bool {
        !self.remembered(task, forge, repo) && crate::precheck::actionable(task, forge, repo)
    }

    /// Whether a remembered skip still describes the repository.
    fn remembered(&self, task: &str, forge: &Forge, repo: &str) -> bool {
        if !cachable(task) {
            return false;
        }
        let Some(mark) = self.cache.marked(&key(task, forge.kind, repo)) else {
            return false;
        };
        // Fail-open like the prechecks: with no head to compare against, the
        // task is drawn and at worst skips again.
        self.head(forge, repo).is_some_and(|head| {
            mark == Mark {
                head,
                prompt: digest(task),
            }
        })
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

    /// The tree-only tasks are cached; everything that can become actionable
    /// without a commit is not.
    #[test]
    fn cachable_covers_the_tree_only_tasks() {
        for task in ["docs", "simplify", "benchmark", "coverage", "mutation"] {
            assert!(cachable(task));
        }
        for task in [
            "feedback", "rebase", "bump", "todo", "roleplay", "audit", "feature",
        ] {
            assert!(!cachable(task));
        }
    }

    /// Every cachable slug still names a task the sweep command defines, so a
    /// renamed section can't quietly stop being cached.
    #[test]
    fn cachable_tasks_are_real_tasks() {
        let slugs = crate::prompts::default_tasks();
        for task in ["docs", "simplify", "benchmark", "coverage", "mutation"] {
            assert!(slugs.iter().any(|slug| slug == task), "{task} is gone");
        }
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

    /// An uncachable task is never held back, whatever the file happens to
    /// hold, and costs no head lookup either.
    #[test]
    fn uncachable_tasks_are_never_remembered() {
        let root = tempfile::tempdir().unwrap();
        let cache = cache(&root);
        cache.remember("feedback", ForgeKind::Github, "me/repo", "abc");
        let probe = Probe::new(&cache);
        assert!(!probe.remembered("feedback", &forge(ForgeKind::Github), "me/repo"));
        assert!(probe.heads.borrow().is_empty());
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
                head: "abc".into(),
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
