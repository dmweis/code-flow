//! Application state, input handling, and the main event loop.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;

use crate::github::{self, Checkout};
use crate::model::PullRequest;
use crate::ui;
use crate::worktree::{self, OpenedWorktree};

const TICK: Duration = Duration::from_millis(200);
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const FLASH_DURATION: Duration = Duration::from_secs(5);

/// Results of background work, sent back to the event loop.
enum Msg {
    Prs(Result<Vec<PullRequest>>),
    CheckedOut(u64, Result<Checkout>),
    Opened(Result<()>),
    OpenedWorktree(u64, Result<OpenedWorktree>),
}

pub struct Flash {
    pub text: String,
    pub is_error: bool,
    at: Instant,
}

/// Asks whether to reset a local branch that has diverged from its PR.
pub struct ForceCheckoutPrompt {
    pub number: u64,
    pub branch: String,
}

pub struct App {
    pub repo: String,
    pub prs: Vec<PullRequest>,
    pub list: ListState,
    pub detail_scroll: u16,
    pub show_help: bool,
    pub force_checkout_prompt: Option<ForceCheckoutPrompt>,
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

pub fn run(terminal: &mut DefaultTerminal, repo: String) -> Result<()> {
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
    fn new(repo: String, tx: Sender<Msg>) -> Self {
        App {
            repo,
            prs: Vec::new(),
            list: ListState::default(),
            detail_scroll: 0,
            show_help: false,
            force_checkout_prompt: None,
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
        if self.loading_since.is_some() {
            return;
        }
        self.loading_since = Some(Instant::now());
        let repo = self.repo.clone();
        self.spawn(move || Msg::Prs(github::fetch_my_prs(&repo)));
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
            Msg::Prs(result) => {
                self.loading_since = None;
                self.last_attempt = Some(Instant::now());
                match result {
                    Ok(prs) => self.set_prs(prs),
                    Err(err) => {
                        let text = format!("{err:#}");
                        if !self.prs.is_empty() {
                            self.set_flash(format!("Refresh failed: {text}"), true);
                        }
                        self.load_error = Some(text);
                    }
                }
            }
            Msg::CheckedOut(number, Ok(Checkout::Done(output))) => {
                let text = match output.is_empty() {
                    true => format!("Checked out #{number}"),
                    false => format!("Checked out #{number}: {output}"),
                };
                self.set_flash(text, false);
            }
            Msg::CheckedOut(number, Ok(Checkout::Diverged { branch })) => {
                self.flash = None;
                self.force_checkout_prompt = Some(ForceCheckoutPrompt { number, branch });
            }
            Msg::OpenedWorktree(number, Ok(opened)) => {
                self.set_flash(opened.summary(number), false);
            }
            Msg::CheckedOut(_, Err(err))
            | Msg::Opened(Err(err))
            | Msg::OpenedWorktree(_, Err(err)) => {
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
        if let Some(prompt) = self.force_checkout_prompt.take() {
            self.on_force_checkout_key(key, prompt);
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
            _ => {}
        }
    }

    fn on_force_checkout_key(&mut self, key: KeyEvent, prompt: ForceCheckoutPrompt) {
        match key.code {
            KeyCode::Char('y') => {
                let (repo, number, branch) = (self.repo.clone(), prompt.number, prompt.branch);
                self.set_flash(format!("Resetting {branch} to PR #{number}…"), false);
                self.spawn(move || {
                    let result = github::force_checkout(&repo, number)
                        .map(|()| Checkout::Done(format!("reset {branch} to match the PR")));
                    Msg::CheckedOut(number, result)
                });
            }
            KeyCode::Char('n') | KeyCode::Char('q') | KeyCode::Esc => {
                self.set_flash(format!("Kept local branch {}", prompt.branch), false);
            }
            _ => self.force_checkout_prompt = Some(prompt),
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
        let mut app = App::new("o/r".into(), tx);
        app.set_prs(prs);
        app
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
        let press = |c| KeyEvent::from(KeyCode::Char(c));
        app.on_key(press('k'));
        assert_eq!(app.list.selected(), Some(0));
        app.on_key(press('G'));
        app.on_key(press('j'));
        assert_eq!(app.list.selected(), Some(1));
        app.on_key(press('g'));
        assert_eq!(app.list.selected(), Some(0));
    }

    /// Only the decline path is tested: accepting spawns a real `gh` command.
    #[test]
    fn diverged_checkout_prompts_until_answered() {
        let mut app = app_with(vec![sample_pr(1, "a"), sample_pr(2, "b")]);
        let diverged = Checkout::Diverged {
            branch: "feature".into(),
        };
        app.on_msg(Msg::CheckedOut(2, Ok(diverged)));
        assert_eq!(app.force_checkout_prompt.as_ref().unwrap().number, 2);

        // Other keys are swallowed while the prompt is open.
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(app.list.selected(), Some(0));
        assert!(app.force_checkout_prompt.is_some());

        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(app.force_checkout_prompt.is_none());
        assert!(!app.should_quit);
        assert_eq!(app.flash().unwrap().text, "Kept local branch feature");
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
