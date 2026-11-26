//! When a person — anyone but the bot — last did something on a repository,
//! so the draw can favour the repositories people are working on. A push, a
//! new issue, a comment, a merge of one of the bot's PRs: each says someone
//! is looking at the repository now, which is where a turn's work is most
//! likely to be read and most likely to matter. A repository only the bot
//! has touched in weeks is the opposite.
//!
//! Read off each forge's repository activity feed, one page per repository,
//! and held for a few minutes so back-to-back draws don't ask again. Purely
//! advisory: a feed that can't be read gives the repository no boost, never
//! an error.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::debug;

use crate::config::{Forge, ForgeKind};
use crate::precheck::{forge_rows, project};

/// How long a repository's reading stands before its feed is asked again.
/// Turns are minutes long, so this spares the forge a page per repository on
/// every draw without letting the reading go meaningfully stale.
const TTL: Duration = Duration::from_secs(10 * 60);

/// How many of the newest feed entries are looked through. The bot's own
/// entries come first on a busy run, so the page needs room past them.
const FEED_LIMIT: u32 = 50;

/// How long human activity keeps counting, in hours. The boost is at its
/// height for activity this very moment and fades linearly to nothing by
/// the end of the window.
const WINDOW_HOURS: f64 = 72.0;

/// How much heavier a repository with human activity this very moment weighs
/// than one with none in the window, less one: ten times as heavy at the
/// height, so it is drawn first about nine times in ten against a quiet one
/// with the same history.
const BOOST: f64 = 9.0;

type Key = (ForgeKind, String, String);

/// Readings keyed by forge and repository, with when each was taken.
static READINGS: Mutex<BTreeMap<Key, (Instant, Option<u64>)>> = Mutex::new(BTreeMap::new());

/// When someone other than the bot last acted on `repo`, in epoch seconds;
/// None when nobody has in the feed's newest page, or the feed can't be read.
pub fn last_seen(forge: &Forge, repo: &str) -> Option<u64> {
    let key = (forge.kind, forge.url.clone(), repo.to_owned());
    if let Some(&(taken, seen)) = crate::sync::lock(&READINGS).get(&key)
        && taken.elapsed() < TTL
    {
        return seen;
    }
    let seen = match feed(forge, repo) {
        Ok(rows) => newest_human(forge.kind, &rows, &forge.login),
        Err(err) => {
            debug!("no activity feed for {repo}: {err:#}");
            None
        }
    };
    crate::sync::lock(&READINGS).insert(key, (Instant::now(), seen));
    seen
}

/// The repository's newest activity, as each forge lists it.
fn feed(forge: &Forge, repo: &str) -> anyhow::Result<Vec<serde_json::Value>> {
    forge_rows(
        forge,
        &format!("repos/{repo}/activities/feeds?limit={FEED_LIMIT}"),
        &format!("repos/{repo}/events?per_page={FEED_LIMIT}"),
        &format!("projects/{}/events?per_page={FEED_LIMIT}", project(repo)),
    )
}

/// The newest entry in `rows` acted by anyone but `login`, in epoch seconds.
/// Each forge spells the actor and the time its own way: `act_user.login`
/// and `created` on Gitea, `actor.login` and `created_at` on GitHub,
/// `author.username` and `created_at` on GitLab. GitHub's app accounts
/// (`…[bot]`) are bots too, and an entry naming no actor is nobody's.
pub fn newest_human(kind: ForgeKind, rows: &[serde_json::Value], login: &str) -> Option<u64> {
    rows.iter()
        .filter_map(|row| {
            let (actor, at) = match kind {
                ForgeKind::Gitea => (row["act_user"]["login"].as_str(), row["created"].as_str()),
                ForgeKind::Github => (row["actor"]["login"].as_str(), row["created_at"].as_str()),
                ForgeKind::Gitlab => (
                    row["author"]["username"].as_str(),
                    row["created_at"].as_str(),
                ),
            };
            let actor = actor.filter(|actor| !actor.is_empty())?;
            if actor.eq_ignore_ascii_case(login) || actor.ends_with("[bot]") {
                return None;
            }
            crate::deliveries::parse_rfc3339(at?)
        })
        .max()
}

/// The factor a repository's weight is multiplied by at `now`, given when a
/// person last acted on it: 1 + BOOST for activity this moment, fading
/// linearly to 1 at the end of the window, and 1 for none at all. Never
/// below 1, so a quiet repository keeps the weight its history earned.
pub fn boost(seen: Option<u64>, now: u64) -> f64 {
    let Some(seen) = seen else {
        return 1.0;
    };
    let age_hours = now.saturating_sub(seen) as f64 / 3600.0;
    1.0 + BOOST * (1.0 - age_hours / WINDOW_HOURS).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_700_000_000;
    const HOUR: u64 = 3600;

    /// The bot's own entries, however new, don't count; the newest of the
    /// rest does, whatever order the feed lists them in.
    #[test]
    fn the_newest_entry_by_someone_else_counts() {
        let rows = [
            json!({"act_user": {"login": "codemine"}, "created": "2026-11-26T14:40:00-06:00"}),
            json!({"act_user": {"login": "cilki"}, "created": "2026-11-26T13:52:51-06:00"}),
            json!({"act_user": {"login": "cilki"}, "created": "2026-11-26T14:32:22-06:00"}),
        ];
        let seen = newest_human(ForgeKind::Gitea, &rows, "codemine");
        assert_eq!(
            seen,
            crate::deliveries::parse_rfc3339("2026-11-26T20:32:22Z")
        );
    }

    /// A feed with only the bot in it, an app account, or entries naming
    /// no actor or no time, is no human activity.
    #[test]
    fn bots_and_anonymous_entries_are_nobody() {
        let rows = [
            json!({"actor": {"login": "CodeMine"}, "created_at": "2026-11-26T14:40:00Z"}),
            json!({"actor": {"login": "dependabot[bot]"}, "created_at": "2026-11-26T14:40:00Z"}),
            json!({"actor": {"login": ""}, "created_at": "2026-11-26T14:40:00Z"}),
            json!({"created_at": "2026-11-26T14:40:00Z"}),
            json!({"actor": {"login": "cilki"}}),
        ];
        assert_eq!(newest_human(ForgeKind::Github, &rows, "codemine"), None);
    }

    /// Each forge's spelling of the actor and the time is the one read.
    #[test]
    fn each_forge_spells_its_feed_its_own_way() {
        let at = "2026-11-26T14:40:00Z";
        let expected = crate::deliveries::parse_rfc3339(at);
        let gitlab = [json!({"author": {"username": "cilki"}, "created_at": at})];
        let github = [json!({"actor": {"login": "cilki"}, "created_at": at})];
        assert_eq!(
            newest_human(ForgeKind::Gitlab, &gitlab, "codemine"),
            expected
        );
        assert_eq!(
            newest_human(ForgeKind::Github, &github, "codemine"),
            expected
        );
        // The same rows read under another forge's spelling name nobody.
        assert_eq!(newest_human(ForgeKind::Gitea, &github, "codemine"), None);
    }

    /// The boost is highest for activity now, fades with age, is gone once
    /// the window is past, and never drops a repository below its own
    /// weight.
    #[test]
    fn the_boost_fades_over_the_window() {
        let now = boost(Some(NOW), NOW);
        let day = boost(Some(NOW - 24 * HOUR), NOW);
        let gone = boost(Some(NOW - 30 * 24 * HOUR), NOW);
        assert_eq!(now, 1.0 + BOOST);
        assert!(1.0 < day && day < now, "{day}");
        assert_eq!(gone, 1.0);
        assert_eq!(boost(None, NOW), 1.0);
        // A stamp from a clock running ahead reads as now, not more.
        assert_eq!(boost(Some(NOW + HOUR), NOW), 1.0 + BOOST);
    }
}
