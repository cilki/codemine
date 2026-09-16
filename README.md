# codemine

Continuously runs [opencode](https://opencode.ai) turns that sweep our
repositories on Gitea, GitHub, and GitLab. Each turn the runner picks a task
and a repository at random, logs the pair, and hands them to the agent, which
delivers its work as a PR or issue. Each turn runs in a fresh workspace and a fresh session, back-to-back
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
| `GITHUB_TOKEN`         | GitHub personal access token                            |
| `GITHUB_URL`           | GitHub base URL (default `https://github.com`)          |
| `GITLAB_TOKEN`         | GitLab personal access token                            |
| `GITLAB_URL`           | GitLab base URL (default `https://gitlab.com`)          |
| `CODEMINE_MODEL`       | Model to run turns with, as provider/model              |
| `CODEMINE_COMMAND`     | The opencode command to run each turn (default `sweep`) |
| `CODEMINE_TASKS`       | Space-separated task pool (default: all tasks)          |
| `CODEMINE_DAILY_LIMIT` | Max completed tasks per day (default: unlimited)        |
| `CODEMINE_TIMEOUT`     | Seconds before a turn is cut off (default 21600)        |
| `CODEMINE_NICE`        | CPU niceness for the agent, 1-19 (default: none)        |
| `CODEMINE_IONICE`      | I/O class: `best-effort` or `idle` (default: none)      |
| `CODEMINE_WEBUI`       | Bind address for the status web UI (default: disabled)  |
| `GIT_AUTHOR_NAME`      | The name commits are authored (and committed) as        |
| `GIT_AUTHOR_EMAIL`     | The email commits are authored (and committed) as       |

Each forge is enabled by setting its token variable, and at least one must be
configured. Gitea also needs `GITEA_USER` and `GITEA_URL`. `gh` and `glab`
authenticate from `GITHUB_TOKEN` and `GITLAB_TOKEN` directly; for self-hosted
instances, set the CLIs' own host variables (`GH_HOST`, `GITLAB_HOST`)
alongside the base URL.

The binary embeds the `sweep` command and one skill per forge (how to clone,
open PRs, respond to feedback, and file issues via the `tea`, `gh`, and
`glab` CLIs) and installs them into opencode's config directory at startup,
so it runs the same inside or outside the image.

Each turn the runner draws one task from `CODEMINE_TASKS` and one repository
from everything the bot's account can reach across the configured forges, and
passes both — plus the repository's forge — into the sweep prompt. The
available tasks:

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

On low-resource machines, `CODEMINE_NICE` and `CODEMINE_IONICE` throttle the
agent by spawning it through `nice`/`ionice`; the whole process tree (cargo,
rustc, test runs, ...) inherits the reduced priorities. Unset means full
priority. I/O priorities only take effect on schedulers that honor them (e.g.
bfq); CPU niceness works everywhere.

Pass `--once` to run a single turn and exit.

## Web UI

Set `CODEMINE_WEBUI` to a bind address (e.g. `0.0.0.0:8080`) to serve a
read-only status page: what the runner is currently doing (with a live log
tail while a turn runs), today's completed count, cumulative totals, and the
recent turns with their durations and token usage. The page polls the server
every couple of seconds. `GET /api/status` returns the same data as JSON and
`GET /api/log` the current log tail as plain text.

Token counts are collected best-effort from opencode's session storage after
each turn and shown as unknown when they can't be read.

## Deployment context

Mount deployment-specific instructions at `/root/.config/opencode/AGENTS.md`;
opencode loads them into every session automatically. This is the place for
anything unique to your projects, such as a description of the branching scheme.

