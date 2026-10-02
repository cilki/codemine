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

/// Open PRs examined per repository; a repo with more open PRs than one page
/// almost certainly has one behind its base anyway.
const PR_LIMIT: usize = 20;

/// Draw a (task, forge, repo) triple worth an agent turn. The gated tasks are
/// probed first — every enabled one against every repository, in a shuffled
/// order — and the first pair with work wins, so responsive work (a stale PR,
/// a review comment) is never waiting on a lucky draw. Only once nothing is
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
/// Fail-open: a probe error is logged and treated as actionable, so a broken
/// probe costs at most what a blind draw did — an agent turn that ends in
/// TASK SKIPPED.
pub fn actionable(task: &str, forge: &Forge, repo: &str) -> bool {
    let probed = match task {
        "feedback" => has_feedback(forge, repo),
        "rebase" => has_stale_pr(forge, repo),
        _ => return true,
    };
    probed.unwrap_or_else(|err| {
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

/// Whether the repository has an unread notification (a pending todo on
/// GitLab, which scopes them to the user rather than the repo).
fn has_feedback(forge: &Forge, repo: &str) -> Result<bool> {
    match forge.kind {
        ForgeKind::Gitea => {
            nonempty_array(&gitea_json(forge, &format!("repos/{repo}/notifications"))?)
        }
        ForgeKind::Github => nonempty_array(&api_json(
            forge,
            "gh",
            &format!("repos/{repo}/notifications"),
        )?),
        ForgeKind::Gitlab => Ok(gitlab_todo_for(&api_json(forge, "glab", "todos")?, repo)),
    }
}

/// Whether any open PR branch is behind its base branch.
fn has_stale_pr(forge: &Forge, repo: &str) -> Result<bool> {
    match forge.kind {
        ForgeKind::Gitea => {
            let prs = gitea_json(
                forge,
                &format!("repos/{repo}/pulls?state=open&limit={PR_LIMIT}"),
            )?;
            for (base, head) in pr_refs(&prs, "ref") {
                // Compared head-first on purpose: Gitea counts the commits
                // the right side has that the left lacks, so this is how far
                // the branch trails its base.
                let compare = gitea_json(forge, &format!("repos/{repo}/compare/{head}...{base}"))?;
                if behind(&compare, "total_commits") {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        ForgeKind::Github => {
            let prs = api_json(
                forge,
                "gh",
                &format!("repos/{repo}/pulls?state=open&per_page={PR_LIMIT}"),
            )?;
            // head.label is `owner:branch`, so fork heads resolve too.
            for (base, head) in pr_refs(&prs, "label") {
                let compare = api_json(
                    forge,
                    "gh",
                    &format!("repos/{repo}/compare/{base}...{head}"),
                )?;
                if behind(&compare, "behind_by") {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        ForgeKind::Gitlab => {
            let project = repo.replace('/', "%2F");
            let mrs = api_json(
                forge,
                "glab",
                &format!("projects/{project}/merge_requests?state=opened&per_page={PR_LIMIT}"),
            )?;
            let iids: Vec<u64> = mrs
                .as_array()
                .context("expected an array of merge requests")?
                .iter()
                .filter_map(|mr| mr["iid"].as_u64())
                .collect();
            for iid in iids {
                let mr = api_json(
                    forge,
                    "glab",
                    &format!(
                        "projects/{project}/merge_requests/{iid}?include_diverged_commits_count=true"
                    ),
                )?;
                if behind(&mr, "diverged_commits_count") {
                    return Ok(true);
                }
            }
            Ok(false)
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

/// One GET against the Gitea API via curl; `tea` has no generic api
/// subcommand. The auth header goes through `--config -` on stdin so the
/// token never lands in argv.
pub fn gitea_json(forge: &Forge, path: &str) -> Result<serde_json::Value> {
    let url = format!("{}/api/v1/{path}", forge.url.trim_end_matches('/'));
    let mut child = Command::new("curl")
        .args(["-sf", "--config", "-", &url])
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

/// The (base ref, head) pairs of a PR listing; `head_key` picks which field
/// of `head` names the branch (`ref` on Gitea, `label` on GitHub).
fn pr_refs(prs: &serde_json::Value, head_key: &str) -> Vec<(String, String)> {
    prs.as_array()
        .map(|prs| {
            prs.iter()
                .filter_map(|pr| {
                    Some((
                        pr["base"]["ref"].as_str()?.to_owned(),
                        pr["head"][head_key].as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a comparison response counts any trailing commits in `field`;
/// a missing field reads as up to date.
fn behind(value: &serde_json::Value, field: &str) -> bool {
    value[field].as_u64().unwrap_or(0) > 0
}

fn nonempty_array(value: &serde_json::Value) -> Result<bool> {
    Ok(!value
        .as_array()
        .context("expected an array of notifications")?
        .is_empty())
}

/// Whether any pending GitLab todo belongs to this project.
fn gitlab_todo_for(todos: &serde_json::Value, repo: &str) -> bool {
    todos.as_array().is_some_and(|todos| {
        todos
            .iter()
            .any(|todo| todo["project"]["path_with_namespace"].as_str() == Some(repo))
    })
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
    fn behind_reads_the_count() {
        assert!(behind(&json!({"behind_by": 3}), "behind_by"));
        assert!(!behind(&json!({"behind_by": 0}), "behind_by"));
        assert!(!behind(&json!({"total_commits": 0}), "total_commits"));
        assert!(behind(&json!({"total_commits": 2}), "total_commits"));
        assert!(!behind(&json!({}), "diverged_commits_count"));
    }

    #[test]
    fn pr_refs_pairs_base_with_the_chosen_head_field() {
        let prs = json!([
            {"base": {"ref": "main"}, "head": {"ref": "fix", "label": "fork:fix"}},
            {"base": {"ref": "dev"}, "head": {"ref": "feat", "label": "me:feat"}},
            {"unrelated": true},
        ]);
        assert_eq!(
            pr_refs(&prs, "label"),
            [
                ("main".to_owned(), "fork:fix".to_owned()),
                ("dev".to_owned(), "me:feat".to_owned()),
            ]
        );
        assert_eq!(
            pr_refs(&prs, "ref"),
            [
                ("main".to_owned(), "fix".to_owned()),
                ("dev".to_owned(), "feat".to_owned()),
            ]
        );
        assert!(pr_refs(&json!({"message": "oops"}), "ref").is_empty());
    }

    #[test]
    fn nonempty_array_shapes() {
        assert!(!nonempty_array(&json!([])).unwrap());
        assert!(nonempty_array(&json!([{"id": 1}])).unwrap());
        assert!(nonempty_array(&json!({"message": "bad token"})).is_err());
    }

    #[test]
    fn todos_match_by_project_path() {
        let todos = json!([{"project": {"path_with_namespace": "me/repo"}}]);
        assert!(gitlab_todo_for(&todos, "me/repo"));
        assert!(!gitlab_todo_for(&todos, "me/other"));
        assert!(!gitlab_todo_for(&json!({}), "me/repo"));
    }

    fn forge() -> Forge {
        Forge {
            kind: ForgeKind::Github,
            token: "tok".into(),
            user: String::new(),
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
