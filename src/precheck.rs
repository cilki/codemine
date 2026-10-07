//! Cheap preconditions for tasks that often have nothing to act on, so the
//! runner can tell before burning an agent session discovering it. The same
//! probes decide priority: a gated task with work waiting outranks whatever a
//! blind draw would have picked.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

use crate::config::{Forge, ForgeKind};

/// Tasks with a precondition probe; every other task is always drawable.
pub fn gated(task: &str) -> bool {
    matches!(task, "feedback" | "rebase")
}

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
/// gated pass too — `feedback` is both probed and remembered — so a gated
/// task can no longer monopolize the draw on a precondition that stays true.
/// None when nothing at all is worth a turn.
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

/// Whether the drawn task has anything to act on in this repository.
/// `feedback` is answered by `cache::Probe` out of the notification feed it
/// reads anyway, so only `rebase` needs a probe of its own here: whether any
/// of the bot's own open PRs conflict with their base. Fail-open: a probe
/// error is logged and treated as actionable, so a broken probe costs at
/// most what a blind draw did — an agent turn that ends in TASK SKIPPED.
pub fn actionable(task: &str, forge: &Forge, repo: &str) -> bool {
    if task != "rebase" {
        return true;
    }
    has_conflicting_pr(forge, repo).unwrap_or_else(|err| {
        warn!("{task} precheck failed for {repo}: {err:#}");
        true
    })
}

/// The commit the repository's default branch points at. Every forge's
/// commit listing defaults to that branch, so one page of one commit answers
/// it; GitHub and Gitea name the field `sha`, GitLab `id`.
pub fn head_sha(forge: &Forge, repo: &str) -> Result<String> {
    let commits = match forge.kind {
        ForgeKind::Gitea => gitea_json(forge, &format!("repos/{repo}/commits?limit=1&stat=false"))?,
        ForgeKind::Github => api_json(forge, "gh", &format!("repos/{repo}/commits?per_page=1"))?,
        ForgeKind::Gitlab => api_json(
            forge,
            "glab",
            &format!(
                "projects/{}/repository/commits?per_page=1",
                repo.replace('/', "%2F")
            ),
        )?,
    };
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

/// Whether any open PR authored by the bot conflicts with its base branch.
/// A branch that is merely behind but merges cleanly doesn't count: there is
/// nothing a rebase turn would have to resolve.
fn has_conflicting_pr(forge: &Forge, repo: &str) -> Result<bool> {
    if forge.login.is_empty() {
        // An unresolved login would silently match no author at all; bailing
        // routes through `actionable`'s fail-open instead.
        bail!("the {} login is unresolved", forge.kind.name());
    }
    match forge.kind {
        ForgeKind::Gitea => {
            // Gitea's listing carries `mergeable`, which also reads false
            // while its conflict check is still running and for draft PRs;
            // the worst case is a dispatched turn that ends in TASK SKIPPED.
            let prs = gitea_json(
                forge,
                &format!("repos/{repo}/pulls?state=open&limit={PR_LIMIT}"),
            )?;
            let prs = prs.as_array().context("expected an array of pulls")?;
            Ok(prs
                .iter()
                .any(|pr| authored_by(pr, &forge.login) && conflicting(pr)))
        }
        ForgeKind::Github => {
            let prs = api_json(
                forge,
                "gh",
                &format!("repos/{repo}/pulls?state=open&per_page={PR_LIMIT}"),
            )?;
            for number in owned_pr_numbers(&prs, &forge.login) {
                // The listing carries no mergeability; the GET both reads it
                // and triggers GitHub's lazy computation, so a null (still
                // computing) answer reads as not conflicting and a later
                // probe sees the computed value.
                let pr = api_json(forge, "gh", &format!("repos/{repo}/pulls/{number}"))?;
                if conflicting(&pr) {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        ForgeKind::Gitlab => {
            let project = repo.replace('/', "%2F");
            // The listing's `has_conflicts` is GitLab's cached merge status;
            // the recheck parameter schedules an async refresh so a stale
            // answer heals by the next probe.
            let mrs = api_json(
                forge,
                "glab",
                &format!(
                    "projects/{project}/merge_requests?state=opened&per_page={PR_LIMIT}&with_merge_status_recheck=true"
                ),
            )?;
            let mrs = mrs
                .as_array()
                .context("expected an array of merge requests")?;
            Ok(mrs
                .iter()
                .any(|mr| authored_by(mr, &forge.login) && conflicting(mr)))
        }
    }
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
    fn gated_matches_the_probed_slugs() {
        assert!(gated("feedback"));
        assert!(gated("rebase"));
        for task in ["bump", "simplify", "todo", "feature"] {
            assert!(!gated(task));
        }
    }

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

    /// The probe's listing filter: only a PR that is both the bot's and
    /// conflicting counts.
    #[test]
    fn only_an_owned_conflicting_pr_counts() {
        let hit = |prs: &serde_json::Value| {
            prs.as_array()
                .unwrap()
                .iter()
                .any(|pr| authored_by(pr, "bot") && conflicting(pr))
        };
        let mut prs = json!([
            {"user": {"login": "bot"}, "mergeable": true},
            {"user": {"login": "someone"}, "mergeable": false},
        ]);
        assert!(!hit(&prs));
        prs.as_array_mut()
            .unwrap()
            .push(json!({"user": {"login": "bot"}, "mergeable": false}));
        assert!(hit(&prs));
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
