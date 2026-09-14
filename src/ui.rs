//! Rendering. Every status has a colored glyph, shown with a short word in
//! the list and a full description in the detail pane.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, HighlightSpacing, List, ListItem, Padding, Paragraph, Wrap};

use crate::app::{App, Prompt, Row};
use crate::model::{
    AutoMerge, CLAUDE_REVIEW_LABEL, Ci, CiState, Label, Merge, MergeMethod, Progress, PullRequest,
    Review, Worktree,
};
use crate::worktree::tilde;

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
        draw_help(frame, app);
    }
    if let Some(prompt) = &app.prompt {
        draw_prompt(frame, prompt, &app.default_branch);
    }
    if app.new_branch.is_some() {
        draw_new_branch(frame, app);
    }
}

fn draw_new_branch(frame: &mut Frame, app: &App) {
    let text = app.new_branch.as_deref().unwrap_or_default();
    let preview = match app.new_branch_name() {
        Some(name) => Line::from(vec!["→ ".dark_gray(), name.bold()]),
        None => Line::from(format!("→ {}…", app.branch_prefix).dark_gray()),
    };
    let lines = vec![
        Line::from(vec![text.into(), "▏".yellow()]),
        preview,
        Line::default(),
        Line::from(
            format!(
                "Creates the branch from a freshly fetched origin/{} in a new worktree.",
                app.default_branch
            )
            .dark_gray(),
        ),
        Line::default(),
        Line::from(vec![
            " Enter ".bold().black().on_yellow(),
            " create    ".into(),
            " Esc ".bold().reversed(),
            " cancel".into(),
        ])
        .centered(),
    ];
    draw_dialog(frame, " New branch ", lines);
}

fn draw_prompt(frame: &mut Frame, prompt: &Prompt, default_branch: &str) {
    let (title, mut lines, yes, no) = match prompt {
        Prompt::ForceCheckout { number, branch } => (
            " Branch has diverged ",
            vec![
                Line::from(vec![
                    "Local branch ".into(),
                    branch.as_str().bold(),
                    format!(" has diverged from PR #{number}, ").into(),
                    "probably because the PR was rebased or force-pushed. ".into(),
                    "You are now on the local branch.".into(),
                ]),
                Line::default(),
                Line::from(
                    "Reset it to match the PR? Commits that only exist on the local branch \
                     will be discarded (git reflog can still recover them).",
                ),
            ],
            "reset branch",
            "keep local branch",
        ),
        Prompt::MoveToWorktree {
            number,
            branch,
            main_path,
        } => (
            " Branch is in your main checkout ",
            vec![
                Line::from(vec![
                    format!("PR #{number}'s branch ").into(),
                    branch.as_str().bold(),
                    " is checked out in your main checkout ".into(),
                    tilde(main_path).bold(),
                    ", and git allows a branch in only one worktree.".into(),
                ]),
                Line::default(),
                Line::from(vec![
                    "Switch the main checkout back to ".into(),
                    default_branch.bold(),
                    " and open the PR in its own worktree?".into(),
                ]),
            ],
            "move to worktree",
            "leave it",
        ),
        Prompt::RemoveWorktree { number, path, .. } => (
            " Remove worktree ",
            vec![
                Line::from(vec![
                    format!("Remove the worktree for PR #{number} at ").into(),
                    tilde(path).bold(),
                    "? Its Herdr workspace, and anything running in it, is closed too.".into(),
                ]),
                Line::default(),
                Line::from(
                    "worktrunk refuses if the worktree has uncommitted changes. \
                     The branch is kept unless it's merged.",
                ),
            ],
            "remove",
            "keep",
        ),
        Prompt::RemoveLocal {
            branch,
            path,
            progress,
        } => (
            " Remove worktree ",
            vec![
                Line::from(vec![
                    "Remove the worktree for ".into(),
                    branch.as_str().bold(),
                    " at ".into(),
                    tilde(path).bold(),
                    "? Its Herdr workspace, and anything running in it, is closed too.".into(),
                ]),
                Line::default(),
                Line::from(format!(
                    "worktrunk refuses if the worktree has uncommitted changes. {}",
                    branch_fate(*progress)
                )),
            ],
            "remove",
            "keep",
        ),
        Prompt::Merge {
            number,
            title,
            base,
            method,
            ..
        } => (
            " Merge pull request ",
            vec![
                Line::from(vec![
                    format!("{} PR #{number} ", merge_verb(*method)).into(),
                    title.as_str().bold(),
                    " into ".into(),
                    base.as_str().bold(),
                    "?".into(),
                ]),
                Line::default(),
                Line::from(
                    "This uses your default merge method for this repository. \
                     GitHub refuses if commits were pushed since the last refresh.",
                ),
            ],
            "merge",
            "cancel",
        ),
    };
    lines.extend([
        Line::default(),
        Line::from(vec![
            " y ".bold().black().on_yellow(),
            format!(" {yes}    ").into(),
            " n ".bold().reversed(),
            format!(" {no}").into(),
        ])
        .centered(),
    ]);
    draw_dialog(frame, title, lines);
}

/// The words on GitHub's merge button.
fn merge_verb(method: MergeMethod) -> &'static str {
    match method {
        MergeMethod::Merge => "Merge",
        MergeMethod::Squash => "Squash and merge",
        MergeMethod::Rebase => "Rebase and merge",
    }
}

/// What `wt remove` does with a branch, which it deletes only once merged.
fn branch_fate(progress: Option<Progress>) -> String {
    match progress {
        Some(Progress::Empty) => {
            "The branch has no commits of its own, so it's deleted too.".into()
        }
        Some(Progress::Merged) => "The branch is merged, so it's deleted too.".into(),
        Some(Progress::Ahead(1)) => "The branch has 1 unmerged commit, so it's kept.".into(),
        Some(Progress::Ahead(n)) => format!("The branch has {n} unmerged commits, so it's kept."),
        None => "The branch is kept unless it's merged.".into(),
    }
}

fn draw_dialog(frame: &mut Frame, title: &str, lines: Vec<Line>) {
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: true });
    let width = 64.min(frame.area().width);
    let height = paragraph.line_count(width.saturating_sub(4)) as u16 + 2;
    let area = centered(frame.area(), width, height);
    let block = Block::bordered()
        .title(title.bold())
        .border_style(Style::new().yellow())
        .padding(Padding::horizontal(1));
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph.block(block), area);
}

/// PRs on top, and below them every local worktree.
fn draw_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let local: Vec<_> = app
        .worktrees
        .iter()
        .map(|worktree| local_item(worktree, app.pr_for(worktree)))
        .collect();
    if local.is_empty() {
        draw_prs(frame, app, area);
        return;
    }
    let height = (local.len() as u16 * 2 + 2).min(area.height / 2);
    let [prs, local_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(height)]).areas(area);
    let title = format!(" Local worktrees ({}) ", local.len());
    let list = selectable(List::new(local).block(Block::bordered().title(title.bold())));
    frame.render_stateful_widget(list, local_area, &mut app.local_list);
    draw_prs(frame, app, prs);
}

fn selectable(list: List) -> List {
    list.highlight_style(Style::new().bg(Color::Indexed(237)).bold())
        .highlight_symbol("▶ ")
        .highlight_spacing(HighlightSpacing::Always)
}

/// The branch, then the title of your PR for it if there is one.
fn local_item(worktree: &Worktree, pr: Option<&PullRequest>) -> ListItem<'static> {
    let mut title = vec![Span::raw(worktree.branch.clone())];
    if let Some(pr) = pr {
        title.extend([
            format!("  #{} ", pr.number).dark_gray(),
            Span::raw(pr.title.clone()),
        ]);
    }
    let mut status = vec![Span::raw("  ")];
    let badges = local_badges(worktree);
    if badges.is_empty() {
        status.push(tilde(&worktree.path).dark_gray());
    }
    for badge in badges {
        status.extend([badge, Span::raw("  ")]);
    }
    ListItem::new(vec![Line::from(title), Line::from(status)])
}

/// Short colored status words for a local row; none without worktrunk.
fn local_badges(worktree: &Worktree) -> Vec<Span<'static>> {
    let Some(status) = worktree.status else {
        return Vec::new();
    };
    let badge = |glyph: Span<'static>, text: &str| {
        Span::styled(format!("{} {text}", glyph.content), glyph.style)
    };
    let mut badges = Vec::new();
    if status.uncommitted {
        badges.push(badge(uncommitted_glyph(), "uncommitted"));
    }
    if let Some(progress) = status.progress {
        let text = match progress {
            Progress::Empty => "empty".to_owned(),
            Progress::Ahead(1) => "1 commit".to_owned(),
            Progress::Ahead(n) => format!("{n} commits"),
            Progress::Merged => "merged".to_owned(),
        };
        badges.push(badge(progress_glyph(progress), &text));
    }
    badges
}

fn draw_prs(frame: &mut Frame, app: &mut App, area: Rect) {
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
        .unwrap_or(1);
    let items = app.prs.iter().map(|pr| {
        let mut title = vec![format!("#{:<number_width$} ", pr.number).dark_gray()];
        title.extend([Span::raw(pr.title.as_str()), Span::raw(" ")]);
        title.extend(label_chips(&pr.labels));
        let mut status = vec![Span::raw(" ".repeat(number_width + 2))];
        for badge in status_badges(
            pr,
            app.worktree_for(pr).is_some(),
            claude_review_badge(app, pr),
            app.merging.contains(&pr.number),
        ) {
            status.extend([badge, Span::raw("  ")]);
        }
        status.pop();
        ListItem::new(vec![Line::from(title), Line::from(status)])
    });
    let list = selectable(List::new(items).block(block));
    frame.render_stateful_widget(list, area, &mut app.list);
}

fn claude_review_badge(app: &App, pr: &PullRequest) -> Option<Span<'static>> {
    app.claude_review_label.as_ref()?;
    Some(if app.adding_claude_review.contains(&pr.number) {
        "⟳ claude-review".yellow()
    } else if pr.has_claude_review() {
        "✓ claude-review".green()
    } else {
        "· no claude-review".dark_gray()
    })
}

/// Short colored status words for a list row. States with nothing to act on
/// (ready, no conflicts, auto-merge off) are left out.
fn status_badges(
    pr: &PullRequest,
    has_worktree: bool,
    claude_review: Option<Span<'static>>,
    merging: bool,
) -> Vec<Span<'static>> {
    let badge = |glyph: Span<'static>, text: &str| {
        Span::styled(format!("{} {text}", glyph.content), glyph.style)
    };
    let mut badges = Vec::new();
    if pr.is_draft {
        badges.push(badge(draft_glyph(true), "draft"));
    }
    // A draft isn't asking for review yet, so only show a verdict if there is one.
    if !(pr.is_draft && pr.review == Review::Required) {
        let text = match pr.review {
            Review::Required => "needs review",
            Review::Approved => "approved",
            Review::ChangesRequested => "changes requested",
        };
        badges.push(badge(review_glyph(pr.review), text));
    }
    let ci = match pr.ci.state {
        CiState::None => "no CI".to_owned(),
        CiState::Running => format!(
            "CI {}/{}",
            pr.ci.total.saturating_sub(pr.ci.pending),
            pr.ci.total
        ),
        CiState::Passed => "CI passed".to_owned(),
        CiState::Failed if pr.ci.failed == 0 => "CI failed".to_owned(),
        CiState::Failed => format!("CI {} failed", pr.ci.failed),
    };
    badges.push(badge(ci_glyph(pr.ci), &ci));
    badges.extend(claude_review);
    match pr.merge {
        Merge::Conflicts => badges.push(badge(merge_glyph(pr.merge), "conflicts")),
        Merge::Behind => badges.push(badge(merge_glyph(pr.merge), "behind base")),
        Merge::Ready | Merge::Clean | Merge::Unknown => {}
    }
    if merging {
        badges.push("⟳ merging".yellow());
    } else if pr.is_ready_to_merge() {
        badges.push(badge(ready_glyph(), "ready to merge"));
    }
    let glyph = auto_merge_glyph(pr.auto_merge);
    match pr.auto_merge {
        AutoMerge::Off => {}
        AutoMerge::Enabled => badges.push(badge(glyph, "auto-merge")),
        AutoMerge::Queued { position: Some(p) } => {
            badges.push(badge(glyph, &format!("queued #{p}")))
        }
        AutoMerge::Queued { position: None } => badges.push(badge(glyph, "queued")),
    }
    match pr.unresolved_threads {
        0 => {}
        1 => badges.push("1 thread".yellow()),
        n => badges.push(format!("{n} threads").yellow()),
    }
    if has_worktree {
        badges.push(badge(worktree_glyph(), "worktree"));
    }
    badges
}

fn draw_detail(frame: &mut Frame, app: &mut App, area: Rect) {
    match app.selected() {
        Some(Row::Pr(_)) => draw_pr_detail(frame, app, area),
        Some(Row::Local(worktree)) => {
            let lines = local_lines(worktree, app.pr_for(worktree), &app.default_branch);
            let block = Block::bordered().title(" Local worktree ".bold());
            let detail = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(block);
            frame.render_widget(detail, area);
        }
        None => frame.render_widget(Block::bordered().title(" Details "), area),
    }
}

fn local_lines<'a>(
    worktree: &'a Worktree,
    pr: Option<&PullRequest>,
    default_branch: &str,
) -> Vec<Line<'a>> {
    let mut lines = vec![Line::from(worktree.branch.as_str().bold())];
    if let Some(subject) = &worktree.subject {
        lines.push(Line::from(format!("Last commit: {subject}").dark_gray()));
    }
    lines.push(Line::default());

    let status =
        |glyph: Span<'static>, text: String| Line::from(vec![glyph, " ".into(), text.into()]);
    match worktree.status {
        Some(worktree_status) => {
            lines.push(match worktree_status.uncommitted {
                true => status(uncommitted_glyph(), "Uncommitted changes".into()),
                false => status("✓".green(), "No uncommitted changes".into()),
            });
            lines.push(match worktree_status.progress {
                Some(progress) => status(
                    progress_glyph(progress),
                    progress_text(progress, default_branch),
                ),
                None => status(
                    "?".dark_gray(),
                    format!("Shares no history with {default_branch}"),
                ),
            });
        }
        None => lines.push(Line::from(
            "Install worktrunk to see this worktree's status".dark_gray(),
        )),
    }
    lines.push(status(
        worktree_glyph(),
        format!("Worktree at {}", tilde(&worktree.path)),
    ));
    lines.push(Line::default());
    lines.push(match pr {
        Some(pr) => Line::from(vec![
            format!("Your PR #{}: ", pr.number).into(),
            pr.title.clone().bold(),
        ]),
        None => Line::from("None of your open PRs uses this branch.".dark_gray()),
    });
    lines
}

fn draw_pr_detail(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(pr) = app.selected_pr() else {
        return;
    };
    let inner_width = area.width.saturating_sub(2);

    let worktree = app.worktree_for(pr).map(tilde);
    let mut lines = header_lines(pr, worktree);
    if let Some(badge) = claude_review_badge(app, pr) {
        lines.insert(2, Line::from(badge));
    }
    let header = Paragraph::new(lines).wrap(Wrap { trim: false });
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

fn header_lines(pr: &PullRequest, worktree: Option<String>) -> Vec<Line<'_>> {
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
    lines.push(match pr.is_ready_to_merge() {
        true => status(ready_glyph(), "Ready to merge".into()),
        false => status(merge_glyph(pr.merge), merge_text(pr.merge).into()),
    });
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
    if let Some(path) = worktree {
        lines.push(status(worktree_glyph(), format!("Worktree at {path}")));
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

    let pull = format!("pull {}", app.default_branch);
    let hint = match app.flash() {
        Some(flash) if flash.is_error => Line::from(format!(" {}", flash.text).red()),
        Some(flash) => Line::from(format!(" {}", flash.text).green()),
        None => {
            let mut keys = vec![("j/k", "move")];
            match app.selected() {
                Some(Row::Pr(pr)) => {
                    keys.extend([("o", "open"), ("c", "checkout"), ("w", "worktree")]);
                    if app.can_merge(pr) {
                        keys.push(("m", "merge"));
                    }
                    if app.can_add_claude_review(pr) {
                        keys.push(("l", "add claude-review"));
                    }
                    // Only offer removal where there's something to remove.
                    if app.worktree_for(pr).is_some() {
                        keys.push(("W", "remove worktree"));
                    }
                }
                Some(Row::Local(_)) => {
                    keys.extend([("w", "workspace"), ("W", "remove worktree")]);
                }
                None => {}
            }
            keys.push(("n", "new branch"));
            if app.can_pull() {
                keys.push(("p", &pull));
            }
            keys.push(("r", "refresh"));
            if !matches!(app.selected(), Some(Row::Local(_))) {
                keys.push(("J/K", "scroll"));
            }
            keys.extend([("?", "keys"), ("q", "quit")]);
            let spans = keys.into_iter().flat_map(|(key, action)| {
                [format!(" {key}").bold(), format!(" {action} ").dark_gray()]
            });
            Line::from_iter(spans)
        }
    };
    frame.render_widget(hint, left);
}

fn draw_help(frame: &mut Frame, app: &App) {
    let key = |keys: &'static str, action: &'static str| {
        Line::from(vec![format!(" {keys:<14}").bold(), action.into()])
    };
    let mut lines = vec![
        key("j/k  ↑/↓", "move selection"),
        key("g/G", "first / last"),
        key("J/K  PgUp/Dn", "scroll description"),
        key("o  Enter", "open in browser"),
        key("c", "gh pr checkout"),
        key("m", "merge, once ready to merge"),
        key("w", "worktree (+ Herdr workspace)"),
        key("W", "remove the selected worktree"),
        key("n", "new branch in its own worktree"),
        key("p", "pull the default branch, when on it"),
        key("r", "refresh now (auto every 60s)"),
        key("q  Esc", "quit"),
    ];
    if app.claude_review_label.is_some() {
        lines.insert(6, key("l", "add claude-review label"));
    }
    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 3;
    let height = lines.len() as u16 + 2;
    let area = centered(frame.area(), width, height);
    let popup = Paragraph::new(lines).block(Block::bordered().title(" Keys ".bold()));
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
        // This label has its own repository-gated status badge.
        if label.name == CLAUDE_REVIEW_LABEL {
            continue;
        }
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

fn worktree_glyph() -> Span<'static> {
    "⌂".blue()
}

fn uncommitted_glyph() -> Span<'static> {
    "●".yellow()
}

fn progress_glyph(progress: Progress) -> Span<'static> {
    match progress {
        Progress::Empty => "◌".dark_gray(),
        Progress::Ahead(_) => "↑".cyan(),
        Progress::Merged => "✓".green(),
    }
}

fn progress_text(progress: Progress, default_branch: &str) -> String {
    match progress {
        Progress::Empty => "No commits of its own yet".into(),
        Progress::Ahead(1) => format!("1 commit not in {default_branch}"),
        Progress::Ahead(n) => format!("{n} commits not in {default_branch}"),
        Progress::Merged => format!("Its changes are already in {default_branch}"),
    }
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
    let checks = if total == 1 { "check" } else { "checks" };
    match ci.state {
        CiState::None => "No CI checks".into(),
        CiState::Running => format!(
            "CI running ({}/{total} done)",
            total.saturating_sub(pending)
        ),
        CiState::Passed => format!("CI passed ({total} {checks})"),
        CiState::Failed if failed == 0 => "CI failed".into(),
        CiState::Failed if pending > 0 => {
            format!("CI failed ({failed} failed, {pending} still running)")
        }
        CiState::Failed => format!("CI failed ({failed} of {total} {checks})"),
    }
}

fn ready_glyph() -> Span<'static> {
    "⇥".green()
}

fn merge_glyph(merge: Merge) -> Span<'static> {
    match merge {
        Merge::Ready | Merge::Clean => "✓".green(),
        Merge::Conflicts => "⚠".red(),
        Merge::Behind => "↓".yellow(),
        Merge::Unknown => "?".dark_gray(),
    }
}

fn merge_text(merge: Merge) -> &'static str {
    match merge {
        Merge::Ready | Merge::Clean => "No conflicts",
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
    use crate::app::tests::{
        app_with, claude_label, press, ready_pr, sample_pr, sample_worktree, unloaded_app,
    };

    fn render(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(150, 24)).unwrap();
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
        assert!(screen.contains("▶ #482 Add retry logic  bug "), "{screen}");
        assert!(
            screen.contains("│       ✓ approved  ⟳ CI 3/7  ⚠ conflicts  » auto-merge  12 threads"),
            "{screen}"
        );
        assert!(screen.contains("CI running (3/7 done)"), "{screen}");
        assert!(screen.contains("12 unresolved review threads"), "{screen}");
        assert!(screen.contains("Body text"), "{screen}");
    }

    #[test]
    fn claude_feature_is_hidden_without_repository_label() {
        let mut app = app_with(vec![sample_pr(1, "a")]);
        assert!(!render(&mut app).contains("claude-review"));
        app.show_help = true;
        assert!(!render(&mut app).contains("claude-review"));
    }

    #[test]
    fn claude_feature_shows_each_pr_status_and_only_offers_eligible_shortcut() {
        let mut labeled = sample_pr(2, "b");
        labeled.labels.push(claude_label());
        let mut app = app_with(vec![sample_pr(1, "a"), labeled]);
        app.claude_review_label = Some(claude_label());
        let screen = render(&mut app);
        assert!(screen.contains("#1 a  bug"), "{screen}");
        assert!(screen.contains("#2 b  bug"), "{screen}");
        assert!(screen.contains("⟳ CI 3/7  · no claude-review"), "{screen}");
        assert!(screen.contains("⟳ CI 3/7  ✓ claude-review"), "{screen}");
        assert!(screen.contains("l add claude-review"), "{screen}");

        app.list.select(Some(1));
        let screen = render(&mut app);
        assert!(!screen.contains("l add claude-review"), "{screen}");
        app.show_help = true;
        assert!(render(&mut app).contains("add claude-review label"));

        app.show_help = false;
        app.list.select(Some(0));
        app.adding_claude_review.insert(1);
        let screen = render(&mut app);
        assert!(screen.contains("⟳ CI 3/7  ⟳ claude-review"), "{screen}");
        assert!(!screen.contains("l add claude-review"), "{screen}");

        app.claude_review_label = None;
        assert!(!render(&mut app).contains("claude-review"));
    }

    #[test]
    fn draft_replaces_needs_review_in_list() {
        let mut pr = sample_pr(7, "Draft");
        pr.is_draft = true;
        pr.review = Review::Required;
        pr.ci = Ci {
            state: CiState::None,
            total: 0,
            pending: 0,
            failed: 0,
        };
        pr.merge = Merge::Clean;
        pr.auto_merge = AutoMerge::Off;
        pr.unresolved_threads = 1;
        let text: Vec<_> = status_badges(&pr, false, None, false)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(text, ["◌ draft", "· no CI", "1 thread"]);

        pr.review = Review::ChangesRequested;
        pr.ci = Ci {
            state: CiState::Failed,
            total: 5,
            pending: 2,
            failed: 1,
        };
        pr.merge = Merge::Behind;
        pr.auto_merge = AutoMerge::Queued { position: Some(2) };
        pr.unresolved_threads = 0;
        let text: Vec<_> = status_badges(&pr, false, None, false)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(
            text,
            [
                "◌ draft",
                "✗ changes requested",
                "✗ CI 1 failed",
                "↓ behind base",
                "≡ queued #2"
            ]
        );
    }

    #[test]
    fn shows_and_offers_merge_only_when_ready() {
        let mut app = app_with(vec![ready_pr(482, "Add retry logic"), sample_pr(9, "b")]);
        let screen = render(&mut app);
        assert!(
            screen.contains("✓ approved  ✓ CI passed  ⇥ ready to merge  12 threads"),
            "{screen}"
        );
        assert!(screen.contains("⇥ Ready to merge"), "{screen}");
        assert!(!screen.contains("No conflicts"), "{screen}");
        assert!(screen.contains(" m merge "), "{screen}");

        app.merging.insert(482);
        let screen = render(&mut app);
        assert!(screen.contains("✓ CI passed  ⟳ merging"), "{screen}");
        assert!(!screen.contains(" m merge "), "{screen}");

        app.list.select(Some(1));
        let screen = render(&mut app);
        assert!(!screen.contains("ready to merge"), "{screen}");
        assert!(!screen.contains(" m merge "), "{screen}");
    }

    #[test]
    fn renders_merge_prompt_with_the_merge_method() {
        let mut app = app_with(vec![ready_pr(482, "Add retry logic")]);
        app.prompt = Some(Prompt::Merge {
            number: 482,
            title: "Add retry logic".into(),
            base: "main".into(),
            head_oid: "abc123".into(),
            method: MergeMethod::Squash,
        });
        let screen = render(&mut app);
        assert!(screen.contains("Merge pull request"), "{screen}");
        assert!(
            screen.contains("Squash and merge PR #482 Add retry logic into main?"),
            "{screen}"
        );
        assert!(screen.contains(" y  merge"), "{screen}");
        assert!(screen.contains(" n  cancel"), "{screen}");
    }

    #[test]
    fn renders_force_checkout_prompt() {
        let mut app = app_with(vec![sample_pr(482, "Add retry logic")]);
        app.prompt = Some(Prompt::ForceCheckout {
            number: 482,
            branch: "feature/retry".into(),
        });
        let screen = render(&mut app);
        assert!(screen.contains("Branch has diverged"), "{screen}");
        assert!(screen.contains("feature/retry"), "{screen}");
        assert!(screen.contains(" y  reset branch"), "{screen}");
        assert!(screen.contains(" n  keep local branch"), "{screen}");
    }

    #[test]
    fn renders_worktree_prompts() {
        let mut app = app_with(vec![sample_pr(482, "Add retry logic")]);
        app.prompt = Some(Prompt::MoveToWorktree {
            number: 482,
            branch: "feature".into(),
            main_path: "/src/repo".into(),
        });
        let screen = render(&mut app);
        assert!(
            screen.contains("Branch is in your main checkout"),
            "{screen}"
        );
        assert!(
            screen.contains("Switch the main checkout back to main"),
            "{screen}"
        );
        assert!(screen.contains(" y  move to worktree"), "{screen}");

        app.prompt = Some(Prompt::RemoveWorktree {
            number: 482,
            branch: "feature".into(),
            path: "/src/repo.feature".into(),
        });
        let screen = render(&mut app);
        assert!(screen.contains("Remove worktree"), "{screen}");
        assert!(screen.contains("/src/repo.feature"), "{screen}");
        assert!(screen.contains(" y  remove"), "{screen}");
    }

    #[test]
    fn shows_worktree_in_list_and_detail() {
        let mut app = app_with(vec![sample_pr(482, "Add retry logic")]);
        assert!(!render(&mut app).contains("W remove worktree"));

        app.worktrees
            .push(sample_worktree("feature", Progress::Ahead(1)));
        let screen = render(&mut app);
        assert!(screen.contains(" W remove worktree "), "{screen}");
        assert!(screen.contains("12 threads  ⌂ worktree"), "{screen}");
        assert!(
            screen.contains("⌂ Worktree at /src/repo.feature"),
            "{screen}"
        );
        // The PR's worktree is also a local row, named by the PR.
        assert!(screen.contains("Local worktrees (1)"), "{screen}");
        assert!(screen.contains("feature  #482 Add retry logic"), "{screen}");

        app.list.select(None);
        app.local_list.select(Some(0));
        let screen = render(&mut app);
        assert!(screen.contains("Your PR #482: Add retry logic"), "{screen}");
    }

    #[test]
    fn renders_empty_state() {
        let mut app = app_with(vec![]);
        assert!(render(&mut app).contains("No open PRs by you in o/r"));
    }

    #[test]
    fn renders_local_worktrees_below_prs() {
        let mut app = app_with(vec![sample_pr(482, "Add retry logic")]);
        let mut dirty = sample_worktree("me/busy", Progress::Ahead(3));
        dirty.status.as_mut().unwrap().uncommitted = true;
        let mut untracked = sample_worktree("me/plain", Progress::Empty);
        untracked.status = None;
        app.worktrees = vec![
            dirty,
            sample_worktree("me/done", Progress::Merged),
            untracked,
        ];
        let screen = render(&mut app);
        assert!(screen.contains("Local worktrees (3)"), "{screen}");
        assert!(screen.contains("me/busy"), "{screen}");
        assert!(screen.contains("● uncommitted  ↑ 3 commits"), "{screen}");
        assert!(screen.contains("✓ merged"), "{screen}");
        // Without worktrunk's status, the path stands in for it.
        assert!(screen.contains("/src/repo.me-plain"), "{screen}");
        assert!(screen.contains(" n new branch "), "{screen}");

        app.local_list.select(Some(0));
        app.list.select(None);
        let screen = render(&mut app);
        assert!(screen.contains("Local worktree "), "{screen}");
        assert!(screen.contains("Last commit: Last commit"), "{screen}");
        assert!(screen.contains("● Uncommitted changes"), "{screen}");
        assert!(screen.contains("↑ 3 commits not in main"), "{screen}");
        assert!(
            screen.contains("⌂ Worktree at /src/repo.me-busy"),
            "{screen}"
        );
        assert!(
            screen.contains(" w workspace  W remove worktree "),
            "{screen}"
        );
        assert!(!screen.contains("checkout"), "{screen}");
        assert!(!screen.contains("J/K"), "{screen}");
    }

    #[test]
    fn remove_local_prompt_says_what_happens_to_the_branch() {
        let mut app = app_with(vec![]);
        app.prompt = Some(Prompt::RemoveLocal {
            branch: "me/busy".into(),
            path: "/src/repo.me-busy".into(),
            progress: Some(Progress::Ahead(3)),
        });
        let screen = render(&mut app);
        assert!(screen.contains("Remove worktree"), "{screen}");
        assert!(
            screen.contains("3 unmerged commits, so it's kept"),
            "{screen}"
        );
        assert_eq!(
            branch_fate(Some(Progress::Empty)),
            "The branch has no commits of its own, so it's deleted too."
        );
        assert_eq!(
            branch_fate(Some(Progress::Merged)),
            "The branch is merged, so it's deleted too."
        );
    }

    #[test]
    fn offers_pull_only_on_the_default_branch() {
        let mut app = app_with(vec![]);
        assert!(!render(&mut app).contains("pull main"));
        app.current_branch = Some("me/feature".into());
        assert!(!render(&mut app).contains("pull main"));

        app.current_branch = Some("main".into());
        let screen = render(&mut app);
        assert!(
            screen.contains(" n new branch  p pull main  r refresh "),
            "{screen}"
        );

        app.pulling = true;
        assert!(!render(&mut app).contains("pull main"));
    }

    #[test]
    fn renders_loading_and_load_errors() {
        let mut app = unloaded_app();
        assert!(render(&mut app).contains("Loading…"));

        app.load_error = Some("gh: not logged in".into());
        let screen = render(&mut app);
        assert!(screen.contains("gh: not logged in"), "{screen}");
        assert!(screen.contains("refresh failed"), "{screen}");
        assert!(!screen.contains("Loading…"), "{screen}");
    }

    #[test]
    fn flash_replaces_the_key_hints() {
        let mut app = app_with(vec![sample_pr(1, "a")]);
        assert!(render(&mut app).contains(" r refresh "));
        press(&mut app, 'W');
        let screen = render(&mut app);
        assert!(screen.contains(" PR #1 has no worktree"), "{screen}");
        assert!(!screen.contains(" r refresh "), "{screen}");
    }

    #[test]
    fn scrolling_stops_at_the_end_of_the_description() {
        let mut pr = sample_pr(1, "a");
        pr.body = (1..=100).map(|n| format!("line {n}\n")).collect();
        let mut app = app_with(vec![pr]);
        app.detail_scroll = 1000;
        let screen = render(&mut app);
        assert!(screen.contains("line 100"), "{screen}");
        assert!(!screen.contains("line 50 "), "{screen}");
        // Stored back, so K scrolls up straight away.
        let bottom = app.detail_scroll;
        assert!(bottom < 100, "{bottom}");
        press(&mut app, 'K');
        render(&mut app);
        assert_eq!(app.detail_scroll, bottom - 3);
    }

    #[test]
    fn renders_a_missing_description() {
        let mut pr = sample_pr(1, "a");
        pr.body = String::new();
        let mut app = app_with(vec![pr]);
        assert!(render(&mut app).contains("No description provided."));
    }

    #[test]
    fn renders_at_any_terminal_size() {
        let mut pr = sample_pr(1, "A long title that has to wrap somewhere");
        pr.body = "line\n".repeat(50);
        let mut app = app_with(vec![pr]);
        app.worktrees
            .push(sample_worktree("feature", Progress::Ahead(1)));
        app.show_help = true;
        app.prompt = Some(Prompt::ForceCheckout {
            number: 1,
            branch: "feature".into(),
        });
        app.new_branch = Some("x".into());
        for width in 0..30 {
            for height in 0..12 {
                app.detail_scroll = 1000;
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            }
        }
    }

    #[test]
    fn more_pending_checks_than_total_does_not_underflow() {
        let mut pr = sample_pr(1, "a");
        pr.ci.total = 2;
        pr.ci.pending = 3;
        let text: Vec<_> = status_badges(&pr, false, None, false)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(text.contains(&"⟳ CI 0/2".to_owned()), "{text:?}");
        assert_eq!(ci_text(pr.ci), "CI running (0/2 done)");
    }

    #[test]
    fn describes_ci_in_full() {
        let ci = |state, total, pending, failed| Ci {
            state,
            total,
            pending,
            failed,
        };
        assert_eq!(ci_text(ci(CiState::None, 0, 0, 0)), "No CI checks");
        assert_eq!(
            ci_text(ci(CiState::Running, 7, 4, 0)),
            "CI running (3/7 done)"
        );
        assert_eq!(ci_text(ci(CiState::Passed, 1, 0, 0)), "CI passed (1 check)");
        assert_eq!(
            ci_text(ci(CiState::Passed, 7, 0, 0)),
            "CI passed (7 checks)"
        );
        // The rollup failed without a count saying which.
        assert_eq!(ci_text(ci(CiState::Failed, 2, 0, 0)), "CI failed");
        assert_eq!(
            ci_text(ci(CiState::Failed, 5, 2, 1)),
            "CI failed (1 failed, 2 still running)"
        );
        assert_eq!(
            ci_text(ci(CiState::Failed, 1, 0, 1)),
            "CI failed (1 of 1 check)"
        );
        assert_eq!(
            ci_text(ci(CiState::Failed, 5, 0, 2)),
            "CI failed (2 of 5 checks)"
        );
    }

    #[test]
    fn list_badges_for_remaining_states() {
        let mut pr = ready_pr(1, "a");
        pr.unresolved_threads = 0;
        pr.ci.state = CiState::Failed;
        pr.ci.failed = 0;
        pr.auto_merge = AutoMerge::Queued { position: None };
        let text: Vec<_> = status_badges(&pr, true, None, false)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(
            text,
            ["✓ approved", "✗ CI failed", "≡ queued", "⌂ worktree"]
        );
    }

    #[test]
    fn describes_auto_merge() {
        assert_eq!(auto_merge_text(AutoMerge::Off), None);
        assert_eq!(
            auto_merge_text(AutoMerge::Enabled).as_deref(),
            Some("Auto-merge enabled")
        );
        let queued = |position| auto_merge_text(AutoMerge::Queued { position });
        assert_eq!(
            queued(Some(2)).as_deref(),
            Some("In merge queue (position 2)")
        );
        assert_eq!(queued(None).as_deref(), Some("In merge queue"));
    }

    #[test]
    fn uses_singular_for_one_commit() {
        assert_eq!(
            branch_fate(Some(Progress::Ahead(1))),
            "The branch has 1 unmerged commit, so it's kept."
        );
        assert_eq!(branch_fate(None), "The branch is kept unless it's merged.");
        assert_eq!(
            progress_text(Progress::Ahead(1), "main"),
            "1 commit not in main"
        );
        let worktree = sample_worktree("me/a", Progress::Ahead(1));
        let badges: Vec<_> = local_badges(&worktree)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(badges, ["↑ 1 commit"]);
    }

    #[test]
    fn formats_age_in_seconds_then_minutes() {
        assert_eq!(format_age(Duration::from_secs(0)), "0s");
        assert_eq!(format_age(Duration::from_secs(59)), "59s");
        assert_eq!(format_age(Duration::from_secs(60)), "1m");
        assert_eq!(format_age(Duration::from_secs(3599)), "59m");
    }

    #[test]
    fn label_chips_pick_readable_text_and_skip_claude_review() {
        let label = |name: &str, rgb| Label {
            name: name.into(),
            rgb,
        };
        let labels = [
            label("bug", (0xd7, 0x3a, 0x4a)),
            label("claude-review", (0xaa, 0xbb, 0xcc)),
            label("docs", (0xfb, 0xca, 0x04)),
        ];
        let chips = label_chips(&labels);
        let text: Vec<_> = chips.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, [" bug ", " ", " docs "]);
        assert_eq!(chips[0].style.fg, Some(Color::White));
        assert_eq!(chips[2].style.fg, Some(Color::Black));
        assert!(label_chips(&labels[1..2]).is_empty());
    }

    #[test]
    fn renders_new_branch_input() {
        let mut app = app_with(vec![]);
        app.new_branch = Some(String::new());
        let screen = render(&mut app);
        assert!(screen.contains("New branch"), "{screen}");
        assert!(screen.contains("→ me/…"), "{screen}");
        assert!(screen.contains("freshly fetched origin/main"), "{screen}");

        app.new_branch = Some("Fix the \"flaky\" CI".into());
        let screen = render(&mut app);
        assert!(screen.contains("Fix the \"flaky\" CI▏"), "{screen}");
        assert!(screen.contains("→ me/fix-the-flaky-ci"), "{screen}");
    }
}
