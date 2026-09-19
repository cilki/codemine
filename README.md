# codemine

Continuously runs [opencode](https://opencode.ai) turns that sweep our
repositories on Gitea, GitHub, and GitLab. Each turn the runner picks a task
and a repository at random, logs the pair, and hands them to the agent, which
delivers its work as a PR or issue. Configuration lives in the always-on web
UI, persisted in the workspace. Each turn runs in a fresh session,
back-to-back with the previous one, against a persistent workspace: the runner
keeps one clone per repository under the workspace root, brings it back to a
current default branch before the turn (hard reset; ignored files like build
caches survive), and maintains a [codegraph](https://github.com/colbymchenry/codegraph)
index in it. The agent explores through codegraph's `codegraph_explore` MCP
tool instead of recloning and rereading the tree every turn, which cuts token
usage substantially.

The Docker image bundles the runner, the sweep commands, and everything the
agent needs at runtime (`tea`, `gh`, `glab`, `codegraph`, the Rust toolchain,
...). The dependency list lives in `nix/runtime.nix` and is shared between
`shell.nix` and the image, both pinned by `nix/nixpkgs.nix`; codegraph is
included when the pin has it and pulled with the upstream installer by the
image otherwise. When `codegraph` is not on the PATH at all, the runner skips
indexing and drops the MCP entry from opencode's config, and the agent falls
back to exploring the tree normally.

## Configuration

The command line only chooses where things live; everything else is
configured through the web UI:

| Flag              | Description                                       |
| ----------------- | ------------------------------------------------- |
| `--listen ADDR`   | Bind address for the web UI (default `0.0.0.0:8080`) |
| `--workspace DIR` | Persistent workspace root (default `~/.codemine`) |
| `--once`          | Run a single turn and exit                        |

Settings are edited on the web UI's settings panel (collapsed by default) and
persisted to `<workspace>/config.json` (mode 0600, since it holds the forge
tokens). Until the settings are complete — at least one forge enabled with a
token, plus a model and git author — the runner idles in an `unconfigured`
state and the UI outlines each field that needs filling in, with the reason as
its tooltip. Changes apply at the next turn boundary.

Per forge (Gitea, GitHub, GitLab) the UI configures: enabled, token, base URL
(GitHub and GitLab default to their public instances; Gitea has no default),
and — Gitea only — the bot account's username. Each forge card can also load
the live repository list and enable/disable individual repositories: repos
are enabled by default, so new repositories join the pool automatically, and
only the disabled set is stored. Tokens are write-only: once saved they are
never shown again and can only be overwritten.

The general settings cover the model (chosen from the Claude models the
bundled OAuth login can reach), the task pool (a checkbox per task in the
sweep command, showing that task's instructions on hover), the daily completed-task limit (blank = unlimited), the git
author name/email, the turn timeout in minutes, and the resource limits (CPU
priority high/normal/low and I/O class `best-effort`/`idle`). Edits save
themselves and apply from the next turn; there is no save button.

The runner injects `GITHUB_TOKEN`/`GITLAB_TOKEN` (and `GH_HOST`/`GITLAB_HOST`
for self-hosted instances) into `gh`, `glab`, and the agent session itself,
and maintains the `tea` login for Gitea, so the forge CLIs work without any
environment setup.

The binary embeds the `sweep` command and one skill per forge (how to open
PRs, respond to feedback, and file issues via the `tea`, `gh`, and `glab`
CLIs) and installs them into opencode's config directory at startup, along
with the codegraph MCP server entry in `opencode.json`, so it runs the same
inside or outside the image.

Mount a volume at the workspace root (`/root/.codemine` unless you point
`--workspace` elsewhere) to keep the settings (`config.json`), clones, and
codegraph indexes across container restarts. Without it each repository is
recloned and reindexed after every restart — and the configuration is lost,
so a persistent workspace is strongly recommended.

Each turn the runner draws one task from the configured task pool and one
repository from everything the bot's account can reach across the configured
forges (minus the repositories disabled in the UI), and passes both — plus
the repository's forge — into the sweep prompt. The available tasks:

- `feedback` — respond to PR review comments, fix failing CI, address assigned
  issues
- `rebase` — rebase open PR branches that are behind their base and force-push
- `bump-deps` — bump dependencies to their latest releases
- `simplify` — remove dead code, simplify implementations, drop trivial tests
- `todo` — handle a TODO comment or AGENTS.md TODO item
- `roleplay` — run the software as a user would and fix what breaks

Each turn ends as either completed or skipped: the agent closes its final
message with a `TASK COMPLETED` or `TASK SKIPPED` marker line, which the
runner reads from the log. The daily limit caps how many completed turns run
per local day — skipped turns don't count, and turns with no marker do. When
the limit is reached the runner sleeps until the date changes. The count
lives in memory, so restarting the container resets it.

A writable mount of Claude Code's OAuth credentials is expected at
`~/.claude/.credentials.json` — `/root/.claude/.credentials.json` in the image,
since the container runs as root; the opencode-claude-auth plugin refreshes the
tokens in place.

On low-resource machines, the nice and I/O class settings throttle the agent
by spawning it through `nice`/`ionice`; the whole process tree (cargo, rustc,
test runs, ...) inherits the reduced priorities. Unset means full priority.
I/O priorities only take effect on schedulers that honor them (e.g. bfq); CPU
niceness works everywhere.

## Web UI

The web UI is always on (bind address via `--listen`) and serves both the
status page and the settings panel. The status page shows what the runner is
currently doing (with a live log tail while a turn runs), today's completed
count, cumulative totals, and the recent turns with their durations and token
usage. It does not poll: the server pushes over SSE (`/api/events`) as soon as
the state changes, and the log tail as it grows. `/api/status` and `/api/log`
remain as one-shot endpoints for scripting.

The UI has no authentication and configures tokens that can push to your
repositories — bind it to a trusted network (or localhost behind a reverse
proxy), not the open internet.

The HTTP API: `GET /api/status` returns the status as JSON and `GET /api/log`
the current log tail as plain text. `GET /api/settings` returns the settings
with each token redacted to a `token_set` flag; `PUT /api/settings` replaces
them, where an empty or absent token keeps the stored one. `GET
/api/repos/{forge}` lists the forge's reachable repositories merged with the
disabled set.

Token counts are collected best-effort from opencode's session storage after
each turn and shown as unknown when they can't be read.

## Deployment context

Mount deployment-specific instructions at `/root/.config/opencode/AGENTS.md`;
opencode loads them into every session automatically. This is the place for
anything unique to your projects, such as a description of the branching scheme.

