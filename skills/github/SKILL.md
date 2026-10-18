---
name: github
description:
  How to list, open pull requests, respond to review feedback, review other
  people's pull requests, and file issues on our GitHub repositories via the
  gh CLI.
---

These repositories live on GitHub. You deliver work as pull requests — never by
pushing to a base branch directly.

Use the `gh` CLI for everything on GitHub; it authenticates from the
`GITHUB_TOKEN` environment variable that is already set. Inside a clone it
works out the repository from the git remote, so the `--repo` flag is only
needed when you are outside one.

See the repositories your account can reach, listed as `<owner>/<repo>`:

```sh
gh api user/repos --paginate --jq '.[].full_name'
```

## Opening a pull request

1. First check that you are not repeating work that has already been done or
   already been turned down:

   ```sh
   gh pr list --repo <owner>/<repo> --state all # every pull request, not just open ones
   ```

   Each is marked `OPEN`, `MERGED`, or `CLOSED`. A merged one already landed
   on the default branch. A closed one was either turned down or closed by you
   as obsolete — read it with `gh pr view <number> --comments` to find out
   which before you go near the same ground again, rather than assuming it was
   rejected. Never reopen a closed PR and never make duplicate PRs.

2. Branch before you touch anything: `git checkout -b <short-topic>`.
3. Commit with a message that explains why the change is being made, not just
   what changed.
4. Push: `git push -u origin <short-topic>`.
5. Open the pull request; the base defaults to the default branch:

   ```sh
   gh pr create --head <short-topic> --title "<title>" --body-file <body-file>
   ```
6. Include how long the session took and how many tokens were consumed in the PR description

If a push is rejected, stop and report it rather than working around it.

## Responding to review feedback

To see your open pull requests and what reviewers have said about them:

```sh
gh pr list --repo <owner>/<repo> --json number,state,headRefName,baseRefName,title,author,mergeable # open pull requests, with their branches; mergeable is MERGEABLE, CONFLICTING, or UNKNOWN
gh pr view <number> --comments --repo <owner>/<repo>                               # the conversation under a pull request
```

When a comment points at a file or line, read that spot in the diff
(`git diff <base>...<head>`) to see what the reviewer is pointing at. To act on
feedback:

1. Check out the pull request's branch (`gh pr checkout <number>`) and pull
   first — the branch may have moved since you last saw it.
2. Make the changes as **new commits**. Do not rewrite history on a branch that
   is under review; force-pushing invalidates the comments the reviewer left.
3. Push. The pull request updates itself — you do not open a new one.
4. Reply so the reviewer knows what happened:

   ```sh
   gh pr comment <number> --body "<what you changed and why>"
   ```

If a comment is ambiguous, or asks for something you think is wrong, say so in
the reply instead of guessing. Address every comment: either make the change or
explain why you didn't.

## Closing a pull request of your own

A pull request whose change no longer applies — the base branch already carries
an equivalent change, the code it touched is gone, or the problem it fixed no
longer exists — is worth less than the reviewer's attention it is holding.
Close it. Say why first, so the next look at it reads as "this went stale" and
not "this was turned down":

```sh
gh pr comment <number> --body "<why the change no longer applies>"
gh pr close <number> --delete-branch   # closes the pull request and deletes its branch
```

Only your own pull requests, and only on that test: another author's is never
yours to close, and a change that still applies is never obsolete however
awkward it has become to land.

## Reviewing someone else's pull request

You review a pull request only when its author asked you to: your account is
listed under its pending review requests. Submitting the review clears the
request, so a pull request you already answered stays out of the list until
someone requests a review from you again.

```sh
gh pr list --repo <owner>/<repo> --json number,title,author,headRefName,baseRefName,reviewRequests # candidates: another author, and you under reviewRequests
gh pr view <number> --comments --repo <owner>/<repo>   # the description and the conversation so far
gh pr diff <number> --repo <owner>/<repo>              # the whole diff against the base
gh pr checkout <number>                                # to build it or run its tests
```

Submit the whole review as one request, so the author is notified once.
Write it as JSON: `event` is `APPROVE`, `REQUEST_CHANGES`, or `COMMENT`;
`body` is the summary; each entry of `comments` anchors one finding to a line
on the new side of the diff (`line` is the line number in the file as it is
after the change, and only lines the diff touches or shows as context can be
anchored):

```sh
cat > review.json <<'EOF'
{
  "event": "REQUEST_CHANGES",
  "body": "<what the change does, what you checked, and the verdict>",
  "comments": [
    {"path": "src/lib.rs", "line": 42, "side": "RIGHT", "body": "<the finding>"}
  ]
}
EOF
gh api --method POST repos/<owner>/<repo>/pulls/<number>/reviews --input review.json
```

If the API rejects a comment's position, drop the `comments` and put the
findings in the body as `path:line` citations instead:

```sh
gh pr review <number> --request-changes --body-file <body-file>   # or --approve, or --comment
```

Never push to, rebase, close, or merge a pull request that isn't yours; a
review is comments and a verdict only. Once it is submitted, mark the review
request's notification read as described below.

## Clearing notifications you have read

Your unread notifications are what marks this repository as having feedback
waiting on it, so a thread you have already dealt with has to be marked read.
Left unread it goes on claiming there is something here to answer, and the
next look at this repository is spent rediscovering that there isn't.

```sh
gh api repos/<owner>/<repo>/notifications         # unread threads on one repository
gh api notifications                              # unread threads across all of them
gh api --method PATCH notifications/threads/<id>  # mark one read, by the id the listing shows
```

Mark a thread read once you have acted on it, replied to it, or read it and
found nothing owing — including when that is the whole of what you found and
you are about to skip the turn. Leave unread only what you still owe an
answer.

## Issues

Create issues for work you are not going to do yourself: something too large for
one pull request, something that needs a decision from the user, or a problem
you found while doing something else.

```sh
gh issue list --repo <owner>/<repo>                # open issues; `--state all` for the rest
gh issue view <number> --comments --repo <owner>/<repo> # the body and its conversation
```

Check the list first — if the issue is already filed, comment on it rather than
filing it again.

```sh
gh issue create --repo <owner>/<repo> --title "<title>" --body-file <body-file>
gh issue comment <number> --body "<what you found>" --repo <owner>/<repo>
```

Write the body to a file and pass it so the report keeps its formatting. Say
what is wrong, where, and what you think should happen — the user is deciding
from what you wrote, so leave out nothing they would need.
