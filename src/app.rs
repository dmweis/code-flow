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
use crate::model::{Label, PullRequest};
use crate::ui;
use crate::worktree::{self, OpenOutcome};

const TICK: Duration = Duration::from_millis(200);
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const FLASH_DURATION: Duration = Duration::from_secs(5);

/// Results of background work, sent back to the event loop.
enum Msg {
    Prs(Result<PrSnapshot>, HashMap<String, String>),
    ClaudeReviewAdded(u64, Label, Result<()>),
    Worktrees(Result<HashMap<String, String>>),
    CheckedOut(u64, Result<Checkout>),
    Opened(Result<()>),
    OpenedWorktree(u64, Result<OpenOutcome>),
    RemovedWorktree(u64, Result<bool>),
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
}

pub struct App {
    pub repo: String,
    pub default_branch: String,
    pub prs: Vec<PullRequest>,
    pub claude_review_label: Option<Label>,
    pub adding_claude_review: HashSet<u64>,
    /// Successful edits that an already-running refresh may not have seen.
    claude_review_updates: HashMap<u64, Label>,
    /// Linked worktree paths by branch name.
    pub worktrees: HashMap<String, String>,
    pub list: ListState,
    pub detail_scroll: u16,
    pub show_help: bool,
    pub prompt: Option<Prompt>,
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
            worktrees: HashMap::new(),
            list: ListState::default(),
            detail_scroll: 0,
            show_help: false,
            prompt: None,
            loading_since: None,
            last_success: None,
            last_attempt: None,
            load_error: None,
            flash: None,
            should_quit: false,
            tx,
        }
    }

    pub fn selected_pr(&self) -> Option<&PullRequest> {
        self.list.selected().and_then(|i| self.prs.get(i))
    }

    pub fn worktree_for(&self, pr: &PullRequest) -> Option<&str> {
        self.worktrees.get(&pr.head).map(String::as_str)
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
                self.worktrees = worktrees;
                match result {
                    Ok(mut snapshot) => {
                        for pr in &mut snapshot.prs {
                            if let Some(label) = self.claude_review_updates.get(&pr.number)
                                && !pr.has_claude_review()
                            {
                                pr.labels.push(label.clone());
                            }
                        }
                        self.claude_review_label = snapshot.claude_review_label;
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
            Msg::Worktrees(Ok(worktrees)) => self.worktrees = worktrees,
            Msg::Worktrees(Err(_)) => {}
            Msg::CheckedOut(number, Ok(Checkout::Done(output))) => {
                let text = match output.is_empty() {
                    true => format!("Checked out #{number}"),
                    false => format!("Checked out #{number}: {output}"),
                };
                self.set_flash(text, false);
            }
            Msg::CheckedOut(number, Ok(Checkout::Diverged { branch })) => {
                self.flash = None;
                self.prompt = Some(Prompt::ForceCheckout { number, branch });
            }
            Msg::OpenedWorktree(number, Ok(OpenOutcome::Opened(opened))) => {
                self.set_flash(opened.summary(number), false);
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
            Msg::RemovedWorktree(number, Ok(closed_workspace)) => {
                let text = match closed_workspace {
                    true => format!(
                        "Removed the worktree for PR #{number} and closed its Herdr workspace"
                    ),
                    false => format!("Removed the worktree for PR #{number}"),
                };
                self.set_flash(text, false);
                self.refresh_worktrees();
            }
            Msg::CheckedOut(_, Err(err))
            | Msg::Opened(Err(err))
            | Msg::OpenedWorktree(_, Err(err))
            | Msg::RemovedWorktree(_, Err(err)) => {
                self.set_flash(format!("{err:#}"), true);
            }
            Msg::Opened(Ok(())) => {}
        }
    }

    /// Replaces the PR list, keeping the same PR selected if it's still open.
    fn set_prs(&mut self, prs: Vec<PullRequest>) {
        let previous = self.selected_pr().map(|pr| pr.number);
        let old_index = self.list.selected().unwrap_or(0);
        self.prs = prs;
        self.load_error = None;
        self.last_success = Some(Instant::now());
        let index = previous
            .and_then(|number| self.prs.iter().position(|pr| pr.number == number))
            .or_else(|| (!self.prs.is_empty()).then(|| old_index.min(self.prs.len() - 1)));
        self.list.select(index);
        if self.selected_pr().map(|pr| pr.number) != previous {
            self.detail_scroll = 0;
        }
    }

    fn select(&mut self, index: usize) {
        if self.prs.is_empty() {
            return;
        }
        let index = index.min(self.prs.len() - 1);
        if self.list.selected() != Some(index) {
            self.list.select(Some(index));
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
        if self.show_help {
            self.show_help = false;
            return;
        }
        let current = self.list.selected().unwrap_or(0);
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
            KeyCode::Char('w') => {
                if let Some(pr) = self.selected_pr() {
                    let (number, branch) = (pr.number, pr.head.clone());
                    self.set_flash(format!("Opening PR #{number} in a worktree…"), false);
                    self.spawn(move || {
                        Msg::OpenedWorktree(number, worktree::open_pr(number, &branch))
                    });
                }
            }
            KeyCode::Char('W') => {
                if let Some(pr) = self.selected_pr() {
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
            }
            _ => {}
        }
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
                self.spawn(move || Msg::RemovedWorktree(number, worktree::remove(&branch, &path)));
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{AutoMerge, Ci, CiState, Label, Merge, Review};

    pub fn sample_pr(number: u64, title: &str) -> PullRequest {
        PullRequest {
            number,
            title: title.into(),
            body: "Body text".into(),
            head: "feature".into(),
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

    pub fn app_with(prs: Vec<PullRequest>) -> App {
        let (tx, _rx) = mpsc::channel();
        let repo = Repo {
            name: "o/r".into(),
            default_branch: "main".into(),
        };
        let mut app = App::new(repo, tx);
        app.set_prs(prs);
        app
    }

    fn press(app: &mut App, c: char) {
        app.on_key(KeyEvent::from(KeyCode::Char(c)));
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
            }),
            HashMap::new(),
        ));
        assert!(app.prs[0].has_claude_review());
        assert!(app.claude_review_updates.is_empty());

        // Later refreshes are authoritative, including external removal.
        app.on_msg(Msg::Prs(
            Ok(PrSnapshot {
                prs: vec![sample_pr(1, "a")],
                claude_review_label: None,
            }),
            HashMap::new(),
        ));
        assert!(!app.prs[0].has_claude_review());
        assert!(app.claude_review_label.is_none());
        assert!(!app.can_add_claude_review(&app.prs[0]));
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
            .insert("feature".into(), "/src/repo.feature".into());
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
        let worktrees = HashMap::from([("feature".to_owned(), "/src/repo.feature".to_owned())]);
        app.on_msg(Msg::Prs(
            Ok(PrSnapshot {
                prs: vec![sample_pr(3, "a")],
                claude_review_label: None,
            }),
            worktrees,
        ));
        assert_eq!(app.worktree_for(&app.prs[0]), Some("/src/repo.feature"));
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
