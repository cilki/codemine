# codemine

Continuously runs [opencode](https://opencode.ai) turns that sweep our Gitea
repositories. Each turn the runner picks a task and a repository at random,
logs the pair, and hands them to the agent, which delivers its work as a PR or
issue. Each turn runs in a fresh workspace and a fresh session, back-to-back
with the previous one.

The Docker image bundles the runner, the sweep commands, and everything the
agent needs at runtime (`tea`, `gh`, `glab`, the Rust toolchain, ...). The
dependency list lives in `nix/runtime.nix` and is shared between `shell.nix` and
the image, both pinned by `nix/nixpkgs.nix`.

## Configuration

| Variable               | Description                                             |
| ---------------------- | ------------------------------------------------------- |
| `GITEA_TOKEN`          | Gitea personal access token, scoped to write:repository |
| `GITEA_USER`           | The bot account's Gitea username                        |
| `GITEA_URL`            | Gitea base URL                                          |
| `CODEMINE_MODEL`       | Model to run turns with, as provider/model              |
| `CODEMINE_COMMAND`     | The opencode command to run each turn (default `sweep`) |
| `CODEMINE_TASKS`       | Space-separated task pool (default: all tasks)          |
| `CODEMINE_DAILY_LIMIT` | Max completed tasks per day (default: unlimited)        |
| `CODEMINE_TIMEOUT`     | Seconds before a turn is cut off (default 21600)        |
| `GIT_AUTHOR_NAME`      | The name commits are authored (and committed) as        |
| `GIT_AUTHOR_EMAIL`     | The email commits are authored (and committed) as       |

The image bakes the `sweep` command into `/root/.config/opencode/commands` and
the `gitea` skill (how to clone, open PRs, respond to feedback, and file
issues via the `tea` CLI) into `/root/.config/opencode/skills`.

Each turn the runner draws one task from `CODEMINE_TASKS` and one repository
from everything the bot's account can reach, and passes both into the sweep
prompt. The available tasks:

- `feedback` — respond to PR review comments, fix failing CI, address assigned
  issues
- `rebase` — rebase open PR branches that are behind their base and force-push
- `bump-deps` — bump dependencies to their latest releases
- `simplify` — remove dead code, simplify implementations, drop trivial tests
- `todo` — handle a TODO comment or AGENTS.md TODO item
- `roleplay` — run the software as a user would and fix what breaks

Each turn ends as either completed or skipped: the agent closes its final
message with a `TASK COMPLETED` or `TASK SKIPPED` marker line, which the
runner reads from the log. `CODEMINE_DAILY_LIMIT` caps how many completed
turns run per local day — skipped turns don't count, and turns with no marker
do. When the limit is reached the runner sleeps until the date changes. The
count lives in memory, so restarting the container resets it.

A writable mount of Claude Code's OAuth credentials is expected at
`/root/.claude/.credentials.json`; the opencode-claude-auth plugin refreshes the
tokens in place.

Pass `--once` to run a single turn and exit.

## Deployment context

Mount deployment-specific instructions at `/root/.config/opencode/AGENTS.md`;
opencode loads them into every session automatically. This is the place for
anything unique to your projects, such as a description of the branching scheme.

