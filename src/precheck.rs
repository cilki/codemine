//! Cheap preconditions for tasks that often have nothing to act on, so the
//! runner can tell before burning an agent session discovering it. The same
//! probes decide priority: a gated task with work waiting outranks whatever a
//! blind draw would have picked.

use std::process::Command;

use anyhow::{Context, Result, bail};
use tracing::{debug, info};

use crate::cache::gated;
use crate::config::{Forge, ForgeKind};

/// Open PRs examined per repository; the bot's own open PRs, and the ones it
/// is asked to review, fit well inside one page.
const PR_LIMIT: usize = 20;

/// Draw a (task, forge, repo) triple worth an agent turn. The gated tasks are
/// probed first — every enabled one against every repository, in a shuffled
/// order — and the first pair with work wins, so responsive work (a
/// conflicting PR, a review comment, a review request) is never waiting on a
/// lucky draw. Only once nothing is pending do the rest get their turn, as
/// one shuffled pass over every (task, repository) pair: `check` waves most
/// of them straight through, so this is the uniform draw it used to be, but
/// a pair the skip cache has already answered for is passed over instead of
/// drawn. The cache applies to the gated pass too — every gated task is
/// probed out of the same state its memory is keyed on — so a gated task can
/// no longer monopolize the draw on a precondition that stays true. None
/// when nothing at all is worth a turn.
pub fn draw<'a>(
    tasks: &'a [String],
    pool: &'a [(&'a Forge, String)],
    check: impl Fn(&str, &Forge, &str) -> bool,
) -> Option<(&'a str, &'a Forge, &'a str)> {
    let candidates = |want_gated: bool| {
        let mut candidates: Vec<(&'a str, &'a Forge, &'a str)> = tasks
            .iter()
            .filter(|task| gated(task) == want_gated)
            .flat_map(|task| {
                pool.iter()
                    .map(move |(forge, repo)| (task.as_str(), *forge, repo.as_str()))
            })
            .collect();
        fastrand::shuffle(&mut candidates);
        candidates
    };
    for (task, forge, repo) in candidates(true).into_iter().chain(candidates(false)) {
        if check(task, forge, repo) {
            return Some((task, forge, repo));
        }
        // A gated miss is worth a line of its own: there are few of them and
        // each cost a forge round trip. The ungated pass can cover every task
        // on every repository, so its misses stay at debug.
        match gated(task) {
            true => info!("nothing for {task} on {repo}"),
            false => debug!("nothing for {task} on {repo}"),
        }
    }
    None
}

/// One GET against a forge's API, through whichever CLI speaks it: `tea` has
/// no generic api subcommand so Gitea goes over HTTP, while gh and glab
/// share the same `api` shape. Each forge spells the same question its own
/// way, so the caller gives all three paths and only its forge's is used.
pub fn forge_json(
    forge: &Forge,
    gitea: &str,
    github: &str,
    gitlab: &str,
) -> Result<serde_json::Value> {
    match forge.kind {
        ForgeKind::Gitea => gitea_json(forge, gitea),
        ForgeKind::Github => api_json(forge, "gh", github),
        ForgeKind::Gitlab => api_json(forge, "glab", gitlab),
    }
}

/// GitLab identifies a project by its URL-encoded path.
fn project(repo: &str) -> String {
    repo.replace('/', "%2F")
}

/// The commit the repository's default branch points at. Every forge's
/// commit listing defaults to that branch, so one page of one commit answers
/// it; GitHub and Gitea name the field `sha`, GitLab `id`.
pub fn head_sha(forge: &Forge, repo: &str) -> Result<String> {
    let commits = forge_json(
        forge,
        &format!("repos/{repo}/commits?limit=1&stat=false"),
        &format!("repos/{repo}/commits?per_page=1"),
        &format!("projects/{}/repository/commits?per_page=1", project(repo)),
    )?;
    first_sha(&commits).context("the commit listing named no commit")
}

/// The first commit's SHA in a listing, under either forge spelling of the
/// field; None for an empty listing (a repository with no commits yet) or any
/// other shape.
fn first_sha(commits: &serde_json::Value) -> Option<String> {
    let first = commits.as_array()?.first()?;
    first["sha"]
        .as_str()
        .or_else(|| first["id"].as_str())
        .map(str::to_owned)
}

/// One page of the repository's open PRs, as every forge spells that listing.
/// Both PR probes read this same page — `rebase` looks through it for the
/// bot's own conflicting PRs, `review` for the ones it has been asked
/// about — so a draw fetches it once per repository and answers both
/// questions off it rather than making the identical request twice.
///
/// An unresolved login is an error rather than an empty page: it would
/// silently match no author and no reviewer at all, where bailing routes
/// through the probes' fail-closed arm. So is a listing that isn't an array,
/// since a forge that answered with something else never said the bot has
/// nothing waiting.
pub fn open_prs(forge: &Forge, repo: &str) -> Result<Vec<serde_json::Value>> {
    if forge.login.is_empty() {
        bail!("the {} login is unresolved", forge.kind.name());
    }
    let listing = forge_json(
        forge,
        // Gitea's listing carries `mergeable`, which also reads false while
        // its conflict check is still running and for draft PRs; the worst
        // case is one dispatched turn whose skip is then remembered until
        // the branch moves.
        &format!("repos/{repo}/pulls?state=open&limit={PR_LIMIT}"),
        &format!("repos/{repo}/pulls?state=open&per_page={PR_LIMIT}"),
        // The listing's `has_conflicts` is GitLab's cached merge status; the
        // recheck parameter schedules an async refresh so a stale answer
        // heals by the next probe. GitLab could also narrow the listing to
        // MRs the bot is a reviewer on, but that would be a second page for
        // the same repository: `requested_from` reads the `reviewers` this
        // one already carries.
        &format!(
            "projects/{}/merge_requests?state=opened&per_page={PR_LIMIT}&with_merge_status_recheck=true",
            project(repo)
        ),
    )?;
    pulls(listing)
}

/// The entries of a PR listing. Not an array at all is an error, so both
/// probes fail closed on it instead of reading it as an empty page.
fn pulls(listing: serde_json::Value) -> Result<Vec<serde_json::Value>> {
    match listing {
        serde_json::Value::Array(prs) => Ok(prs),
        _ => bail!("expected an array of pulls"),
    }
}

/// The bot's open PRs that conflict with their base branch, as sorted
/// `number@head-sha` keys, read off the page [`open_prs`] fetched. A branch
/// that is merely behind but merges cleanly doesn't count: there is nothing a
/// rebase turn would have to resolve. The set doubles as `rebase`'s
/// precondition — non-empty means work — and as the state a skipped turn is
/// remembered against.
pub fn conflicting_prs(
    forge: &Forge,
    repo: &str,
    open: &[serde_json::Value],
) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for pr in open.iter().filter(|pr| authored_by(pr, &forge.login)) {
        // GitHub's listing is the one that carries no mergeability, so each
        // of the bot's PRs costs a GET of its own. That GET both reads
        // mergeability and triggers GitHub's lazy computation of it, so a
        // null (still computing) answer reads as not conflicting and a later
        // probe sees the computed value. The other two forges answer out of
        // the listing entry itself.
        let fetched = match forge.kind {
            ForgeKind::Github => match pr["number"].as_u64() {
                Some(number) => {
                    let path = format!("repos/{repo}/pulls/{number}");
                    Some(api_json(forge, "gh", &path)?)
                }
                // A listed PR with no number can't be fetched or keyed, so
                // it is dropped here as `pr_key` would have dropped it.
                None => continue,
            },
            _ => None,
        };
        let pr = fetched.as_ref().unwrap_or(pr);
        if conflicting(pr) {
            keys.extend(pr_key(pr));
        }
    }
    // Listing order isn't stable across fetches, and the skip cache compares
    // the keys as one joined string, so only a sorted set is deterministic.
    keys.sort();
    Ok(keys)
}

/// The open PRs on which the bot is a pending requested reviewer, as sorted
/// `number@head-sha` keys, read off the same page [`open_prs`] fetched. Who
/// authored the PR doesn't come into it: a request on one of the bot's own
/// PRs is as much work as any other. The set doubles as `review`'s
/// precondition — non-empty means work — and as the state a skipped turn is
/// remembered against: submitting the review clears the request, and a
/// re-request or a push to the branch moves the key.
pub fn review_requests(
    forge: &Forge,
    repo: &str,
    open: &[serde_json::Value],
) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for pr in open.iter().filter(|pr| requested_from(pr, &forge.login)) {
        // Gitea and GitHub drop the bot from `requested_reviewers` once it
        // submits, so the listing alone says whether a review is owed.
        // GitLab keeps the reviewer and tracks a state per reviewer instead,
        // which `/request_review` resets to `unreviewed` and a submitted
        // review moves off it — one GET per candidate reads it.
        if forge.kind == ForgeKind::Gitlab {
            let Some(iid) = pr["iid"].as_u64() else {
                continue;
            };
            let path = format!("projects/{}/merge_requests/{iid}/reviewers", project(repo));
            if !unreviewed_by(&api_json(forge, "glab", &path)?, &forge.login) {
                continue;
            }
        }
        keys.extend(pr_key(pr));
    }
    keys.sort();
    Ok(keys)
}

/// One GET via the gh/glab `api` subcommand, parsed as JSON. Unlike
/// `turn::list_rows` there is no pagination: one page decides the answer.
pub fn api_json(forge: &Forge, program: &str, path: &str) -> Result<serde_json::Value> {
    let stdout = crate::child::output(Command::new(program).args(["api", path]).envs(forge.env()))?;
    serde_json::from_slice(&stdout)
        .with_context(|| format!("{program} api returned unexpected output"))
}

/// How long one Gitea API request may take. Without a cap a forge that
/// accepts the connection and then says nothing hangs the caller forever,
/// and the callers are the activity watcher and the draw's probes — the
/// runner would stop taking turns at all.
const API_TIMEOUT_SECS: u64 = 30;

/// One GET against the Gitea API; `tea` has no generic api subcommand, so it
/// goes over HTTP directly like the proxy's management calls do.
pub fn gitea_json(forge: &Forge, path: &str) -> Result<serde_json::Value> {
    let url = format!("{}/api/v1/{path}", forge.url.trim_end_matches('/'));
    let auth = format!("token {}", forge.token);
    let (status, body) = crate::http::get(
        &url,
        Some(("Authorization", auth.as_str())),
        API_TIMEOUT_SECS,
    )?;
    if !(200..300).contains(&status) {
        bail!("the gitea api answered HTTP {status} for {url}");
    }
    serde_json::from_str(&body).context("gitea api returned unexpected output")
}

/// Whether the PR was authored by `login`, under either forge spelling of
/// the author field (`user.login` on Gitea and GitHub, `author.username` on
/// GitLab).
fn authored_by(pr: &serde_json::Value, login: &str) -> bool {
    pr["user"]["login"]
        .as_str()
        .or_else(|| pr["author"]["username"].as_str())
        == Some(login)
}

/// Whether `login` is among the PR's requested reviewers, under either forge
/// spelling (`requested_reviewers[].login` on Gitea and GitHub,
/// `reviewers[].username` on GitLab).
fn requested_from(pr: &serde_json::Value, login: &str) -> bool {
    let named = |reviewers: &serde_json::Value, field: &str| {
        reviewers
            .as_array()
            .is_some_and(|rs| rs.iter().any(|r| r[field].as_str() == Some(login)))
    };
    named(&pr["requested_reviewers"], "login") || named(&pr["reviewers"], "username")
}

/// Whether GitLab's per-reviewer listing for an MR has `login` still owing a
/// review: its entry's `state` is `unreviewed`, which is where
/// `/request_review` puts it and a submitted review or approval moves it off.
fn unreviewed_by(reviewers: &serde_json::Value, login: &str) -> bool {
    reviewers.as_array().is_some_and(|rs| {
        rs.iter().any(|r| {
            r["user"]["username"].as_str() == Some(login)
                && r["state"].as_str() == Some("unreviewed")
        })
    })
}

/// Whether the forge reports the PR as unable to merge into its base, under
/// either spelling: `mergeable == false` on Gitea and GitHub,
/// `has_conflicts == true` on GitLab. A missing or null field (GitHub still
/// computing) reads as not conflicting, failing toward skipping the turn.
fn conflicting(pr: &serde_json::Value) -> bool {
    pr["mergeable"].as_bool() == Some(false) || pr["has_conflicts"].as_bool() == Some(true)
}

/// `number@head-sha` naming the PR's current revision, under either forge
/// spelling (`number` plus `head.sha` on Gitea and GitHub, `iid` plus `sha`
/// on GitLab). None for a malformed entry, which is dropped rather than
/// failing the whole probe.
fn pr_key(pr: &serde_json::Value) -> Option<String> {
    let number = pr["number"].as_u64().or_else(|| pr["iid"].as_u64())?;
    let sha = pr["head"]["sha"].as_str().or_else(|| pr["sha"].as_str())?;
    Some(format!("{number}@{sha}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::*;

    #[test]
    fn authored_by_reads_either_author_spelling() {
        assert!(authored_by(&json!({"user": {"login": "bot"}}), "bot"));
        assert!(authored_by(&json!({"author": {"username": "bot"}}), "bot"));
        assert!(!authored_by(&json!({"user": {"login": "someone"}}), "bot"));
        assert!(!authored_by(&json!({}), "bot"));
        // An empty login never matches a missing author field.
        assert!(!authored_by(&json!({}), ""));
    }

    #[test]
    fn conflicting_reads_either_conflict_spelling() {
        assert!(conflicting(&json!({"mergeable": false})));
        assert!(!conflicting(&json!({"mergeable": true})));
        // GitHub still computing mergeability reads as not conflicting.
        assert!(!conflicting(&json!({"mergeable": null})));
        assert!(conflicting(&json!({"has_conflicts": true})));
        assert!(!conflicting(&json!({"has_conflicts": false})));
        assert!(!conflicting(&json!({})));
    }

    /// A listing that is not an array is an error, not an empty page: both
    /// probes fail closed, where reading it as "nothing waiting" would both
    /// hand the draw a wrong answer and let a skip be remembered against it.
    #[test]
    fn a_listing_that_is_not_an_array_is_an_error() {
        assert!(pulls(json!([])).unwrap().is_empty());
        assert_eq!(pulls(json!([{"number": 1}])).unwrap().len(), 1);
        assert!(pulls(json!({"message": "oops"})).is_err());
        assert!(pulls(json!(null)).is_err());
    }

    /// An unresolved login matches no author and no reviewer, so asking at
    /// all would answer "nothing waiting" for every repository. Both probes
    /// read the one listing, so this is the one place it is checked.
    #[test]
    fn an_unresolved_login_fails_the_listing() {
        assert!(open_prs(&gitea(""), "me/repo").is_err());
    }

    #[test]
    fn requested_from_reads_either_reviewer_spelling() {
        assert!(requested_from(
            &json!({"requested_reviewers": [{"login": "a"}, {"login": "bot"}]}),
            "bot"
        ));
        assert!(requested_from(
            &json!({"reviewers": [{"username": "bot"}]}),
            "bot"
        ));
        assert!(!requested_from(
            &json!({"requested_reviewers": [{"login": "a"}]}),
            "bot"
        ));
        assert!(!requested_from(
            &json!({"requested_reviewers": null}),
            "bot"
        ));
        assert!(!requested_from(&json!({}), "bot"));
    }

    /// GitLab's reviewer state is what tells a fresh request from one the
    /// bot has already answered.
    #[test]
    fn unreviewed_by_reads_the_bots_reviewer_state() {
        let pending = json!([
            {"user": {"username": "a"}, "state": "reviewed"},
            {"user": {"username": "bot"}, "state": "unreviewed"},
        ]);
        assert!(unreviewed_by(&pending, "bot"));
        let done = json!([{"user": {"username": "bot"}, "state": "approved"}]);
        assert!(!unreviewed_by(&done, "bot"));
        assert!(!unreviewed_by(&json!([]), "bot"));
        assert!(!unreviewed_by(&json!({"message": "oops"}), "bot"));
    }

    /// A PR that names the bot as a requested reviewer is a review candidate
    /// whoever authored it — the bot's own included, when someone asks it
    /// to — and a PR nobody asked it about is left alone.
    #[test]
    fn requested_prs_keeps_every_pr_the_bot_is_asked_about() {
        let prs = pulls(json!([
            {"number": 1, "user": {"login": "someone"}, "requested_reviewers": [{"login": "bot"}], "head": {"sha": "aaa"}},
            {"number": 2, "user": {"login": "bot"}, "requested_reviewers": [{"login": "bot"}], "head": {"sha": "bbb"}},
            {"number": 3, "user": {"login": "someone"}, "requested_reviewers": [], "head": {"sha": "ccc"}},
            {"number": 5, "user": {"login": "bot"}, "requested_reviewers": [{"login": "someone"}], "head": {"sha": "eee"}},
            // Keyless, and the bot's to review: dropped rather than keyed
            // or made to fail the whole probe.
            {"requested_reviewers": [{"login": "bot"}], "head": {"sha": "fff"}},
            {"iid": 4, "author": {"username": "someone"}, "reviewers": [{"username": "bot"}], "sha": "ddd"},
        ]))
        .unwrap();
        assert_eq!(
            review_requests(&gitea("bot"), "me/repo", &prs).unwrap(),
            ["1@aaa", "2@bbb", "4@ddd"]
        );
        assert!(
            review_requests(&gitea("bot"), "me/repo", &[])
                .unwrap()
                .is_empty()
        );
    }

    /// A PR's key names its number and current head revision, under either
    /// forge spelling, and a malformed entry yields nothing instead of a
    /// bogus key.
    #[test]
    fn pr_key_reads_either_forge_spelling() {
        assert_eq!(
            pr_key(&json!({"number": 12, "head": {"sha": "abc"}})),
            Some("12@abc".to_owned())
        );
        assert_eq!(
            pr_key(&json!({"iid": 7, "sha": "def"})),
            Some("7@def".to_owned())
        );
        assert_eq!(pr_key(&json!({"head": {"sha": "abc"}})), None);
        assert_eq!(pr_key(&json!({"number": 12})), None);
    }

    /// What `conflicting_prs` makes of a listing that carries mergeability
    /// itself (Gitea and GitLab): only a PR that is both the bot's and
    /// conflicting gets a key, a keyless entry is dropped rather than
    /// failing the whole probe, and the keys come back sorted — the skip
    /// cache compares them as one joined string, and listing order isn't
    /// stable across fetches.
    #[test]
    fn only_the_bots_conflicting_prs_are_keyed() {
        let prs = pulls(json!([
            {"number": 15, "user": {"login": "bot"}, "mergeable": false, "head": {"sha": "eee"}},
            {"number": 1, "user": {"login": "bot"}, "mergeable": true, "head": {"sha": "aaa"}},
            {"number": 2, "user": {"login": "someone"}, "mergeable": false, "head": {"sha": "bbb"}},
            {"user": {"login": "bot"}, "mergeable": false, "head": {"sha": "ddd"}},
            {"number": 12, "user": {"login": "bot"}, "mergeable": false, "head": {"sha": "ccc"}},
        ]))
        .unwrap();
        assert_eq!(
            conflicting_prs(&gitea("bot"), "me/repo", &prs).unwrap(),
            ["12@ccc", "15@eee"]
        );
    }

    fn forge() -> Forge {
        Forge {
            kind: ForgeKind::Github,
            token: "tok".into(),
            user: String::new(),
            login: String::new(),
            email: String::new(),
            url: "https://github.com".into(),
            enabled_repos: BTreeSet::new(),
        }
    }

    /// A Gitea forge with `login` as its resolved account. Gitea is the kind
    /// whose listing answers both probes on its own, so the keys they build
    /// can be checked without a forge to ask.
    fn gitea(login: &str) -> Forge {
        Forge {
            kind: ForgeKind::Gitea,
            login: login.into(),
            url: "https://gitea.example.com".into(),
            ..forge()
        }
    }

    #[test]
    fn draw_prefers_a_gated_task_that_has_work() {
        let tasks = ["bump".to_owned(), "rebase".to_owned()];
        let forge = forge();
        let pool = [(&forge, "me/repo".to_owned())];
        for _ in 0..16 {
            let (task, ..) = draw(&tasks, &pool, |task, _, _| task == "rebase").unwrap();
            assert_eq!(task, "rebase");
        }
    }

    /// Every (gated task, repo) pair is probed before giving up on them.
    #[test]
    fn draw_probes_every_repository() {
        let tasks = ["feedback".to_owned(), "rebase".to_owned()];
        let forge = forge();
        let pool = [
            (&forge, "me/one".to_owned()),
            (&forge, "me/two".to_owned()),
            (&forge, "me/three".to_owned()),
        ];
        let seen = std::cell::RefCell::new(BTreeSet::new());
        assert!(
            draw(&tasks, &pool, |task, _, repo| {
                seen.borrow_mut().insert(format!("{task} {repo}"));
                false
            })
            .is_none()
        );
        assert_eq!(seen.borrow().len(), tasks.len() * pool.len());
    }

    #[test]
    fn draw_returns_an_actionable_pair() {
        let tasks = ["feedback".to_owned(), "rebase".to_owned()];
        let forge = forge();
        let pool = [(&forge, "me/repo".to_owned())];
        let (task, _, repo) = draw(&tasks, &pool, |_, _, _| true).unwrap();
        assert!(gated(task));
        assert_eq!(repo, "me/repo");
    }

    #[test]
    fn draw_falls_back_to_an_ungated_task() {
        let tasks = [
            "feedback".to_owned(),
            "rebase".to_owned(),
            "bump".to_owned(),
        ];
        let forge = forge();
        let pool = [(&forge, "me/repo".to_owned())];
        let (task, ..) = draw(&tasks, &pool, |task, _, _| !gated(task)).unwrap();
        assert_eq!(task, "bump");
    }

    /// The ungated pass is checked too, so the skip cache can hold a task
    /// back; only once nothing is left does the draw come up empty.
    #[test]
    fn draw_skips_an_ungated_task_the_check_rejects() {
        let tasks = ["docs".to_owned(), "bump".to_owned()];
        let forge = forge();
        let pool = [(&forge, "me/repo".to_owned())];
        for _ in 0..16 {
            let (task, ..) = draw(&tasks, &pool, |task, _, _| task != "docs").unwrap();
            assert_eq!(task, "bump");
        }
        assert!(draw(&tasks, &pool, |_, _, _| false).is_none());
    }

    #[test]
    fn draw_gives_up_when_every_task_is_gated_and_idle() {
        let tasks = ["feedback".to_owned(), "rebase".to_owned()];
        let forge = forge();
        let pool = [(&forge, "me/repo".to_owned())];
        assert!(draw(&tasks, &pool, |_, _, _| false).is_none());
    }

    /// Every gated pair is probed before an ungated task is considered, so a
    /// cheap cached task can never outrank a review comment.
    #[test]
    fn draw_probes_every_gated_pair_first() {
        let tasks = ["docs".to_owned(), "rebase".to_owned()];
        let forge = forge();
        let pool = [(&forge, "me/one".to_owned()), (&forge, "me/two".to_owned())];
        for _ in 0..16 {
            let probed = std::cell::RefCell::new(Vec::new());
            let (task, ..) = draw(&tasks, &pool, |task, _, repo| {
                probed.borrow_mut().push(format!("{task} {repo}"));
                task == "docs"
            })
            .unwrap();
            assert_eq!(task, "docs");
            let probed = probed.borrow();
            assert_eq!(probed.len(), 3, "{probed:?}");
            assert!(probed[..2].iter().all(|entry| entry.starts_with("rebase")));
        }
    }

    #[test]
    fn first_sha_reads_either_field() {
        assert_eq!(
            first_sha(&json!([{"sha": "abc"}, {"sha": "old"}])),
            Some("abc".to_owned())
        );
        assert_eq!(first_sha(&json!([{"id": "abc"}])), Some("abc".to_owned()));
        assert_eq!(first_sha(&json!([])), None);
        assert_eq!(first_sha(&json!({"message": "empty repository"})), None);
    }
}
