//! Application state, input handling, and the main event loop.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;

use crate::github;
use crate::model::PullRequest;
use crate::ui;

const TICK: Duration = Duration::from_millis(200);
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const FLASH_DURATION: Duration = Duration::from_secs(5);

/// Results of background work, sent back to the event loop.
enum Msg {
    Prs(Result<Vec<PullRequest>>),
    CheckedOut(u64, Result<String>),
    Opened(Result<()>),
}

pub struct Flash {
    pub text: String,
    pub is_error: bool,
    at: Instant,
}

pub struct App {
    pub repo: String,
    pub prs: Vec<PullRequest>,
    pub table: TableState,
    pub detail_scroll: u16,
    pub show_help: bool,
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
            table: TableState::default(),
            detail_scroll: 0,
            show_help: false,
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
        self.table.selected().and_then(|i| self.prs.get(i))
    }

    pub fn flash(&self) -> Option<&Flash> {
        self.flash
            .as_ref()
            .filter(|flash| flash.at.elapsed() < FLASH_DURATION)
    }

    fn set_flash(&mut self, text: impl Into<String>, is_error: bool) {
        self.flash = Some(Flash {
            text: text.into(),
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
            Msg::CheckedOut(number, Ok(output)) => {
                let text = match output.is_empty() {
                    true => format!("Checked out #{number}"),
                    false => format!("Checked out #{number}: {output}"),
                };
                self.set_flash(text, false);
            }
            Msg::CheckedOut(_, Err(err)) | Msg::Opened(Err(err)) => {
                self.set_flash(format!("{err:#}"), true);
            }
            Msg::Opened(Ok(())) => {}
        }
    }

    /// Replaces the PR list, keeping the same PR selected if it's still open.
    fn set_prs(&mut self, prs: Vec<PullRequest>) {
        let previous = self.selected_pr().map(|pr| pr.number);
        let old_index = self.table.selected().unwrap_or(0);
        self.prs = prs;
        self.load_error = None;
        self.last_success = Some(Instant::now());
        let index = previous
            .and_then(|number| self.prs.iter().position(|pr| pr.number == number))
            .or_else(|| (!self.prs.is_empty()).then(|| old_index.min(self.prs.len() - 1)));
        self.table.select(index);
        if self.selected_pr().map(|pr| pr.number) != previous {
            self.detail_scroll = 0;
        }
    }

    fn select(&mut self, index: usize) {
        if self.prs.is_empty() {
            return;
        }
        let index = index.min(self.prs.len() - 1);
        if self.table.selected() != Some(index) {
            self.table.select(Some(index));
            self.detail_scroll = 0;
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if self.show_help {
            self.show_help = false;
            return;
        }
        let current = self.table.selected().unwrap_or(0);
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
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
            _ => {}
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
        assert_eq!(app.table.selected(), Some(0));
        app.on_key(press('G'));
        app.on_key(press('j'));
        assert_eq!(app.table.selected(), Some(1));
        app.on_key(press('g'));
        assert_eq!(app.table.selected(), Some(0));
    }
}
