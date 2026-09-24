<p align="center">
	<img src="https://raw.githubusercontent.com/cilki/cilki/master/emblems/codemine.svg" style="width:90%; height:auto;"/>
</p>

![License](https://img.shields.io/github/license/cilki/codemine)
![Stars](https://img.shields.io/github/stars/cilki/codemine?style=social)

<hr>

**codemine** runs AI agents on git repos while you sleep, delivering tested PRs
and useful issues.

Each "turn" does one of the following on a repo:

- `feedback`: responds to reviewer feedback on issues/PRs, fix failing CI,
  address assigned issues.
- `rebase`: rebase branches that are behind the default branch.
- `bump`: update dependencies, handling any migration issues.
- `simplify`: remove dead code, simplify implementations, drop trivial tests.
- `todo`: implement TODOs in the repo.
- `roleplay`: run the project like a user would, fixing what breaks along the
  way.
- `benchmark`: run the project like a user would, searching for performance
  improvements.
- `audit`: evaluate the security of the project.
- `docs`: fix outdated docs.
- `coverage`: improve test coverage.
- `mutation`: mutate load-bearing code to find gaps the test suite misses.
- `feature`: implement new features.

> I run **codemine** on a cluster of physically isolated Raspberry PIs.

### Features

#### Codegraph

With [codegraph](https://github.com/colbymchenry/codegraph) installed, the agent
avoids rereading the tree every turn, which cuts token usage substantially on
large projects.

#### Prioritization

On low-resource machines, the nice and I/O class settings throttle the agent by
spawning it through `nice`/`ionice`; the whole process tree (cargo, rustc, test
runs, ...) inherits the reduced priorities. I/O priorities only take effect on
schedulers that honor them (e.g. bfq); CPU niceness works everywhere.

#### Scheduling

Off by default. When enabled in the settings, turns only *start* inside a
daily window — 22:00 to 06:00 keeps the agent to the small hours, and a window
that runs past midnight is one window rather than two. A turn already under
way is left to finish, so pair a narrow window with a turn timeout that fits
inside it. Times are read in the container's timezone, so set `TZ` if it isn't
already yours.

#### Multiple forge support

**codemine** works with Gitea, GitHub, and GitLab. Just add an access token and
select what repos **codemine** is enabled on.

#### `AGENTS.md` customizations

Mount deployment-specific instructions at `/root/.config/opencode/AGENTS.md`;
opencode loads them into every session automatically. This is the place for
anything unique to your projects, such as a description of the branching scheme.

#### Web interface

You can manage the **codemine** instance via a simple web UI on port 8080. This
is where you configure your forge settings, rate limits, select a model, choose
what task types are run, view agent logs, etc. Updates to the settings take
effect on the next turn.
