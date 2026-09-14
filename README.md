# code-flow

[![CI](https://github.com/dmweis/code-flow/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/dmweis/code-flow/actions/workflows/ci.yml?query=branch%3Amain)

A terminal UI that lists your open pull requests in the current GitHub
repository, with their review, CI, and merge status at a glance.

## Install

Requires the [GitHub CLI](https://cli.github.com/), logged in with `gh auth login`.

```sh
cargo install --path .
```

Then run `code-flow` from inside a clone of a GitHub repository.

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

## Keys

| Key | Action |
| --- | --- |
| `j`/`k`, `↑`/`↓` | move selection |
| `g`/`G` | first / last PR |
| `J`/`K`, `PgUp`/`PgDn` | scroll description |
| `o`, `Enter` | open PR in browser |
| `l` | add `claude-review` to the selected PR, if the repository defines the label and the PR does not already have it |
| `c` | `gh pr checkout` the selected PR; if your local branch has diverged (e.g. the PR was rebased), offers to reset it |
| `w` | open the PR in a [worktrunk](https://worktrunk.dev) worktree and, inside [Herdr](https://herdr.dev), a Herdr workspace there (both optional); if the PR's branch is checked out in your main checkout, offers to move it to a worktree |
| `W` | remove the PR's worktree and close its Herdr workspace, after confirming |
| `r` | refresh (also automatic every 60s) |
| `?` | key bindings |
| `q`, `Esc` | quit |

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
