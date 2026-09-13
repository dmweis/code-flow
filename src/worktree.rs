//! Opens a PR in its own worktrunk worktree and, when running inside Herdr, in
//! a Herdr workspace there. Both tools are optional: a missing tool is
//! reported to the user instead of breaking the app.

use std::env;
use std::io;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

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
pub fn open_pr(number: u64, branch: &str) -> Result<OpenedWorktree> {
    let path = switch_worktree(number)?;
    let workspace = open_workspace(&path, &format!("#{number} {branch}"))?;
    Ok(OpenedWorktree { path, workspace })
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
        bail!("{}", wt_error(&String::from_utf8_lossy(&output.stderr)));
    }
    let switched: WtSwitch =
        serde_json::from_slice(&output.stdout).context("unexpected output from `wt switch`")?;
    Ok(switched.path)
}

/// Picks the useful line out of worktrunk's decorated progress output.
fn wt_error(stderr: &str) -> String {
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
        "`wt switch` failed: {}",
        line.trim_start_matches('✗').trim()
    )
}

fn open_workspace(path: &str, label: &str) -> Result<Workspace> {
    if env::var("HERDR_ENV").as_deref() != Ok("1") {
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
    let response: HerdrResponse =
        serde_json::from_slice(&output.stdout).context("unexpected output from herdr")?;
    Ok(match response.result.already_open {
        true => Workspace::AlreadyOpen,
        false => Workspace::Opened,
    })
}

/// Herdr reports server errors as JSON on stderr.
fn herdr_error(stderr: &[u8]) -> String {
    match serde_json::from_slice::<HerdrErrorResponse>(stderr) {
        Ok(response) => response.error.message,
        Err(_) => String::from_utf8_lossy(stderr).trim().to_owned(),
    }
}

fn tilde(path: &str) -> String {
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
struct HerdrResponse {
    result: HerdrOpened,
}

#[derive(Deserialize)]
struct HerdrOpened {
    already_open: bool,
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

    // Outputs below were captured from wt 0.77.0 and herdr 0.8.2.

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
        assert!(wt_error(stderr).contains("run `wt config approvals add`"));
    }

    #[test]
    fn picks_wt_error_line() {
        let stderr = "◎ Fetching PR #99...\n✗ No PR #99 found\n↳ some hint";
        assert_eq!(wt_error(stderr), "`wt switch` failed: No PR #99 found");
        assert_eq!(
            wt_error("plain failure\n"),
            "`wt switch` failed: plain failure"
        );
    }

    #[test]
    fn parses_herdr_responses() {
        let opened = r#"{"id":"cli:worktree:open","result":{"already_open":false,"type":"worktree_opened"}}"#;
        let response: HerdrResponse = serde_json::from_str(opened).unwrap();
        assert!(!response.result.already_open);

        let error = br#"{"error":{"code":"worktree_not_found","message":"worktree path not found"},"id":"cli:worktree:open"}"#;
        assert_eq!(herdr_error(error), "worktree path not found");
        assert_eq!(herdr_error(b"connection refused\n"), "connection refused");
    }
}
