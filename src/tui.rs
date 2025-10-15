use std::cmp::Ordering;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, TryRecvError};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{execute, terminal};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use time::OffsetDateTime;

use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Injector, Nucleo, Utf32String};

use crate::cli::spawn_resume_command;
use crate::discovery::SessionStream;
use crate::search::Scorer;
use crate::session::{SearchResult, Session};
use crate::util::{format_relative, format_time_of_day};

pub struct TuiConfig {
    pub limit: usize,
    pub resume_command: String,
    pub dry_run: bool,
    pub initial_query: String,
    pub empty_status: Option<String>,
    pub total_expected: usize,
}

enum AppOutcome {
    Exit,
    Selected(String),
}

pub fn run(config: TuiConfig, stream: SessionStream) -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        event::EnableMouseCapture
    )
    .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;
    terminal.clear()?;

    let session_rx = stream.receiver();

    let outcome = run_app(
        &mut terminal,
        session_rx,
        config.limit,
        config.initial_query.clone(),
        config.empty_status.clone(),
        config.total_expected,
    );

    terminal.show_cursor()?;
    disable_raw_mode().context("failed to disable raw mode")?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        terminal::LeaveAlternateScreen,
        event::DisableMouseCapture
    )
    .context("failed to leave alternate screen")?;

    stream.join();

    match outcome? {
        AppOutcome::Exit => Ok(()),
        AppOutcome::Selected(uuid) => {
            if config.dry_run {
                println!("{}", config.resume_command.replace("{uuid}", &uuid));
                Ok(())
            } else {
                spawn_resume_command(&config.resume_command, &uuid)
            }
        }
    }
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    session_rx: Receiver<Session>,
    limit: usize,
    initial_query: String,
    empty_status: Option<String>,
    total_expected: usize,
) -> Result<AppOutcome> {
    let mut app = App::new(
        session_rx,
        limit,
        initial_query,
        empty_status,
        total_expected,
    );
    loop {
        let ingested = app.ingest_new_sessions();
        if ingested || app.query_dirty || app.results_dirty {
            app.refresh_results()?;
        }

        terminal.draw(|frame| app.draw(frame))?;

        if !event::poll(Duration::from_millis(120))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) => {
                if key.kind == KeyEventKind::Press {
                    if let Some(outcome) = app.on_key(key)? {
                        return Ok(outcome);
                    }
                }
            }
            Event::Resize(_, _) => {
                // redraw next loop iteration
            }
            Event::Mouse(_) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
        }
    }
}

struct App {
    sessions: Vec<Arc<Session>>,
    results: Vec<SearchResult>,
    query: String,
    limit: usize,
    selected: usize,
    table_state: TableState,
    message: Option<String>,
    empty_state_message: Option<String>,
    session_rx: Receiver<Session>,
    stream_finished: bool,
    total_expected: usize,
    nucleo: Nucleo<Arc<Session>>,
    injector: Injector<Arc<Session>>,
    last_query: String,
    query_dirty: bool,
    results_dirty: bool,
}

impl App {
    fn new(
        session_rx: Receiver<Session>,
        limit: usize,
        initial_query: String,
        empty_state_message: Option<String>,
        total_expected: usize,
    ) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        let initial_message = empty_state_message.clone().or_else(|| {
            if total_expected > 0 {
                Some("Indexing sessions…".into())
            } else {
                None
            }
        });

        let notify: Arc<dyn Fn() + Sync + Send> = Arc::new(|| {});
        let nucleo: Nucleo<Arc<Session>> = Nucleo::new(Config::DEFAULT, notify, None, 1);
        let injector = nucleo.injector();

        Self {
            sessions: Vec::new(),
            results: Vec::new(),
            query: initial_query.clone(),
            limit,
            selected: 0,
            table_state,
            message: initial_message,
            empty_state_message,
            session_rx,
            stream_finished: false,
            total_expected,
            nucleo,
            injector,
            last_query: initial_query,
            query_dirty: true,
            results_dirty: true,
        }
    }

    fn refresh_results(&mut self) -> Result<()> {
        if self.query_dirty {
            let append = self.query.len() > self.last_query.len()
                && self.query.starts_with(&self.last_query);
            self.nucleo.pattern.reparse(
                0,
                &self.query,
                CaseMatching::Smart,
                Normalization::Smart,
                append,
            );
            self.last_query = self.query.clone();
            self.query_dirty = false;
            self.results_dirty = true;
        }

        let status = self.nucleo.tick(1);

        if self.results_dirty || status.changed {
            self.rebuild_results()?;
        }

        if self.stream_finished && self.sessions.is_empty() {
            if let Some(message) = self.empty_state_message.clone() {
                self.message = Some(message);
            }
        }

        Ok(())
    }

    fn rebuild_results(&mut self) -> Result<()> {
        let mut scorer = Scorer::new(&self.query);
        self.results.clear();

        if scorer.is_query_empty() {
            for session in self.sessions.iter().take(self.limit) {
                if let Some(result) = scorer.score_session_arc(Arc::clone(session)) {
                    self.results.push(result);
                }
            }
        } else {
            let snapshot = self.nucleo.snapshot();
            let take = usize::min(self.limit, snapshot.matched_item_count() as usize);
            for idx in 0..take {
                if let Some(item) = snapshot.get_matched_item(idx as u32) {
                    let session_arc = Arc::clone(item.data);
                    if let Some(result) = scorer.score_session_arc(session_arc) {
                        self.results.push(result);
                    }
                }
            }
        }

        self.results
            .sort_by(|a, b| match b.match_timestamp().cmp(&a.match_timestamp()) {
                Ordering::Equal => b.score.cmp(&a.score),
                other => other,
            });

        if self.results.is_empty() {
            self.selected = 0;
            self.table_state.select(None);
            self.message = if self.sessions.is_empty() {
                self.empty_state_message.clone().or_else(|| {
                    if self.stream_finished {
                        Some("No matches".into())
                    } else {
                        Some("Indexing sessions…".into())
                    }
                })
            } else if self.stream_finished {
                Some("No matches".into())
            } else {
                Some("Indexing sessions…".into())
            };
        } else {
            self.message = None;
            if self.selected >= self.results.len() {
                self.selected = self.results.len().saturating_sub(1);
            }
            self.table_state.select(Some(self.selected));
        }

        self.results_dirty = false;
        Ok(())
    }

    fn ingest_new_sessions(&mut self) -> bool {
        let mut updated = false;
        loop {
            match self.session_rx.try_recv() {
                Ok(session) => {
                    let session_arc = Arc::new(session);
                    self.injector.push(session_arc.clone(), |item, columns| {
                        columns[0] = Utf32String::from(item.search_blob.as_str());
                    });
                    self.sessions.push(session_arc);
                    updated = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.stream_finished = true;
                    updated = true;
                    break;
                }
            }
        }
        if updated {
            self.results_dirty = true;
        }
        updated
    }

    fn draw(&mut self, frame: &mut Frame) {
        let size = frame.size();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(size);

        frame.render_widget(self.search_widget(), chunks[0]);
        let header_cells = [
            Cell::from(Span::styled(
                "Preview",
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            )),
            Cell::from(Span::styled(
                "Message",
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            )),
            Cell::from(Span::styled(
                "Updated",
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            )),
            Cell::from(Span::styled(
                "Created",
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            )),
        ];
        let header = Row::new(header_cells).bottom_margin(1);

        let now = OffsetDateTime::now_utc();
        let rows: Vec<Row> = self
            .results
            .iter()
            .map(|result| {
                let preview_text = build_preview_text(result);
                let message_rel = result
                    .matched_message
                    .as_ref()
                    .and_then(|m| m.timestamp)
                    .or(result.session.latest_message_time)
                    .map(|dt| format_relative(dt, now))
                    .unwrap_or_else(|| "--".into());
                let updated_rel = format_relative(result.session.updated_at, now);
                let created_rel = result
                    .session
                    .created_at
                    .map(|dt| format_relative(dt, now))
                    .unwrap_or_else(|| "--".into());

                Row::new(vec![
                    Cell::from(preview_text),
                    Cell::from(message_rel),
                    Cell::from(updated_rel),
                    Cell::from(created_rel),
                ])
                .height(2)
            })
            .collect();

        let widths = [
            Constraint::Percentage(60),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
        ];
        let shown = self.results.len();
        let indexed = self.sessions.len();
        let total = if self.total_expected == 0 {
            indexed
        } else {
            self.total_expected
        };
        let table = Table::new(rows, widths)
            .header(header)
            .block(Block::default().borders(Borders::ALL).title(format!(
                "Results: {shown} shown • {indexed}/{total} indexed"
            )))
            .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::Yellow))
            .highlight_symbol("▶ ");

        frame.render_stateful_widget(table, chunks[1], &mut self.table_state);
        frame.render_widget(self.status_widget(), chunks[2]);
    }

    fn search_widget(&self) -> Paragraph<'static> {
        let prompt = Span::styled(
            "> ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        let query = Span::raw(self.query.clone());
        let text = Line::from(vec![prompt, query]);
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("Search"))
    }

    fn status_widget(&self) -> Paragraph<'static> {
        let message = if let Some(msg) = &self.message {
            msg.clone()
        } else {
            let total = if self.total_expected == 0 {
                self.sessions.len()
            } else {
                self.total_expected
            };
            let progress = if self.stream_finished || self.sessions.len() >= total {
                format!("Indexed {}/{} sessions", self.sessions.len(), total)
            } else {
                format!("Indexing {}/{} sessions…", self.sessions.len(), total)
            };
            format!("Enter: open • Esc: quit • {}", progress)
        };
        Paragraph::new(message).style(Style::default().fg(Color::Gray))
    }

    fn on_key(&mut self, key: KeyEvent) -> Result<Option<AppOutcome>> {
        match key.code {
            KeyCode::Esc => return Ok(Some(AppOutcome::Exit)),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(Some(AppOutcome::Exit));
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.clear();
                self.query_dirty = true;
                self.results_dirty = true;
                return Ok(None);
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                truncate_last_word(&mut self.query);
                self.query_dirty = true;
                self.results_dirty = true;
                return Ok(None);
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.query_dirty = true;
                self.results_dirty = true;
                return Ok(None);
            }
            KeyCode::Enter => {
                if let Some(result) = self.results.get(self.selected) {
                    return Ok(Some(AppOutcome::Selected(result.session.uuid.clone())));
                }
            }
            KeyCode::Down => {
                if !self.results.is_empty() {
                    self.selected = (self.selected + 1).min(self.results.len() - 1);
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::Up => {
                if !self.results.is_empty() {
                    self.selected = self.selected.saturating_sub(1);
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::PageDown => {
                if !self.results.is_empty() {
                    let jump = (self.results.len() / 5).max(5);
                    self.selected = (self.selected + jump).min(self.results.len() - 1);
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::PageUp => {
                if !self.results.is_empty() {
                    let jump = (self.results.len() / 5).max(5);
                    self.selected = self.selected.saturating_sub(jump);
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::Home => {
                if !self.results.is_empty() {
                    self.selected = 0;
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::End => {
                if !self.results.is_empty() {
                    self.selected = self.results.len() - 1;
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(None);
                }
                self.query.push(c);
                self.query_dirty = true;
                self.results_dirty = true;
            }
            KeyCode::Tab => {}
            _ => {}
        }
        Ok(None)
    }
}

fn build_preview_text(result: &SearchResult) -> Text<'static> {
    let session = result.session.as_ref();
    let mut lines = Vec::new();

    let mut metadata: Vec<Span> = Vec::new();
    let label = session.label.trim();
    if !label.is_empty() && !label.eq_ignore_ascii_case("rollout") {
        metadata.push(Span::styled(
            label.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }

    if let Some(message) = result
        .matched_message
        .as_ref()
        .or_else(|| session.preview())
    {
        let role = match message.role {
            crate::session::MessageRole::User => "you",
            crate::session::MessageRole::Assistant => "codex",
        };
        metadata.push(Span::styled(
            role.to_string(),
            Style::default().fg(Color::Gray),
        ));

        if let Some(ts) = message
            .timestamp
            .or(session.latest_message_time)
            .map(|dt| format_time_of_day(dt))
        {
            metadata.push(Span::raw(ts));
        }
    } else if let Some(ts) = session.latest_message_time.map(|dt| format_time_of_day(dt)) {
        metadata.push(Span::raw(ts));
    }

    if metadata.is_empty() {
        metadata.push(Span::raw(session.uuid.clone()));
    }

    let mut header_spans: Vec<Span> = Vec::new();
    for (idx, span) in metadata.into_iter().enumerate() {
        if idx > 0 {
            header_spans.push(Span::raw("  ·  "));
        }
        header_spans.push(span);
    }
    lines.push(Line::from(header_spans));

    let mut snippet_spans: Vec<Span> = result
        .snippet
        .segments
        .iter()
        .map(|seg| {
            if seg.highlighted {
                Span::styled(
                    seg.text.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw(seg.text.clone())
            }
        })
        .collect();

    if snippet_spans.is_empty() {
        snippet_spans.push(Span::raw(""));
    }

    lines.push(Line::from(snippet_spans));

    Text::from(lines)
}

fn truncate_last_word(buffer: &mut String) {
    if buffer.is_empty() {
        return;
    }
    let mut trimmed = buffer.trim_end().to_string();
    while let Some(ch) = trimmed.pop() {
        if ch.is_whitespace() {
            break;
        }
    }
    *buffer = trimmed.trim_end().to_string();
}
