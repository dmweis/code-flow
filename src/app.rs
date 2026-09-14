//! Application state, input handling, and the main event loop.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;

use crate::github::{self, Checkout, PrSnapshot, Repo};
use crate::model::{Label, MergeMethod, Progress, PullRequest, Worktree};
use crate::ui;
use crate::worktree::{self, OpenOutcome, OpenedWorktree};

const TICK: Duration = Duration::from_millis(200);
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const FLASH_DURATION: Duration = Duration::from_secs(5);

/// Results of background work, sent back to the event loop.
enum Msg {
    Prs(Result<PrSnapshot>, Vec<Worktree>),
    ClaudeReviewAdded(u64, Label, Result<()>),
    /// Whether the PR merged, rather than joining a merge queue.
    Merged(u64, Result<bool>),
    Worktrees(Result<Vec<Worktree>>),
    CheckedOut(u64, Result<Checkout>),
    Opened(Result<()>),
    OpenedWorktree(u64, Result<OpenOutcome>),
    /// A new or existing local branch opened in its worktree.
    OpenedBranch(String, Result<OpenedWorktree>),
    /// Names what lost its worktree: "PR #3" or a branch.
    RemovedWorktree(String, Result<bool>),
    /// The branch checked out where code-flow runs.
    Branch(Result<String>),
    /// Commits fast-forwarded into the default branch, or `None` when it
    /// isn't checked out.
    PulledDefaultBranch(Result<Option<u32>>),
}

/// A selectable row: one of your open PRs, or a local worktree without one.
pub enum Row<'a> {
    Pr(&'a PullRequest),
    Local(&'a Worktree),
}

/// Identifies a row across refreshes, which can reorder or remove rows.
#[derive(Clone, PartialEq, Eq)]
enum RowId {
    Pr(u64),
    Local(String),
}

pub struct Flash {
    pub text: String,
    pub is_error: bool,
    at: Instant,
}

/// A yes/no question that captures input until it's answered.
pub enum Prompt {
    /// The PR's local branch has diverged from the PR, typically because the
    /// PR was rebased or force-pushed.
    ForceCheckout { number: u64, branch: String },
    /// The PR's branch is checked out in the main checkout, so it can't also
    /// have its own worktree.
    MoveToWorktree {
        number: u64,
        branch: String,
        main_path: String,
    },
    RemoveWorktree {
        number: u64,
        branch: String,
        path: String,
    },
    /// A local worktree, whose branch isn't one of your open PRs.
    RemoveLocal {
        branch: String,
        path: String,
        /// Decides whether worktrunk deletes the branch too.
        progress: Option<Progress>,
    },
    Merge {
        number: u64,
        title: String,
        base: String,
        head_oid: String,
        method: MergeMethod,
    },
}

pub struct App {
    pub repo: String,
    pub default_branch: String,
    pub prs: Vec<PullRequest>,
    pub claude_review_label: Option<Label>,
    pub adding_claude_review: HashSet<u64>,
    /// Successful edits that an already-running refresh may not have seen.
    claude_review_updates: HashMap<u64, Label>,
    pub merge_method: MergeMethod,
    pub merging: HashSet<u64>,
    /// Merged PRs that GitHub's search may still list as open for a while.
    merged: HashSet<u64>,
    /// Linked worktrees, newest commit first.
    pub worktrees: Vec<Worktree>,
    /// The selection among PR rows. `local_list` holds it among local rows;
    /// at most one of the two has a selection.
    pub list: ListState,
    pub local_list: ListState,
    pub detail_scroll: u16,
    pub show_help: bool,
    pub prompt: Option<Prompt>,
    /// Prepended to new branch names, e.g. `dweis/`.
    pub branch_prefix: String,
    /// What's typed so far while the new branch input is open.
    pub new_branch: Option<String>,
    /// The branch checked out where code-flow runs, once known.
    pub current_branch: Option<String>,
    pub pulling: bool,
    /// When the in-flight fetch started, if one is running.
    pub loading_since: Option<Instant>,
    pub last_success: Option<Instant>,
    last_attempt: Option<Instant>,
    /// Error from the most recent fetch, if it failed.
    pub load_error: Option<String>,
    flash: Option<Flash>,
    should_quit: bool,
    tx: Sender<Msg>,
}

pub fn run(terminal: &mut DefaultTerminal, repo: Repo) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(repo, tx);
    app.branch_prefix = worktree::branch_prefix();
    app.refresh();
    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, &mut app))?;
        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.on_key(key);
        }
        app.drain(&rx);
        if app.refresh_due() {
            app.refresh();
        }
    }
    Ok(())
}

impl App {
    fn new(repo: Repo, tx: Sender<Msg>) -> Self {
        App {
            repo: repo.name,
            default_branch: repo.default_branch,
            prs: Vec::new(),
            claude_review_label: None,
            adding_claude_review: HashSet::new(),
            claude_review_updates: HashMap::new(),
            merge_method: MergeMethod::Merge,
            merging: HashSet::new(),
            merged: HashSet::new(),
            worktrees: Vec::new(),
            list: ListState::default(),
            local_list: ListState::default(),
            detail_scroll: 0,
            show_help: false,
            prompt: None,
            branch_prefix: String::new(),
            new_branch: None,
            current_branch: None,
            pulling: false,
            loading_since: None,
            last_success: None,
            last_attempt: None,
            load_error: None,
            flash: None,
            should_quit: false,
            tx,
        }
    }

    pub fn selected(&self) -> Option<Row<'_>> {
        if let Some(i) = self.list.selected() {
            return self.prs.get(i).map(Row::Pr);
        }
        let i = self.local_list.selected()?;
        self.worktrees.get(i).map(Row::Local)
    }

    pub fn selected_pr(&self) -> Option<&PullRequest> {
        self.list.selected().and_then(|i| self.prs.get(i))
    }

    pub fn worktree_for(&self, pr: &PullRequest) -> Option<&str> {
        self.worktrees
            .iter()
            .find(|worktree| worktree.branch == pr.head)
            .map(|worktree| worktree.path.as_str())
    }

    /// Your open PR for the worktree's branch, if there is one.
    pub fn pr_for(&self, worktree: &Worktree) -> Option<&PullRequest> {
        self.prs.iter().find(|pr| pr.head == worktree.branch)
    }

    /// The branch the new branch input would create, if it names one.
    pub fn new_branch_name(&self) -> Option<String> {
        let slug = worktree::slugify(self.new_branch.as_deref()?);
        (!slug.is_empty()).then(|| format!("{}{slug}", self.branch_prefix))
    }

    pub fn can_add_claude_review(&self, pr: &PullRequest) -> bool {
        self.claude_review_label.is_some()
            && !pr.has_claude_review()
            && !self.adding_claude_review.contains(&pr.number)
    }

    fn add_claude_review(&mut self) {
        let Some(pr) = self
            .selected_pr()
            .filter(|pr| self.can_add_claude_review(pr))
        else {
            return;
        };
        let number = pr.number;
        let label = self.claude_review_label.clone().unwrap();
        let repo = self.repo.clone();
        self.adding_claude_review.insert(number);
        self.set_flash(format!("Adding claude-review to #{number}…"), false);
        self.spawn(move || {
            Msg::ClaudeReviewAdded(number, label, github::add_claude_review(&repo, number))
        });
    }

    pub fn can_merge(&self, pr: &PullRequest) -> bool {
        pr.is_ready_to_merge() && !self.merging.contains(&pr.number)
    }

    fn ask_to_merge(&mut self) {
        let Some(pr) = self.selected_pr().filter(|pr| self.can_merge(pr)) else {
            return;
        };
        self.prompt = Some(Prompt::Merge {
            number: pr.number,
            title: pr.title.clone(),
            base: pr.base.clone(),
            head_oid: pr.head_oid.clone(),
            method: self.merge_method,
        });
    }

    pub fn flash(&self) -> Option<&Flash> {
        self.flash
            .as_ref()
            .filter(|flash| flash.at.elapsed() < FLASH_DURATION)
    }

    fn set_flash(&mut self, text: impl Into<String>, is_error: bool) {
        // The footer has room for one line; git's first line says what went wrong.
        let text = text.into().lines().next().unwrap_or_default().to_owned();
        self.flash = Some(Flash {
            text,
            is_error,
            at: Instant::now(),
        });
    }

    fn spawn(&self, job: impl FnOnce() -> Msg + Send + 'static) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            // The receiver only goes away when the app is exiting.
            let _ = tx.send(job());
        });
    }

    fn refresh(&mut self) {
        if self.loading_since.is_some() || !self.adding_claude_review.is_empty() {
            return;
        }
        self.loading_since = Some(Instant::now());
        let repo = self.repo.clone();
        self.spawn(move || {
            let prs = github::fetch_my_prs(&repo);
            // Worktrees are local and optional; failing to list them only
            // hides the worktree marker.
            let worktrees = worktree::list_worktrees().unwrap_or_default();
            Msg::Prs(prs, worktrees)
        });
        self.refresh_branch();
    }

    fn refresh_branch(&self) {
        self.spawn(|| Msg::Branch(github::current_branch()));
    }

    /// Pulling is offered only with the default branch checked out here.
    pub fn can_pull(&self) -> bool {
        !self.pulling && self.current_branch.as_ref() == Some(&self.default_branch)
    }

    /// Brings the default branch up to date with `origin` if that's a
    /// fast-forward, and warns otherwise.
    fn pull_default_branch(&mut self) {
        if !self.can_pull() {
            return;
        }
        let default_branch = self.default_branch.clone();
        self.pulling = true;
        self.set_flash(format!("Pulling {default_branch} from origin…"), false);
        self.spawn(move || Msg::PulledDefaultBranch(github::pull_default_branch(&default_branch)));
    }

    fn refresh_worktrees(&self) {
        self.spawn(|| Msg::Worktrees(worktree::list_worktrees()));
    }

    fn refresh_due(&self) -> bool {
        self.loading_since.is_none()
            && self
                .last_attempt
                .is_some_and(|at| at.elapsed() >= REFRESH_INTERVAL)
    }

    fn drain(&mut self, rx: &Receiver<Msg>) {
        while let Ok(msg) = rx.try_recv() {
            self.on_msg(msg);
        }
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Prs(result, worktrees) => {
                self.loading_since = None;
                self.last_attempt = Some(Instant::now());
                self.set_worktrees(worktrees);
                match result {
                    Ok(mut snapshot) => {
                        let listed: HashSet<u64> =
                            snapshot.prs.iter().map(|pr| pr.number).collect();
                        self.merged.retain(|number| listed.contains(number));
                        snapshot.prs.retain(|pr| !self.merged.contains(&pr.number));
                        for pr in &mut snapshot.prs {
                            if let Some(label) = self.claude_review_updates.get(&pr.number)
                                && !pr.has_claude_review()
                            {
                                pr.labels.push(label.clone());
                            }
                        }
                        self.claude_review_label = snapshot.claude_review_label;
                        self.merge_method = snapshot.merge_method;
                        self.set_prs(snapshot.prs);
                    }
                    Err(err) => {
                        let text = format!("{err:#}");
                        if !self.prs.is_empty() {
                            self.set_flash(format!("Refresh failed: {text}"), true);
                        }
                        self.load_error = Some(text);
                    }
                }
                self.claude_review_updates.clear();
            }
            Msg::ClaudeReviewAdded(number, label, result) => {
                self.adding_claude_review.remove(&number);
                match result {
                    Ok(()) => {
                        if let Some(pr) = self.prs.iter_mut().find(|pr| pr.number == number)
                            && !pr.has_claude_review()
                        {
                            pr.labels.push(label.clone());
                        }
                        if self.loading_since.is_some() {
                            self.claude_review_updates.insert(number, label);
                        }
                        self.set_flash(format!("Added claude-review to #{number}"), false);
                    }
                    Err(err) => self.set_flash(
                        format!("Could not add claude-review to #{number}: {err:#}"),
                        true,
                    ),
                }
            }
            Msg::Merged(number, result) => {
                self.merging.remove(&number);
                match result {
                    Ok(true) => {
                        self.merged.insert(number);
                        let (previous, old_index) =
                            (self.selected_id(), self.selected_index().unwrap_or(0));
                        self.prs.retain(|pr| pr.number != number);
                        self.restore_selection(previous, old_index);
                        self.set_flash(format!("Merged #{number}"), false);
                    }
                    Ok(false) => {
                        self.set_flash(format!("Added #{number} to the merge queue"), false)
                    }
                    Err(err) => self.set_flash(format!("Could not merge #{number}: {err:#}"), true),
                }
                self.refresh();
            }
            Msg::Worktrees(Ok(worktrees)) => self.set_worktrees(worktrees),
            Msg::Worktrees(Err(_)) => {}
            Msg::CheckedOut(number, Ok(Checkout::Done(output))) => {
                let text = match output.is_empty() {
                    true => format!("Checked out #{number}"),
                    false => format!("Checked out #{number}: {output}"),
                };
                self.set_flash(text, false);
                self.refresh_branch();
            }
            Msg::CheckedOut(number, Ok(Checkout::Diverged { branch })) => {
                self.flash = None;
                self.prompt = Some(Prompt::ForceCheckout { number, branch });
                self.refresh_branch();
            }
            Msg::OpenedWorktree(number, Ok(OpenOutcome::Opened(opened))) => {
                self.set_flash(opened.summary(&format!("PR #{number}")), false);
                self.refresh_worktrees();
                // Moving a PR to a worktree may have switched this checkout.
                self.refresh_branch();
            }
            Msg::OpenedBranch(branch, Ok(opened)) => {
                self.set_flash(opened.summary(&branch), false);
                self.refresh_worktrees();
            }
            Msg::OpenedWorktree(number, Ok(OpenOutcome::InMainCheckout { branch, path })) => {
                self.flash = None;
                self.prompt = Some(Prompt::MoveToWorktree {
                    number,
                    branch,
                    main_path: path,
                });
            }
            Msg::RemovedWorktree(subject, Ok(closed_workspace)) => {
                let text = match closed_workspace {
                    true => {
                        format!("Removed the worktree for {subject} and closed its Herdr workspace")
                    }
                    false => format!("Removed the worktree for {subject}"),
                };
                self.set_flash(text, false);
                self.refresh_worktrees();
            }
            Msg::Branch(result) => self.current_branch = result.ok(),
            Msg::PulledDefaultBranch(result) => {
                self.pulling = false;
                let default_branch = &self.default_branch;
                let (text, is_error) = match result {
                    Ok(None) => {
                        // The branch changed outside code-flow.
                        self.refresh_branch();
                        (
                            format!("Not on {default_branch}, so nothing was pulled"),
                            false,
                        )
                    }
                    Ok(Some(0)) => (format!("{default_branch} is up to date"), false),
                    Ok(Some(count)) => {
                        let noun = if count == 1 { "commit" } else { "commits" };
                        // Worktree progress is measured against the default branch.
                        self.refresh_worktrees();
                        (
                            format!("Pulled {count} new {noun} into {default_branch}"),
                            false,
                        )
                    }
                    Err(err) => (format!("Didn't pull {default_branch}: {err:#}"), true),
                };
                self.set_flash(text, is_error);
            }
            Msg::CheckedOut(_, Err(err))
            | Msg::Opened(Err(err))
            | Msg::OpenedWorktree(_, Err(err))
            | Msg::OpenedBranch(_, Err(err))
            | Msg::RemovedWorktree(_, Err(err)) => {
                self.set_flash(format!("{err:#}"), true);
            }
            Msg::Opened(Ok(())) => {}
        }
    }

    /// Replaces the PR list, keeping the same row selected if it's still there.
    fn set_prs(&mut self, prs: Vec<PullRequest>) {
        let (previous, old_index) = (self.selected_id(), self.selected_index().unwrap_or(0));
        self.prs = prs;
        self.load_error = None;
        self.last_success = Some(Instant::now());
        self.restore_selection(previous, old_index);
    }

    fn set_worktrees(&mut self, worktrees: Vec<Worktree>) {
        let (previous, old_index) = (self.selected_id(), self.selected_index().unwrap_or(0));
        self.worktrees = worktrees;
        self.restore_selection(previous, old_index);
    }

    /// Selects `previous` again after the rows changed, or failing that,
    /// whatever is now at its position.
    fn restore_selection(&mut self, previous: Option<RowId>, old_index: usize) {
        let count = self.row_count();
        let index = previous
            .as_ref()
            .and_then(|id| self.index_of(id))
            .or_else(|| (count > 0).then(|| old_index.min(count - 1)));
        self.set_selection(index);
        if self.selected_id() != previous {
            self.detail_scroll = 0;
        }
    }

    /// PR rows come first, then local rows.
    fn row_count(&self) -> usize {
        self.prs.len() + self.worktrees.len()
    }

    fn selected_index(&self) -> Option<usize> {
        self.list
            .selected()
            .or_else(|| self.local_list.selected().map(|i| i + self.prs.len()))
    }

    fn selected_id(&self) -> Option<RowId> {
        Some(match self.selected()? {
            Row::Pr(pr) => RowId::Pr(pr.number),
            Row::Local(worktree) => RowId::Local(worktree.branch.clone()),
        })
    }

    fn index_of(&self, id: &RowId) -> Option<usize> {
        match id {
            RowId::Pr(number) => self.prs.iter().position(|pr| pr.number == *number),
            RowId::Local(branch) => self
                .worktrees
                .iter()
                .position(|worktree| &worktree.branch == branch)
                .map(|i| i + self.prs.len()),
        }
    }

    fn set_selection(&mut self, index: Option<usize>) {
        let prs = self.prs.len();
        self.list.select(index.filter(|&i| i < prs));
        self.local_list
            .select(index.filter(|&i| i >= prs).map(|i| i - prs));
    }

    fn select(&mut self, index: usize) {
        let count = self.row_count();
        if count == 0 {
            return;
        }
        let index = index.min(count - 1);
        if self.selected_index() != Some(index) {
            self.set_selection(Some(index));
            self.detail_scroll = 0;
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }
        if let Some(prompt) = self.prompt.take() {
            self.on_prompt_key(key, prompt);
            return;
        }
        if let Some(text) = self.new_branch.take() {
            self.on_new_branch_key(key, text);
            return;
        }
        if self.show_help {
            self.show_help = false;
            return;
        }
        let current = self.selected_index().unwrap_or(0);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.select(current + 1),
            KeyCode::Char('k') | KeyCode::Up => self.select(current.saturating_sub(1)),
            KeyCode::Char('g') | KeyCode::Home => self.select(0),
            KeyCode::Char('G') | KeyCode::End => self.select(usize::MAX),
            KeyCode::Char('J') => self.detail_scroll = self.detail_scroll.saturating_add(3),
            KeyCode::Char('K') => self.detail_scroll = self.detail_scroll.saturating_sub(3),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('l') => self.add_claude_review(),
            KeyCode::Char('m') => self.ask_to_merge(),
            KeyCode::Char('o') | KeyCode::Enter => {
                if let Some(pr) = self.selected_pr() {
                    let (repo, number) = (self.repo.clone(), pr.number);
                    self.spawn(move || Msg::Opened(github::open_in_browser(&repo, number)));
                }
            }
            KeyCode::Char('c') => {
                if let Some(pr) = self.selected_pr() {
                    let (repo, number) = (self.repo.clone(), pr.number);
                    self.set_flash(format!("Checking out #{number}…"), false);
                    self.spawn(move || Msg::CheckedOut(number, github::checkout(&repo, number)));
                }
            }
            KeyCode::Char('n') => self.new_branch = Some(String::new()),
            KeyCode::Char('p') => self.pull_default_branch(),
            KeyCode::Char('w') => match self.selected() {
                Some(Row::Pr(pr)) => {
                    let (number, branch) = (pr.number, pr.head.clone());
                    self.set_flash(format!("Opening PR #{number} in a worktree…"), false);
                    self.spawn(move || {
                        Msg::OpenedWorktree(number, worktree::open_pr(number, &branch))
                    });
                }
                Some(Row::Local(local)) => {
                    let (branch, path) = (local.branch.clone(), local.path.clone());
                    self.set_flash(format!("Opening {branch}…"), false);
                    self.spawn(move || {
                        let result = worktree::open_branch(&branch, &path);
                        Msg::OpenedBranch(branch, result)
                    });
                }
                None => {}
            },
            KeyCode::Char('W') => match self.selected() {
                Some(Row::Pr(pr)) => {
                    let (number, branch) = (pr.number, pr.head.clone());
                    match self.worktree_for(pr).map(str::to_owned) {
                        Some(path) => {
                            self.prompt = Some(Prompt::RemoveWorktree {
                                number,
                                branch,
                                path,
                            })
                        }
                        None => self.set_flash(format!("PR #{number} has no worktree"), false),
                    }
                }
                Some(Row::Local(local)) => {
                    self.prompt = Some(Prompt::RemoveLocal {
                        branch: local.branch.clone(),
                        path: local.path.clone(),
                        progress: local.status.and_then(|status| status.progress),
                    })
                }
                None => {}
            },
            _ => {}
        }
    }

    fn on_new_branch_key(&mut self, key: KeyEvent, mut text: String) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => {
                let slug = worktree::slugify(&text);
                if !slug.is_empty() {
                    self.create_branch(format!("{}{slug}", self.branch_prefix));
                    return;
                }
            }
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                text.push(c)
            }
            _ => {}
        }
        self.new_branch = Some(text);
    }

    fn create_branch(&mut self, branch: String) {
        let default_branch = self.default_branch.clone();
        self.set_flash(
            format!("Creating {branch} from origin/{default_branch}…"),
            false,
        );
        self.spawn(move || {
            let result = worktree::create(&branch, &default_branch);
            Msg::OpenedBranch(branch, result)
        });
    }

    fn on_prompt_key(&mut self, key: KeyEvent, prompt: Prompt) {
        match key.code {
            KeyCode::Char('y') => self.accept(prompt),
            KeyCode::Char('n') | KeyCode::Char('q') | KeyCode::Esc => {
                let text = match prompt {
                    Prompt::ForceCheckout { branch, .. } => format!("Kept local branch {branch}"),
                    Prompt::MoveToWorktree { number, .. } => {
                        format!("Left PR #{number} in the main checkout")
                    }
                    Prompt::RemoveWorktree { number, .. } => {
                        format!("Kept the worktree for PR #{number}")
                    }
                    Prompt::RemoveLocal { branch, .. } => {
                        format!("Kept the worktree for {branch}")
                    }
                    Prompt::Merge { number, .. } => format!("Left #{number} unmerged"),
                };
                self.set_flash(text, false);
            }
            _ => self.prompt = Some(prompt),
        }
    }

    fn accept(&mut self, prompt: Prompt) {
        match prompt {
            Prompt::ForceCheckout { number, branch } => {
                let repo = self.repo.clone();
                self.set_flash(format!("Resetting {branch} to PR #{number}…"), false);
                self.spawn(move || {
                    let result = github::force_checkout(&repo, number)
                        .map(|()| Checkout::Done(format!("reset {branch} to match the PR")));
                    Msg::CheckedOut(number, result)
                });
            }
            Prompt::MoveToWorktree {
                number,
                branch,
                main_path,
            } => {
                let default_branch = self.default_branch.clone();
                self.set_flash(
                    format!("Switching the main checkout to {default_branch}…"),
                    false,
                );
                self.spawn(move || {
                    let result =
                        worktree::move_to_worktree(number, &branch, &main_path, &default_branch);
                    Msg::OpenedWorktree(number, result)
                });
            }
            Prompt::RemoveWorktree {
                number,
                branch,
                path,
            } => {
                self.set_flash(format!("Removing the worktree for PR #{number}…"), false);
                self.spawn(move || {
                    Msg::RemovedWorktree(format!("PR #{number}"), worktree::remove(&branch, &path))
                });
            }
            Prompt::RemoveLocal { branch, path, .. } => {
                self.set_flash(format!("Removing the worktree for {branch}…"), false);
                self.spawn(move || {
                    let result = worktree::remove(&branch, &path);
                    Msg::RemovedWorktree(branch, result)
                });
            }
            Prompt::Merge {
                number,
                head_oid,
                method,
                ..
            } => {
                let repo = self.repo.clone();
                self.merging.insert(number);
                self.set_flash(format!("Merging #{number}…"), false);
                self.spawn(move || {
                    Msg::Merged(number, github::merge(&repo, number, method, &head_oid))
                });
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{AutoMerge, Ci, CiState, Label, Merge, Review, WorktreeStatus};

    pub fn sample_pr(number: u64, title: &str) -> PullRequest {
        PullRequest {
            number,
            title: title.into(),
            body: "Body text".into(),
            head: "feature".into(),
            head_oid: "abc123".into(),
            base: "main".into(),
            is_draft: false,
            review: Review::Approved,
            ci: Ci {
                state: CiState::Running,
                total: 7,
                pending: 4,
                failed: 0,
            },
            merge: Merge::Conflicts,
            auto_merge: AutoMerge::Enabled,
            unresolved_threads: 12,
            labels: vec![Label {
                name: "bug".into(),
                rgb: (0xd7, 0x3a, 0x4a),
            }],
        }
    }

    /// A PR that GitHub would merge now.
    pub fn ready_pr(number: u64, title: &str) -> PullRequest {
        let mut pr = sample_pr(number, title);
        pr.ci.state = CiState::Passed;
        pr.ci.pending = 0;
        pr.merge = Merge::Ready;
        pr.auto_merge = AutoMerge::Off;
        pr
    }

    fn snapshot(prs: Vec<PullRequest>) -> Msg {
        Msg::Prs(
            Ok(PrSnapshot {
                prs,
                claude_review_label: None,
                merge_method: MergeMethod::Squash,
            }),
            Vec::new(),
        )
    }

    pub fn sample_worktree(branch: &str, progress: Progress) -> Worktree {
        Worktree {
            branch: branch.into(),
            path: format!("/src/repo.{}", branch.replace('/', "-")),
            subject: Some("Last commit".into()),
            committed_at: None,
            status: Some(WorktreeStatus {
                uncommitted: false,
                progress: Some(progress),
            }),
        }
    }

    fn unloaded_app() -> App {
        let (tx, _rx) = mpsc::channel();
        let repo = Repo {
            name: "o/r".into(),
            default_branch: "main".into(),
        };
        let mut app = App::new(repo, tx);
        app.branch_prefix = "me/".into();
        app
    }

    pub fn app_with(prs: Vec<PullRequest>) -> App {
        let mut app = unloaded_app();
        app.set_prs(prs);
        app
    }

    fn press(app: &mut App, c: char) {
        app.on_key(KeyEvent::from(KeyCode::Char(c)));
    }

    fn press_key(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::from(code));
    }

    fn selected_branch(app: &App) -> Option<&str> {
        match app.selected()? {
            Row::Pr(_) => None,
            Row::Local(worktree) => Some(&worktree.branch),
        }
    }

    pub fn claude_label() -> Label {
        Label {
            name: "claude-review".into(),
            rgb: (170, 187, 204),
        }
    }

    #[test]
    fn claude_shortcut_is_disabled_when_unavailable_already_labeled_or_pending() {
        let mut app = app_with(vec![sample_pr(1, "a")]);
        press(&mut app, 'l');
        assert!(app.adding_claude_review.is_empty());
        assert!(app.flash().is_none());

        app.claude_review_label = Some(claude_label());
        assert!(app.can_add_claude_review(&app.prs[0]));
        app.adding_claude_review.insert(1);
        assert!(!app.can_add_claude_review(&app.prs[0]));
        press(&mut app, 'l');
        assert!(app.flash().is_none());

        app.adding_claude_review.clear();
        app.prs[0].labels.push(claude_label());
        assert!(!app.can_add_claude_review(&app.prs[0]));
        press(&mut app, 'l');
        assert!(app.adding_claude_review.is_empty());
        assert!(app.flash().is_none());

        app.set_prs(vec![]);
        press(&mut app, 'l');
        assert!(app.adding_claude_review.is_empty());
    }

    #[test]
    fn claude_success_updates_target_even_after_selection_changes() {
        let mut app = app_with(vec![sample_pr(1, "a"), sample_pr(2, "b")]);
        app.claude_review_label = Some(claude_label());
        app.adding_claude_review.insert(1);
        app.select(1);
        app.on_msg(Msg::ClaudeReviewAdded(1, claude_label(), Ok(())));
        assert!(app.prs[0].has_claude_review());
        assert!(!app.prs[1].has_claude_review());
        assert!(app.adding_claude_review.is_empty());
        assert_eq!(app.selected_pr().unwrap().number, 2);
        assert!(!app.flash().unwrap().is_error);
        // A refresh that already included the label must not produce duplicates.
        app.on_msg(Msg::ClaudeReviewAdded(1, claude_label(), Ok(())));
        assert_eq!(app.prs[0].labels.len(), 2);
    }

    #[test]
    fn claude_failure_preserves_labels_and_allows_retry() {
        let mut app = app_with(vec![sample_pr(1, "a")]);
        app.claude_review_label = Some(claude_label());
        app.adding_claude_review.insert(1);
        let labels = app.prs[0].labels.clone();
        app.on_msg(Msg::ClaudeReviewAdded(
            1,
            claude_label(),
            Err(anyhow::anyhow!("permission denied")),
        ));
        assert_eq!(app.prs[0].labels, labels);
        assert!(app.can_add_claude_review(&app.prs[0]));
        assert!(app.flash().unwrap().is_error);
        assert!(app.flash().unwrap().text.contains("permission denied"));
    }

    #[test]
    fn refresh_does_not_undo_a_label_added_while_fetching() {
        let mut app = app_with(vec![sample_pr(1, "a")]);
        app.loading_since = Some(Instant::now());
        app.on_msg(Msg::ClaudeReviewAdded(1, claude_label(), Ok(())));
        app.on_msg(Msg::Prs(
            Ok(PrSnapshot {
                prs: vec![sample_pr(1, "a")],
                claude_review_label: Some(claude_label()),
                merge_method: MergeMethod::Merge,
            }),
            Vec::new(),
        ));
        assert!(app.prs[0].has_claude_review());
        assert!(app.claude_review_updates.is_empty());

        // Later refreshes are authoritative, including external removal.
        app.on_msg(Msg::Prs(
            Ok(PrSnapshot {
                prs: vec![sample_pr(1, "a")],
                claude_review_label: None,
                merge_method: MergeMethod::Merge,
            }),
            Vec::new(),
        ));
        assert!(!app.prs[0].has_claude_review());
        assert!(app.claude_review_label.is_none());
        assert!(!app.can_add_claude_review(&app.prs[0]));
    }

    #[test]
    fn merge_shortcut_asks_first_and_only_for_ready_prs() {
        let mut app = app_with(vec![sample_pr(1, "a"), ready_pr(2, "b")]);
        app.on_msg(snapshot(vec![sample_pr(1, "a"), ready_pr(2, "b")]));
        press(&mut app, 'm');
        assert!(app.prompt.is_none());

        press(&mut app, 'j');
        press(&mut app, 'm');
        assert!(matches!(
            &app.prompt,
            Some(Prompt::Merge { number: 2, head_oid, method: MergeMethod::Squash, .. })
                if head_oid == "abc123"
        ));
        press(&mut app, 'n');
        assert!(app.prompt.is_none());
        assert!(app.merging.is_empty());
        assert_eq!(app.flash().unwrap().text, "Left #2 unmerged");

        // No second merge while one is running.
        app.merging.insert(2);
        assert!(!app.can_merge(&app.prs[1]));
        press(&mut app, 'm');
        assert!(app.prompt.is_none());
    }

    #[test]
    fn merged_pr_leaves_the_list_even_if_search_still_lists_it() {
        let mut app = app_with(vec![ready_pr(1, "a"), ready_pr(2, "b"), ready_pr(3, "c")]);
        app.select(1);
        app.merging.insert(2);
        // Keep the refresh the merge triggers from spawning `gh`.
        app.loading_since = Some(Instant::now());
        app.on_msg(Msg::Merged(2, Ok(true)));
        assert!(app.merging.is_empty());
        assert_eq!(
            app.prs.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            [1, 3]
        );
        assert_eq!(app.selected_pr().unwrap().number, 3);
        assert_eq!(app.flash().unwrap().text, "Merged #2");

        app.on_msg(snapshot(vec![ready_pr(1, "a"), ready_pr(2, "b")]));
        assert_eq!(app.prs.iter().map(|pr| pr.number).collect::<Vec<_>>(), [1]);
        assert_eq!(app.merged, HashSet::from([2]));
        // Once search catches up, the number is forgotten.
        app.on_msg(snapshot(vec![ready_pr(1, "a")]));
        assert!(app.merged.is_empty());
    }

    #[test]
    fn queued_or_failed_merge_keeps_the_pr() {
        let mut app = app_with(vec![ready_pr(1, "a")]);
        app.loading_since = Some(Instant::now());
        app.merging.insert(1);
        app.on_msg(Msg::Merged(1, Ok(false)));
        assert_eq!(app.prs.len(), 1);
        assert_eq!(app.flash().unwrap().text, "Added #1 to the merge queue");

        app.merging.insert(1);
        app.on_msg(Msg::Merged(
            1,
            Err(anyhow::anyhow!("Head branch was modified")),
        ));
        assert_eq!(app.prs.len(), 1);
        assert!(app.flash().unwrap().is_error);
        assert!(app.flash().unwrap().text.contains("was modified"));
        assert!(app.can_merge(&app.prs[0]));
    }

    #[test]
    fn refresh_keeps_selected_pr_by_number() {
        let mut app = app_with(vec![
            sample_pr(1, "a"),
            sample_pr(2, "b"),
            sample_pr(3, "c"),
        ]);
        app.select(1);
        app.detail_scroll = 5;
        app.set_prs(vec![
            sample_pr(9, "new"),
            sample_pr(1, "a"),
            sample_pr(2, "b"),
        ]);
        assert_eq!(app.selected_pr().unwrap().number, 2);
        assert_eq!(app.detail_scroll, 5);
    }

    #[test]
    fn refresh_clamps_selection_when_selected_pr_closes() {
        let mut app = app_with(vec![
            sample_pr(1, "a"),
            sample_pr(2, "b"),
            sample_pr(3, "c"),
        ]);
        app.select(2);
        app.set_prs(vec![sample_pr(1, "a")]);
        assert_eq!(app.selected_pr().unwrap().number, 1);
        app.set_prs(vec![]);
        assert!(app.selected_pr().is_none());
    }

    #[test]
    fn navigation_is_bounded() {
        let mut app = app_with(vec![sample_pr(1, "a"), sample_pr(2, "b")]);
        press(&mut app, 'k');
        assert_eq!(app.list.selected(), Some(0));
        press(&mut app, 'G');
        press(&mut app, 'j');
        assert_eq!(app.list.selected(), Some(1));
        press(&mut app, 'g');
        assert_eq!(app.list.selected(), Some(0));
    }

    // Prompt tests only decline: accepting spawns real git, gh and wt commands.

    #[test]
    fn diverged_checkout_prompts_until_answered() {
        let mut app = app_with(vec![sample_pr(1, "a"), sample_pr(2, "b")]);
        let diverged = Checkout::Diverged {
            branch: "feature".into(),
        };
        app.on_msg(Msg::CheckedOut(2, Ok(diverged)));
        assert!(matches!(
            app.prompt,
            Some(Prompt::ForceCheckout { number: 2, .. })
        ));

        // Other keys are swallowed while the prompt is open.
        press(&mut app, 'j');
        assert_eq!(app.list.selected(), Some(0));
        assert!(app.prompt.is_some());

        press(&mut app, 'n');
        assert!(app.prompt.is_none());
        assert!(!app.should_quit);
        assert_eq!(app.flash().unwrap().text, "Kept local branch feature");
    }

    #[test]
    fn branch_in_main_checkout_prompts_to_move_it() {
        let mut app = app_with(vec![sample_pr(3, "a")]);
        let outcome = OpenOutcome::InMainCheckout {
            branch: "feature".into(),
            path: "/src/repo".into(),
        };
        app.on_msg(Msg::OpenedWorktree(3, Ok(outcome)));
        assert!(matches!(
            &app.prompt,
            Some(Prompt::MoveToWorktree { number: 3, main_path, .. }) if main_path == "/src/repo"
        ));
        press(&mut app, 'n');
        assert!(app.prompt.is_none());
        assert_eq!(app.flash().unwrap().text, "Left PR #3 in the main checkout");
    }

    #[test]
    fn remove_worktree_needs_a_worktree_and_confirmation() {
        let mut app = app_with(vec![sample_pr(3, "a")]);
        press(&mut app, 'W');
        assert!(app.prompt.is_none());
        assert_eq!(app.flash().unwrap().text, "PR #3 has no worktree");

        app.worktrees
            .push(sample_worktree("feature", Progress::Ahead(1)));
        press(&mut app, 'W');
        assert!(matches!(
            &app.prompt,
            Some(Prompt::RemoveWorktree { number: 3, path, .. }) if path == "/src/repo.feature"
        ));
        press(&mut app, 'n');
        assert!(app.prompt.is_none());
        assert_eq!(app.flash().unwrap().text, "Kept the worktree for PR #3");
    }

    #[test]
    fn refresh_updates_worktrees() {
        let mut app = app_with(vec![sample_pr(3, "a")]);
        let worktrees = vec![sample_worktree("feature", Progress::Ahead(1))];
        app.on_msg(Msg::Prs(
            Ok(PrSnapshot {
                prs: vec![sample_pr(3, "a")],
                claude_review_label: None,
                merge_method: MergeMethod::Merge,
            }),
            worktrees,
        ));
        assert_eq!(app.worktree_for(&app.prs[0]), Some("/src/repo.feature"));
    }

    #[test]
    fn local_rows_include_worktrees_of_your_prs() {
        let mut app = unloaded_app();
        app.set_worktrees(vec![
            sample_worktree("feature", Progress::Ahead(1)),
            sample_worktree("me/a", Progress::Empty),
        ]);
        // Local rows don't wait for PRs to load.
        assert_eq!(selected_branch(&app), Some("feature"));
        assert!(app.pr_for(&app.worktrees[0]).is_none());

        app.set_prs(vec![sample_pr(3, "a")]);
        assert_eq!(app.pr_for(&app.worktrees[0]).unwrap().number, 3);
        assert!(app.pr_for(&app.worktrees[1]).is_none());
    }

    #[test]
    fn selection_moves_from_prs_into_local_rows() {
        let mut app = app_with(vec![sample_pr(1, "a")]);
        app.set_worktrees(vec![
            sample_worktree("feature", Progress::Ahead(1)),
            sample_worktree("me/a", Progress::Empty),
            sample_worktree("me/b", Progress::Merged),
        ]);

        assert_eq!(app.selected_pr().unwrap().number, 1);
        press(&mut app, 'j');
        assert_eq!(selected_branch(&app), Some("feature"));
        assert!(app.selected_pr().is_none());
        assert_eq!(app.list.selected(), None);
        press(&mut app, 'j');
        assert_eq!(selected_branch(&app), Some("me/a"));
        press(&mut app, 'G');
        assert_eq!(selected_branch(&app), Some("me/b"));
        press(&mut app, 'j');
        assert_eq!(selected_branch(&app), Some("me/b"));
        press(&mut app, 'g');
        assert_eq!(app.selected_pr().unwrap().number, 1);
        assert_eq!(app.local_list.selected(), None);
    }

    #[test]
    fn refresh_keeps_selected_local_row_by_branch() {
        let mut app = app_with(vec![sample_pr(1, "a")]);
        app.set_worktrees(vec![
            sample_worktree("me/a", Progress::Empty),
            sample_worktree("me/b", Progress::Empty),
        ]);
        press(&mut app, 'G');
        app.set_worktrees(vec![
            sample_worktree("me/new", Progress::Empty),
            sample_worktree("me/b", Progress::Empty),
        ]);
        assert_eq!(selected_branch(&app), Some("me/b"));

        // A new PR row above doesn't move the selection off its branch.
        app.set_prs(vec![sample_pr(1, "a"), sample_pr(2, "b")]);
        assert_eq!(selected_branch(&app), Some("me/b"));
        assert_eq!(app.local_list.selected(), Some(1));

        // When the branch goes away, the same position is selected.
        app.set_worktrees(vec![sample_worktree("me/new", Progress::Empty)]);
        assert_eq!(selected_branch(&app), Some("me/new"));
    }

    #[test]
    fn pr_only_keys_do_nothing_on_local_rows() {
        let mut app = app_with(vec![]);
        app.claude_review_label = Some(claude_label());
        app.set_worktrees(vec![sample_worktree("me/a", Progress::Empty)]);
        for key in ['o', 'c', 'l', 'm'] {
            press(&mut app, key);
        }
        press_key(&mut app, KeyCode::Enter);
        assert!(app.flash().is_none());
        assert!(app.prompt.is_none());
    }

    #[test]
    fn remove_local_worktree_needs_confirmation() {
        let mut app = app_with(vec![]);
        app.set_worktrees(vec![sample_worktree("me/a", Progress::Ahead(2))]);
        press(&mut app, 'W');
        assert!(matches!(
            &app.prompt,
            Some(Prompt::RemoveLocal { branch, path, progress: Some(Progress::Ahead(2)) })
                if branch == "me/a" && path == "/src/repo.me-a"
        ));
        press(&mut app, 'n');
        assert!(app.prompt.is_none());
        assert_eq!(app.flash().unwrap().text, "Kept the worktree for me/a");
    }

    #[test]
    fn new_branch_input_previews_and_cancels() {
        let mut app = app_with(vec![sample_pr(1, "a"), sample_pr(2, "b")]);
        press(&mut app, 'n');
        assert_eq!(app.new_branch.as_deref(), Some(""));
        assert_eq!(app.new_branch_name(), None);

        // Enter does nothing until the text names a branch.
        press_key(&mut app, KeyCode::Enter);
        assert!(app.new_branch.is_some());
        assert!(app.flash().is_none());

        // Keys that normally act are typed instead.
        for c in "Fix Bob's jq!".chars() {
            press(&mut app, c);
        }
        assert_eq!(app.new_branch_name().as_deref(), Some("me/fix-bobs-jq"));
        assert!(!app.should_quit);
        assert_eq!(app.list.selected(), Some(0));
        press_key(&mut app, KeyCode::Backspace);
        press_key(&mut app, KeyCode::Backspace);
        assert_eq!(app.new_branch_name().as_deref(), Some("me/fix-bobs-j"));

        press_key(&mut app, KeyCode::Esc);
        assert!(app.new_branch.is_none());
        assert!(!app.should_quit);
        assert!(app.flash().is_none());
    }

    #[test]
    fn opened_branch_reports_errors() {
        let mut app = app_with(vec![]);
        app.on_msg(Msg::OpenedBranch(
            "me/a".into(),
            Err(anyhow::anyhow!(
                "`wt switch` failed: Branch me/a already exists"
            )),
        ));
        assert!(app.flash().unwrap().is_error);
        assert!(app.flash().unwrap().text.contains("already exists"));
    }

    // Pull tests never press `p` on the default branch: that would fetch and
    // merge in whatever checkout the tests run from.

    #[test]
    fn pull_is_offered_only_on_the_default_branch() {
        let mut app = app_with(vec![]);
        assert!(!app.can_pull());
        press(&mut app, 'p');
        assert!(!app.pulling);
        assert!(app.flash().is_none());

        app.on_msg(Msg::Branch(Ok("me/feature".into())));
        assert!(!app.can_pull());
        press(&mut app, 'p');
        assert!(app.flash().is_none());

        app.on_msg(Msg::Branch(Ok("main".into())));
        assert!(app.can_pull());
        // Not while a pull is already running.
        app.pulling = true;
        assert!(!app.can_pull());
        press(&mut app, 'p');
        assert!(app.flash().is_none());

        app.on_msg(Msg::Branch(Err(anyhow::anyhow!("not a git repository"))));
        assert_eq!(app.current_branch, None);
    }

    #[test]
    fn reports_pulling_the_default_branch() {
        let mut app = app_with(vec![]);
        app.pulling = true;
        app.on_msg(Msg::PulledDefaultBranch(Ok(Some(0))));
        assert!(!app.pulling);
        assert_eq!(app.flash().unwrap().text, "main is up to date");

        app.on_msg(Msg::PulledDefaultBranch(Ok(Some(1))));
        assert_eq!(app.flash().unwrap().text, "Pulled 1 new commit into main");
        app.on_msg(Msg::PulledDefaultBranch(Ok(Some(3))));
        assert_eq!(app.flash().unwrap().text, "Pulled 3 new commits into main");
        assert!(!app.flash().unwrap().is_error);
    }

    #[test]
    fn warns_when_the_default_branch_cant_fast_forward() {
        let mut app = app_with(vec![]);
        app.pulling = true;
        app.on_msg(Msg::PulledDefaultBranch(Err(anyhow::anyhow!(
            "your main has commits origin/main doesn't, so it can't fast-forward"
        ))));
        assert!(!app.pulling);
        let flash = app.flash().unwrap();
        assert!(flash.is_error);
        assert_eq!(
            flash.text,
            "Didn't pull main: your main has commits origin/main doesn't, so it can't fast-forward"
        );
    }

    #[test]
    fn flash_keeps_first_line_only() {
        let mut app = app_with(vec![]);
        app.set_flash(
            "error: local changes would be overwritten\n\tREADME.md",
            true,
        );
        assert_eq!(
            app.flash().unwrap().text,
            "error: local changes would be overwritten"
        );
    }
}
