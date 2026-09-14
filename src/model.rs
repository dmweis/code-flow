//! Domain types describing the triage state of pull requests and local
//! worktrees.
//!
//! These are derived from the raw GitHub response in `github.rs` and from
//! worktrunk in `worktree.rs`, and are what the UI renders. They deliberately
//! contain no presentation details.

pub const CLAUDE_REVIEW_LABEL: &str = "claude-review";

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub head: String,
    /// The head commit, so a merge can refuse commits pushed since.
    pub head_oid: String,
    pub base: String,
    pub is_draft: bool,
    pub review: Review,
    pub ci: Ci,
    pub merge: Merge,
    pub auto_merge: AutoMerge,
    pub unresolved_threads: usize,
    pub labels: Vec<Label>,
}

impl PullRequest {
    pub fn has_claude_review(&self) -> bool {
        self.labels
            .iter()
            .any(|label| label.name == CLAUDE_REVIEW_LABEL)
    }

    /// GitHub would merge it now, and nothing says it shouldn't be: not a
    /// draft (which GitHub can still report as mergeable), no changes
    /// requested, CI not failing or running, and not already set to merge.
    pub fn is_ready_to_merge(&self) -> bool {
        self.merge == Merge::Ready
            && !self.is_draft
            && self.review != Review::ChangesRequested
            && matches!(self.ci.state, CiState::Passed | CiState::None)
            && self.auto_merge == AutoMerge::Off
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Review {
    Required,
    Approved,
    ChangesRequested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiState {
    None,
    Running,
    Passed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ci {
    pub state: CiState,
    pub total: u32,
    pub pending: u32,
    pub failed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merge {
    /// No conflicts, and GitHub reports nothing else blocking a merge.
    Ready,
    /// No conflicts, but something may block a merge, such as a required
    /// review or a failing check.
    Clean,
    Conflicts,
    Behind,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoMerge {
    Off,
    Enabled,
    Queued { position: Option<u32> },
}

/// How GitHub combines a PR into its base branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    pub rgb: (u8, u8, u8),
}

/// A linked worktree (never the main checkout) with a branch checked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub branch: String,
    pub path: String,
    /// Subject of the branch's last commit.
    pub subject: Option<String>,
    /// Committer time of the last commit, RFC 3339 UTC, so it sorts as text.
    pub committed_at: Option<String>,
    /// `None` when worktrunk isn't available to report it.
    pub status: Option<WorktreeStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorktreeStatus {
    pub uncommitted: bool,
    /// `None` when the branch shares no history with the default branch.
    pub progress: Option<Progress>,
}

/// How the branch relates to the default branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// No commits of its own yet.
    Empty,
    /// Commits the default branch doesn't have.
    Ahead(u32),
    /// Its changes are already in the default branch, e.g. after its PR merged.
    Merged,
}
