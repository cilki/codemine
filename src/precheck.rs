//! Cheap preconditions for tasks that often have nothing to act on, so the
//! runner can tell before burning an agent session discovering it. The same
//! probes decide priority: a gated task with work waiting outranks whatever a
//! blind draw would have picked.

use std::process::Command;

use anyhow::{Context, Result, bail};
use reqwest::Method;
use tracing::{debug, info};

use crate::cache::gated;
use crate::config::{Forge, ForgeKind};

/// Open PRs examined per repository; the bot's own open PRs, and the ones it
/// is asked to review, fit well inside one page. It also caps the open-PR
/// budget, which can't count past one page.
pub const PR_LIMIT: usize = 20;

/// Draw a (task, repo) pair worth an agent turn. The pairs are every
/// repository in `pool` with every task it has on (`Forge::repos`). The gated
/// tasks are probed first, in a shuffled order, and the first pair with work
/// wins, so responsive work (a conflicting PR, a review comment, a review
/// request) is never waiting on a lucky draw. Only once nothing is pending do
/// the rest get their turn, as one pass over every remaining pair in an
/// order drawn by `weigh`: a pair is placed ahead of another with the odds
/// their weights give it, so a heavier pair is more likely — never certain —
/// to go first. The caller's weights favour a repository a person has been
/// active on lately (`humans`). `check` waves most of them straight through, but a pair the
/// skip cache has already answered for is passed over instead of drawn. The
/// cache applies to the gated pass too — every gated task is probed out of
/// the same state its memory is keyed on — so a gated task can no longer
/// monopolize the draw on a precondition that stays true. None when nothing
/// at all is worth a turn.
pub fn draw<'a>(
    forge: &'a Forge,
    pool: &'a [String],
    check: impl Fn(&str, &str) -> bool,
    weigh: impl Fn(&str, &str) -> f64,
) -> Option<(&'a str, &'a str)> {
    let candidates = |want_gated: bool| -> Vec<(&'a str, &'a str)> {
        pool.iter()
            .flat_map(|repo| {
                forge
                    .repos
                    .get(repo)
                    .into_iter()
                    .flatten()
                    .filter(|task| gated(task) == want_gated)
                    .map(move |task| (task.as_str(), repo.as_str()))
            })
            .collect()
    };
    let mut gated_pairs = candidates(true);
    fastrand::shuffle(&mut gated_pairs);
    // Weighed only once the gated pass comes up empty: a weight can cost a
    // forge request (see `humans`), which a draw that ends on gated work
    // never needs.
    let ungated = std::iter::once(())
        .flat_map(|()| weighted_order(candidates(false), |(task, repo)| weigh(task, repo)));
    for (task, repo) in gated_pairs.into_iter().chain(ungated) {
        if check(task, repo) {
            return Some((task, repo));
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

/// `items` in a random order where each is placed with the odds its weight
/// gives it: a weighted sample without replacement, taken all at once by
/// keying every item with `u^(1/w)` for a uniform `u` and sorting the keys
/// down (Efraimidis and Spirakis). The whole order is wanted, not just the
/// first item, because the caller walks it until `check` accepts one; an
/// order makes that one pass where a redraw per rejected item would not.
/// A weight that isn't positive keys as 0, which lands the item last.
fn weighted_order<T>(items: Vec<T>, weight: impl Fn(&T) -> f64) -> Vec<T> {
    let mut keyed: Vec<(f64, T)> = items
        .into_iter()
        .map(|item| {
            let weight = weight(&item);
            let key = match weight > 0.0 {
                true => fastrand::f64().powf(1.0 / weight),
                false => 0.0,
            };
            (key, item)
        })
        .collect();
    keyed.sort_by(|a, b| b.0.total_cmp(&a.0));
    keyed.into_iter().map(|(_, item)| item).collect()
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
    let body = forge_call(forge, Method::GET, gitea, github, gitlab, &[])?;
    serde_json::from_slice(&body)
        .with_context(|| format!("{} api returned unexpected output", forge.kind.name()))
}

/// The body of one request against a forge's API, through whichever
/// transport speaks it; only the forge's own path is used. `fields` are the
/// request's string parameters: a JSON object on Gitea, `-f` fields to gh
/// and glab.
pub fn forge_call(
    forge: &Forge,
    method: Method,
    gitea: &str,
    github: &str,
    gitlab: &str,
    fields: &[(&str, &str)],
) -> Result<Vec<u8>> {
    match forge.kind {
        ForgeKind::Gitea => gitea_call(forge, method, gitea, fields),
        ForgeKind::Github => api_call(forge, "gh", method, github, fields),
        ForgeKind::Gitlab => api_call(forge, "glab", method, gitlab, fields),
    }
}

/// One page of a forge listing, through [`forge_json`]. An answer that isn't
/// an array is an error, so every caller fails closed on it instead of
/// reading it as an empty page.
pub fn forge_rows(
    forge: &Forge,
    gitea: &str,
    github: &str,
    gitlab: &str,
) -> Result<Vec<serde_json::Value>> {
    match forge_json(forge, gitea, github, gitlab)? {
        serde_json::Value::Array(rows) => Ok(rows),
        _ => bail!(
            "{} returned something other than a listing",
            forge.kind.name()
        ),
    }
}

/// GitLab identifies a project by its URL-encoded path.
pub fn project(repo: &str) -> String {
    crate::http::encode(repo, false)
}

/// The resolved login of the bot's account. Unresolved is an error rather
/// than an empty string: it would silently match no author and no reviewer
/// at all, where bailing routes through the callers' fail-closed arm.
fn login(forge: &Forge) -> Result<&str> {
    match forge.login.as_str() {
        "" => bail!("the {} login is unresolved", forge.kind.name()),
        login => Ok(login),
    }
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
/// An unresolved login is an error rather than an empty page, as is a
/// listing that isn't an array: a forge that answered with something else
/// never said the bot has nothing waiting.
pub fn open_prs(forge: &Forge, repo: &str) -> Result<Vec<serde_json::Value>> {
    login(forge)?;
    forge_rows(
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
    )
}

/// How many of the PRs on the page [`open_prs`] fetched are the bot's own,
/// which is what the open-PR budget is measured in.
pub fn own_prs(forge: &Forge, open: &[serde_json::Value]) -> usize {
    open.iter()
        .filter(|pr| authored_by(pr, &forge.login))
        .count()
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
                    Some(forge_json(forge, &path, &path, &path)?)
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
            if !unreviewed_by(&forge_json(forge, &path, &path, &path)?, &forge.login) {
                continue;
            }
        }
        keys.extend(pr_key(pr));
    }
    keys.sort();
    Ok(keys)
}

/// The PRs and issues the bot opened on the repository at or after `since`
/// (epoch seconds): what a completed turn delivered. Asked after the turn
/// rather than diffed against a listing taken before it, so it costs one
/// request (two on GitLab, which lists merge requests and issues apart) and
/// still sees a PR the turn opened and closed again. The forges filter by
/// author and by update time — anything created since was updated since —
/// and the creation time is checked off each row. One page is plenty: a
/// turn opens one PR, or an issue, not fifty.
pub fn opened_since(
    forge: &Forge,
    repo: &str,
    since: u64,
) -> Result<Vec<crate::deliveries::Opened>> {
    use crate::deliveries::{Kind, format_rfc3339, opened_in};
    let (login, after) = (login(forge)?, format_rfc3339(since));
    // Gitea's and GitHub's issue listing holds their PRs too, each marked as
    // one; GitLab keeps one kind per listing.
    let listings: &[_] = match forge.kind {
        ForgeKind::Gitlab => &[
            ("merge_requests", Some(Kind::Pr)),
            ("issues", Some(Kind::Issue)),
        ],
        _ => &[("issues", None)],
    };
    let mut opened = Vec::new();
    for &(listing, kind) in listings {
        let rows = forge_rows(
            forge,
            &format!(
                "repos/{repo}/issues?state=all&created_by={login}&since={after}&limit={PR_LIMIT}"
            ),
            &format!(
                "repos/{repo}/issues?state=all&creator={login}&since={after}&per_page={PR_LIMIT}"
            ),
            &format!(
                "projects/{}/{listing}?state=all&author_username={login}&created_after={after}&per_page={PR_LIMIT}",
                project(repo)
            ),
        )?;
        opened.extend(opened_in(&rows, login, since, kind));
    }
    Ok(opened)
}

/// What became of one PR or issue the bot opened, and for a PR its head
/// branch. Each forge has a PR endpoint that answers both in one request;
/// an issue is asked about on the issue endpoint and has no head.
pub fn delivery_state(
    forge: &Forge,
    repo: &str,
    kind: crate::deliveries::Kind,
    number: u64,
) -> Result<crate::deliveries::Answer> {
    let (pulls, listing) = match kind {
        crate::deliveries::Kind::Pr => ("pulls", "merge_requests"),
        crate::deliveries::Kind::Issue => ("issues", "issues"),
    };
    let item = forge_json(
        forge,
        &format!("repos/{repo}/{pulls}/{number}"),
        &format!("repos/{repo}/{pulls}/{number}"),
        &format!("projects/{}/{listing}/{number}", project(repo)),
    )?;
    Ok(crate::deliveries::Answer {
        state: crate::deliveries::state_of(&item).context("the forge's answer named no state")?,
        head: match kind {
            crate::deliveries::Kind::Pr => crate::deliveries::head_of(&item),
            crate::deliveries::Kind::Issue => None,
        },
    })
}

/// Leave `body` as a comment on one of the repository's PRs or issues. Gitea
/// and GitHub comment on both through the issue endpoint; GitLab keeps notes
/// on merge requests and issues apart, and numbers them apart too.
pub fn comment(
    forge: &Forge,
    repo: &str,
    kind: crate::deliveries::Kind,
    number: u64,
    body: &str,
) -> Result<()> {
    let listing = match kind {
        crate::deliveries::Kind::Pr => "merge_requests",
        crate::deliveries::Kind::Issue => "issues",
    };
    let path = format!("repos/{repo}/issues/{number}/comments");
    forge_call(
        forge,
        Method::POST,
        &path,
        &path,
        &format!("projects/{}/{listing}/{number}/notes", project(repo)),
        &[("body", body)],
    )?;
    Ok(())
}

/// Delete the head branch of one of the bot's settled PRs, unless something
/// says the branch is still somebody's: Ok(None) once it is deleted,
/// Ok(Some(why)) for a branch kept. It is kept when it lives in a fork, is
/// the repository's default branch or a protected one, has moved past the
/// PR's last head commit, or is the head of another open PR. Even a deleted
/// branch's commits stay on the forge under the PR's own ref
/// (`refs/pull/<n>/head`, `refs/merge-requests/<iid>/head`), so the PR page
/// can restore it.
pub fn delete_head_branch(
    forge: &Forge,
    repo: &str,
    head: &crate::deliveries::Head,
) -> Result<Option<String>> {
    if !head.same_repo {
        return Ok(Some("it lives in a fork".into()));
    }
    // GitHub and Gitea route a branch name with its slashes as they are,
    // GitLab wants the whole name as one encoded segment.
    let name = crate::http::encode(&head.branch, true);
    let branch_path = format!("repos/{repo}/branches/{name}");
    let gitlab_path = format!(
        "projects/{}/repository/branches/{}",
        project(repo),
        crate::http::encode(&head.branch, false)
    );
    let branch = forge_json(forge, &branch_path, &branch_path, &gitlab_path)
        .context("failed to look the branch up (already deleted?)")?;
    let meta = forge_json(
        forge,
        &format!("repos/{repo}"),
        &format!("repos/{repo}"),
        &format!("projects/{}", project(repo)),
    )?;
    let open = open_prs(forge, repo)?;
    if let Some(why) = keep_branch(head, &branch, meta["default_branch"].as_str(), &open) {
        return Ok(Some(why));
    }
    // Every forge answers 204 with no body, so there is nothing to parse.
    forge_call(
        forge,
        Method::DELETE,
        &branch_path,
        &format!("repos/{repo}/git/refs/heads/{name}"),
        &gitlab_path,
        &[],
    )?;
    Ok(None)
}

/// Why a settled PR's head branch must stay, read off the forge's answer for
/// the branch, the repository's default branch, and its open PRs; None when
/// nothing does. Fails closed: a branch whose protection or tip the answer
/// doesn't state, or a repository that names no default branch, is kept.
fn keep_branch(
    head: &crate::deliveries::Head,
    branch: &serde_json::Value,
    default_branch: Option<&str>,
    open: &[serde_json::Value],
) -> Option<String> {
    let name = &head.branch;
    match default_branch {
        Some(default) if default == name => return Some("it is the default branch".into()),
        Some(_) => {}
        None => return Some("the repository names no default branch".into()),
    }
    if branch["protected"].as_bool() != Some(false) {
        return Some("it is protected".into());
    }
    // Gitea and GitLab name the tip commit `id`, GitHub `sha`.
    let commit = &branch["commit"];
    let tip = commit["id"].as_str().or_else(|| commit["sha"].as_str());
    if tip != Some(head.sha.as_str()) {
        return Some(format!(
            "it has moved past the PR's head {} to {}",
            head.sha,
            tip.unwrap_or("an unknown commit")
        ));
    }
    let heads = |pr: &serde_json::Value| {
        pr["head"]["ref"]
            .as_str()
            .or_else(|| pr["source_branch"].as_str())
            == Some(name)
    };
    if let Some(pr) = open.iter().find(|pr| heads(pr)) {
        let number = pr["number"].as_u64().or_else(|| pr["iid"].as_u64());
        return Some(format!(
            "it is also the head of open PR #{}",
            number.map_or("?".into(), |n| n.to_string())
        ));
    }
    None
}

/// One request via the gh/glab `api` subcommand, which share the same
/// shape; a refusal is a failed exit.
fn api_call(
    forge: &Forge,
    program: &str,
    method: Method,
    path: &str,
    fields: &[(&str, &str)],
) -> Result<Vec<u8>> {
    let mut command = Command::new(program);
    command.args(["api", "-X", method.as_str(), path]);
    for (key, value) in fields {
        command.arg("-f").arg(format!("{key}={value}"));
    }
    crate::child::output(command.envs(forge.env()))
}

/// How long one Gitea API request may take. Without a cap a forge that
/// accepts the connection and then says nothing hangs the caller forever,
/// and the callers are the activity watcher and the draw's probes — the
/// runner would stop taking turns at all.
const API_TIMEOUT_SECS: u64 = 30;

/// One request against the Gitea API; `tea` has no generic api subcommand,
/// so it goes over HTTP directly like the proxy's management calls do.
fn gitea_call(
    forge: &Forge,
    method: Method,
    path: &str,
    fields: &[(&str, &str)],
) -> Result<Vec<u8>> {
    let url = format!("{}/api/v1/{path}", forge.url);
    let auth = format!("token {}", forge.token);
    let payload = (!fields.is_empty()).then(|| {
        let object = fields
            .iter()
            .map(|(key, value)| ((*key).to_owned(), serde_json::Value::from(*value)))
            .collect();
        serde_json::Value::Object(object)
    });
    let (status, body) = crate::http::send(
        method,
        &url,
        Some(("Authorization", auth.as_str())),
        payload.as_ref(),
        API_TIMEOUT_SECS,
    )?;
    if !(200..300).contains(&status) {
        bail!("the gitea api answered HTTP {status} for {url}: {body}");
    }
    Ok(body.into_bytes())
}

/// Whether the PR or issue was authored by `login`, under either forge
/// spelling of the author field (`user.login` on Gitea and GitHub,
/// `author.username` on GitLab).
pub fn authored_by(pr: &serde_json::Value, login: &str) -> bool {
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
        let prs = json!([
            {"number": 1, "user": {"login": "someone"}, "requested_reviewers": [{"login": "bot"}], "head": {"sha": "aaa"}},
            {"number": 2, "user": {"login": "bot"}, "requested_reviewers": [{"login": "bot"}], "head": {"sha": "bbb"}},
            {"number": 3, "user": {"login": "someone"}, "requested_reviewers": [], "head": {"sha": "ccc"}},
            {"number": 5, "user": {"login": "bot"}, "requested_reviewers": [{"login": "someone"}], "head": {"sha": "eee"}},
            // Keyless, and the bot's to review: dropped rather than keyed
            // or made to fail the whole probe.
            {"requested_reviewers": [{"login": "bot"}], "head": {"sha": "fff"}},
            {"iid": 4, "author": {"username": "someone"}, "reviewers": [{"username": "bot"}], "sha": "ddd"},
        ]);
        assert_eq!(
            review_requests(&gitea("bot"), "me/repo", prs.as_array().unwrap()).unwrap(),
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
        let prs = json!([
            {"number": 15, "user": {"login": "bot"}, "mergeable": false, "head": {"sha": "eee"}},
            {"number": 1, "user": {"login": "bot"}, "mergeable": true, "head": {"sha": "aaa"}},
            {"number": 2, "user": {"login": "someone"}, "mergeable": false, "head": {"sha": "bbb"}},
            {"user": {"login": "bot"}, "mergeable": false, "head": {"sha": "ddd"}},
            {"number": 12, "user": {"login": "bot"}, "mergeable": false, "head": {"sha": "ccc"}},
        ]);
        assert_eq!(
            conflicting_prs(&gitea("bot"), "me/repo", prs.as_array().unwrap()).unwrap(),
            ["12@ccc", "15@eee"]
        );
    }

    /// A Gitea forge with `login` as its resolved account. Gitea is the kind
    /// whose listing answers both probes on its own, so the keys they build
    /// can be checked without a forge to ask.
    fn gitea(login: &str) -> Forge {
        Forge {
            login: login.into(),
            ..Forge::test(ForgeKind::Gitea, "https://gitea.example.com")
        }
    }

    /// A draw with every pair weighted alike, which is the uniform draw the
    /// tests below were written against.
    fn uniform<'a>(
        forge: &'a Forge,
        pool: &'a [String],
        check: impl Fn(&str, &str) -> bool,
    ) -> Option<(&'a str, &'a str)> {
        draw(forge, pool, check, |_, _| 1.0)
    }

    /// The weights decide the ungated order: a pair weighted so far above
    /// the rest that its key rounds to 1 comes first every time, and a pair
    /// weighted at nothing comes last.
    #[test]
    fn draw_takes_the_ungated_pairs_in_weighted_order() {
        let (forge, pool) = Forge::sweep(&["me/one", "me/two", "me/three"], &["docs"]);
        for _ in 0..64 {
            let probed = std::cell::RefCell::new(Vec::new());
            assert!(
                draw(
                    &forge,
                    &pool,
                    |_, repo| {
                        probed.borrow_mut().push(repo.to_owned());
                        false
                    },
                    |_, repo| match repo {
                        "me/one" => 1e300,
                        "me/two" => 1.0,
                        _ => 0.0,
                    },
                )
                .is_none()
            );
            let probed = probed.borrow();
            assert_eq!(*probed, ["me/one", "me/two", "me/three"]);
        }
    }

    /// A heavier pair is more likely to go first, not certain to: the order
    /// is still a draw, so a lighter pair gets its turn now and then.
    #[test]
    fn a_lighter_pair_is_still_drawn_sometimes() {
        let (forge, pool) = Forge::sweep(&["me/heavy", "me/light"], &["docs"]);
        let mut light_first = 0;
        for _ in 0..512 {
            let (_, repo) = draw(
                &forge,
                &pool,
                |_, _| true,
                |_, repo| if repo == "me/heavy" { 3.0 } else { 1.0 },
            )
            .unwrap();
            if repo == "me/light" {
                light_first += 1;
            }
        }
        // Expected one in four; the bounds are many standard deviations
        // out, so the test is about the shape of the draw and not its seed.
        assert!((40..=220).contains(&light_first), "{light_first}");
    }

    /// A draw that lands on gated work never weighs anything: a weight can
    /// cost a forge request, which that draw has no use for.
    #[test]
    fn gated_work_is_drawn_without_weighing() {
        let (forge, pool) = Forge::sweep(&["me/one", "me/two"], &["docs", "rebase"]);
        let weighed = std::cell::Cell::new(0);
        let (task, _) = draw(
            &forge,
            &pool,
            |task, _| task == "rebase",
            |_, _| {
                weighed.set(weighed.get() + 1);
                1.0
            },
        )
        .unwrap();
        assert_eq!(task, "rebase");
        assert_eq!(weighed.get(), 0);
    }

    /// The gated pass is not weighted: every gated pair is probed before
    /// any ungated one however the weights fall.
    #[test]
    fn weights_do_not_reach_the_gated_pass() {
        let (forge, pool) = Forge::sweep(&["me/one", "me/two"], &["docs", "rebase"]);
        for _ in 0..16 {
            let probed = std::cell::RefCell::new(Vec::new());
            let (task, _) = draw(
                &forge,
                &pool,
                |task, repo| {
                    probed.borrow_mut().push(format!("{task} {repo}"));
                    task == "docs"
                },
                |task, _| if task == "docs" { 1e6 } else { 0.0 },
            )
            .unwrap();
            assert_eq!(task, "docs");
            let probed = probed.borrow();
            assert_eq!(probed.len(), 3, "{probed:?}");
            assert!(probed[..2].iter().all(|entry| entry.starts_with("rebase")));
        }
    }

    #[test]
    fn weighted_order_keeps_every_item_and_sinks_weightless_ones() {
        let order = weighted_order(vec![0.0, 2.0, -1.0, 5.0, 1.0], |w| *w);
        assert_eq!(order.len(), 5);
        assert!(order[3..].iter().all(|w| *w <= 0.0), "{order:?}");
        assert!(weighted_order(Vec::<f64>::new(), |w| *w).is_empty());
    }

    #[test]
    fn draw_prefers_a_gated_task_that_has_work() {
        let (forge, pool) = Forge::sweep(&["me/repo"], &["bump", "rebase"]);
        for _ in 0..16 {
            let (task, _) = uniform(&forge, &pool, |task, _| task == "rebase").unwrap();
            assert_eq!(task, "rebase");
        }
    }

    /// Every (gated task, repo) pair is probed before giving up on them.
    #[test]
    fn draw_probes_every_repository() {
        let (forge, pool) = Forge::sweep(&["me/one", "me/two", "me/three"], &["respond", "rebase"]);
        let seen = std::cell::RefCell::new(BTreeSet::new());
        assert!(
            uniform(&forge, &pool, |task, repo| {
                seen.borrow_mut().insert(format!("{task} {repo}"));
                false
            })
            .is_none()
        );
        assert_eq!(seen.borrow().len(), 2 * pool.len());
    }

    /// The ungated pass is checked too, so the skip cache can hold a task
    /// back; only once nothing is left does the draw come up empty.
    #[test]
    fn draw_skips_an_ungated_task_the_check_rejects() {
        let (forge, pool) = Forge::sweep(&["me/repo"], &["docs", "bump"]);
        for _ in 0..16 {
            let (task, _) = uniform(&forge, &pool, |task, _| task != "docs").unwrap();
            assert_eq!(task, "bump");
        }
        assert!(uniform(&forge, &pool, |_, _| false).is_none());
    }

    /// Every gated pair is probed before an ungated task is considered, so a
    /// cheap cached task can never outrank a review comment.
    #[test]
    fn draw_probes_every_gated_pair_first() {
        let (forge, pool) = Forge::sweep(&["me/one", "me/two"], &["docs", "rebase"]);
        for _ in 0..16 {
            let probed = std::cell::RefCell::new(Vec::new());
            let (task, _) = uniform(&forge, &pool, |task, repo| {
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

    /// Tasks are picked per repository: a task that is off for one
    /// repository is never drawn there, however often it is on elsewhere,
    /// and a repository the forge lists but the settings don't enable
    /// contributes no pairs at all.
    #[test]
    fn draw_only_pairs_a_task_with_the_repositories_that_have_it_on() {
        let (mut forge, mut pool) = Forge::sweep(&["me/one", "me/two"], &["docs", "bump"]);
        forge.repos.get_mut("me/two").unwrap().remove("docs");
        pool.push("me/unlisted".into());
        let seen = std::cell::RefCell::new(BTreeSet::new());
        assert!(
            uniform(&forge, &pool, |task, repo| {
                seen.borrow_mut().insert(format!("{task} {repo}"));
                false
            })
            .is_none()
        );
        let seen = seen.borrow();
        assert_eq!(
            *seen,
            ["docs me/one", "bump me/one", "bump me/two"]
                .map(str::to_owned)
                .into()
        );
    }

    /// A settled PR's branch is deleted only when nothing says it is still
    /// somebody's, read off each forge's spelling of the branch and of the
    /// open PRs; a field the answer leaves out keeps the branch.
    #[test]
    fn a_settled_prs_branch_is_kept_whenever_it_may_still_be_somebodys() {
        use crate::deliveries::Head;
        let head = Head {
            branch: "topic".into(),
            sha: "abc".into(),
            same_repo: true,
        };
        // Gitea and GitLab name the tip `commit.id`, GitHub `commit.sha`.
        let gitea = json!({"name": "topic", "protected": false, "commit": {"id": "abc"}});
        let github = json!({"name": "topic", "protected": false, "commit": {"sha": "abc"}});
        let others = [
            json!({"number": 9, "head": {"ref": "other"}}),
            json!({"iid": 10, "source_branch": "another"}),
        ];
        for branch in [&gitea, &github] {
            assert_eq!(keep_branch(&head, branch, Some("main"), &others), None);
        }

        let kept = |head: &Head,
                    branch: &serde_json::Value,
                    default: Option<&str>,
                    open: &[serde_json::Value]| {
            keep_branch(head, branch, default, open).expect("the branch was not kept")
        };
        assert!(kept(&head, &gitea, Some("topic"), &[]).contains("default"));
        assert!(kept(&head, &gitea, None, &[]).contains("default"));
        for protected in [json!(true), json!(null)] {
            let mut branch = gitea.clone();
            branch["protected"] = protected;
            assert!(kept(&head, &branch, Some("main"), &[]).contains("protected"));
        }
        let moved = json!({"protected": false, "commit": {"id": "def"}});
        assert!(kept(&head, &moved, Some("main"), &[]).contains("moved past"));
        let tipless = json!({"protected": false});
        assert!(kept(&head, &tipless, Some("main"), &[]).contains("moved past"));
        let reused = [json!({"number": 12, "head": {"ref": "topic"}})];
        assert!(kept(&head, &gitea, Some("main"), &reused).contains("#12"));
        let reused = [json!({"iid": 4, "source_branch": "topic"})];
        assert!(kept(&head, &gitea, Some("main"), &reused).contains("#4"));
    }

    /// A receipt reaches Gitea as a POST to the issue's comment endpoint —
    /// PRs included, which Gitea comments on as issues — carrying the token
    /// and the body as JSON; a refusal is an error the caller can log.
    #[test]
    fn a_gitea_comment_is_posted_to_the_issue_endpoint() {
        use std::io::{Read, Write};
        let serve = |status: &'static str| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let request = std::thread::spawn(move || {
                let (mut conn, _) = listener.accept().unwrap();
                let mut seen = Vec::new();
                let mut buf = [0; 4096];
                // Headers, then as much body as Content-Length says.
                loop {
                    let n = conn.read(&mut buf).unwrap();
                    seen.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&seen).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if seen.len() >= end + 4 + length || n == 0 {
                            break;
                        }
                    }
                }
                write!(
                    conn,
                    "HTTP/1.1 {status}\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}"
                )
                .unwrap();
                String::from_utf8(seen).unwrap()
            });
            (url, request)
        };

        let (url, request) = serve("201 Created");
        let forge = Forge {
            login: "bot".into(),
            ..Forge::test(ForgeKind::Gitea, &url)
        };
        comment(
            &forge,
            "me/repo",
            crate::deliveries::Kind::Pr,
            7,
            "cost: 3 tokens",
        )
        .unwrap();
        let request = request.join().unwrap();
        assert!(
            request.starts_with("POST /api/v1/repos/me/repo/issues/7/comments HTTP/1.1"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: token tok"),
            "{request}"
        );
        assert!(
            request.ends_with(r#"{"body":"cost: 3 tokens"}"#),
            "{request}"
        );

        let (url, request) = serve("403 Forbidden");
        let forge = Forge {
            login: "bot".into(),
            ..Forge::test(ForgeKind::Gitea, &url)
        };
        let err = comment(&forge, "me/repo", crate::deliveries::Kind::Issue, 3, "x").unwrap_err();
        assert!(format!("{err:#}").contains("403"), "{err:#}");
        request.join().unwrap();
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
