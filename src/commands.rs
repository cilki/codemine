//! Forge commands: a comment that opens a line with `@<bot> <task>` queues
//! that task on the comment's repository, the same run-now request the web
//! UI's button queues, with the rest of the comment passed to the turn as
//! what to look at.
//!
//! The watcher reads them off the notification feed it already polls, so no
//! request is added for a poll with nothing new. Only someone with write
//! access to the repository may ask — anyone else could otherwise spend the
//! owner's turns and the bot's push rights — and the bot's own comments are
//! never read as commands, since its replies and PR descriptions name tasks
//! all the time. A command that is taken gets an 👀 reaction and its
//! notification marked read, so it doesn't also queue a feedback turn that
//! would have to work out what to do with it. The reaction doubles as the
//! guard against running a command twice: the queue doesn't survive a
//! restart, so a command whose thread couldn't be marked read is recognized
//! by the reaction instead.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use reqwest::Method;
use serde_json::Value;
use tracing::{info, warn};

use crate::config::{Forge, ForgeKind};
use crate::events::{Focus, Pending, Request, repo_of, stamp};
use crate::precheck::{forge_call, forge_json, project};

/// The reaction a taken command is acknowledged with; every forge spells it
/// the same.
const ACK: &str = "eyes";

/// Queue the commands on the feed's fresh entries and drop each entry that
/// carried one from `feed`, so the caller doesn't also queue its repository
/// for a feedback turn. `seen` is what the previous poll saw, thread id →
/// stamp, and is replaced by this poll's.
pub fn take(
    forge: &mut Forge,
    feed: &mut Value,
    seen: &mut BTreeMap<String, String>,
    pending: &Pending,
) {
    let Some(entries) = feed.as_array_mut() else {
        return;
    };
    let current: BTreeMap<String, String> = entries
        .iter()
        .filter_map(|entry| Some((thread_id(entry)?, stamp(forge.kind, entry).to_owned())))
        .collect();
    let fresh: Vec<String> = current
        .iter()
        .filter(|(id, stamp)| seen.get(*id).is_none_or(|old| old < *stamp))
        .map(|(id, _)| id.clone())
        .collect();
    if fresh.is_empty() {
        *seen = current;
        return;
    }
    // The login is what a command has to mention, and what tells the bot's
    // own comments apart. Without it nothing is marked seen, so the next
    // poll tries the same entries again.
    if forge.login.is_empty()
        && let Err(err) = crate::identity::resolve(forge)
    {
        warn!("not reading forge commands: {err:#}");
        return;
    }
    *seen = current;
    let mut handled = BTreeSet::new();
    for entry in entries.iter() {
        let Some(id) = thread_id(entry).filter(|id| fresh.contains(id)) else {
            continue;
        };
        let Some(repo) = repo_of(forge.kind, entry).filter(|r| forge.repos.contains_key(*r)) else {
            continue;
        };
        match command(forge, repo, entry) {
            Ok(Found::New(request)) => {
                react(forge, repo, entry);
                mark_read(forge, repo, &id);
                pending.request(request);
                handled.insert(id);
            }
            Ok(Found::Taken) => {
                mark_read(forge, repo, &id);
                handled.insert(id);
            }
            Ok(Found::Nothing) => {}
            Err(err) => {
                // Forgotten, so the next poll reads the comment again.
                seen.remove(&id);
                warn!("failed to read a forge command on {repo}: {err:#}");
            }
        }
    }
    entries.retain(|entry| thread_id(entry).is_none_or(|id| !handled.contains(&id)));
}

/// What one feed entry's newest comment turned out to be.
enum Found {
    /// Not a command the bot takes.
    Nothing,
    /// A command taken before a restart, whose thread only needs clearing.
    Taken,
    New(Request),
}

/// What one feed entry's newest comment asks for.
fn command(forge: &Forge, repo: &str, entry: &Value) -> Result<Found> {
    let Some(comment) = comment(forge, repo, entry)? else {
        return Ok(Found::Nothing);
    };
    if comment.author.eq_ignore_ascii_case(&forge.login) {
        return Ok(Found::Nothing);
    }
    let Some((task, text)) = parse(&comment.body, &forge.login) else {
        return Ok(Found::Nothing);
    };
    // A forge refusing to say — most answer a non-member's permission with
    // 403 or 404 — is no write access either.
    if let Ok(false) | Err(_) = may_ask(forge, repo, &comment) {
        info!(
            "ignoring @{} {task} on {repo} from {}: no write access",
            forge.login, comment.author
        );
        return Ok(Found::Nothing);
    }
    if acknowledged(forge, repo, &comment) {
        info!(
            "@{} {task} on {repo} from {} was taken already",
            forge.login, comment.author
        );
        return Ok(Found::Taken);
    }
    info!(
        "@{} {task} on {repo} from {}; queueing it as run now",
        forge.login, comment.author
    );
    Ok(Found::New(Request {
        kind: forge.kind,
        repo: repo.to_owned(),
        task,
        focus: Some(Focus {
            author: comment.author,
            url: comment.url,
            text,
        }),
    }))
}

/// The task a comment asks for and the rest of what it says, if it is a
/// command: the first line that opens with `@login` (in any case) followed
/// by the slug of a task the runner defines. Whatever follows the slug on
/// that line, past a separator (`—`, `-`, `:`), and every line after it is
/// the focus. Quoted lines and code blocks are passed over, so quoting an
/// earlier command doesn't run it again; a first mention whose next word is
/// no task's slug is ordinary conversation, not a command.
pub fn parse(body: &str, login: &str) -> Option<(&'static str, String)> {
    if login.is_empty() {
        return None;
    }
    let mut fenced = false;
    let mut lines = body.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced || trimmed.starts_with('>') || line.starts_with("    ") || line.starts_with('\t')
        {
            continue;
        }
        let Some(rest) = mention(trimmed, login) else {
            continue;
        };
        let rest = rest.trim_start_matches([':', ',']).trim_start();
        let (word, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let task = crate::prompts::task(word.trim_end_matches([':', ',', '.']))?;
        let mut focus = after
            .trim()
            .trim_start_matches(['—', '–', '-', ':'])
            .trim()
            .to_owned();
        for line in lines {
            focus.push('\n');
            focus.push_str(line);
        }
        return Some((&task.slug, focus.trim().to_owned()));
    }
    None
}

/// The rest of `line` after a leading `@login`, which has to end there: a
/// longer login that merely starts the same is someone else.
fn mention<'a>(line: &'a str, login: &str) -> Option<&'a str> {
    let rest = line.strip_prefix('@')?;
    let name = rest.get(..login.len())?;
    let after = &rest[login.len()..];
    let ends = after
        .chars()
        .next()
        .is_none_or(|c| c.is_whitespace() || c == ':' || c == ',');
    (name.eq_ignore_ascii_case(login) && ends).then_some(after)
}

/// The comment a feed entry is about, as far as a command needs it.
struct Comment {
    /// The comment's own id: what a reaction is posted against.
    id: u64,
    author: String,
    /// GitLab's author id, which its membership endpoint is keyed on.
    author_id: Option<u64>,
    body: String,
    url: String,
}

/// The newest comment of a feed entry. Gitea and GitHub notifications only
/// point at it, so it costs one GET, and only for an issue or PR comment —
/// a thread whose newest event is a push or a review has no comment body to
/// read. A GitLab todo carries its note's text itself, and only mentions
/// and direct addresses are read.
fn comment(forge: &Forge, repo: &str, entry: &Value) -> Result<Option<Comment>> {
    if forge.kind == ForgeKind::Gitlab {
        let action = entry["action_name"].as_str().unwrap_or_default();
        if !matches!(action, "mentioned" | "directly_addressed") {
            return Ok(None);
        }
        let url = entry["target_url"].as_str().unwrap_or_default();
        let (Some(author), Some(id)) = (entry["author"]["username"].as_str(), note_id(url)) else {
            return Ok(None);
        };
        return Ok(Some(Comment {
            id,
            author: author.to_owned(),
            author_id: entry["author"]["id"].as_u64(),
            body: entry["body"].as_str().unwrap_or_default().to_owned(),
            url: url.to_owned(),
        }));
    }
    let Some(id) = comment_id(
        entry["subject"]["latest_comment_url"]
            .as_str()
            .unwrap_or_default(),
    ) else {
        return Ok(None);
    };
    let path = format!("repos/{repo}/issues/comments/{id}");
    let comment = forge_json(forge, &path, &path, "")?;
    Ok(Some(Comment {
        id,
        author: comment["user"]["login"]
            .as_str()
            .context("the comment names no author")?
            .to_owned(),
        author_id: None,
        body: comment["body"].as_str().unwrap_or_default().to_owned(),
        url: comment["html_url"].as_str().unwrap_or_default().to_owned(),
    }))
}

/// The comment id at the end of an issue-comment API URL
/// (`…/repos/o/r/issues/comments/123`); None for anything else the newest
/// event of a thread can point at.
fn comment_id(url: &str) -> Option<u64> {
    let (before, id) = url.rsplit_once('/')?;
    before
        .ends_with("/issues/comments")
        .then(|| id.parse().ok())?
}

/// The note id at the end of a GitLab todo's target URL (`…#note_123`).
fn note_id(url: &str) -> Option<u64> {
    url.rsplit_once("#note_")?.1.parse().ok()
}

/// Whether the comment's author may push to the repository: write access or
/// better, as each forge spells it.
fn may_ask(forge: &Forge, repo: &str, comment: &Comment) -> Result<bool> {
    let gitlab = match (forge.kind, comment.author_id) {
        (ForgeKind::Gitlab, None) => return Ok(false),
        (_, id) => format!(
            "projects/{}/members/all/{}",
            project(repo),
            id.unwrap_or_default()
        ),
    };
    let path = format!("repos/{repo}/collaborators/{}/permission", comment.author);
    Ok(writes(&forge_json(forge, &path, &path, &gitlab)?))
}

/// Whether a permission answer grants write access: Gitea's and GitHub's
/// `permission` name, or GitLab's `access_level` (30 is Developer).
fn writes(answer: &Value) -> bool {
    matches!(
        answer["permission"].as_str(),
        Some("write" | "maintain" | "admin" | "owner")
    ) || answer["access_level"]
        .as_u64()
        .is_some_and(|level| level >= 30)
}

/// Whether the bot already reacted to the comment, which is how a command
/// taken before a restart is told apart. GitLab needs no such check: a todo
/// marked done leaves its pending list for good. A reaction listing that
/// can't be read counts as no reaction.
fn acknowledged(forge: &Forge, repo: &str, comment: &Comment) -> bool {
    if forge.kind == ForgeKind::Gitlab {
        return false;
    }
    let path = format!("repos/{repo}/issues/comments/{}/reactions", comment.id);
    forge_json(forge, &path, &path, "").is_ok_and(|reactions| reacted(&reactions, &forge.login))
}

/// Whether `login` is among the reactions with the acknowledgment.
fn reacted(reactions: &Value, login: &str) -> bool {
    reactions.as_array().is_some_and(|all| {
        all.iter().any(|r| {
            r["content"].as_str() == Some(ACK)
                && r["user"]["login"]
                    .as_str()
                    .is_some_and(|who| who.eq_ignore_ascii_case(login))
        })
    })
}

/// React to the command, so its author sees it was taken. Best-effort, like
/// [`mark_read`]: a failure is logged and the command still runs.
fn react(forge: &Forge, repo: &str, entry: &Value) {
    let reaction = match forge.kind {
        ForgeKind::Gitlab => {
            gitlab_note_path(repo, entry).map(|path| format!("{path}/award_emoji"))
        }
        _ => comment_id(
            entry["subject"]["latest_comment_url"]
                .as_str()
                .unwrap_or_default(),
        )
        .map(|id| format!("repos/{repo}/issues/comments/{id}/reactions")),
    };
    if let Some(path) = reaction {
        let field = match forge.kind {
            ForgeKind::Gitlab => ("name", ACK),
            _ => ("content", ACK),
        };
        if let Err(err) = forge_call(forge, Method::POST, &path, &path, &path, &[field]) {
            warn!("failed to react to the command on {repo}: {err:#}");
        }
    }
}

/// Mark the command's notification thread (GitLab: todo) read, so it isn't
/// also taken for feedback waiting.
fn mark_read(forge: &Forge, repo: &str, thread: &str) {
    let threads = format!("notifications/threads/{thread}");
    if let Err(err) = match forge.kind {
        ForgeKind::Gitlab => {
            let todo = format!("todos/{thread}/mark_as_done");
            forge_call(forge, Method::POST, "", "", &todo, &[])
        }
        _ => forge_call(forge, Method::PATCH, &threads, &threads, "", &[]),
    } {
        warn!("failed to mark the command's notification read on {repo}: {err:#}");
    }
}

/// The API path of the note a GitLab todo points at.
fn gitlab_note_path(repo: &str, entry: &Value) -> Option<String> {
    let noteable = match entry["target_type"].as_str()? {
        "Issue" => "issues",
        "MergeRequest" => "merge_requests",
        _ => return None,
    };
    let iid = entry["target"]["iid"].as_u64()?;
    let note = note_id(entry["target_url"].as_str()?)?;
    Some(format!(
        "projects/{}/{noteable}/{iid}/notes/{note}",
        project(repo)
    ))
}

/// A feed entry's thread id: a number on Gitea and GitLab, a string on
/// GitHub.
fn thread_id(entry: &Value) -> Option<String> {
    let id = &entry["id"];
    id.as_u64()
        .map(|id| id.to_string())
        .or_else(|| id.as_str().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_command_names_the_bot_then_a_task() {
        assert_eq!(
            parse("@codemine docs", "codemine"),
            Some(("docs", String::new()))
        );
        assert_eq!(
            parse("@CodeMine docs — the port is 8080 now", "codemine"),
            Some(("docs", "the port is 8080 now".into()))
        );
        assert_eq!(
            parse("@codemine: bump: please", "codemine"),
            Some(("bump", "please".into()))
        );
        // Lines after the command are part of the focus; lines before a
        // mention are not.
        assert_eq!(
            parse(
                "Thanks!\n\n@codemine audit - the tar crate\nand its users",
                "codemine"
            ),
            Some(("audit", "the tar crate\nand its users".into()))
        );
    }

    #[test]
    fn anything_else_is_not_a_command() {
        // An unknown slug is conversation, and so is a mention mid-line.
        assert_eq!(parse("@codemine thanks for this", "codemine"), None);
        assert_eq!(parse("ask @codemine docs", "codemine"), None);
        // Only the first mention line counts.
        assert_eq!(parse("@codemine hi\n@codemine docs", "codemine"), None);
        // A longer login that starts the same is somebody else.
        assert_eq!(parse("@codeminer docs", "codemine"), None);
        // Quoted and fenced commands are passed over.
        assert_eq!(parse("> @codemine docs", "codemine"), None);
        assert_eq!(parse("```\n@codemine docs\n```", "codemine"), None);
        assert_eq!(parse("    @codemine docs", "codemine"), None);
        assert_eq!(
            parse("> @codemine docs\n\n@codemine bump", "codemine"),
            Some(("bump", String::new()))
        );
        // No login, no command.
        assert_eq!(parse("@ docs", ""), None);
    }

    #[test]
    fn write_access_is_read_off_every_forges_answer() {
        for permission in ["write", "maintain", "admin", "owner"] {
            assert!(writes(&json!({"permission": permission})), "{permission}");
        }
        for permission in ["read", "triage", "none"] {
            assert!(!writes(&json!({"permission": permission})), "{permission}");
        }
        assert!(writes(&json!({"access_level": 30})));
        assert!(writes(&json!({"access_level": 50})));
        assert!(!writes(&json!({"access_level": 20})));
        assert!(!writes(&json!({"message": "404 Not found"})));
    }

    #[test]
    fn only_the_bots_eyes_acknowledge_a_command() {
        let reactions = json!([
            {"user": {"login": "someone"}, "content": "eyes"},
            {"user": {"login": "codemine"}, "content": "+1"},
        ]);
        assert!(!reacted(&reactions, "codemine"));
        let reactions = json!([{"user": {"login": "CodeMine"}, "content": "eyes"}]);
        assert!(reacted(&reactions, "codemine"));
        assert!(!reacted(&json!(null), "codemine"));
    }

    #[test]
    fn ids_are_read_off_the_urls_that_carry_them() {
        assert_eq!(
            comment_id("https://git.example.com/api/v1/repos/o/r/issues/comments/42"),
            Some(42)
        );
        assert_eq!(comment_id("https://api.github.com/repos/o/r/pulls/7"), None);
        assert_eq!(
            comment_id("https://api.github.com/repos/o/r/pulls/comments/9"),
            None
        );
        assert_eq!(comment_id(""), None);
        assert_eq!(
            note_id("https://gitlab.com/o/r/-/issues/3#note_99"),
            Some(99)
        );
        assert_eq!(note_id("https://gitlab.com/o/r/-/issues/3"), None);
        let todo = json!({
            "target_type": "MergeRequest",
            "target": {"iid": 3},
            "target_url": "https://gitlab.com/o/r/-/merge_requests/3#note_99",
        });
        assert_eq!(
            gitlab_note_path("o/r", &todo).as_deref(),
            Some("projects/o%2Fr/merge_requests/3/notes/99")
        );
    }

    /// Without a fresh entry nothing is fetched or queued — the login isn't
    /// even resolved — and the entries stay on the feed for the activity
    /// queue.
    #[test]
    fn a_feed_with_nothing_new_costs_nothing() {
        let (mut forge, _) = Forge::sweep(&["me/repo"], &["docs"]);
        let mut feed = json!([
            {"id": "1", "repository": {"full_name": "me/repo"}, "updated_at": "2"},
        ]);
        let mut seen = BTreeMap::from([("1".to_owned(), "2".to_owned())]);
        let pending = Pending::new();
        take(&mut forge, &mut feed, &mut seen, &pending);
        assert!(pending.is_empty());
        assert_eq!(feed.as_array().unwrap().len(), 1);
        assert_eq!(seen["1"], "2");
    }
}
