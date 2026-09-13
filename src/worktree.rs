//! Opens a PR in its own worktrunk worktree and, when running inside Herdr, in
//! a Herdr workspace there. Both tools are optional: a missing tool is
//! reported to the user instead of breaking the app.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::github::git_in;

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
    pub fn summary(&self, number: u64) -> String {
        let path = tilde(&self.path);
        match self.workspace {
            Workspace::Opened => format!("Opened PR #{number} in a Herdr workspace at {path}"),
            Workspace::AlreadyOpen => format!("Switched to the Herdr workspace for PR #{number}"),
            Workspace::NotInHerdr => {
                format!(
                    "PR #{number} worktree at {path} (not inside Herdr, so no workspace opened)"
                )
            }
            Workspace::HerdrMissing => {
                format!(
                    "PR #{number} worktree at {path} (herdr not installed, so no workspace opened)"
                )
            }
        }
    }
}

/// Checks out the PR into a worktree (reusing an existing one) and opens it.
pub fn open_pr(number: u64, branch: &str) -> Result<OpenOutcome> {
    let path = switch_worktree(number)?;
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
    if !git_in(
        main_path,
        &["status", "--porcelain", "--untracked-files=no"],
    )?
    .is_empty()
    {
        bail!("Your main checkout has uncommitted changes; commit or stash them first");
    }
    git_in(main_path, &["switch", default_branch])?;
    open_pr(number, branch)
}

/// Linked worktrees by branch name. The main checkout is left out: it's where
/// you work, not a PR's worktree.
pub fn list_worktrees() -> Result<HashMap<String, String>> {
    Ok(parse_worktrees(&git_in(
        ".",
        &["worktree", "list", "--porcelain"],
    )?))
}

/// Removes the PR's worktree with worktrunk, which refuses if it has
/// uncommitted changes, then closes its Herdr workspace if one is open.
/// Returns whether a workspace was closed.
pub fn remove(branch: &str, path: &str) -> Result<bool> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    if fs::canonicalize(&cwd)?.starts_with(fs::canonicalize(path)?) {
        bail!("pr-triage is running inside this worktree; run it from elsewhere to remove it");
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

fn switch_worktree(number: u64) -> Result<String> {
    // Without a terminal, worktrunk refuses to run unapproved project hooks
    // rather than prompting. Deliberately no --yes: that would run whatever
    // hooks the repository's config asks for.
    let pr = format!("pr:{number}");
    let Some(output) = run("wt", &["switch", &pr, "--no-cd", "--format", "json"])? else {
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

fn parse_worktrees(porcelain: &str) -> HashMap<String, String> {
    porcelain
        .split("\n\n")
        .skip(1)
        .filter_map(|entry| {
            let path = entry.lines().find_map(|l| l.strip_prefix("worktree "))?;
            let branch = entry
                .lines()
                .find_map(|l| l.strip_prefix("branch refs/heads/"))?;
            Some((branch.to_owned(), path.to_owned()))
        })
        .collect()
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
    // Group the worktree under the workspace pr-triage runs in. Herdr refuses
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
        // Never close the workspace pr-triage itself runs in.
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
    match env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
}

#[derive(Deserialize)]
struct WtSwitch {
    path: String,
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
        let created = r#"{"action":"created","branch":"mock/failing-test","path":"/src/pr-triage.mock-failing-test","created_branch":false,"from_remote":"origin/mock/failing-test"}"#;
        let switched: WtSwitch = serde_json::from_str(created).unwrap();
        assert_eq!(switched.path, "/src/pr-triage.mock-failing-test");
        let existing = r#"{"action":"existing","branch":"mock/failing-test","path":"/src/pr-triage.mock-failing-test"}"#;
        assert!(serde_json::from_str::<WtSwitch>(existing).is_ok());
    }

    #[test]
    fn explains_hook_approval_failure() {
        let stderr = "◎ Fetching PR #5...\n\
            ▲ pr-triage needs approval to execute 1 command:\n\
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
            {"workspace_id":"w2","label":"pr-triage","focused":true},
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
        assert_eq!(worktrees["mock/feature"], "/src/repo.feature");
    }
}
