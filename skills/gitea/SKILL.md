---
name: gitea
description:
  How to list, open pull requests, respond to review feedback, review other
  people's pull requests, and file issues on our Gitea repositories via the
  tea CLI.
---

These repositories live on Gitea. You deliver work as pull requests — never
by pushing to a base branch directly.

Use the `tea` CLI for everything on Gitea; it is already logged in. Inside a
clone it works out the repository from the git remote, so the `--repo` flag is
only needed when you are outside one.

See the repositories your account can reach. Ask for `owner` as well as `name`:
every `--repo` below wants an `<owner>/<repo>` slug, and `tea` prints the two as
separate columns for you to join.

```sh
tea repos ls --fields owner,name,updated,description
```

## Opening a pull request

1. First check that you are not repeating work that has already been done or
   already been turned down:

   ```sh
   tea pr ls --repo <owner>/<repo> --state all # every pull request, not just open ones
   ```

   Each is marked `open`, `merged`, or `closed`. A `merged` one already landed
   on the default branch. A `closed` one was either turned down or closed by
   you as obsolete — read it with `tea pr <number> --comments` to find out
   which before you go near the same ground again, rather than assuming it was
   rejected. Never reopen a closed PR and never make duplicate PRs.

2. Branch before you touch anything: `git checkout -b <short-topic>`.
3. Commit with a message that explains why the change is being made, not just
   what changed.
4. Push: `git push -u origin <short-topic>`.
5. Open the pull request; the base defaults to the default branch:

   ```sh
   tea pr create --head <short-topic> --title "<title>" --description "$(cat <body-file>)"
   ```
6. Include how long the session took and how many tokens were consumed in the PR description

If a push is rejected, stop and report it rather than working around it.

## Responding to review feedback

To see your open pull requests and what reviewers have said about them:

```sh
tea pr ls --repo <owner>/<repo> --fields index,state,head,base,title,author,mergeable # open pull requests, with their branches
tea pr <number> --comments --repo <owner>/<repo>                     # the conversation under a pull request
```

When a comment points at a file or line, read that spot in the diff
(`git diff <base>...<head>`) to see what the reviewer is pointing at. To act on
feedback:

1. Check out the pull request's branch (`tea pr ls` shows it under `head`) and
   pull first — the branch may have moved since you last saw it. Not
   `tea pr checkout`: it leaves a detached HEAD, or with `-b` a local
   `pulls/<number>` branch, and pushing from either does not update the pull
   request.
2. Make the changes as **new commits**. Do not rewrite history on a branch that
   is under review; force-pushing invalidates the comments the reviewer left.
3. Push. The pull request updates itself — you do not open a new one.
4. Reply so the reviewer knows what happened:

   ```sh
   tea comment <number> "<what you changed and why>"
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
tea comment <number> "<why the change no longer applies>"
tea pr close <number>
git push origin --delete <head>   # tea's close leaves the branch behind
```

Only your own pull requests, and only on that test: another author's is never
yours to close, and a change that still applies is never obsolete however
awkward it has become to land.

## Reviewing someone else's pull request

You review a pull request only when its author asked you to: your account is
listed under its requested reviewers. Submitting the review clears the
request, so a pull request you already answered stays out of the list until
someone requests a review from you again. The listing doesn't show
reviewers; the detail view does, as a `- REQUEST_REVIEW by @<you>` line
while a review is owed (`APPROVED` or `REQUEST_CHANGES` once you've
answered):

```sh
tea pr ls --repo <owner>/<repo> --fields index,title,author,head,base  # open pull requests; skip your own
tea pr <number> --comments --repo <owner>/<repo>   # the description, who is asked to review, and the conversation so far
git fetch origin <base> <head> && git diff origin/<base>...origin/<head>   # the whole diff against the base
git checkout origin/<head>                         # to build it or run its tests
```

`tea pr review` is interactive and can't be scripted, so inline comments
aren't available here: cite each finding in the summary as `path:line`, then
submit the summary with the verdict in one command. The second argument is
the review body:

```sh
tea pr reject <number> "<what the change does, what you checked, each finding as path:line, and why it blocks>"
tea pr approve <number> "<what the change does, what you checked, and why it is fine to merge>"
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
tea notifications ls           # unread threads on this repository; add --mine for every repository
tea notifications read <id>    # mark one read, by the ID the listing shows
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
tea issues ls --repo <owner>/<repo>              # open issues; `--state all` for the rest
tea issues <number> --comments --repo <owner>/<repo> # the body and its conversation
```

Check the list first — if the issue is already filed, comment on it rather than
filing it again.

```sh
tea issue create --repo <owner>/<repo> --title "<title>" --description "$(cat <body-file>)"
tea comment <number> "<what you found>" --repo <owner>/<repo>
```

Write the body to a file and pass it so the report keeps its formatting. Say
what is wrong, where, and what you think should happen — the user is deciding
from what you wrote, so leave out nothing they would need.
