use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

pub struct WhyUiModel {
    pub package: String,
    pub base: String,
    pub paths: Vec<Vec<WhyUiItem>>,
    pub cycles: Vec<Vec<String>>,
}

pub struct WhyUiItem {
    pub label: String,
    pub detail: String,
}

pub fn run(model: WhyUiModel) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("interactive why UI requires a terminal");
    }
    if model.paths.is_empty() || model.paths.iter().any(Vec::is_empty) {
        bail!("why UI has no impact path to display");
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let result = run_loop(&mut terminal, &model);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    model: &WhyUiModel,
) -> Result<()> {
    let mut state = ListState::default().with_selected(Some(0));
    let mut path_index = 0;

    loop {
        terminal.draw(|frame| {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(4),
                    Constraint::Length(1),
                ])
                .split(frame.area());
            let header = Paragraph::new(Line::from(vec![
                "Why ".into(),
                model.package.clone().bold().cyan(),
                " is affected".into(),
                format!("  path {}/{}", path_index + 1, model.paths.len()).yellow(),
                format!("  base: {}", model.base).dark_gray(),
            ]))
            .block(Block::default().borders(Borders::ALL));
            frame.render_widget(header, rows[0]);

            let body = if rows[1].width >= 90 {
                Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
                    .split(rows[1])
            } else {
                Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .split(rows[1])
            };
            let path = &model.paths[path_index];
            let items: Vec<_> = path
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let prefix = if index == 0 {
                        "● "
                    } else if index + 1 == path.len() {
                        "◆ "
                    } else {
                        "↓ "
                    };
                    ListItem::new(format!("{prefix}{}", item.label))
                })
                .collect();
            let list = List::new(items)
                .block(Block::default().title("Impact path").borders(Borders::ALL))
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );
            frame.render_stateful_widget(list, body[0], &mut state);

            let selected = state.selected().unwrap_or_default();
            let item = &path[selected];
            let mut details = vec![Line::from(item.detail.clone())];
            for cycle in &model.cycles {
                if cycle.iter().any(|member| {
                    item.label == *member || item.label.starts_with(&format!("{member}#"))
                }) {
                    details.push(Line::from(""));
                    details.push(Line::styled(
                        "Runtime module cycle",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ));
                    details.extend(cycle.iter().map(|member| Line::from(format!("  {member}"))));
                }
            }
            let detail = Paragraph::new(Text::from(details))
                .block(
                    Block::default()
                        .title("Selected node")
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(detail, body[1]);
            frame.render_widget(
                Paragraph::new(
                    "↑/↓ or j/k steps   ←/→ or h/l paths   g/G first/last   q or Esc quit",
                )
                .style(Style::default().fg(Color::DarkGray)),
                rows[2],
            );
        })?;

        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            let selected = state.selected().unwrap_or_default();
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Down | KeyCode::Char('j') => {
                    state.select(Some((selected + 1).min(model.paths[path_index].len() - 1)));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    state.select(Some(selected.saturating_sub(1)));
                }
                KeyCode::Char('g') | KeyCode::Home => state.select(Some(0)),
                KeyCode::Char('G') | KeyCode::End => {
                    state.select(Some(model.paths[path_index].len() - 1));
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    path_index = (path_index + 1).min(model.paths.len() - 1);
                    state.select(Some(0));
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    path_index = path_index.saturating_sub(1);
                    state.select(Some(0));
                }
                _ => {}
            }
        }
    }
}
