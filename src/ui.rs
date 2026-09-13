//! Rendering. Every status has a glyph (used in the list) and a description
//! (used in the detail pane), so the detail pane doubles as a legend.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Clear, HighlightSpacing, Paragraph, Row, Table, Wrap};

use crate::app::App;
use crate::model::{AutoMerge, Ci, CiState, Label, Merge, PullRequest, Review};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [main, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
    let [list, detail] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(main);
    draw_list(frame, app, list);
    draw_detail(frame, app, detail);
    draw_footer(frame, app, footer);
    if app.show_help {
        draw_help(frame);
    }
}

fn draw_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let title = format!(" My PRs · {} ({}) ", app.repo, app.prs.len());
    let block = Block::bordered().title(title.bold());

    if app.prs.is_empty() {
        let message = match (&app.load_error, app.last_success) {
            (Some(err), _) => Text::from(err.as_str()).red(),
            (None, Some(_)) => {
                Text::from(format!("No open PRs by you in {}", app.repo)).dark_gray()
            }
            (None, None) => Text::from("Loading…").dark_gray(),
        };
        let paragraph = Paragraph::new(message)
            .wrap(Wrap { trim: false })
            .block(block);
        frame.render_widget(paragraph, area);
        return;
    }

    let number_width = app
        .prs
        .iter()
        .map(|pr| pr.number.to_string().len())
        .max()
        .unwrap_or(1) as u16;
    let header = Row::new([
        Cell::from(Line::from("#").right_aligned()),
        "D".into(),
        "R".into(),
        "CI".into(),
        "M".into(),
        "A".into(),
        "T".into(),
        "Title".into(),
    ])
    .style(Style::new().dark_gray().bold());
    let rows = app.prs.iter().map(|pr| {
        let threads = match pr.unresolved_threads {
            0 => Span::raw(""),
            n @ 1..=9 => n.to_string().yellow(),
            _ => "9+".yellow(),
        };
        let mut title = vec![Span::raw(pr.title.as_str()), Span::raw(" ")];
        title.extend(label_chips(&pr.labels));
        Row::new([
            Cell::from(Line::from(pr.number.to_string()).right_aligned()),
            draft_glyph(pr.is_draft).into(),
            review_glyph(pr.review).into(),
            ci_glyph(pr.ci).into(),
            merge_glyph(pr.merge).into(),
            auto_merge_glyph(pr.auto_merge).into(),
            threads.into(),
            Line::from(title).into(),
        ])
    });
    let widths = [
        Constraint::Length(number_width),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Fill(1),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(Style::new().bg(Color::Indexed(237)).bold())
        .highlight_symbol("▶ ")
        .highlight_spacing(HighlightSpacing::Always);
    frame.render_stateful_widget(table, area, &mut app.table);
}

fn draw_detail(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(pr) = app.selected_pr() else {
        frame.render_widget(Block::bordered().title(" Details "), area);
        return;
    };
    let inner_width = area.width.saturating_sub(2);

    let header = Paragraph::new(header_lines(pr)).wrap(Wrap { trim: false });
    let header_height =
        (header.line_count(inner_width) as u16 + 2).min(area.height.saturating_sub(5));
    let [top, bottom] =
        Layout::vertical([Constraint::Length(header_height), Constraint::Fill(1)]).areas(area);
    let header = header.block(Block::bordered().title(format!(" #{} ", pr.number).bold()));
    frame.render_widget(header, top);

    let body = match pr.body.is_empty() {
        true => Text::from("No description provided.").dark_gray().italic(),
        false => Text::from(pr.body.as_str()),
    };
    let body = Paragraph::new(body).wrap(Wrap { trim: false });
    let visible = bottom.height.saturating_sub(2) as usize;
    let max_scroll = body.line_count(inner_width).saturating_sub(visible) as u16;
    let scroll = app.detail_scroll.min(max_scroll);
    let body = body
        .scroll((scroll, 0))
        .block(Block::bordered().title(" Description "));
    frame.render_widget(body, bottom);
    app.detail_scroll = scroll;
}

fn header_lines(pr: &PullRequest) -> Vec<Line<'_>> {
    let mut lines = vec![
        Line::from(pr.title.as_str().bold()),
        Line::from(format!("{} → {}", pr.head, pr.base).dark_gray()),
    ];
    if !pr.labels.is_empty() {
        lines.push(Line::from(label_chips(&pr.labels)));
    }
    lines.push(Line::default());

    let status =
        |glyph: Span<'static>, text: String| Line::from(vec![glyph, " ".into(), text.into()]);
    lines.push(status(
        draft_glyph(pr.is_draft),
        draft_text(pr.is_draft).into(),
    ));
    lines.push(status(
        review_glyph(pr.review),
        review_text(pr.review).into(),
    ));
    lines.push(status(ci_glyph(pr.ci), ci_text(pr.ci)));
    lines.push(status(merge_glyph(pr.merge), merge_text(pr.merge).into()));
    if let Some(text) = auto_merge_text(pr.auto_merge) {
        lines.push(status(auto_merge_glyph(pr.auto_merge), text));
    }
    if pr.unresolved_threads > 0 {
        let n = pr.unresolved_threads;
        let noun = if n == 1 { "thread" } else { "threads" };
        lines.push(status(
            n.to_string().yellow(),
            format!("unresolved review {noun}"),
        ));
    }
    lines
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let status = if let Some(since) = app.loading_since {
        let frame_index = (since.elapsed().as_millis() / 100) as usize % SPINNER.len();
        Line::from(format!("{} refreshing ", SPINNER[frame_index]).dark_gray())
    } else if app.load_error.is_some() {
        Line::from("refresh failed ".red())
    } else if let Some(at) = app.last_success {
        Line::from(format!("updated {} ago ", format_age(at.elapsed())).dark_gray())
    } else {
        Line::default()
    };
    let [left, right] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(status.width() as u16),
    ])
    .areas(area);
    frame.render_widget(status, right);

    let hint = match app.flash() {
        Some(flash) if flash.is_error => Line::from(format!(" {}", flash.text).red()),
        Some(flash) => Line::from(format!(" {}", flash.text).green()),
        None => {
            let keys = [
                ("j/k", "move"),
                ("o", "open"),
                ("c", "checkout"),
                ("r", "refresh"),
                ("J/K", "scroll"),
                ("?", "legend"),
                ("q", "quit"),
            ];
            let spans = keys.into_iter().flat_map(|(key, action)| {
                [format!(" {key}").bold(), format!(" {action} ").dark_gray()]
            });
            Line::from_iter(spans)
        }
    };
    frame.render_widget(hint, left);
}

fn draw_help(frame: &mut Frame) {
    let row = |label: &'static str, glyphs: Vec<Span<'static>>| {
        let mut spans = vec![format!(" {label:<3} ").bold()];
        spans.extend(glyphs);
        Line::from(spans)
    };
    let key = |keys: &'static str, action: &'static str| {
        Line::from(vec![format!(" {keys:<14}").bold(), action.into()])
    };
    let lines = vec![
        Line::from(" Columns".dark_gray()),
        row(
            "D",
            vec![
                draft_glyph(false),
                " ready  ".into(),
                draft_glyph(true),
                " draft".into(),
            ],
        ),
        row(
            "R",
            vec![
                review_glyph(Review::Required),
                " needs review  ".into(),
                review_glyph(Review::Approved),
                " approved  ".into(),
                review_glyph(Review::ChangesRequested),
                " changes requested".into(),
            ],
        ),
        row(
            "CI",
            vec![
                "⟳".yellow(),
                " running  ".into(),
                "✓".green(),
                " passed  ".into(),
                "✗".red(),
                " failed  ".into(),
                "·".dark_gray(),
                " no checks".into(),
            ],
        ),
        row(
            "M",
            vec![
                merge_glyph(Merge::Clean),
                " no conflicts  ".into(),
                merge_glyph(Merge::Conflicts),
                " conflicts  ".into(),
                merge_glyph(Merge::Behind),
                " behind base  ".into(),
                merge_glyph(Merge::Unknown),
                " unknown".into(),
            ],
        ),
        row(
            "A",
            vec![
                auto_merge_glyph(AutoMerge::Enabled),
                " auto-merge on  ".into(),
                auto_merge_glyph(AutoMerge::Queued { position: None }),
                " in merge queue".into(),
            ],
        ),
        row("T", vec!["2".yellow(), " unresolved review threads".into()]),
        Line::default(),
        Line::from(" Keys".dark_gray()),
        key("j/k  ↑/↓", "move selection"),
        key("g/G", "first / last"),
        key("J/K  PgUp/Dn", "scroll description"),
        key("o  Enter", "open in browser"),
        key("c", "gh pr checkout"),
        key("r", "refresh now (auto every 60s)"),
        key("q  Esc", "quit"),
    ];
    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 3;
    let height = lines.len() as u16 + 2;
    let area = centered(frame.area(), width, height);
    let popup = Paragraph::new(lines).block(Block::bordered().title(" Legend ".bold()));
    frame.render_widget(Clear, area);
    frame.render_widget(popup, area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    area
}

fn format_age(age: Duration) -> String {
    match age.as_secs() {
        s @ 0..60 => format!("{s}s"),
        s => format!("{}m", s / 60),
    }
}

fn label_chips(labels: &[Label]) -> Vec<Span<'_>> {
    let mut spans = Vec::with_capacity(labels.len() * 2);
    for label in labels {
        let (r, g, b) = label.rgb;
        let luminance = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
        let fg = if luminance > 150.0 {
            Color::Black
        } else {
            Color::White
        };
        let style = Style::new().bg(Color::Rgb(r, g, b)).fg(fg);
        spans.push(Span::styled(format!(" {} ", label.name), style));
        spans.push(Span::raw(" "));
    }
    spans.pop();
    spans
}

fn draft_glyph(is_draft: bool) -> Span<'static> {
    match is_draft {
        true => "◌".dark_gray(),
        false => "●".into(),
    }
}

fn draft_text(is_draft: bool) -> &'static str {
    match is_draft {
        true => "Draft",
        false => "Ready for review",
    }
}

fn review_glyph(review: Review) -> Span<'static> {
    match review {
        Review::Required => "◷".yellow(),
        Review::Approved => "✓".green(),
        Review::ChangesRequested => "✗".red(),
    }
}

fn review_text(review: Review) -> &'static str {
    match review {
        Review::Required => "Needs review",
        Review::Approved => "Approved",
        Review::ChangesRequested => "Changes requested",
    }
}

fn ci_glyph(ci: Ci) -> Span<'static> {
    match ci.state {
        CiState::None => "·".dark_gray(),
        CiState::Running => "⟳".yellow(),
        CiState::Passed => "✓".green(),
        CiState::Failed => "✗".red(),
    }
}

fn ci_text(ci: Ci) -> String {
    let Ci {
        total,
        pending,
        failed,
        ..
    } = ci;
    match ci.state {
        CiState::None => "No CI checks".into(),
        CiState::Running => format!("CI running ({}/{total} done)", total - pending),
        CiState::Passed => format!("CI passed ({total} checks)"),
        CiState::Failed if failed == 0 => "CI failed".into(),
        CiState::Failed if pending > 0 => {
            format!("CI failed ({failed} failed, {pending} still running)")
        }
        CiState::Failed => format!("CI failed ({failed} of {total} checks)"),
    }
}

fn merge_glyph(merge: Merge) -> Span<'static> {
    match merge {
        Merge::Clean => "✓".green(),
        Merge::Conflicts => "⚠".red(),
        Merge::Behind => "↓".yellow(),
        Merge::Unknown => "?".dark_gray(),
    }
}

fn merge_text(merge: Merge) -> &'static str {
    match merge {
        Merge::Clean => "No conflicts",
        Merge::Conflicts => "Merge conflicts",
        Merge::Behind => "Behind base branch",
        Merge::Unknown => "Mergeability unknown",
    }
}

fn auto_merge_glyph(auto_merge: AutoMerge) -> Span<'static> {
    match auto_merge {
        AutoMerge::Off => Span::raw(""),
        AutoMerge::Enabled => "»".cyan(),
        AutoMerge::Queued { .. } => "≡".magenta(),
    }
}

fn auto_merge_text(auto_merge: AutoMerge) -> Option<String> {
    match auto_merge {
        AutoMerge::Off => None,
        AutoMerge::Enabled => Some("Auto-merge enabled".into()),
        AutoMerge::Queued { position: Some(p) } => Some(format!("In merge queue (position {p})")),
        AutoMerge::Queued { position: None } => Some("In merge queue".into()),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::tests::{app_with, sample_pr};

    fn render(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        buffer
            .content
            .chunks(buffer.area.width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_list_row_and_detail() {
        let mut app = app_with(vec![sample_pr(482, "Add retry logic")]);
        let screen = render(&mut app);
        assert!(screen.contains("My PRs · o/r (1)"), "{screen}");
        assert!(
            screen.contains("▶ 482 ● ✓ ⟳  ⚠ » 9+ Add retry logic  bug "),
            "{screen}"
        );
        assert!(screen.contains("CI running (3/7 done)"), "{screen}");
        assert!(screen.contains("12 unresolved review threads"), "{screen}");
        assert!(screen.contains("Body text"), "{screen}");
    }

    #[test]
    fn renders_empty_state() {
        let mut app = app_with(vec![]);
        assert!(render(&mut app).contains("No open PRs by you in o/r"));
    }
}
