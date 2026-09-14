//! Talks to GitHub through the `gh` CLI so we reuse its authentication.

use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::model::{
    AutoMerge, CLAUDE_REVIEW_LABEL, Ci, CiState, Label, Merge, MergeMethod, PullRequest, Review,
};

const QUERY: &str = r#"
query($q: String!, $owner: String!, $name: String!, $label: String!) {
  repository(owner: $owner, name: $name) {
    label(name: $label) { name color }
    viewerDefaultMergeMethod
  }
  search(query: $q, type: ISSUE, first: 100) {
    nodes {
      ... on PullRequest {
        number title body isDraft headRefName headRefOid baseRefName
        reviewDecision mergeable mergeStateStatus
        isInMergeQueue mergeQueueEntry { position }
        autoMergeRequest { enabledAt }
        labels(first: 100) { nodes { name color } }
        latestReviews(first: 50) { nodes { state } }
        reviewThreads(first: 100) { nodes { isResolved } }
        commits(last: 1) { nodes { commit { statusCheckRollup {
          state
          contexts(first: 0) {
            totalCount
            checkRunCountsByState { state count }
            statusContextCountsByState { state count }
          }
        } } } }
      }
    }
  }
}
"#;

/// Check run and status context states that mean "not finished yet".
const PENDING_STATES: &[&str] = &[
    "QUEUED",
    "IN_PROGRESS",
    "PENDING",
    "WAITING",
    "REQUESTED",
    "EXPECTED",
];

/// Check run and status context states that mean "this check failed".
const FAILED_STATES: &[&str] = &[
    "FAILURE",
    "ERROR",
    "TIMED_OUT",
    "STARTUP_FAILURE",
    "CANCELLED",
    "ACTION_REQUIRED",
];

fn run(program: &str, args: &[&str]) -> Result<Output> {
    run_in(".", program, args)
}

/// Runs a command in `dir`, returning its output whether or not it succeeded.
/// Messages are forced to English so git's errors can be recognized.
fn run_in(dir: &str, program: &str, args: &[&str]) -> Result<Output> {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run `{program}`; is it installed and on PATH?"))
}

fn check(program: &str, args: &[&str], output: Output) -> Result<Output> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`{program} {}` failed: {}",
            args[..args.len().min(2)].join(" "),
            stderr.trim()
        );
    }
    Ok(output)
}

fn gh(args: &[&str]) -> Result<Output> {
    check("gh", args, run("gh", args)?)
}

fn git(args: &[&str]) -> Result<String> {
    git_in(".", args)
}

/// Runs git in `dir`, returning its trimmed stdout.
pub fn git_in(dir: &str, args: &[&str]) -> Result<String> {
    let output = check("git", args, run_in(dir, "git", args)?)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Whether tracked files in `dir` have changes, staged or not, that aren't
/// committed. Untracked files don't count.
pub fn has_uncommitted_changes(dir: &str) -> Result<bool> {
    let status = git_in(dir, &["status", "--porcelain", "--untracked-files=no"])?;
    Ok(!status.is_empty())
}

/// The last line of a command's stderr, where git reports the branch switch.
fn last_line(output: &Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr);
    text.lines().last().unwrap_or_default().trim().to_owned()
}

pub struct Repo {
    /// `owner/name`
    pub name: String,
    pub default_branch: String,
}

/// Resolves the GitHub repository for the current directory.
pub fn current_repo() -> Result<Repo> {
    let output = gh(&[
        "repo",
        "view",
        "--json",
        "nameWithOwner,defaultBranchRef",
        "--jq",
        ".nameWithOwner + \" \" + .defaultBranchRef.name",
    ])?;
    let text = String::from_utf8_lossy(&output.stdout);
    let (name, default_branch) = text
        .trim()
        .split_once(' ')
        .context("unexpected output from `gh repo view`")?;
    Ok(Repo {
        name: name.to_owned(),
        default_branch: default_branch.to_owned(),
    })
}

/// Fetches the open pull requests authored by the authenticated user.
pub fn fetch_my_prs(repo: &str) -> Result<PrSnapshot> {
    let (owner, name) = repo
        .split_once('/')
        .context("expected owner/name repository")?;
    let search = format!("repo:{repo} is:pr is:open author:@me sort:updated-desc");
    let output = gh(&[
        "api",
        "graphql",
        "-f",
        &format!("query={QUERY}"),
        "-f",
        &format!("q={search}"),
        "-f",
        &format!("owner={owner}"),
        "-f",
        &format!("name={name}"),
        "-f",
        &format!("label={CLAUDE_REVIEW_LABEL}"),
    ])?;
    parse_response(&output.stdout)
}

/// Adds an existing repository label; `gh pr edit` does not create labels.
pub fn add_claude_review(repo: &str, number: u64) -> Result<()> {
    gh(&[
        "pr",
        "edit",
        &number.to_string(),
        "--repo",
        repo,
        "--add-label",
        CLAUDE_REVIEW_LABEL,
    ])?;
    Ok(())
}

/// Merges the PR with `method`, but only if its head is still `head_oid`, so
/// commits pushed since the last refresh aren't merged unseen. Returns whether
/// it's merged now; with a merge queue, gh queues it instead.
pub fn merge(repo: &str, number: u64, method: MergeMethod, head_oid: &str) -> Result<bool> {
    let number = number.to_string();
    let method = match method {
        MergeMethod::Merge => "--merge",
        MergeMethod::Squash => "--squash",
        MergeMethod::Rebase => "--rebase",
    };
    gh(&[
        "pr",
        "merge",
        &number,
        "--repo",
        repo,
        method,
        "--match-head-commit",
        head_oid,
    ])?;
    let output = gh(&[
        "pr", "view", &number, "--repo", repo, "--json", "state", "--jq", ".state",
    ])?;
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "MERGED")
}

pub enum Checkout {
    Done(String),
    /// The existing local branch and the PR have diverged, typically because
    /// the PR was rebased or force-pushed. gh has still switched to the branch.
    Diverged {
        branch: String,
    },
}

pub fn checkout(repo: &str, number: u64) -> Result<Checkout> {
    let args = ["pr", "checkout", &number.to_string(), "--repo", repo];
    let output = run("gh", &args)?;
    if !output.status.success() && is_diverged(&String::from_utf8_lossy(&output.stderr)) {
        let branch = current_branch()?;
        return Ok(Checkout::Diverged { branch });
    }
    let output = check("gh", &args, output)?;
    Ok(Checkout::Done(last_line(&output)))
}

/// Resets the PR's local branch to the PR head, discarding local-only commits.
///
/// When the branch is already checked out, `gh pr checkout --force` runs
/// `git reset --hard`, so refuse if tracked files have uncommitted changes.
pub fn force_checkout(repo: &str, number: u64) -> Result<()> {
    if has_uncommitted_changes(".")? {
        bail!("You have uncommitted changes; commit or stash them, then check out again");
    }
    gh(&[
        "pr",
        "checkout",
        &number.to_string(),
        "--repo",
        repo,
        "--force",
    ])?;
    Ok(())
}

fn is_diverged(stderr: &str) -> bool {
    stderr.contains("Not possible to fast-forward")
}

/// The branch checked out in the current directory; empty when detached.
pub fn current_branch() -> Result<String> {
    git(&["branch", "--show-current"])
}

/// Fast-forwards the default branch to `origin` if it's checked out in `dir`,
/// returning how many commits came in, or `None` on any other branch.
///
/// Never merges, rebases or resets: when local commits have diverged or
/// uncommitted changes would be overwritten, git refuses and this fails.
pub fn pull_default_branch(dir: &str, default_branch: &str) -> Result<Option<u32>> {
    if git_in(dir, &["branch", "--show-current"])? != default_branch {
        return Ok(None);
    }
    git_in(dir, &["fetch", "origin", default_branch])?;
    let before = git_in(dir, &["rev-parse", "HEAD"])?;
    let upstream = format!("origin/{default_branch}");
    let args = ["merge", "--ff-only", &upstream];
    let output = run_in(dir, "git", &args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_diverged(&stderr) {
            bail!("your {default_branch} has commits {upstream} doesn't, so it can't fast-forward");
        }
        if stderr.contains("would be overwritten") {
            bail!("uncommitted changes would be overwritten; commit or stash them first");
        }
    }
    check("git", &args, output)?;
    let count = git_in(dir, &["rev-list", "--count", &format!("{before}..HEAD")])?;
    Ok(Some(
        count
            .parse()
            .context("unexpected output from `git rev-list`")?,
    ))
}

pub fn open_in_browser(repo: &str, number: u64) -> Result<()> {
    gh(&["pr", "view", &number.to_string(), "--repo", repo, "--web"])?;
    Ok(())
}

pub struct PrSnapshot {
    pub prs: Vec<PullRequest>,
    pub claude_review_label: Option<Label>,
    /// The method GitHub preselects for you in this repository.
    pub merge_method: MergeMethod,
}

fn parse_response(json: &[u8]) -> Result<PrSnapshot> {
    let response: Response =
        serde_json::from_slice(json).context("unexpected response from GitHub")?;
    let repository = response.data.repository;
    let merge_method = match repository.viewer_default_merge_method.as_str() {
        "SQUASH" => MergeMethod::Squash,
        "REBASE" => MergeMethod::Rebase,
        _ => MergeMethod::Merge,
    };
    let claude_review_label = repository
        .label
        .map(Label::from)
        .filter(|label| label.name == CLAUDE_REVIEW_LABEL);
    let prs = response
        .data
        .search
        .nodes
        .into_iter()
        .map(PullRequest::from)
        .collect();
    Ok(PrSnapshot {
        prs,
        claude_review_label,
        merge_method,
    })
}

#[derive(Deserialize)]
struct Response {
    data: Data,
}

#[derive(Deserialize)]
struct Data {
    repository: RawRepository,
    search: Nodes<RawPr>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRepository {
    label: Option<RawLabel>,
    viewer_default_merge_method: String,
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPr {
    number: u64,
    title: String,
    body: String,
    is_draft: bool,
    head_ref_name: String,
    head_ref_oid: String,
    base_ref_name: String,
    review_decision: Option<String>,
    mergeable: String,
    merge_state_status: String,
    is_in_merge_queue: bool,
    merge_queue_entry: Option<QueueEntry>,
    auto_merge_request: Option<serde_json::Value>,
    labels: Nodes<RawLabel>,
    latest_reviews: Nodes<RawReview>,
    review_threads: Nodes<RawThread>,
    commits: Nodes<RawCommitNode>,
}

#[derive(Deserialize)]
struct QueueEntry {
    position: Option<u32>,
}

#[derive(Deserialize)]
struct RawLabel {
    name: String,
    color: String,
}

#[derive(Deserialize)]
struct RawReview {
    state: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThread {
    is_resolved: bool,
}

#[derive(Deserialize)]
struct RawCommitNode {
    commit: RawCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCommit {
    status_check_rollup: Option<RawRollup>,
}

#[derive(Deserialize)]
struct RawRollup {
    state: String,
    contexts: RawContexts,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawContexts {
    total_count: u32,
    check_run_counts_by_state: Option<Vec<RawCount>>,
    status_context_counts_by_state: Option<Vec<RawCount>>,
}

#[derive(Deserialize)]
struct RawCount {
    state: String,
    count: u32,
}

impl From<RawPr> for PullRequest {
    fn from(raw: RawPr) -> Self {
        let rollup = raw
            .commits
            .nodes
            .into_iter()
            .next()
            .and_then(|node| node.commit.status_check_rollup);
        PullRequest {
            number: raw.number,
            review: review_status(raw.review_decision.as_deref(), &raw.latest_reviews.nodes),
            ci: ci_status(rollup.as_ref()),
            merge: merge_status(&raw.mergeable, &raw.merge_state_status),
            auto_merge: if raw.is_in_merge_queue {
                AutoMerge::Queued {
                    position: raw.merge_queue_entry.and_then(|entry| entry.position),
                }
            } else if raw.auto_merge_request.is_some() {
                AutoMerge::Enabled
            } else {
                AutoMerge::Off
            },
            unresolved_threads: raw
                .review_threads
                .nodes
                .iter()
                .filter(|thread| !thread.is_resolved)
                .count(),
            labels: raw.labels.nodes.into_iter().map(Label::from).collect(),
            body: clean_body(&raw.body),
            title: raw.title,
            head: raw.head_ref_name,
            head_oid: raw.head_ref_oid,
            base: raw.base_ref_name,
            is_draft: raw.is_draft,
        }
    }
}

impl From<RawLabel> for Label {
    fn from(raw: RawLabel) -> Self {
        let hex = u32::from_str_radix(&raw.color, 16).unwrap_or(0x808080);
        Label {
            name: raw.name,
            rgb: ((hex >> 16) as u8, (hex >> 8) as u8, hex as u8),
        }
    }
}

/// `reviewDecision` is null when the repository doesn't require reviews, so
/// fall back to the latest review from each reviewer.
fn review_status(decision: Option<&str>, latest_reviews: &[RawReview]) -> Review {
    match decision {
        Some("APPROVED") => Review::Approved,
        Some("CHANGES_REQUESTED") => Review::ChangesRequested,
        Some(_) => Review::Required,
        None if latest_reviews
            .iter()
            .any(|r| r.state == "CHANGES_REQUESTED") =>
        {
            Review::ChangesRequested
        }
        None if latest_reviews.iter().any(|r| r.state == "APPROVED") => Review::Approved,
        None => Review::Required,
    }
}

/// A single failed check marks CI as failed even while others are still
/// running, since that's the actionable fact.
fn ci_status(rollup: Option<&RawRollup>) -> Ci {
    let Some(rollup) = rollup else {
        return Ci {
            state: CiState::None,
            total: 0,
            pending: 0,
            failed: 0,
        };
    };
    let counts = || {
        let contexts = &rollup.contexts;
        let check_runs = contexts.check_run_counts_by_state.iter().flatten();
        check_runs.chain(contexts.status_context_counts_by_state.iter().flatten())
    };
    let sum = |states: &[&str]| -> u32 {
        counts()
            .filter(|c| states.contains(&c.state.as_str()))
            .map(|c| c.count)
            .sum()
    };
    let pending = sum(PENDING_STATES);
    let failed = sum(FAILED_STATES);
    let state = if failed > 0 || matches!(rollup.state.as_str(), "FAILURE" | "ERROR") {
        CiState::Failed
    } else if pending > 0 || matches!(rollup.state.as_str(), "PENDING" | "EXPECTED") {
        CiState::Running
    } else {
        CiState::Passed
    };
    Ci {
        state,
        total: rollup.contexts.total_count,
        pending,
        failed,
    }
}

/// `HAS_HOOKS` is `CLEAN` on GitHub Enterprise with pre-receive hooks.
fn merge_status(mergeable: &str, merge_state_status: &str) -> Merge {
    match (mergeable, merge_state_status) {
        ("CONFLICTING", _) | (_, "DIRTY") => Merge::Conflicts,
        (_, "BEHIND") => Merge::Behind,
        ("UNKNOWN", _) => Merge::Unknown,
        ("MERGEABLE", "CLEAN" | "HAS_HOOKS") => Merge::Ready,
        _ => Merge::Clean,
    }
}

/// Normalizes a PR body for terminal display: drops the HTML comments that PR
/// templates leave behind, carriage returns, and tabs.
fn clean_body(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => rest = "",
        }
    }
    out.push_str(rest);
    out.replace('\r', "")
        .replace('\t', "    ")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::{env, fs, process};

    use super::*;

    #[test]
    fn detects_repository_label_even_with_no_open_prs() {
        for (label, expected) in [
            (serde_json::Value::Null, false),
            (
                serde_json::json!({"name": "claude-review", "color": "aabbcc"}),
                true,
            ),
            (
                serde_json::json!({"name": "claude-review-other", "color": "aabbcc"}),
                false,
            ),
        ] {
            let response = serde_json::json!({"data": {
                "repository": {"label": label, "viewerDefaultMergeMethod": "MERGE"},
                "search": {"nodes": []}
            }});
            let snapshot = parse_response(response.to_string().as_bytes()).unwrap();
            assert_eq!(snapshot.claude_review_label.is_some(), expected);
            assert!(snapshot.prs.is_empty());
        }
    }

    #[test]
    fn recognizes_only_the_claude_review_label_on_a_pr() {
        assert!(
            !pr(r#""labels": {"nodes": [
            {"name": "claude-review-other", "color": "aabbcc"}]}"#)
            .has_claude_review()
        );
        assert!(
            pr(r#""labels": {"nodes": [
            {"name": "claude-review", "color": "aabbcc"}]}"#)
            .has_claude_review()
        );
    }

    /// Builds a PR from a baseline response node with `fields` overriding it.
    fn pr(fields: &str) -> PullRequest {
        let mut node: serde_json::Value = serde_json::from_str(
            r#"{
                "number": 1, "title": "t", "body": "",
                "isDraft": false, "headRefName": "feature", "headRefOid": "abc123",
                "baseRefName": "main",
                "reviewDecision": null, "mergeable": "MERGEABLE",
                "mergeStateStatus": "CLEAN", "isInMergeQueue": false,
                "mergeQueueEntry": null, "autoMergeRequest": null,
                "labels": {"nodes": []}, "latestReviews": {"nodes": []},
                "reviewThreads": {"nodes": []},
                "commits": {"nodes": [{"commit": {"statusCheckRollup": null}}]}
            }"#,
        )
        .unwrap();
        let overrides: serde_json::Value = serde_json::from_str(&format!("{{{fields}}}")).unwrap();
        for (key, value) in overrides.as_object().unwrap() {
            node[key] = value.clone();
        }
        let response = serde_json::json!({"data": {
            "repository": {"label": null, "viewerDefaultMergeMethod": "MERGE"},
            "search": {"nodes": [node]}
        }});
        parse_response(response.to_string().as_bytes())
            .unwrap()
            .prs
            .remove(0)
    }

    fn rollup(state: &str, total: u32, check_runs: &str, statuses: &str) -> String {
        format!(
            r#""commits": {{"nodes": [{{"commit": {{"statusCheckRollup": {{
                "state": "{state}",
                "contexts": {{"totalCount": {total},
                    "checkRunCountsByState": [{check_runs}],
                    "statusContextCountsByState": [{statuses}]}}
            }}}}}}]}}"#
        )
    }

    #[test]
    fn defaults_to_ready_needs_review_no_ci_mergeable() {
        let pr = pr(r#""number": 7"#);
        assert_eq!(pr.number, 7);
        assert_eq!(pr.head_oid, "abc123");
        assert!(!pr.is_draft);
        assert_eq!(pr.review, Review::Required);
        assert_eq!(pr.ci.state, CiState::None);
        assert_eq!(pr.merge, Merge::Ready);
        assert_eq!(pr.auto_merge, AutoMerge::Off);
        assert_eq!(pr.unresolved_threads, 0);
    }

    #[test]
    fn draft() {
        assert!(pr(r#""isDraft": true"#).is_draft);
    }

    #[test]
    fn review_decision_wins() {
        let approved = pr(r#""reviewDecision": "APPROVED""#);
        assert_eq!(approved.review, Review::Approved);
        let changes = pr(r#""reviewDecision": "CHANGES_REQUESTED""#);
        assert_eq!(changes.review, Review::ChangesRequested);
        let required = pr(r#""reviewDecision": "REVIEW_REQUIRED",
            "latestReviews": {"nodes": [{"state": "APPROVED"}]}"#);
        assert_eq!(required.review, Review::Required);
    }

    #[test]
    fn review_falls_back_to_latest_reviews() {
        let approved =
            pr(r#""latestReviews": {"nodes": [{"state": "COMMENTED"}, {"state": "APPROVED"}]}"#);
        assert_eq!(approved.review, Review::Approved);
        let changes = pr(
            r#""latestReviews": {"nodes": [{"state": "APPROVED"}, {"state": "CHANGES_REQUESTED"}]}"#,
        );
        assert_eq!(changes.review, Review::ChangesRequested);
    }

    #[test]
    fn ci_passed_counts_skipped_as_done() {
        let pr = pr(&rollup(
            "SUCCESS",
            7,
            r#"{"state": "SKIPPED", "count": 5}, {"state": "SUCCESS", "count": 2}"#,
            "",
        ));
        assert_eq!(
            pr.ci,
            Ci {
                state: CiState::Passed,
                total: 7,
                pending: 0,
                failed: 0
            }
        );
    }

    #[test]
    fn ci_running() {
        let pr = pr(&rollup(
            "PENDING",
            7,
            r#"{"state": "SUCCESS", "count": 3}, {"state": "IN_PROGRESS", "count": 2}, {"state": "QUEUED", "count": 1}"#,
            r#"{"state": "PENDING", "count": 1}"#,
        ));
        assert_eq!(
            pr.ci,
            Ci {
                state: CiState::Running,
                total: 7,
                pending: 4,
                failed: 0
            }
        );
    }

    #[test]
    fn ci_failed_while_others_still_running() {
        let pr = pr(&rollup(
            "PENDING",
            5,
            r#"{"state": "FAILURE", "count": 1}, {"state": "IN_PROGRESS", "count": 2}"#,
            r#"{"state": "ERROR", "count": 1}"#,
        ));
        assert_eq!(
            pr.ci,
            Ci {
                state: CiState::Failed,
                total: 5,
                pending: 2,
                failed: 2
            }
        );
    }

    #[test]
    fn merge_states() {
        assert_eq!(
            pr(r#""mergeable": "CONFLICTING", "mergeStateStatus": "DIRTY""#).merge,
            Merge::Conflicts
        );
        assert_eq!(pr(r#""mergeStateStatus": "BEHIND""#).merge, Merge::Behind);
        assert_eq!(
            pr(r#""mergeable": "UNKNOWN", "mergeStateStatus": "UNKNOWN""#).merge,
            Merge::Unknown
        );
        assert_eq!(pr(r#""mergeStateStatus": "BLOCKED""#).merge, Merge::Clean);
        assert_eq!(pr(r#""mergeStateStatus": "UNSTABLE""#).merge, Merge::Clean);
        assert_eq!(pr(r#""mergeStateStatus": "HAS_HOOKS""#).merge, Merge::Ready);
        // GitHub may not have computed mergeability yet.
        assert_eq!(
            pr(r#""mergeable": "UNKNOWN", "mergeStateStatus": "CLEAN""#).merge,
            Merge::Unknown
        );
    }

    #[test]
    fn ready_to_merge_needs_more_than_a_clean_merge_state() {
        // Without required reviews, an unreviewed PR is ready.
        assert!(pr("").is_ready_to_merge());
        // Captured from a draft on github.com: its merge state is still CLEAN.
        assert!(!pr(r#""isDraft": true"#).is_ready_to_merge());
        assert!(!pr(r#""mergeStateStatus": "BLOCKED""#).is_ready_to_merge());
        let changes = r#""latestReviews": {"nodes": [{"state": "CHANGES_REQUESTED"}]}"#;
        assert!(!pr(changes).is_ready_to_merge());
        let failed = rollup("FAILURE", 1, r#"{"state": "FAILURE", "count": 1}"#, "");
        assert!(!pr(&failed).is_ready_to_merge());
        let passed = rollup("SUCCESS", 1, r#"{"state": "SUCCESS", "count": 1}"#, "");
        assert!(pr(&passed).is_ready_to_merge());
        let auto = r#""autoMergeRequest": {"enabledAt": "2026-09-13T00:00:00Z"}"#;
        assert!(!pr(auto).is_ready_to_merge());
    }

    #[test]
    fn reads_default_merge_method() {
        for (raw, expected) in [
            ("SQUASH", MergeMethod::Squash),
            ("REBASE", MergeMethod::Rebase),
            ("MERGE", MergeMethod::Merge),
        ] {
            let response = serde_json::json!({"data": {
                "repository": {"label": null, "viewerDefaultMergeMethod": raw},
                "search": {"nodes": []}
            }});
            let snapshot = parse_response(response.to_string().as_bytes()).unwrap();
            assert_eq!(snapshot.merge_method, expected);
        }
    }

    #[test]
    fn auto_merge_and_queue() {
        let enabled = pr(r#""autoMergeRequest": {"enabledAt": "2026-09-13T00:00:00Z"}"#);
        assert_eq!(enabled.auto_merge, AutoMerge::Enabled);
        let queued = pr(
            r#""isInMergeQueue": true, "mergeQueueEntry": {"position": 3},
            "autoMergeRequest": {"enabledAt": "2026-09-13T00:00:00Z"}"#,
        );
        assert_eq!(queued.auto_merge, AutoMerge::Queued { position: Some(3) });
    }

    #[test]
    fn counts_unresolved_threads() {
        let pr = pr(r#""reviewThreads": {"nodes": [
            {"isResolved": false}, {"isResolved": true}, {"isResolved": false}]}"#);
        assert_eq!(pr.unresolved_threads, 2);
    }

    #[test]
    fn parses_label_colors() {
        let pr = pr(r#""labels": {"nodes": [
            {"name": "bug", "color": "d73a4a"}, {"name": "odd", "color": "nothex"}]}"#);
        assert_eq!(
            pr.labels,
            vec![
                Label {
                    name: "bug".into(),
                    rgb: (0xd7, 0x3a, 0x4a)
                },
                Label {
                    name: "odd".into(),
                    rgb: (0x80, 0x80, 0x80)
                },
            ]
        );
    }

    #[test]
    fn recognizes_diverged_checkout() {
        // Captured from `gh pr checkout` with gh 2.100.0 and git 2.54.0.
        let diverged = "Switched to branch 'mock/readme-requirements'\n\
            Your branch and 'origin/mock/readme-requirements' have diverged,\n\
            hint: Diverging branches can't be fast-forwarded, you need to either:\n\
            fatal: Not possible to fast-forward, aborting.\n\
            failed to run git: exit status 128";
        assert!(is_diverged(diverged));
        let dirty = "error: Your local changes to the following files would be overwritten by checkout:\n\
            \tREADME.md\nAborting\nfailed to run git: exit status 1";
        assert!(!is_diverged(dirty));
    }

    #[test]
    fn cleans_body() {
        let body = "<!-- template hint -->\r\nFixes the thing.\r\n\tIndented<!-- unterminated";
        assert_eq!(clean_body(body), "Fixes the thing.\n    Indented");
    }

    #[test]
    fn ci_without_counts_goes_by_the_rollup_state() {
        let without_counts = |state: &str| {
            let rollup = format!(
                r#""commits": {{"nodes": [{{"commit": {{"statusCheckRollup": {{
                    "state": "{state}",
                    "contexts": {{"totalCount": 2,
                        "checkRunCountsByState": null,
                        "statusContextCountsByState": null}}
                }}}}}}]}}"#
            );
            pr(&rollup).ci
        };
        let ci = without_counts("SUCCESS");
        assert_eq!((ci.state, ci.total), (CiState::Passed, 2));
        // Failed or running with no count saying which.
        assert_eq!(
            without_counts("FAILURE"),
            Ci {
                state: CiState::Failed,
                total: 2,
                pending: 0,
                failed: 0
            }
        );
        assert_eq!(without_counts("ERROR").state, CiState::Failed);
        assert_eq!(without_counts("EXPECTED").state, CiState::Running);
        // A PR with no commits has no rollup at all.
        assert_eq!(pr(r#""commits": {"nodes": []}"#).ci.state, CiState::None);
    }

    /// A throwaway directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let name = format!("code-flow-test-{}-{n}", process::id());
            let path = env::temp_dir().join(name);
            fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Runs git to set up a test, ignoring the user's git config so settings
    /// like commit signing can't get in the way.
    fn setup_git(dir: &str, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "git {args:?} failed: {stderr}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn write(dir: &str, file: &str, content: &str) {
        fs::write(Path::new(dir).join(file), content).unwrap();
    }

    fn commit(dir: &str, file: &str, content: &str) {
        write(dir, file, content);
        setup_git(dir, &["add", file]);
        setup_git(dir, &["commit", "--quiet", "--message", file]);
    }

    fn head(dir: &str) -> String {
        setup_git(dir, &["rev-parse", "HEAD"])
    }

    /// Your clone of a local `origin` with one commit on `main`, and a second
    /// clone standing in for someone else pushing to it.
    struct Repos {
        local: String,
        other: String,
        _root: TempDir,
    }

    impl Repos {
        fn new() -> Self {
            let root = TempDir::new();
            let path = |name: &str| root.0.join(name).to_str().unwrap().to_owned();
            let (local, origin, other) = (path("local"), path("origin.git"), path("other"));
            let root_path = path("");
            setup_git(
                &root_path,
                &["init", "--quiet", "--initial-branch=main", &local],
            );
            commit(&local, "a.txt", "one");
            setup_git(&root_path, &["clone", "--quiet", "--bare", &local, &origin]);
            setup_git(&local, &["remote", "add", "origin", &origin]);
            setup_git(&local, &["fetch", "--quiet", "origin"]);
            setup_git(&root_path, &["clone", "--quiet", &origin, &other]);
            Repos {
                local,
                other,
                _root: root,
            }
        }

        fn push_from_other(&self, file: &str, content: &str) {
            commit(&self.other, file, content);
            setup_git(&self.other, &["push", "--quiet", "origin", "main"]);
        }
    }

    #[test]
    fn pulls_new_commits_by_fast_forwarding() {
        let repos = Repos::new();
        assert_eq!(pull_default_branch(&repos.local, "main").unwrap(), Some(0));

        repos.push_from_other("b.txt", "two");
        repos.push_from_other("c.txt", "three");
        assert_eq!(pull_default_branch(&repos.local, "main").unwrap(), Some(2));
        assert_eq!(head(&repos.local), head(&repos.other));
    }

    #[test]
    fn keeps_local_commits_when_origin_has_nothing_new() {
        let repos = Repos::new();
        commit(&repos.local, "b.txt", "mine");
        let before = head(&repos.local);
        assert_eq!(pull_default_branch(&repos.local, "main").unwrap(), Some(0));
        assert_eq!(head(&repos.local), before);
    }

    #[test]
    fn pulls_nothing_on_another_branch() {
        let repos = Repos::new();
        let fetched = setup_git(&repos.local, &["rev-parse", "origin/main"]);
        repos.push_from_other("b.txt", "two");
        setup_git(&repos.local, &["switch", "--quiet", "--create", "feature"]);
        assert_eq!(pull_default_branch(&repos.local, "main").unwrap(), None);
        // It didn't even fetch.
        let after = setup_git(&repos.local, &["rev-parse", "origin/main"]);
        assert_eq!(after, fetched);
    }

    #[test]
    fn refuses_to_pull_into_a_diverged_default_branch() {
        let repos = Repos::new();
        repos.push_from_other("b.txt", "theirs");
        commit(&repos.local, "c.txt", "mine");
        let before = head(&repos.local);
        let err = pull_default_branch(&repos.local, "main").unwrap_err();
        assert_eq!(
            err.to_string(),
            "your main has commits origin/main doesn't, so it can't fast-forward"
        );
        assert_eq!(head(&repos.local), before);
    }

    #[test]
    fn refuses_to_pull_over_uncommitted_changes() {
        let repos = Repos::new();
        repos.push_from_other("a.txt", "theirs");
        write(&repos.local, "a.txt", "mine, uncommitted");
        let before = head(&repos.local);
        let err = pull_default_branch(&repos.local, "main").unwrap_err();
        assert_eq!(
            err.to_string(),
            "uncommitted changes would be overwritten; commit or stash them first"
        );
        assert_eq!(head(&repos.local), before);
        let content = fs::read_to_string(Path::new(&repos.local).join("a.txt")).unwrap();
        assert_eq!(content, "mine, uncommitted");
    }

    #[test]
    fn only_changes_to_tracked_files_are_uncommitted() {
        let repos = Repos::new();
        let local = &repos.local;
        assert!(!has_uncommitted_changes(local).unwrap());
        write(local, "untracked.txt", "new");
        assert!(!has_uncommitted_changes(local).unwrap());
        write(local, "a.txt", "changed");
        assert!(has_uncommitted_changes(local).unwrap());
        setup_git(local, &["add", "a.txt"]);
        assert!(has_uncommitted_changes(local).unwrap());
    }
}
