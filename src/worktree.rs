//! Opens a PR, or a new branch, in its own worktrunk worktree and, when
//! running inside Herdr, in a Herdr workspace there. Both tools are optional:
//! a missing tool is reported to the user instead of breaking the app.

use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::github::{git_in, has_uncommitted_changes};
use crate::model::{Progress, Worktree, WorktreeStatus};

pub struct OpenedWorktree {
    pub path: String,
    pub workspace: Workspace,
}

pub enum Workspace {
    Opened,
    AlreadyOpen,
    NotInHerdr,
    HerdrMissing,
}

pub enum OpenOutcome {
    Opened(OpenedWorktree),
    /// worktrunk reused the main checkout because the PR's branch is checked
    /// out there, and git allows a branch in only one worktree.
    InMainCheckout {
        branch: String,
        path: String,
    },
}

impl OpenedWorktree {
    /// Describes the outcome for `subject`, e.g. "PR #3" or a branch name.
    pub fn summary(&self, subject: &str) -> String {
        let path = tilde(&self.path);
        match self.workspace {
            Workspace::Opened => format!("Opened {subject} in a Herdr workspace at {path}"),
            Workspace::AlreadyOpen => format!("Switched to the Herdr workspace for {subject}"),
            Workspace::NotInHerdr => {
                format!("{subject} worktree at {path} (not inside Herdr, so no workspace opened)")
            }
            Workspace::HerdrMissing => {
                format!(
                    "{subject} worktree at {path} (herdr not installed, so no workspace opened)"
                )
            }
        }
    }
}

/// The prefix for new branches: the lowercased user name and a slash, or
/// nothing if the user name can't be read.
pub fn branch_prefix() -> String {
    run("whoami", &[])
        .ok()
        .flatten()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .to_lowercase()
        })
        .filter(|name| !name.is_empty())
        .map(|name| format!("{name}/"))
        .unwrap_or_default()
}

/// Turns free text into a branch name: lowercase letters and digits joined by
/// single dashes. Spaces and `-_/.` separate words; anything else is dropped.
pub fn slugify(text: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(c.to_ascii_lowercase());
        } else if c.is_whitespace() || "-_/.".contains(c) {
            pending_dash = true;
        }
    }
    slug
}

/// Creates `branch` from the freshly fetched default branch in a new worktree
/// and opens it. Fails if the branch already exists.
pub fn create(branch: &str, default_branch: &str) -> Result<OpenedWorktree> {
    git_in(".", &["fetch", "origin", default_branch])?;
    let base = format!("origin/{default_branch}");
    let path = wt_switch(&["--create", branch, "--base", &base])?;
    let workspace = open_workspace(&path, branch)?;
    Ok(OpenedWorktree { path, workspace })
}

/// Opens the Herdr workspace for an existing worktree.
pub fn open_branch(branch: &str, path: &str) -> Result<OpenedWorktree> {
    let workspace = open_workspace(path, branch)?;
    Ok(OpenedWorktree {
        path: path.to_owned(),
        workspace,
    })
}

/// Checks out the PR into a worktree (reusing an existing one) and opens it.
pub fn open_pr(number: u64, branch: &str) -> Result<OpenOutcome> {
    let path = wt_switch(&[&format!("pr:{number}")])?;
    if same_path(&path, &main_worktree()?) {
        return Ok(OpenOutcome::InMainCheckout {
            branch: branch.to_owned(),
            path,
        });
    }
    let workspace = open_workspace(&path, &format!("#{number} {branch}"))?;
    Ok(OpenOutcome::Opened(OpenedWorktree { path, workspace }))
}

/// Frees the PR's branch by switching the main checkout to the default
/// branch, then opens the PR in its own worktree.
pub fn move_to_worktree(
    number: u64,
    branch: &str,
    main_path: &str,
    default_branch: &str,
) -> Result<OpenOutcome> {
    // Switching would carry uncommitted changes onto the default branch.
    if has_uncommitted_changes(main_path)? {
        bail!("Your main checkout has uncommitted changes; commit or stash them first");
    }
    git_in(main_path, &["switch", default_branch])?;
    open_pr(number, branch)
}

/// Linked worktrees with a branch checked out, newest commit first. The main
/// checkout is left out: it's where you work, not a PR's worktree. Without
/// worktrunk, falls back to git and reports no status.
pub fn list_worktrees() -> Result<Vec<Worktree>> {
    let from_wt = run("wt", &["list", "--format", "json"])
        .ok()
        .flatten()
        .filter(|output| output.status.success())
        .and_then(|output| parse_wt_list(&output.stdout).ok());
    let mut worktrees = match from_wt {
        Some(worktrees) => worktrees,
        None => parse_worktrees(&git_in(".", &["worktree", "list", "--porcelain"])?),
    };
    // Option orders None first, so reversing puts undated worktrees last.
    worktrees.sort_by(|a, b| b.committed_at.cmp(&a.committed_at));
    Ok(worktrees)
}

/// Removes the PR's worktree with worktrunk, which refuses if it has
/// uncommitted changes, then closes its Herdr workspace if one is open.
/// Returns whether a workspace was closed.
pub fn remove(branch: &str, path: &str) -> Result<bool> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    if fs::canonicalize(&cwd)?.starts_with(fs::canonicalize(path)?) {
        bail!("code-flow is running inside this worktree; run it from elsewhere to remove it");
    }
    let Some(output) = run("wt", &["remove", branch, "--foreground"])? else {
        bail!("worktrunk (wt) is not installed; see https://worktrunk.dev");
    };
    if !output.status.success() {
        bail!(
            "{}",
            wt_error("wt remove", &String::from_utf8_lossy(&output.stderr))
        );
    }
    close_workspace_at(path).context("removed the worktree but couldn't close its Herdr workspace")
}

/// Runs a program with no stdin, returning `None` if it isn't installed.
fn run(program: &str, args: &[&str]) -> Result<Option<Output>> {
    match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => Ok(Some(output)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to run `{program}`")),
    }
}

/// Runs `wt switch` with `args`, returning the worktree's path.
fn wt_switch(args: &[&str]) -> Result<String> {
    // Without a terminal, worktrunk refuses to run unapproved project hooks
    // rather than prompting. Deliberately no --yes: that would run whatever
    // hooks the repository's config asks for.
    let args = [&["switch"][..], args, &["--no-cd", "--format", "json"]].concat();
    let Some(output) = run("wt", &args)? else {
        bail!("worktrunk (wt) is not installed; see https://worktrunk.dev");
    };
    if !output.status.success() {
        bail!(
            "{}",
            wt_error("wt switch", &String::from_utf8_lossy(&output.stderr))
        );
    }
    let switched: WtSwitch =
        serde_json::from_slice(&output.stdout).context("unexpected output from `wt switch`")?;
    Ok(switched.path)
}

/// Picks the useful line out of worktrunk's decorated progress output.
fn wt_error(command: &str, stderr: &str) -> String {
    if stderr.contains("needs approval") {
        return "worktrunk hooks need approval: run `wt config approvals add` in this repo, \
                then retry"
            .into();
    }
    let line = stderr
        .lines()
        .find(|line| line.starts_with('✗'))
        .or_else(|| stderr.lines().rev().find(|line| !line.trim().is_empty()))
        .unwrap_or("unknown error");
    format!(
        "`{command}` failed: {}",
        line.trim_start_matches('✗').trim()
    )
}

fn main_worktree() -> Result<String> {
    let porcelain = git_in(".", &["worktree", "list", "--porcelain"])?;
    porcelain
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .map(str::to_owned)
        .context("`git worktree list` returned no worktrees")
}

fn parse_worktrees(porcelain: &str) -> Vec<Worktree> {
    porcelain
        .split("\n\n")
        .skip(1)
        .filter_map(|entry| {
            let path = entry.lines().find_map(|l| l.strip_prefix("worktree "))?;
            let branch = entry
                .lines()
                .find_map(|l| l.strip_prefix("branch refs/heads/"))?;
            Some(Worktree {
                branch: branch.to_owned(),
                path: path.to_owned(),
                subject: None,
                committed_at: None,
                status: None,
            })
        })
        .collect()
}

fn parse_wt_list(json: &[u8]) -> Result<Vec<Worktree>> {
    let list: WtList = serde_json::from_slice(json).context("unexpected output from `wt list`")?;
    Ok(list
        .items
        .into_iter()
        .filter_map(|item| {
            let worktree = item.worktree.filter(|worktree| !worktree.main)?;
            let changes = worktree.changes.unwrap_or_default();
            let (subject, committed_at) = item
                .head
                .map(|head| (head.subject, head.committed_at))
                .unwrap_or_default();
            Some(Worktree {
                branch: item.branch?,
                path: worktree.path,
                subject,
                committed_at,
                status: Some(WorktreeStatus {
                    uncommitted: changes.staged
                        || changes.modified
                        || changes.untracked
                        || changes.renamed
                        || changes.deleted
                        || changes.conflicted == Some(true),
                    progress: item.default_branch.and_then(progress),
                }),
            })
        })
        .collect())
}

fn progress(default_branch: WtDefaultBranch) -> Option<Progress> {
    // Integration is checked first: a branch merged with a merge commit is no
    // longer ahead, but it isn't empty either.
    Some(match (default_branch.integration, default_branch.ahead?) {
        (Some(integration), _) if integration.reason == "same_commit" => Progress::Empty,
        (Some(_), _) => Progress::Merged,
        (None, 0) => Progress::Empty,
        (None, ahead) => Progress::Ahead(ahead),
    })
}

/// Compares paths after resolving symlinks, e.g. macOS's /tmp -> /private/tmp.
fn same_path(a: &str, b: &str) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => Path::new(a) == Path::new(b),
    }
}

fn in_herdr() -> bool {
    env::var("HERDR_ENV").as_deref() == Ok("1")
}

fn open_workspace(path: &str, label: &str) -> Result<Workspace> {
    if !in_herdr() {
        return Ok(Workspace::NotInHerdr);
    }
    let open = |target: &[&str]| {
        let mut args = vec![
            "worktree", "open", "--path", path, "--label", label, "--focus",
        ];
        args.extend_from_slice(target);
        run("herdr", &args)
    };
    // Group the worktree under the workspace code-flow runs in. Herdr refuses
    // if that workspace belongs to another directory; then let it find the
    // repo from our working directory, creating a workspace group if needed.
    let workspace_id = env::var("HERDR_WORKSPACE_ID").unwrap_or_default();
    let mut output = None;
    if !workspace_id.is_empty() {
        output = open(&["--workspace", &workspace_id])?;
    }
    if output
        .as_ref()
        .is_none_or(|output| !output.status.success())
    {
        let cwd = env::current_dir().context("failed to read the current directory")?;
        output = open(&["--cwd", &cwd.to_string_lossy()])?;
    }
    let Some(output) = output else {
        return Ok(Workspace::HerdrMissing);
    };
    if !output.status.success() {
        bail!("herdr: {}", herdr_error(&output.stderr));
    }
    let response: HerdrResponse<HerdrOpened> =
        serde_json::from_slice(&output.stdout).context("unexpected output from herdr")?;
    Ok(match response.result.already_open {
        true => Workspace::AlreadyOpen,
        false => Workspace::Opened,
    })
}

/// Closes the Herdr workspace showing the worktree at `path`, if there is one.
fn close_workspace_at(path: &str) -> Result<bool> {
    if !in_herdr() {
        return Ok(false);
    }
    let Some(output) = run("herdr", &["workspace", "list"])? else {
        return Ok(false);
    };
    if !output.status.success() {
        bail!("herdr: {}", herdr_error(&output.stderr));
    }
    let response: HerdrResponse<HerdrWorkspaces> =
        serde_json::from_slice(&output.stdout).context("unexpected output from herdr")?;
    let own_workspace = env::var("HERDR_WORKSPACE_ID").unwrap_or_default();
    let Some(workspace) = response.result.workspaces.into_iter().find(|workspace| {
        // Never close the workspace code-flow itself runs in.
        workspace.workspace_id != own_workspace
            && workspace
                .worktree
                .as_ref()
                .is_some_and(|worktree| same_path(&worktree.checkout_path, path))
    }) else {
        return Ok(false);
    };
    let output = run("herdr", &["workspace", "close", &workspace.workspace_id])?
        .context("herdr disappeared")?;
    if !output.status.success() {
        bail!("herdr: {}", herdr_error(&output.stderr));
    }
    Ok(true)
}

/// Herdr reports server errors as JSON on stderr.
fn herdr_error(stderr: &[u8]) -> String {
    match serde_json::from_slice::<HerdrErrorResponse>(stderr) {
        Ok(response) => response.error.message,
        Err(_) => String::from_utf8_lossy(stderr).trim().to_owned(),
    }
}

/// Abbreviates the home directory to `~`.
pub fn tilde(path: &str) -> String {
    tilde_from(path, &env::var("HOME").unwrap_or_default())
}

/// Matches whole path components, so `/home/bob2` isn't under `/home/bob`.
fn tilde_from(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_owned();
    }
    match Path::new(path).strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.to_owned(),
    }
}

#[derive(Deserialize)]
struct WtSwitch {
    path: String,
}

#[derive(Deserialize)]
struct WtList {
    items: Vec<WtItem>,
}

#[derive(Deserialize)]
struct WtItem {
    /// `None` for a detached HEAD.
    branch: Option<String>,
    head: Option<WtHead>,
    /// `None` for rows that are only a branch.
    worktree: Option<WtWorktree>,
    /// `None` on the default branch itself.
    default_branch: Option<WtDefaultBranch>,
}

#[derive(Deserialize)]
struct WtHead {
    subject: Option<String>,
    committed_at: Option<String>,
}

#[derive(Deserialize)]
struct WtWorktree {
    path: String,
    main: bool,
    changes: Option<WtChanges>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WtChanges {
    staged: bool,
    modified: bool,
    untracked: bool,
    renamed: bool,
    deleted: bool,
    conflicted: Option<bool>,
}

#[derive(Deserialize)]
struct WtDefaultBranch {
    /// `None` when the branch shares no history with the default branch.
    ahead: Option<u32>,
    /// Absent when not integrated, null when uncommitted changes skipped the
    /// check; both read as not integrated.
    integration: Option<WtIntegration>,
}

#[derive(Deserialize)]
struct WtIntegration {
    reason: String,
}

#[derive(Deserialize)]
struct HerdrResponse<T> {
    result: T,
}

#[derive(Deserialize)]
struct HerdrOpened {
    already_open: bool,
}

#[derive(Deserialize)]
struct HerdrWorkspaces {
    workspaces: Vec<HerdrWorkspace>,
}

#[derive(Deserialize)]
struct HerdrWorkspace {
    workspace_id: String,
    worktree: Option<HerdrWorktree>,
}

#[derive(Deserialize)]
struct HerdrWorktree {
    checkout_path: String,
}

#[derive(Deserialize)]
struct HerdrErrorResponse {
    error: HerdrError,
}

#[derive(Deserialize)]
struct HerdrError {
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Outputs below were captured from wt 0.77.0, herdr 0.8.2 and git 2.54.0.

    #[test]
    fn parses_wt_switch_json() {
        let created = r#"{"action":"created","branch":"mock/failing-test","path":"/src/code-flow.mock-failing-test","created_branch":false,"from_remote":"origin/mock/failing-test"}"#;
        let switched: WtSwitch = serde_json::from_str(created).unwrap();
        assert_eq!(switched.path, "/src/code-flow.mock-failing-test");
        let existing = r#"{"action":"existing","branch":"mock/failing-test","path":"/src/code-flow.mock-failing-test"}"#;
        assert!(serde_json::from_str::<WtSwitch>(existing).is_ok());
    }

    #[test]
    fn explains_hook_approval_failure() {
        let stderr = "◎ Fetching PR #5...\n\
            ▲ code-flow needs approval to execute 1 command:\n\
            ○ pre-start:\n  echo hook-ran > hook-ran.txt\n\
            ✗ Cannot prompt for approval in non-interactive environment\n\
            ↳ To skip prompts in CI/CD, add --yes; to pre-approve commands, run wt config approvals add --yes";
        assert!(wt_error("wt switch", stderr).contains("run `wt config approvals add`"));
    }

    #[test]
    fn picks_wt_error_line() {
        let dirty = "✗ Cannot remove worktree: mock/failing-test has uncommitted changes\n\
            \x20 ?? untracked.txt\n\
            ↳ Commit or stash changes first, or to lose uncommitted changes, run wt remove --force mock/failing-test";
        assert_eq!(
            wt_error("wt remove", dirty),
            "`wt remove` failed: Cannot remove worktree: mock/failing-test has uncommitted changes"
        );
        assert_eq!(
            wt_error("wt switch", "plain failure\n"),
            "`wt switch` failed: plain failure"
        );
    }

    #[test]
    fn parses_herdr_responses() {
        let opened = r#"{"id":"cli:worktree:open","result":{"already_open":false,"type":"worktree_opened"}}"#;
        let response: HerdrResponse<HerdrOpened> = serde_json::from_str(opened).unwrap();
        assert!(!response.result.already_open);

        let list = r#"{"id":"cli:workspace:list","result":{"type":"workspace_list","workspaces":[
            {"workspace_id":"w2","label":"code-flow","focused":true},
            {"workspace_id":"w4","label":"PR 3","worktree":{"checkout_path":"/src/r.x","is_linked_worktree":true}}]}}"#;
        let response: HerdrResponse<HerdrWorkspaces> = serde_json::from_str(list).unwrap();
        let paths: Vec<_> = response
            .result
            .workspaces
            .iter()
            .map(|w| w.worktree.as_ref().map(|t| t.checkout_path.as_str()))
            .collect();
        assert_eq!(paths, [None, Some("/src/r.x")]);

        let error = br#"{"error":{"code":"worktree_not_found","message":"worktree path not found"},"id":"cli:worktree:open"}"#;
        assert_eq!(herdr_error(error), "worktree path not found");
        assert_eq!(herdr_error(b"connection refused\n"), "connection refused");
    }

    #[test]
    fn lists_linked_worktrees_by_branch() {
        let porcelain = "worktree /src/repo\nHEAD 15f9749\nbranch refs/heads/main\n\n\
            worktree /src/repo.feature\nHEAD 9129d1a\nbranch refs/heads/mock/feature\n\n\
            worktree /src/repo.detached\nHEAD 4c27a2c\ndetached\n";
        let worktrees = parse_worktrees(porcelain);
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].branch, "mock/feature");
        assert_eq!(worktrees[0].path, "/src/repo.feature");
        assert_eq!(worktrees[0].status, None);
    }

    #[test]
    fn parses_wt_list_json() {
        // Trimmed from wt 0.77.0: the main checkout, a squash-merged branch,
        // a fresh branch with untracked files, a branch with commits, and a
        // detached worktree.
        let json = br#"{"schema":2,"repo":{"default_branch":"main"},"items":[
            {"branch":"main","head":{"subject":"init","committed_at":"2026-09-14T00:32:20Z"},
             "worktree":{"path":"/src/repo","main":true,"changes":{"staged":false,"modified":false,"untracked":false,"renamed":false,"deleted":false,"conflicted":false}}},
            {"branch":"dweis/merged","head":{"subject":"wip","committed_at":"2026-09-14T00:35:09Z"},
             "worktree":{"path":"/src/repo.dweis-merged","main":false,"changes":{"staged":false,"modified":false,"untracked":false,"renamed":false,"deleted":false,"conflicted":false}},
             "default_branch":{"ahead":1,"behind":1,"orphan":false,"integration":{"reason":"patch_id_match"}}},
            {"branch":"dweis/fresh","head":{"subject":"init","committed_at":"2026-09-14T00:32:20Z"},
             "worktree":{"path":"/src/repo.dweis-fresh","main":false,"changes":{"staged":false,"modified":false,"untracked":true,"renamed":false,"deleted":false,"conflicted":false}},
             "default_branch":{"ahead":0,"behind":0,"orphan":false,"integration":null}},
            {"branch":"dweis/busy","head":{"subject":"Add retry","committed_at":"2026-09-13T10:00:00Z"},
             "worktree":{"path":"/src/repo.dweis-busy","main":false,"changes":null},
             "default_branch":{"ahead":3,"behind":0,"orphan":false}},
            {"branch":null,"head":{"subject":"x","committed_at":"2026-09-13T10:00:00Z"},
             "worktree":{"path":"/src/repo.detached","main":false}}]}"#;
        let worktrees = parse_wt_list(json).unwrap();
        let summary: Vec<_> = worktrees
            .iter()
            .map(|w| (w.branch.as_str(), w.status.unwrap()))
            .collect();
        let status = |uncommitted, progress| WorktreeStatus {
            uncommitted,
            progress: Some(progress),
        };
        assert_eq!(
            summary,
            [
                ("dweis/merged", status(false, Progress::Merged)),
                ("dweis/fresh", status(true, Progress::Empty)),
                ("dweis/busy", status(false, Progress::Ahead(3))),
            ]
        );
        assert_eq!(worktrees[2].subject.as_deref(), Some("Add retry"));
        assert_eq!(worktrees[2].path, "/src/repo.dweis-busy");
    }

    #[test]
    fn integration_decides_progress_before_ahead_count() {
        let progress_of = |ahead, reason: Option<&str>| {
            progress(WtDefaultBranch {
                ahead,
                integration: reason.map(|reason| WtIntegration {
                    reason: reason.into(),
                }),
            })
        };
        assert_eq!(
            progress_of(Some(0), Some("same_commit")),
            Some(Progress::Empty)
        );
        // Merged with a merge commit: nothing ahead, but not empty either.
        assert_eq!(
            progress_of(Some(0), Some("ancestor")),
            Some(Progress::Merged)
        );
        assert_eq!(progress_of(Some(0), None), Some(Progress::Empty));
        assert_eq!(progress_of(Some(2), None), Some(Progress::Ahead(2)));
        assert_eq!(progress_of(None, None), None);
    }

    #[test]
    fn slugifies_branch_names() {
        assert_eq!(
            slugify(r#"Fix Bob's "flaky" CI_retry"#),
            "fix-bobs-flaky-ci-retry"
        );
        assert_eq!(slugify("  --Add  v1.2 / docs--  "), "add-v1-2-docs");
        assert_eq!(slugify("Café résumé!"), "caf-rsum");
        assert_eq!(slugify("?!'\""), "");
    }

    #[test]
    fn abbreviates_only_paths_inside_home() {
        let home = "/home/bob";
        assert_eq!(tilde_from("/home/bob", home), "~");
        assert_eq!(tilde_from("/home/bob/src/repo", home), "~/src/repo");
        assert_eq!(tilde_from("/home/bob/src/repo", "/home/bob/"), "~/src/repo");
        assert_eq!(tilde_from("/home/bob2/repo", home), "/home/bob2/repo");
        assert_eq!(tilde_from("/src/repo", home), "/src/repo");
        assert_eq!(tilde_from("/home/bob/repo", ""), "/home/bob/repo");
    }

    #[test]
    fn refuses_to_remove_the_worktree_it_runs_in() {
        // Both are refused before worktrunk runs.
        for path in [".", ".."] {
            let err = remove("code-flow-test-no-such-branch", path).unwrap_err();
            assert!(
                err.to_string().contains("running inside this worktree"),
                "{err}"
            );
        }
    }

    #[test]
    fn summarizes_opened_worktree_for_any_subject() {
        let opened = OpenedWorktree {
            path: "/src/repo.x".into(),
            workspace: Workspace::Opened,
        };
        assert_eq!(
            opened.summary("PR #3"),
            "Opened PR #3 in a Herdr workspace at /src/repo.x"
        );
        let opened = OpenedWorktree {
            workspace: Workspace::NotInHerdr,
            ..opened
        };
        assert_eq!(
            opened.summary("dweis/x"),
            "dweis/x worktree at /src/repo.x (not inside Herdr, so no workspace opened)"
        );
    }
}
