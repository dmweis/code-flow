# pr-triage

[![CI](https://github.com/dmweis/pr-triage/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/dmweis/pr-triage/actions/workflows/ci.yml?query=branch%3Amain)

A terminal UI that lists your open pull requests in the current GitHub
repository, with their review, CI, and merge status at a glance.

## Install

Requires the [GitHub CLI](https://cli.github.com/), logged in with `gh auth login`.

```sh
cargo install --path .
```

Then run `pr-triage` from inside a clone of a GitHub repository.

## Status columns

| Column | Meaning |
| --- | --- |
| D | `●` ready, `◌` draft |
| R | `◷` needs review, `✓` approved, `✗` changes requested |
| CI | `⟳` running, `✓` passed, `✗` failed, `·` no checks |
| M | `✓` no conflicts, `⚠` conflicts, `↓` behind base, `?` unknown |
| A | `»` auto-merge enabled, `≡` in merge queue |
| T | number of unresolved review threads |

Labels are shown after the title. Press `?` in the app for the legend and key bindings.

## Keys

| Key | Action |
| --- | --- |
| `j`/`k`, `↑`/`↓` | move selection |
| `g`/`G` | first / last PR |
| `J`/`K`, `PgUp`/`PgDn` | scroll description |
| `o`, `Enter` | open PR in browser |
| `c` | `gh pr checkout` the selected PR |
| `r` | refresh (also automatic every 60s) |
| `?` | legend |
| `q`, `Esc` | quit |

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
