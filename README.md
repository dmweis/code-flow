# code-flow

[![CI](https://github.com/dmweis/code-flow/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/dmweis/code-flow/actions/workflows/ci.yml?query=branch%3Amain)

A terminal UI that lists your open pull requests in the current GitHub
repository, with their review, CI, and merge status at a glance.

## Install

Requires the [GitHub CLI](https://cli.github.com/), logged in with `gh auth login`.

```sh
cargo install --path .
```

This installs both `code-flow` and `cl`. Run either command from inside a clone
of a GitHub repository; both launch the same application. For local development,
plain `cargo run` launches `code-flow`, and `cargo run --bin cl` launches `cl`.

If you have [just](https://just.systems), the [justfile](justfile) wraps the
steps below: `just install` installs both commands, `just install-tools` installs
worktrunk and Herdr for your OS, `just install-skills` installs their agent
skills, and `just install-all` does all three. Run `just` to list every recipe.

### Optional: worktrunk and Herdr

The `w`, `W` and `n` keys use [worktrunk](https://worktrunk.dev) (`wt`) to
manage worktrees, and [Herdr](https://herdr.dev) to open a workspace for each
one. Both are optional; everything else works without them.

#### macOS

Both are in Homebrew:

```sh
brew install worktrunk herdr
wt config shell install
```

#### Ubuntu

Install worktrunk with Cargo, which you already have from installing code-flow:

```sh
cargo install worktrunk
wt config shell install
```

Install Herdr with its install script, which puts `herdr` on your `PATH`:

```sh
curl -fsSL https://herdr.dev/install.sh | sh
```

If you use [Homebrew on Linux](https://docs.brew.sh/Homebrew-on-Linux), the
macOS instructions work too.

`wt config shell install` adds shell integration so `wt switch` can change your
directory; restart your shell afterwards. To get Herdr workspaces, run
`code-flow` from inside Herdr. Keep them up to date with `brew upgrade` on
macOS, and with `cargo install worktrunk` and `herdr update` on Ubuntu.

#### Agent skills

The Herdr skill and the worktrunk plugin teach coding agents to use these
tools. The commands are the same on macOS and Ubuntu. The Herdr skill needs
Node.js for `npx`; the worktrunk plugin needs the [Claude Code](https://claude.com/claude-code) CLI:

```sh
npx skills add herdrdev/herdr --skill herdr -g
wt config plugins claude install
```

`npx skills` asks which agents to install the Herdr skill for. For the
worktrunk plugin in Codex, run `wt config plugins codex install` instead.

## Statuses

Each PR takes two lines: its number, title and labels, then short status
words. Only states worth acting on are shown:

| Status | Words |
| --- | --- |
| Draft | `◌ draft` (replaces "needs review" until someone reviews) |
| Review | `◷ needs review`, `✓ approved`, `✗ changes requested` |
| CI | `⟳ CI 3/7` (running, done/total), `✓ CI passed`, `✗ CI 2 failed`, `· no CI` |
| Merge | `⚠ conflicts`, `↓ behind base` |
| Auto-merge | `» auto-merge`, `≡ queued #2` |
| Threads | `2 threads` unresolved review threads |
| Worktree | `⌂ worktree` the PR has its own worktree |

The detail pane on the right spells out every status in full and shows the PR description.

For repositories that define a `claude-review` label, each PR's status line also shows
`✓ claude-review` when labeled or `· no claude-review` otherwise. Press `l`
to add the label to the selected PR. While the request runs, the tag shows
`⟳ claude-review`; failures appear in the footer and can be retried.
The tag and shortcut are hidden and disabled in repositories without this
label. Label availability is checked on every refresh.

## New branches and local worktrees

Press `n` to start a new change. Type a name; it becomes a branch named
after your user name, e.g. `Fix "flaky" CI` becomes `dweis/fix-flaky-ci`.
code-flow fetches the default branch from `origin`, creates the branch from
it in a new worktree, and opens a Herdr workspace there with a shell. It
fails if the branch already exists.

Every linked worktree is listed under **Local worktrees** below your PRs:
new branches, your PRs' worktrees, and worktrees left over from merged PRs.
If the branch is one of your open PRs, the row shows that PR's number and
title. With worktrunk installed, each also shows:

| Words | Meaning |
| --- | --- |
| `● uncommitted` | uncommitted changes in the worktree |
| `◌ empty` | no commits of its own yet |
| `↑ 3 commits` | commits the default branch doesn't have |
| `✓ merged` | its changes are already in the default branch |

`w` reopens a local worktree's Herdr workspace, and `W` removes it. Removing
deletes the branch too if it's empty or merged, and keeps it otherwise.

## Keys

| Key | Action |
| --- | --- |
| `j`/`k`, `↑`/`↓` | move selection |
| `g`/`G` | first / last PR |
| `J`/`K`, `PgUp`/`PgDn` | scroll description |
| `o`, `Enter` | open PR in browser |
| `l` | add `claude-review` to the selected PR, if the repository defines the label and the PR does not already have it |
| `c` | `gh pr checkout` the selected PR; if your local branch has diverged (e.g. the PR was rebased), offers to reset it |
| `w` | open the PR in a [worktrunk](https://worktrunk.dev) worktree and, inside [Herdr](https://herdr.dev), a Herdr workspace there (both optional); if the PR's branch is checked out in your main checkout, offers to move it to a worktree. On a local worktree, reopens its Herdr workspace |
| `W` | remove the selected worktree and close its Herdr workspace, after confirming |
| `n` | create a new branch in its own worktree and Herdr workspace; see [New branches](#new-branches-and-local-worktrees) |
| `r` | refresh (also automatic every 60s) |
| `?` | key bindings |
| `q`, `Esc` | quit |

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
