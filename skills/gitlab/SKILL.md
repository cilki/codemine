---
name: gitlab
description:
  How to list, clone, open merge requests, respond to review feedback, and
  file issues on our GitLab projects via the glab CLI.
---

These repositories live on GitLab, where pull requests are called merge
requests. You deliver work as merge requests — never by pushing to a base
branch directly.

Use the `glab` CLI for everything on GitLab; it authenticates from the
`GITLAB_TOKEN` environment variable that is already set. Inside a clone it
works out the project from the git remote, so the `--repo` flag is only needed
when you are outside one.

See the projects your account can reach, listed as `<namespace>/<project>`:

```sh
glab repo list --member --per-page 100
```

Clone over HTTPS; credentials and your commit identity are already configured:

```sh
glab repo clone <namespace>/<project>
```

## Opening a merge request

1. First check that you are not repeating work that has already been done or
   already been turned down:

   ```sh
   glab mr list --repo <namespace>/<project> --all # every merge request, not just open ones
   ```

   Each is marked open, merged, or closed. A merged one already landed on the
   default branch. A closed one was turned down — read it with
   `glab mr view <number> --comments` to find out why before you go near the
   same ground again. Never reopen a closed MR and never make duplicate MRs.

2. Branch before you touch anything: `git checkout -b <short-topic>`.
3. Commit with a message that explains why the change is being made, not just
   what changed.
4. Push: `git push -u origin <short-topic>`.
5. Open the merge request; the target defaults to the default branch:

   ```sh
   glab mr create --source-branch <short-topic> --title "<title>" --description "$(cat <body-file>)" --yes
   ```
6. Include how long the session took and how many tokens were consumed in the MR description

If a push is rejected, stop and report it rather than working around it.

## Responding to review feedback

To see your open merge requests and what reviewers have said about them:

```sh
glab mr list --repo <namespace>/<project>              # open merge requests, with their branches
glab mr view <number> --comments --repo <namespace>/<project> # the conversation under a merge request
```

When a comment points at a file or line, read that spot in the diff
(`git diff <base>...<head>`) to see what the reviewer is pointing at. To act on
feedback:

1. Check out the merge request's branch (`glab mr checkout <number>`) and pull
   first — the branch may have moved since you last saw it.
2. Make the changes as **new commits**. Do not rewrite history on a branch that
   is under review; force-pushing invalidates the comments the reviewer left.
3. Push. The merge request updates itself — you do not open a new one.
4. Reply so the reviewer knows what happened:

   ```sh
   glab mr note <number> --message "<what you changed and why>"
   ```

If a comment is ambiguous, or asks for something you think is wrong, say so in
the reply instead of guessing. Address every comment: either make the change or
explain why you didn't.

## Issues

Create issues for work you are not going to do yourself: something too large
for one merge request, something that needs a decision from the user, or a
problem you found while doing something else.

```sh
glab issue list --repo <namespace>/<project>                # open issues; `--all` for the rest
glab issue view <number> --comments --repo <namespace>/<project> # the body and its conversation
```

Check the list first — if the issue is already filed, comment on it rather than
filing it again.

```sh
glab issue create --repo <namespace>/<project> --title "<title>" --description "$(cat <body-file>)" --yes
glab issue note <number> --message "<what you found>" --repo <namespace>/<project>
```

Write the body to a file and pass it so the report keeps its formatting. Say
what is wrong, where, and what you think should happen — the user is deciding
from what you wrote, so leave out nothing they would need.
