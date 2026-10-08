//! Cheap preconditions for tasks that often have nothing to act on, so the
//! runner can tell before burning an agent session discovering it. The same
//! probes decide priority: a gated task with work waiting outranks whatever a
//! blind draw would have picked.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use tracing::{debug, info};

use crate::cache::gated;
use crate::config::{Forge, ForgeKind};

/// Open PRs examined per repository; the bot's own open PRs fit well inside
/// one page.
const PR_LIMIT: usize = 20;

/// Draw a (task, forge, repo) triple worth an agent turn. The gated tasks are
/// probed first — every enabled one against every repository, in a shuffled
/// order — and the first pair with work wins, so responsive work (a
/// conflicting PR, a review comment) is never waiting on a lucky draw. Only once nothing is
/// pending do the rest get their turn, as one shuffled pass over every (task,
/// repository) pair: `check` waves most of them straight through, so this is
/// the uniform draw it used to be, but a pair the skip cache has already
/// answered for is passed over instead of drawn. The cache applies to the
/// gated pass too — both gated tasks are probed out of the same state their
/// memory is keyed on — so a gated task can no longer monopolize the draw on
/// a precondition that stays true. None when nothing at all is worth a turn.
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
/// no generic api subcommand so Gitea goes through curl, while gh and glab
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

/// The bot's open PRs that conflict with their base branch, as sorted
/// `number@head-sha` keys. A branch that is merely behind but merges cleanly
/// doesn't count: there is nothing a rebase turn would have to resolve. The
/// set doubles as `rebase`'s precondition — non-empty means work — and as
/// the state a skipped turn is remembered against.
pub fn conflicting_prs(forge: &Forge, repo: &str) -> Result<Vec<String>> {
    if forge.login.is_empty() {
        // An unresolved login would silently match no author at all; bailing
        // routes through the probe's fail-closed arm instead.
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
        // heals by the next probe.
        &format!(
            "projects/{}/merge_requests?state=opened&per_page={PR_LIMIT}&with_merge_status_recheck=true",
            project(repo)
        ),
    )?;
    let mut keys = match forge.kind {
        // GitHub's listing is the one that carries no mergeability, so each
        // of the bot's PRs costs a GET of its own. That GET both reads
        // mergeability and triggers GitHub's lazy computation of it, so a
        // null (still computing) answer reads as not conflicting and a later
        // probe sees the computed value.
        ForgeKind::Github => {
            let mut keys = Vec::new();
            for number in owned_pr_numbers(&listing, &forge.login) {
                let pr = api_json(forge, "gh", &format!("repos/{repo}/pulls/{number}"))?;
                if conflicting(&pr) {
                    keys.extend(pr_key(&pr));
                }
            }
            keys
        }
        _ => conflict_keys(&listing, &forge.login)?,
    };
    // Listing order isn't stable across fetches, and the skip cache compares
    // the keys as one joined string, so only a sorted set is deterministic.
    keys.sort();
    Ok(keys)
}

/// One GET via the gh/glab `api` subcommand, parsed as JSON. Unlike
/// `turn::list_api_repos` there is no pagination: one page decides the answer.
pub fn api_json(forge: &Forge, program: &str, path: &str) -> Result<serde_json::Value> {
    let output = Command::new(program)
        .args(["api", path])
        .envs(forge.env())
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} api exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("{program} api returned unexpected output"))
}

/// How long one Gitea API request may take. Without a cap a forge that
/// accepts the connection and then says nothing hangs the caller forever,
/// and the callers are the activity watcher and the draw's probes — the
/// runner would stop taking turns at all.
const API_TIMEOUT_SECS: u32 = 30;

/// One GET against the Gitea API via curl; `tea` has no generic api
/// subcommand. The auth header goes through `--config -` on stdin so the
/// token never lands in argv.
pub fn gitea_json(forge: &Forge, path: &str) -> Result<serde_json::Value> {
    let url = format!("{}/api/v1/{path}", forge.url.trim_end_matches('/'));
    let mut child = Command::new("curl")
        .args(["-sf", "--max-time", &API_TIMEOUT_SECS.to_string()])
        .args(["--config", "-", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run curl")?;
    let config = format!("header = \"Authorization: token {}\"\n", forge.token);
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(config.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "curl {url} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("gitea api returned unexpected output")
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

/// The keys of the listed PRs authored by `login` that their forge reports
/// as conflicting, for the listings that carry mergeability (Gitea and
/// GitLab; GitHub's needs a GET per PR instead).
fn conflict_keys(prs: &serde_json::Value, login: &str) -> Result<Vec<String>> {
    Ok(prs
        .as_array()
        .context("expected an array of pulls")?
        .iter()
        .filter(|pr| authored_by(pr, login) && conflicting(pr))
        .filter_map(pr_key)
        .collect())
}

/// The numbers of the listed PRs authored by `login`; GitHub's listing
/// carries no mergeability, so each costs a GET of its own.
fn owned_pr_numbers(prs: &serde_json::Value, login: &str) -> Vec<u64> {
    prs.as_array()
        .map(|prs| {
            prs.iter()
                .filter(|pr| authored_by(pr, login))
                .filter_map(|pr| pr["number"].as_u64())
                .collect()
        })
        .unwrap_or_default()
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

    #[test]
    fn owned_pr_numbers_keeps_only_the_bots_prs() {
        let prs = json!([
            {"number": 1, "user": {"login": "bot"}},
            {"number": 2, "user": {"login": "someone"}},
            {"user": {"login": "bot"}},
            {"number": 4, "user": {"login": "bot"}},
        ]);
        assert_eq!(owned_pr_numbers(&prs, "bot"), [1, 4]);
        assert!(owned_pr_numbers(&json!({"message": "oops"}), "bot").is_empty());
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

    /// The listing filter: only a PR that is both the bot's and conflicting
    /// gets a key, a keyless entry is dropped rather than fatal, and a
    /// non-array listing is an error (which the probe fails closed on).
    #[test]
    fn conflict_keys_keep_only_owned_conflicting_prs() {
        let prs = json!([
            {"number": 15, "user": {"login": "bot"}, "mergeable": false, "head": {"sha": "eee"}},
            {"number": 1, "user": {"login": "bot"}, "mergeable": true, "head": {"sha": "aaa"}},
            {"number": 2, "user": {"login": "someone"}, "mergeable": false, "head": {"sha": "bbb"}},
            {"user": {"login": "bot"}, "mergeable": false, "head": {"sha": "ddd"}},
            {"number": 12, "user": {"login": "bot"}, "mergeable": false, "head": {"sha": "ccc"}},
        ]);
        assert_eq!(conflict_keys(&prs, "bot").unwrap(), ["15@eee", "12@ccc"]);
        assert!(conflict_keys(&json!({"message": "oops"}), "bot").is_err());
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
