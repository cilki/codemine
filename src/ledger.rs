//! What every (task, repository) pair has yielded so far, and when it last
//! had a turn, so the draw can weigh its candidates instead of treating them
//! all alike. The skip cache (`cache.rs`) holds a pair back outright while
//! the state it answered for stands still; this is the softer memory for
//! everything the cache lets through. Without it the ungated pass was a
//! uniform draw, which has two failure modes on a quiet pool: the same pair
//! can come up three turns running while others wait, and the tasks the
//! cache never remembers (`bump`, `todo`, `roleplay`, `feature` — anything
//! without a basis) are drawn as readily on a repository where they have
//! skipped ten times as on one where they have never run.
//!
//! The ledger lives in `ledger.json` at the workspace root, beside the skip
//! cache, and is keyed the same way. Like the cache, it is advisory: losing
//! it costs nothing but a few less well chosen draws.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::ForgeKind;
use crate::status::Outcome;

/// How idle a pair can count as, in hours. A pair that has never had a turn,
/// or hasn't had one in a week, is as stale as it gets; beyond that the
/// draw is deciding between pairs on their yield alone.
const IDLE_CAP_HOURS: f64 = 24.0 * 7.0;

/// One pair's history.
#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Eq, Debug)]
struct Entry {
    /// When the pair's most recent turn started, in epoch seconds.
    drawn: u64,
    /// Turns that changed something on the forge.
    completed: u32,
    /// Turns that answered `TASK SKIPPED`.
    skipped: u32,
}

/// The per-pair history, keyed by forge, repository, and task.
pub struct Ledger {
    path: PathBuf,
    entries: Mutex<BTreeMap<String, Entry>>,
}

impl Ledger {
    /// Load the ledger from `path`. An unreadable or malformed file is logged
    /// and dropped rather than failing startup: it is only ever a hint to the
    /// draw.
    pub fn load(path: PathBuf) -> Ledger {
        let entries = match std::fs::read(&path) {
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
        Ledger {
            path,
            entries: Mutex::new(entries),
        }
    }

    /// Note that the pair had a turn starting at `drawn` that ended in
    /// `outcome`. Only a completed or skipped turn says anything about the
    /// pair's yield; a turn that failed, timed out, or was cancelled never
    /// got to answer, so it moves the pair's clock and nothing else.
    pub fn record(&self, task: &str, forge: ForgeKind, repo: &str, drawn: u64, outcome: Outcome) {
        let mut entries = self.lock();
        let entry = entries
            .entry(crate::cache::key(task, forge, repo))
            .or_default();
        entry.drawn = drawn;
        match outcome {
            Outcome::Completed => entry.completed += 1,
            Outcome::Skipped => entry.skipped += 1,
            Outcome::Failed | Outcome::Timeout | Outcome::Canceled => {}
        }
        if let Err(err) = crate::workspace::write_json(&self.path, &*entries) {
            warn!("failed to save the draw ledger: {err:#}");
        }
    }

    /// How much the draw should favour the pair at `now`, relative to the
    /// others on the table. Positive, and larger for a pair that has waited
    /// longer and paid off more often.
    pub fn weight(&self, task: &str, forge: ForgeKind, repo: &str, now: u64) -> f64 {
        weigh(self.lock().get(&crate::cache::key(task, forge, repo)), now)
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Entry>> {
        crate::sync::lock(&self.entries)
    }
}

/// The weight of a pair with history `entry`, or none, at `now`: the hours
/// it has sat idle (plus one, so a pair drawn this very hour still has a
/// weight) capped at a week, times its hit rate with a Laplace prior. A
/// never-drawn pair counts as fully idle and at even odds, so it outranks
/// everything that has run recently and everything that has mostly skipped,
/// but not a long-idle pair that has usually completed. A pair that always
/// skips sinks toward — but never reaches — zero, so it is still drawn once
/// everything else has had its turn.
fn weigh(entry: Option<&Entry>, now: u64) -> f64 {
    let (idle_hours, completed, skipped) = match entry {
        None => (IDLE_CAP_HOURS, 0, 0),
        Some(entry) => {
            let idle = now.saturating_sub(entry.drawn) as f64 / 3600.0;
            (
                (idle + 1.0).min(IDLE_CAP_HOURS),
                entry.completed,
                entry.skipped,
            )
        }
    };
    let hit_rate = (f64::from(completed) + 1.0) / (f64::from(completed + skipped) + 2.0);
    idle_hours * hit_rate
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3600;
    const NOW: u64 = 1_700_000_000;

    fn ledger(root: &tempfile::TempDir) -> Ledger {
        Ledger::load(root.path().join("ledger.json"))
    }

    fn weight_of(ledger: &Ledger, task: &str, repo: &str) -> f64 {
        ledger.weight(task, ForgeKind::Github, repo, NOW)
    }

    /// A pair that has never had a turn outranks one that just had one, and
    /// the longer a pair has waited the better its odds, up to the cap.
    #[test]
    fn an_idle_pair_outranks_a_recent_one() {
        let root = tempfile::tempdir().unwrap();
        let ledger = ledger(&root);
        let never = weight_of(&ledger, "docs", "me/never");
        ledger.record("docs", ForgeKind::Github, "me/now", NOW, Outcome::Completed);
        ledger.record(
            "docs",
            ForgeKind::Github,
            "me/day",
            NOW - 24 * HOUR,
            Outcome::Completed,
        );
        ledger.record(
            "docs",
            ForgeKind::Github,
            "me/month",
            NOW - 30 * 24 * HOUR,
            Outcome::Completed,
        );
        let now = weight_of(&ledger, "docs", "me/now");
        let day = weight_of(&ledger, "docs", "me/day");
        let month = weight_of(&ledger, "docs", "me/month");
        assert!(now > 0.0);
        assert!(now < day, "{now} vs {day}");
        assert!(day < month, "{day} vs {month}");
        // A month idle is capped at a week, which with one completion is
        // better odds than never having run at all.
        assert!(never < month, "{never} vs {month}");
        assert!(never > day, "{never} vs {day}");
    }

    /// A pair that keeps skipping sinks below one that keeps delivering, and
    /// below one that has never run, without ever reaching zero.
    #[test]
    fn a_pair_that_always_skips_sinks() {
        let root = tempfile::tempdir().unwrap();
        let ledger = ledger(&root);
        let drawn = NOW - 30 * 24 * HOUR;
        for _ in 0..5 {
            ledger.record("bump", ForgeKind::Github, "me/dry", drawn, Outcome::Skipped);
            ledger.record(
                "bump",
                ForgeKind::Github,
                "me/rich",
                drawn,
                Outcome::Completed,
            );
        }
        let dry = weight_of(&ledger, "bump", "me/dry");
        let rich = weight_of(&ledger, "bump", "me/rich");
        let never = weight_of(&ledger, "bump", "me/never");
        assert!(dry > 0.0);
        assert!(dry < never, "{dry} vs {never}");
        assert!(never < rich, "{never} vs {rich}");
    }

    /// A turn that never got to answer moves the pair's clock and nothing
    /// else, so an outage neither rewards nor punishes a pair.
    #[test]
    fn a_turn_that_did_not_answer_only_moves_the_clock() {
        let root = tempfile::tempdir().unwrap();
        let ledger = ledger(&root);
        let drawn = NOW - 2 * HOUR;
        for outcome in [Outcome::Failed, Outcome::Timeout, Outcome::Canceled] {
            ledger.record("docs", ForgeKind::Github, "me/repo", drawn, outcome);
        }
        let entry = ledger.lock().get("github:me/repo:docs").cloned().unwrap();
        assert_eq!(
            entry,
            Entry {
                drawn,
                completed: 0,
                skipped: 0
            }
        );
        assert_eq!(
            weight_of(&ledger, "docs", "me/repo"),
            weigh(Some(&entry), NOW)
        );
    }

    /// The same pair under another forge, repository, or task is its own
    /// entry.
    #[test]
    fn entries_are_per_forge_repository_and_task() {
        let root = tempfile::tempdir().unwrap();
        let ledger = ledger(&root);
        ledger.record("docs", ForgeKind::Github, "me/repo", NOW, Outcome::Skipped);
        let drawn = weight_of(&ledger, "docs", "me/repo");
        let untouched = weight_of(&ledger, "docs", "me/never");
        assert!(drawn < untouched);
        assert_eq!(weight_of(&ledger, "coverage", "me/repo"), untouched);
        assert_eq!(weight_of(&ledger, "docs", "me/other"), untouched);
        assert_eq!(
            ledger.weight("docs", ForgeKind::Gitea, "me/repo", NOW),
            untouched
        );
    }

    #[test]
    fn entries_survive_a_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ledger.json");
        let ledger = Ledger::load(path.clone());
        ledger.record(
            "docs",
            ForgeKind::Github,
            "me/repo",
            NOW,
            Outcome::Completed,
        );
        ledger.record("docs", ForgeKind::Github, "me/repo", NOW, Outcome::Skipped);
        drop(ledger);
        let reloaded = Ledger::load(path);
        let entry = reloaded.lock().get("github:me/repo:docs").cloned();
        assert_eq!(
            entry,
            Some(Entry {
                drawn: NOW,
                completed: 1,
                skipped: 1
            })
        );
    }

    /// A ledger that can't be read is dropped, not fatal.
    #[test]
    fn a_malformed_ledger_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ledger.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(Ledger::load(path).lock().is_empty());
    }
}
