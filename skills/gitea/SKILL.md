---
name: gitea
description:
  How to list, clone, open pull requests, respond to review feedback, and file
  issues on our Gitea repositories via the tea CLI.
---

All of our repositories live on Gitea. You deliver work as pull requests — never
by pushing to a base branch directly.

Use the `tea` CLI for everything on Gitea; it is already logged in. Inside a
clone it works out the repository from the git remote, so the `--repo` flag is
only needed when you are outside one.

See the repositories your account can reach, listed as `<owner>/<repo>`:

```sh
tea repos ls --fields name,updated,description
```

Clone over HTTPS; credentials and your commit identity are already configured,
and the Gitea base URL is in the `GITEA_URL` environment variable:

```sh
git clone "$GITEA_URL/<owner>/<repo>"
```

## Opening a pull request

1. First check that you are not repeating work that has already been done or
   already been turned down:

   ```sh
   tea pr ls --repo <owner>/<repo> --state all # every pull request, not just open ones
   ```

   Each is marked `open`, `merged`, or `closed`. A `merged` one already landed
   on the default branch. A `closed` one was turned down — read it with
   `tea pr <number> --comments` to find out why before you go near the same
   ground again. Never reopen a closed PR and never make duplicate PRs.

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
tea pr ls --repo <owner>/<repo> --fields index,state,head,base,title # open pull requests, with their branches
tea pr <number> --comments --repo <owner>/<repo>                     # the conversation under a pull request
```

When a comment points at a file or line, read that spot in the diff
(`git diff <base>...<head>`) to see what the reviewer is pointing at. To act on
feedback:

1. Check out the pull request's branch (`tea pr ls` shows it) and pull first —
   the branch may have moved since you last saw it.
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
