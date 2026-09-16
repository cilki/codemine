---
name: github
description:
  How to list, clone, open pull requests, respond to review feedback, and file
  issues on our GitHub repositories via the gh CLI.
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

Clone over HTTPS; credentials and your commit identity are already configured:

```sh
gh repo clone <owner>/<repo>
```

## Opening a pull request

1. First check that you are not repeating work that has already been done or
   already been turned down:

   ```sh
   gh pr list --repo <owner>/<repo> --state all # every pull request, not just open ones
   ```

   Each is marked `OPEN`, `MERGED`, or `CLOSED`. A merged one already landed
   on the default branch. A closed one was turned down — read it with
   `gh pr view <number> --comments` to find out why before you go near the
   same ground again. Never reopen a closed PR and never make duplicate PRs.

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
gh pr list --repo <owner>/<repo> --json number,state,headRefName,baseRefName,title # open pull requests, with their branches
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
