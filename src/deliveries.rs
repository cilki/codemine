//! What the bot's turns delivered, followed past the turn that delivered it.
//!
//! A turn is recorded as Completed the moment the agent signs off, which
//! says nothing about whether the PR it opened was merged or closed unread.
//! This keeps the PRs and issues each completed turn opened, asks the forge
//! now and then what became of the ones still open, and adds the answers up
//! per task, so the page can show which tasks pay for the tokens they spend.
//!
//! Kept in `deliveries.json` at the workspace root, next to the skip cache,
//! since the turn list is gone with every restart and this is worth keeping
//! across them. Losing it costs the history and nothing else, so a malformed
//! file is logged and dropped rather than failing startup.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::{Forge, ForgeKind};
use crate::status::TokenUsage;

/// How often a delivery that is still open is asked about again. A PR is
/// reviewed on the scale of a day, so an hour is quick enough for the page
/// and costs one request per open delivery per hour.
const RECHECK_SECS: u64 = 3600;

/// How far before the turn's start a PR may have been created and still be
/// the turn's: the forge's clock and the runner's need not agree. A PR that
/// an earlier turn opened inside the slack is already recorded, and a
/// delivery is recorded once whichever turn finds it, so the slack can't
/// count anything twice.
pub const CLOCK_SLACK_SECS: u64 = 120;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Pr,
    Issue,
}

/// What became of a delivery. An issue is never `Merged`: closing one says
/// nothing about whether it was acted on, so issues are counted apart.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Open,
    Merged,
    Closed,
}

/// A PR or issue a turn opened, as the turn's record names it.
#[derive(Serialize, Clone, PartialEq, Eq, Debug)]
pub struct Opened {
    pub kind: Kind,
    pub number: u64,
    pub url: String,
}

/// The branch a PR was opened from, as the forge's answer for the PR names
/// it: what a settled PR's branch is checked against before it is deleted.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Head {
    pub branch: String,
    /// The PR's last head commit; a branch that has moved past it since is
    /// somebody's live work.
    pub sha: String,
    /// Whether the branch lives in the PR's own repository, not a fork.
    pub same_repo: bool,
}

/// What the forge says became of one delivery, and for a PR, its head.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Answer {
    pub state: State,
    pub head: Option<Head>,
}

/// One delivery as the file keeps it: what the summary counts it under and
/// what settling it needs. The page links a delivery from its turn's record,
/// and what a turn spent is in its task's tally, so neither is kept here.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct Delivery {
    forge: ForgeKind,
    repo: String,
    task: String,
    kind: Kind,
    number: u64,
    state: State,
    /// When the forge was last asked about it, which paces the rechecks.
    checked: u64,
}

impl Delivery {
    /// The page's key for it: the forge, repository, kind and number, since
    /// GitLab numbers its issues and merge requests apart.
    fn key(&self) -> String {
        key(self.forge, &self.repo, self.kind, self.number)
    }
}

fn key(forge: ForgeKind, repo: &str, kind: Kind, number: u64) -> String {
    let kind = match kind {
        Kind::Pr => "pr",
        Kind::Issue => "issue",
    };
    format!("{}:{repo}:{kind}:{number}", forge.name())
}

/// Every turn a task took and what they spent together, delivered or not:
/// what the deliveries are weighed against.
#[derive(Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Tally {
    turns: u64,
    tokens: u64,
}

#[derive(Serialize, Deserialize, Default, PartialEq, Debug)]
struct Ledger {
    #[serde(default)]
    tasks: BTreeMap<String, Tally>,
    #[serde(default)]
    deliveries: Vec<Delivery>,
}

/// One task's line in the page's summary.
#[derive(Serialize, Default, PartialEq, Eq, Debug)]
pub struct TaskSummary {
    pub task: String,
    pub turns: u64,
    pub tokens: u64,
    pub prs: u64,
    pub merged: u64,
    /// PRs closed without being merged.
    pub closed: u64,
    /// PRs still open.
    pub open: u64,
    pub issues: u64,
    pub issues_closed: u64,
    /// Everything the task spent per PR of its that was merged; None while
    /// none has been, rather than a division by zero.
    pub tokens_per_merged: Option<u64>,
}

/// The deliveries and the per-task tallies, shared between the main loop,
/// which records and settles them, and the web UI, which shows them.
pub struct Deliveries {
    path: PathBuf,
    ledger: Mutex<Ledger>,
    /// Bumped whenever what the page shows moves, so the event stream can
    /// tell without serializing the lot to compare.
    revision: AtomicU64,
}

impl Deliveries {
    pub fn load(path: PathBuf) -> Deliveries {
        let ledger = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                warn!("ignoring {}: {err}", path.display());
                Ledger::default()
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ledger::default(),
            Err(err) => {
                warn!("failed to read {}: {err}", path.display());
                Ledger::default()
            }
        };
        Deliveries {
            path,
            ledger: Mutex::new(ledger),
            revision: AtomicU64::new(0),
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    /// Count a finished turn against its task, and keep what it opened. A
    /// delivery already on record — one an earlier turn opened inside the
    /// clock slack — stays that turn's.
    pub fn record_turn(
        &self,
        forge: ForgeKind,
        repo: &str,
        task: &str,
        started: u64,
        tokens: Option<&TokenUsage>,
        opened: &[Opened],
    ) {
        let tokens = tokens.map_or(0, total);
        let mut ledger = crate::sync::lock(&self.ledger);
        let tally = ledger.tasks.entry(task.to_owned()).or_default();
        tally.turns += 1;
        tally.tokens += tokens;
        for opened in opened {
            let fresh = key(forge, repo, opened.kind, opened.number);
            if ledger.deliveries.iter().any(|d| d.key() == fresh) {
                continue;
            }
            info!("{task} on {repo} opened {fresh}");
            ledger.deliveries.push(Delivery {
                forge,
                repo: repo.to_owned(),
                task: task.to_owned(),
                kind: opened.kind,
                number: opened.number,
                state: State::Open,
                checked: started,
            });
        }
        self.save(&ledger);
    }

    /// Ask the forge about every open delivery on it that hasn't been asked
    /// about in the last `RECHECK_SECS`. Settled ones are never asked about
    /// again. A failed request is tried again after the same pause rather
    /// than on every pass, so a deleted repository costs one request an hour.
    ///
    /// A PR found merged or closed has its head branch deleted, once, at the
    /// pass that settles it; see [`crate::precheck::delete_head_branch`] for
    /// what keeps a branch. A failed deletion is logged and not tried again:
    /// it never holds the delivery open.
    pub fn settle(&self, forge: &Forge, now: u64) {
        self.settle_with(
            forge.kind,
            now,
            |repo, kind, number| crate::precheck::delivery_state(forge, repo, kind, number),
            |repo, head| crate::precheck::delete_head_branch(forge, repo, head),
        );
    }

    fn settle_with(
        &self,
        forge: ForgeKind,
        now: u64,
        mut ask: impl FnMut(&str, Kind, u64) -> anyhow::Result<Answer>,
        mut delete: impl FnMut(&str, &Head) -> anyhow::Result<Option<String>>,
    ) {
        let due: Vec<(String, Kind, u64)> = crate::sync::lock(&self.ledger)
            .deliveries
            .iter()
            .filter(|d| d.forge == forge && d.state == State::Open)
            .filter(|d| d.checked + RECHECK_SECS <= now)
            .map(|d| (d.repo.clone(), d.kind, d.number))
            .collect();
        if due.is_empty() {
            return;
        }
        // Asked with the lock released: these are network round trips, and
        // the page reads the ledger meanwhile.
        let answers: Vec<_> = due
            .into_iter()
            .map(|(repo, kind, number)| {
                let key = key(forge, &repo, kind, number);
                let answer = ask(&repo, kind, number).map(|answer| {
                    // Only due deliveries were asked about, and only open
                    // ones are due, so any settled answer is the transition.
                    let settled = kind == Kind::Pr && answer.state != State::Open;
                    if let Some(head) = answer.head.as_ref().filter(|_| settled) {
                        let branch = &head.branch;
                        match delete(&repo, head) {
                            Ok(None) => {
                                info!("deleted branch {branch} of {key} ({:?})", answer.state)
                            }
                            Ok(Some(why)) => info!("kept branch {branch} of {key}: {why}"),
                            Err(err) => warn!("failed to delete branch {branch} of {key}: {err:#}"),
                        }
                    }
                    answer.state
                });
                (key, answer)
            })
            .collect();
        let mut ledger = crate::sync::lock(&self.ledger);
        for (key, answer) in answers {
            let Some(delivery) = ledger.deliveries.iter_mut().find(|d| d.key() == key) else {
                continue;
            };
            delivery.checked = now;
            match answer {
                Ok(State::Open) => {}
                Ok(state) => {
                    info!("{key} is now {state:?}");
                    delivery.state = state;
                }
                Err(err) => warn!("failed to ask what became of {key}: {err:#}"),
            }
        }
        self.save(&ledger);
    }

    /// What the page shows: each delivery's state by its key, for the turn
    /// rows, and one line per task.
    pub fn view(&self) -> serde_json::Value {
        let ledger = crate::sync::lock(&self.ledger);
        let states: BTreeMap<String, State> = ledger
            .deliveries
            .iter()
            .map(|d| (d.key(), d.state))
            .collect();
        serde_json::json!({ "states": states, "summary": summary(&ledger) })
    }

    /// Write the ledger back out. A failure costs the history after the next
    /// restart, so it is logged rather than propagated; the revision moves
    /// either way, since what the page shows did.
    fn save(&self, ledger: &Ledger) {
        self.revision.fetch_add(1, Ordering::Relaxed);
        if let Err(err) = crate::workspace::write_json(&self.path, ledger) {
            warn!("failed to save the deliveries: {err:#}");
        }
    }
}

fn total(t: &TokenUsage) -> u64 {
    t.input + t.output + t.reasoning + t.cache_read + t.cache_write
}

fn summary(ledger: &Ledger) -> Vec<TaskSummary> {
    let mut lines: BTreeMap<&str, TaskSummary> = BTreeMap::new();
    for (task, tally) in &ledger.tasks {
        let line = lines.entry(task).or_default();
        line.turns = tally.turns;
        line.tokens = tally.tokens;
    }
    for d in &ledger.deliveries {
        let line = lines.entry(&d.task).or_default();
        match (d.kind, d.state) {
            (Kind::Pr, state) => {
                line.prs += 1;
                match state {
                    State::Open => line.open += 1,
                    State::Merged => line.merged += 1,
                    State::Closed => line.closed += 1,
                }
            }
            (Kind::Issue, State::Open) => line.issues += 1,
            (Kind::Issue, _) => {
                line.issues += 1;
                line.issues_closed += 1;
            }
        }
    }
    lines
        .into_iter()
        .map(|(task, mut line)| {
            line.task = task.to_owned();
            line.tokens_per_merged = (line.merged > 0).then(|| line.tokens / line.merged);
            line
        })
        .collect()
}

/// The PRs and issues in one page of a forge listing that `login` opened at
/// or after `since`. `kind` is the listing's when it holds one kind only
/// (GitLab's two); Gitea's and GitHub's issue listings hold both, and mark a
/// PR by its `pull_request` member.
pub fn opened_in(
    rows: &[serde_json::Value],
    login: &str,
    since: u64,
    kind: Option<Kind>,
) -> Vec<Opened> {
    rows.iter()
        .filter(|row| crate::precheck::authored_by(row, login))
        .filter(|row| {
            row["created_at"]
                .as_str()
                .and_then(parse_rfc3339)
                .is_some_and(|created| created >= since)
        })
        .filter_map(|row| {
            Some(Opened {
                kind: kind.unwrap_or(match row["pull_request"].is_null() {
                    true => Kind::Issue,
                    false => Kind::Pr,
                }),
                number: row["iid"].as_u64().or_else(|| row["number"].as_u64())?,
                url: row["html_url"]
                    .as_str()
                    .or_else(|| row["web_url"].as_str())
                    .unwrap_or_default()
                    .to_owned(),
            })
        })
        .collect()
}

/// What a forge's answer for one PR or issue says became of it: Gitea's and
/// GitHub's PR endpoint (which says whether it merged at the top level), or
/// their issue endpoint (which answers for PRs too, with a `pull_request`
/// member saying so), or GitLab's merge request or issue. None for an answer
/// of no shape these know.
pub fn state_of(item: &serde_json::Value) -> Option<State> {
    let merged = |of: &serde_json::Value| {
        of["merged"].as_bool() == Some(true) || of["merged_at"].is_string()
    };
    match item["state"].as_str()? {
        // GitLab's `locked` is a merge request in the middle of merging.
        "open" | "opened" | "locked" => Some(State::Open),
        "merged" => Some(State::Merged),
        "closed" if merged(item) || merged(&item["pull_request"]) => Some(State::Merged),
        "closed" => Some(State::Closed),
        _ => None,
    }
}

/// The head branch a forge's answer for one PR names: `head.ref` and
/// `head.sha` on Gitea's and GitHub's PR endpoint, `source_branch` and `sha`
/// on a GitLab merge request. The branch is the PR's own repository's when
/// the head and base repositories are the same one — a fork's, or one whose
/// repository is gone (`head.repo` null), is not. None for an answer that
/// names no head, such as an issue's.
pub fn head_of(item: &serde_json::Value) -> Option<Head> {
    let (branch, sha, same_repo) = match item["source_branch"].as_str() {
        Some(branch) => {
            let source = item["source_project_id"].as_u64();
            (
                branch,
                item["sha"].as_str()?,
                source.is_some() && source == item["target_project_id"].as_u64(),
            )
        }
        None => {
            let head = &item["head"];
            let repo = head["repo"]["id"].as_u64();
            let same = repo.is_some() && repo == item["base"]["repo"]["id"].as_u64();
            (head["ref"].as_str()?, head["sha"].as_str()?, same)
        }
    };
    Some(Head {
        branch: branch.to_owned(),
        sha: sha.to_owned(),
        same_repo,
    })
}

/// Epoch seconds as RFC 3339 in UTC, which every forge's `since` and
/// `created_after` filter reads.
pub fn format_rfc3339(epoch: u64) -> String {
    let days = (epoch / 86400) as i64;
    let secs = epoch % 86400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// An RFC 3339 timestamp as epoch seconds, the way the forges write them:
/// `2026-11-22T19:50:00Z`, with a fraction (GitLab) or an offset (Gitea).
fn parse_rfc3339(s: &str) -> Option<u64> {
    fn num(s: &str, range: std::ops::Range<usize>) -> Option<i64> {
        let part = s.get(range)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    }
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let (y, mo, d) = (num(s, 0..4)?, num(s, 5..7)?, num(s, 8..10)?);
    let (h, mi, sec) = (num(s, 11..13)?, num(s, 14..16)?, num(s, 17..19)?);
    let mut rest = s.get(19..)?;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        rest = &fraction[digits..];
    }
    let offset = match rest.as_bytes() {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
            let secs = num(rest, 1..3)? * 3600 + num(rest, 4..6)? * 60;
            if *sign == b'+' { secs } else { -secs }
        }
        _ => return None,
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let epoch = days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec - offset;
    u64::try_from(epoch).ok()
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;

    fn ledger() -> (tempfile::TempDir, Deliveries) {
        let dir = tempfile::tempdir().unwrap();
        let deliveries = Deliveries::load(dir.path().join("deliveries.json"));
        (dir, deliveries)
    }

    fn pr(number: u64) -> Opened {
        Opened {
            kind: Kind::Pr,
            number,
            url: format!("https://forge.example/me/repo/pulls/{number}"),
        }
    }

    fn tokens(input: u64) -> TokenUsage {
        TokenUsage {
            input,
            cache_read: input,
            ..Default::default()
        }
    }

    impl Deliveries {
        /// Settle on bare states: answers that name no head, so a deletion
        /// asked for here is a bug.
        fn settle_states(
            &self,
            forge: ForgeKind,
            now: u64,
            mut ask: impl FnMut(&str, Kind, u64) -> anyhow::Result<State>,
        ) {
            self.settle_with(
                forge,
                now,
                |repo, kind, number| {
                    ask(repo, kind, number).map(|state| Answer { state, head: None })
                },
                |_, head| panic!("asked to delete {head:?} with no head named"),
            );
        }
    }

    fn head(branch: &str) -> Head {
        Head {
            branch: branch.into(),
            sha: format!("{branch}-tip"),
            same_repo: true,
        }
    }

    /// A settled PR's branch is handed to `delete` once, at the pass that
    /// settles it, whether it merged or closed unmerged: an issue never, an
    /// open PR never. A failed or refused deletion still settles the
    /// delivery and isn't retried.
    #[test]
    fn a_settled_prs_branch_is_deleted_once() {
        let (_dir, ledger) = ledger();
        let issue = Opened {
            kind: Kind::Issue,
            ..pr(5)
        };
        ledger.record_turn(
            ForgeKind::Gitea,
            "me/repo",
            "docs",
            0,
            None,
            &[pr(1), pr(2), pr(3), pr(4), issue],
        );
        let ask = |_: &str, kind: Kind, number: u64| {
            Ok(Answer {
                state: match number {
                    1 | 3 | 5 => State::Merged,
                    2 => State::Closed,
                    _ => State::Open,
                },
                // The issue answers with a head too, to show that its kind,
                // not the answer's shape, keeps it out.
                head: Some(head(&format!("{kind:?}-{number}"))),
            })
        };
        let deleted = RefCell::new(Vec::new());
        let delete = |repo: &str, head: &Head| {
            assert_eq!(repo, "me/repo");
            deleted.borrow_mut().push(head.branch.clone());
            match head.branch.as_str() {
                "Pr-3" => anyhow::bail!("forge refused"),
                _ => Ok(None),
            }
        };
        ledger.settle_with(ForgeKind::Gitea, RECHECK_SECS, ask, delete);
        // Asked again later: the settled ones aren't, so nothing more is
        // deleted, the failed one included.
        ledger.settle_with(ForgeKind::Gitea, 2 * RECHECK_SECS, ask, delete);
        let view = ledger.view();
        assert_eq!(view["states"]["gitea:me/repo:pr:3"], "merged");
        assert_eq!(view["states"]["gitea:me/repo:pr:4"], "open");
        assert_eq!(deleted.into_inner(), ["Pr-1", "Pr-2", "Pr-3"]);
    }

    /// Each forge's PR answer names its head, and tells the PR's own
    /// repository's branch from a fork's or a deleted repository's.
    #[test]
    fn each_forges_pr_answer_names_its_head() {
        let pull = |head_repo: serde_json::Value| {
            json!({"state": "closed", "merged": true,
                   "head": {"ref": "topic", "sha": "abc", "repo": head_repo},
                   "base": {"ref": "main", "repo": {"id": 7}}})
        };
        let own = head_of(&pull(json!({"id": 7}))).unwrap();
        assert_eq!(
            own,
            Head {
                branch: "topic".into(),
                sha: "abc".into(),
                same_repo: true
            }
        );
        assert!(!head_of(&pull(json!({"id": 8}))).unwrap().same_repo);
        assert!(!head_of(&pull(json!(null))).unwrap().same_repo);
        // The PR endpoint says it merged at the top level.
        assert_eq!(state_of(&pull(json!({"id": 7}))), Some(State::Merged));

        let mr = |source: u64| {
            json!({"state": "merged", "source_branch": "topic", "sha": "def",
                   "source_project_id": source, "target_project_id": 3})
        };
        let own = head_of(&mr(3)).unwrap();
        assert_eq!(
            (own.branch.as_str(), own.sha.as_str(), own.same_repo),
            ("topic", "def", true)
        );
        assert!(!head_of(&mr(4)).unwrap().same_repo);

        assert_eq!(
            head_of(&json!({"state": "closed", "pull_request": null})),
            None
        );
    }

    /// Only what the bot opened since the turn started is the turn's: an
    /// older PR of its that the turn merely updated, and somebody else's
    /// fresh one, are not. Gitea's and GitHub's issue listing holds both
    /// kinds and marks a PR by `pull_request`; GitLab's listings hold one
    /// kind each and number by `iid`.
    #[test]
    fn only_what_the_bot_opened_since_the_start_is_attributed() {
        let since = parse_rfc3339("2026-11-22T19:00:00Z").unwrap();
        let rows = [
            // Gitea writes an offset: 13:30 at -06:00 is 19:30 UTC.
            json!({"number": 107, "user": {"login": "bot"}, "created_at": "2026-11-22T13:30:00-06:00",
                   "html_url": "https://forge.example/me/repo/pulls/107", "pull_request": {"merged": false}}),
            json!({"number": 108, "user": {"login": "bot"}, "created_at": "2026-11-22T19:05:00Z",
                   "html_url": "https://forge.example/me/repo/issues/108", "pull_request": null}),
            // Opened before the turn, updated during it.
            json!({"number": 99, "user": {"login": "bot"}, "created_at": "2026-11-22T12:59:59-06:00",
                   "pull_request": {}}),
            json!({"number": 109, "user": {"login": "someone"}, "created_at": "2026-11-22T19:10:00Z"}),
            json!({"number": 110, "user": {"login": "bot"}}),
        ];
        assert_eq!(
            opened_in(&rows, "bot", since, None),
            [
                Opened {
                    kind: Kind::Pr,
                    number: 107,
                    url: "https://forge.example/me/repo/pulls/107".into()
                },
                Opened {
                    kind: Kind::Issue,
                    number: 108,
                    url: "https://forge.example/me/repo/issues/108".into()
                },
            ]
        );
        let gitlab = [json!({"iid": 4, "id": 9000, "author": {"username": "bot"},
                             "created_at": "2026-11-22T19:00:00.123Z", "web_url": "https://gl/mr/4"})];
        assert_eq!(
            opened_in(&gitlab, "bot", since, Some(Kind::Pr)),
            [Opened {
                kind: Kind::Pr,
                number: 4,
                url: "https://gl/mr/4".into()
            }]
        );
    }

    /// Each forge's answer for one PR or issue, mapped to what became of it.
    #[test]
    fn each_forges_answer_reads_as_a_state() {
        // Gitea's issue endpoint, for a PR and for an issue.
        let gitea = |state: &str, merged: bool| json!({"state": state, "pull_request": {"merged": merged, "merged_at": null}});
        assert_eq!(state_of(&gitea("open", false)), Some(State::Open));
        assert_eq!(state_of(&gitea("closed", true)), Some(State::Merged));
        assert_eq!(state_of(&gitea("closed", false)), Some(State::Closed));
        assert_eq!(
            state_of(&json!({"state": "closed", "pull_request": null})),
            Some(State::Closed)
        );
        // GitHub's says so by `merged_at` alone.
        let github =
            json!({"state": "closed", "pull_request": {"merged_at": "2026-11-22T19:00:00Z"}});
        assert_eq!(state_of(&github), Some(State::Merged));
        let github = json!({"state": "closed", "pull_request": {"merged_at": null}});
        assert_eq!(state_of(&github), Some(State::Closed));
        // GitLab's merge request and issue.
        for (state, want) in [
            ("opened", State::Open),
            ("locked", State::Open),
            ("merged", State::Merged),
            ("closed", State::Closed),
        ] {
            assert_eq!(state_of(&json!({"state": state})), Some(want), "{state}");
        }
        assert_eq!(state_of(&json!({"message": "404 Not found"})), None);
        assert_eq!(state_of(&json!({"state": "weird"})), None);
    }

    /// An open delivery is asked about once the recheck pause is up, a
    /// settled one never again, and a failed answer waits out the same pause
    /// rather than being retried on every pass.
    #[test]
    fn settling_asks_only_about_open_deliveries_that_are_due() {
        let (_dir, ledger) = ledger();
        ledger.record_turn(
            ForgeKind::Gitea,
            "me/repo",
            "docs",
            1000,
            None,
            &[pr(1), pr(2), pr(3)],
        );
        // Another forge's delivery is that forge's to ask about.
        ledger.record_turn(ForgeKind::Github, "me/repo", "docs", 1000, None, &[pr(4)]);

        let asked = RefCell::new(Vec::new());
        let answer = |_: &str, _: Kind, number: u64| {
            asked.borrow_mut().push(number);
            match number {
                1 => Ok(State::Merged),
                2 => Ok(State::Open),
                _ => anyhow::bail!("forge down"),
            }
        };
        ledger.settle_states(ForgeKind::Gitea, 1000 + RECHECK_SECS - 1, answer);
        assert!(asked.borrow().is_empty(), "asked before the pause was up");

        let now = 1000 + RECHECK_SECS;
        ledger.settle_states(ForgeKind::Gitea, now, answer);
        assert_eq!(*asked.borrow(), [1, 2, 3]);

        asked.borrow_mut().clear();
        ledger.settle_states(ForgeKind::Gitea, now + 60, answer);
        assert!(
            asked.borrow().is_empty(),
            "a checked delivery was asked again at once"
        );

        ledger.settle_states(ForgeKind::Gitea, now + RECHECK_SECS, answer);
        assert_eq!(
            *asked.borrow(),
            [2, 3],
            "the merged PR was asked about again"
        );

        let view = ledger.view();
        assert_eq!(view["states"]["gitea:me/repo:pr:1"], "merged");
        assert_eq!(view["states"]["gitea:me/repo:pr:2"], "open");
        assert_eq!(view["states"]["github:me/repo:pr:4"], "open");
    }

    /// The summary counts every turn and every delivery per task, weighs
    /// what the task spent against its merged PRs, and leaves that blank
    /// rather than dividing by zero for a task none of whose PRs merged.
    #[test]
    fn the_summary_counts_per_task_and_never_divides_by_zero() {
        let (_dir, ledger) = ledger();
        let issue = Opened {
            kind: Kind::Issue,
            ..pr(10)
        };
        ledger.record_turn(
            ForgeKind::Gitea,
            "me/repo",
            "simplify",
            0,
            Some(&tokens(100)),
            &[pr(1)],
        );
        ledger.record_turn(
            ForgeKind::Gitea,
            "me/repo",
            "simplify",
            0,
            Some(&tokens(50)),
            &[pr(2)],
        );
        ledger.record_turn(
            ForgeKind::Gitea,
            "me/repo",
            "simplify",
            0,
            Some(&tokens(25)),
            &[],
        );
        ledger.record_turn(
            ForgeKind::Gitea,
            "me/repo",
            "feature",
            0,
            Some(&tokens(10)),
            &[issue],
        );
        ledger.record_turn(ForgeKind::Gitea, "me/repo", "benchmark", 0, None, &[pr(3)]);
        // Found again by a later turn inside the clock slack: still the
        // first turn's, and counted once.
        ledger.record_turn(ForgeKind::Gitea, "me/repo", "benchmark", 0, None, &[pr(3)]);
        ledger.settle_states(ForgeKind::Gitea, RECHECK_SECS, |_, _, number| {
            Ok(match number {
                1 => State::Merged,
                3 | 10 => State::Closed,
                _ => State::Open,
            })
        });

        let view = ledger.view();
        let line = |task: &str| {
            view["summary"]
                .as_array()
                .unwrap()
                .iter()
                .find(|line| line["task"] == task)
                .cloned()
                .unwrap()
        };
        let simplify = line("simplify");
        assert_eq!(simplify["turns"], 3);
        assert_eq!(simplify["tokens"], 350);
        assert_eq!(
            (
                simplify["prs"].clone(),
                simplify["merged"].clone(),
                simplify["open"].clone()
            ),
            (json!(2), json!(1), json!(1))
        );
        assert_eq!(simplify["tokens_per_merged"], 350);
        let benchmark = line("benchmark");
        assert_eq!(
            (
                benchmark["turns"].clone(),
                benchmark["prs"].clone(),
                benchmark["closed"].clone()
            ),
            (json!(2), json!(1), json!(1))
        );
        assert!(benchmark["tokens_per_merged"].is_null());
        let feature = line("feature");
        assert_eq!(
            (
                feature["prs"].clone(),
                feature["issues"].clone(),
                feature["issues_closed"].clone()
            ),
            (json!(0), json!(1), json!(1))
        );
        assert!(feature["tokens_per_merged"].is_null());
    }

    /// The ledger outlives the process, and a file it can't read is dropped
    /// rather than failing startup.
    #[test]
    fn the_ledger_survives_a_restart_and_a_malformed_one_loads_empty() {
        let (dir, ledger) = ledger();
        ledger.record_turn(
            ForgeKind::Gitea,
            "me/repo",
            "docs",
            0,
            Some(&tokens(5)),
            &[pr(7)],
        );
        let path = dir.path().join("deliveries.json");
        let reloaded = Deliveries::load(path.clone());
        assert_eq!(reloaded.view(), ledger.view());
        assert_eq!(reloaded.view()["summary"][0]["prs"], 1);

        std::fs::write(&path, "{ not json").unwrap();
        let reloaded = Deliveries::load(path);
        assert_eq!(reloaded.view(), json!({"states": {}, "summary": []}));
    }

    #[test]
    fn timestamps_round_trip_the_way_the_forges_write_them() {
        let epoch = 1_795_398_609; // 2026-11-23T01:50:09Z
        assert_eq!(format_rfc3339(epoch), "2026-11-23T01:50:09Z");
        assert_eq!(parse_rfc3339("2026-11-23T01:50:09Z"), Some(epoch));
        assert_eq!(parse_rfc3339("2026-11-22T19:50:09-06:00"), Some(epoch));
        assert_eq!(parse_rfc3339("2026-11-23T07:20:09+05:30"), Some(epoch));
        assert_eq!(parse_rfc3339("2026-11-23T01:50:09.456Z"), Some(epoch));
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        for bad in [
            "",
            "2026-11-23",
            "2026-11-23T01:50:09",
            "2026-13-23T01:50:09Z",
            "2026-11-23T01:50:09+0600",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?}");
        }
    }
}
