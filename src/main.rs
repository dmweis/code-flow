mod app;
mod github;
mod model;
mod ui;

use anyhow::Result;

fn main() -> Result<()> {
    // Resolve the repo before entering the TUI so errors print normally.
    let repo = github::current_repo()?;
    let mut terminal = ratatui::init();
    let result = app::run(&mut terminal, repo);
    ratatui::restore();
    result
}
