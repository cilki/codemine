---
name: gitlab
description:
  How to list, open merge requests, respond to review feedback, review merge
  requests, and file issues on our GitLab projects via the glab CLI.
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

## Opening a merge request

1. First check that you are not repeating work that has already been done or
   already been turned down:

   ```sh
   glab mr list --repo <namespace>/<project> --all --per-page 100 # every merge request, not just open ones
   ```

   `--all` widens the *state* filter, not the page: it is "any state", not "all
   of them". `glab` still lists 30 per page, newest first, so on a project with
   any history the listing looks complete while hiding exactly the older merge
   requests you are checking for — open ones included. `--per-page` is what
   lifts that; check that the count you get back is the whole project.

   Each is marked open, merged, or closed. A merged one already landed on the
   default branch. A closed one was either turned down or closed by you as
   obsolete — read it with `glab mr view <number> --comments` to find out
   which before you go near the same ground again, rather than assuming it was
   rejected. Never reopen a closed MR and never make duplicate MRs.

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
glab mr list --repo <namespace>/<project> --per-page 100 # open merge requests, with their branches
glab mr view <number> --comments --repo <namespace>/<project> # the conversation under a merge request
glab api "projects/:id/merge_requests?state=opened&author_username=<you>&per_page=100" | jq '.[] | {iid, source_branch, has_conflicts}' # which of your merge requests conflict with their base
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
   glab mr note create <number> --message "<what you changed and why>"
   ```

If a comment is ambiguous, or asks for something you think is wrong, say so in
the reply instead of guessing. Address every comment: either make the change or
explain why you didn't.

## Closing a merge request of your own

A merge request whose change no longer applies — the target branch already
carries an equivalent change, the code it touched is gone, or the problem it
fixed no longer exists — is worth less than the reviewer's attention it is
holding. Close it. Say why first, so the next look at it reads as "this went
stale" and not "this was turned down":

```sh
glab mr note create <number> --message "<why the change no longer applies>"
glab mr close <number>
git push origin --delete <source-branch>   # glab's close leaves the branch behind
```

Only your own merge requests, and only on that test: another author's is never
yours to close, and a change that still applies is never obsolete however
awkward it has become to land.

## Reviewing a merge request

You review a merge request only when someone asked you to: your account is
one of its reviewers and your review state on it is `unreviewed`. That is the
whole test — who opened the merge request, you included, doesn't come into
it. Submitting the review moves the state on, so a merge request you already
answered stays out of the list until someone runs `/request_review @<you>` on
it again.

`glab api` has no `--jq` flag (that is `gh`); pipe its output through `jq`
instead:

```sh
glab api "projects/:id/merge_requests?state=opened&reviewer_username=<you>" | jq '.[] | {iid, title, author: .author.username, source_branch, target_branch}'
glab api "projects/:id/merge_requests/<iid>/reviewers" | jq '.[] | select(.user.username == "<you>") | .state' # unreviewed means a review is owed
glab mr view <iid> --comments     # the description and the conversation so far
glab mr diff <iid>                # the whole diff against the target branch
glab mr checkout <iid>            # to build it or run its tests
```

A review on GitLab is a set of **draft notes** that you publish all at once
with the verdict, so the author is notified once and every finding lands as
its own thread on the line it is about. A plain `glab mr note create` holding
your findings is not a review: it is one unanchored comment, nobody can
resolve the findings one by one, and it does not record a verdict. Nor is
`glab mr note create --file … --line …`: each of those is published, and
notifies the author, the moment you post it.

1. Clear out anything left over. Drafts are only visible to you and are all
   published together, so a draft from an earlier attempt would go out with
   this review. List yours and delete any you didn't just write:

   ```sh
   glab api "projects/:id/merge_requests/<iid>/draft_notes" | jq '.[] | {id, note, path: .position.new_path, line: (.position.new_line // .position.old_line)}'
   glab api --method DELETE "projects/:id/merge_requests/<iid>/draft_notes/<draft-id>"
   ```

2. Save the merge request's three diff SHAs; every anchor needs them:

   ```sh
   glab api "projects/:id/merge_requests/<iid>" | jq .diff_refs > refs.json   # base_sha, start_sha, head_sha
   ```

3. Draft one note per finding, anchored to its line. The anchor is a nested
   `position` object, so build the body as JSON with `jq` and send it with
   `--input` and an explicit JSON content type. `-f "position[new_line]=42"`
   does **not** work: `glab` sends that name as a literal key, which GitLab
   ignores, and you get an unanchored comment instead of a line comment.
   Write each finding to a file first so its formatting survives:

   ```sh
   jq -n --slurpfile refs refs.json --rawfile note finding.md \
     '{note: $note, position: ($refs[0] + {position_type: "text", old_path: "src/lib.rs", new_path: "src/lib.rs", new_line: 42})}' > draft.json
   glab api --method POST "projects/:id/merge_requests/<iid>/draft_notes" \
     --header "Content-Type: application/json" --input draft.json
   ```

   Which line numbers to give depends on the line in the diff
   (`git diff <target>...<source>` shows them in each hunk's
   `@@ -old,count +new,count @@` header):

   - a line the diff **adds** (`+`): `new_line` only
   - a line the diff **removes** (`-`): `old_line` only
   - an unchanged context line shown in the hunk: **both** `old_line` and
     `new_line`, which differ wherever lines were added or removed above it

   `new_path` is the file as it is after the change; `old_path` is the same
   unless the merge request renames it. Only lines inside a hunk can be
   anchored. Check each response: it echoes the `position` back. If GitLab
   rejects the anchor, or the `position` comes back without your path and
   line, delete that draft and draft the finding again without `position`,
   citing `path:line` at the top of the note. Never fold the findings into
   one comment because one anchor failed.

4. Draft the summary — what the change does, what you checked, and the
   verdict — as one more note, without a `position`:

   ```sh
   jq -n --rawfile note summary.md '{note: $note}' > draft.json
   glab api --method POST "projects/:id/merge_requests/<iid>/draft_notes" \
     --header "Content-Type: application/json" --input draft.json
   ```

5. List your drafts again (the command in step 1) and check that every
   finding is there with the path and line you meant.

6. Submit the review. The verdict is a quick action, and a quick action only
   takes effect when it is the whole note. It publishes every draft at once,
   records the verdict, moves your review state, and notifies the author:

   ```sh
   glab mr note create <iid> --message "/submit_review requested_changes"   # or: /submit_review approve
   ```

   Afterwards, the draft list should be empty and your reviewer state should
   no longer be `unreviewed`. If the drafts are still there, the review was not
   submitted. Don't post the findings again any other way.

Never push to, rebase, close, or merge the merge request you are reviewing,
your own included; a review is comments and a verdict only. Once it is
submitted, mark the review request's todo done as described below.

## Clearing todos you have handled

GitLab tracks what is waiting on you as todos — they fire when someone
mentions you or assigns you something — and a pending todo is what marks this
project as having feedback waiting on it. One you have already dealt with has
to be marked done. Left pending it goes on claiming there is something here to
answer, and the next look at this project is spent rediscovering that there
isn't.

```sh
glab api todos                                  # your pending todos, with the project each belongs to
glab api --method POST todos/<id>/mark_as_done  # mark one done, by the id the listing shows
```

Mark a todo done once you have acted on it, replied to it, or read it and
found nothing owing — including when that is the whole of what you found and
you are about to skip the turn. Leave pending only what you still owe an
answer.

## Issues

Create issues for work you are not going to do yourself: something too large
for one merge request, something that needs a decision from the user, or a
problem you found while doing something else.

```sh
glab issue list --repo <namespace>/<project> --per-page 100 # open issues; `--all` for the rest
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
