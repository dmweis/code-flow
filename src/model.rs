//! Domain types describing the triage state of a pull request.
//!
//! These are derived from the raw GitHub response in `github.rs` and are what
//! the UI renders. They deliberately contain no presentation details.

pub const CLAUDE_REVIEW_LABEL: &str = "claude-review";

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub head: String,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    pub rgb: (u8, u8, u8),
}
