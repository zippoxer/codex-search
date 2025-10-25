use std::cmp::Ordering;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, TryRecvError, unbounded, Sender};
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
use crate::util::format_relative;

// Limit how many nucleo matches we rescore per refresh.
// Keep this small to avoid blocking the UI thread.
const MAX_TUI_CANDIDATES: usize = 100;
// Limit how many sessions we ingest from the stream each UI tick.
const MAX_INGEST_PER_TICK: usize = 20;
// Coalesce rescoring to at most once per interval to avoid UI spikes.
const MIN_REBUILD_INTERVAL_MS: u64 = 80;

pub struct TuiConfig {
    pub limit: usize,
    pub resume_command: String,
    pub dry_run: bool,
    pub initial_query: String,
    pub empty_status: Option<String>,
    pub total_expected: usize,
    pub filter_cwd: Option<std::path::PathBuf>,
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
        config.filter_cwd.clone(),
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
    filter_cwd: Option<PathBuf>,
) -> Result<AppOutcome> {
    let mut app = App::new(
        session_rx,
        limit,
        initial_query,
        empty_status,
        total_expected,
        filter_cwd,
    );
    loop {
        let ingested = app.ingest_new_sessions();
        if ingested || app.query_dirty || app.results_dirty {
            app.refresh_results()?;
        }

        terminal.draw(|frame| app.draw(frame))?;

        if !event::poll(Duration::from_millis(30))? {
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
    last_rebuild_at: Instant,
    filter_cwd: Option<std::path::PathBuf>,
    worker_tx: Sender<ScoreJob>,
    worker_rx: Receiver<ScoreResult>,
    next_job_id: u64,
    pending_job: Option<u64>,
}

impl App {
    fn new(
        session_rx: Receiver<Session>,
        limit: usize,
        initial_query: String,
        empty_state_message: Option<String>,
        total_expected: usize,
        filter_cwd: Option<std::path::PathBuf>,
    ) -> Self {
        let (job_tx, job_rx) = unbounded::<ScoreJob>();
        let (res_tx, res_rx) = unbounded::<ScoreResult>();
        std::thread::spawn(move || worker_loop(job_rx, res_tx));
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
            last_rebuild_at: Instant::now(),
            filter_cwd,
            worker_tx: job_tx,
            worker_rx: res_rx,
            next_job_id: 1,
            pending_job: None,
        }
    }

    fn refresh_results(&mut self) -> Result<()> {
        // Poll worker results first (non-blocking)
        while let Ok(result) = self.worker_rx.try_recv() {
            if Some(result.id) == self.pending_job {
                self.results = result.results;
                self.pending_job = None;
                self.results_dirty = false;
            }
        }
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
            let now = Instant::now();
            if now.duration_since(self.last_rebuild_at) >= Duration::from_millis(MIN_REBUILD_INTERVAL_MS) {
                self.schedule_rebuild()?;
                self.last_rebuild_at = now;
            }
        }

        if self.stream_finished && self.sessions.is_empty() {
            if let Some(message) = self.empty_state_message.clone() {
                self.message = Some(message);
            }
        }

        Ok(())
    }

    fn schedule_rebuild(&mut self) -> Result<()> {
        // Build candidate list quickly on UI thread
        let mut candidates: Vec<Arc<Session>>;
        if self.query.is_empty() {
            candidates = Vec::with_capacity(self.limit.min(self.sessions.len()));
            for s in self.sessions.iter().take(self.limit) {
                candidates.push(Arc::clone(s));
            }
        } else {
            // For non-empty queries, rescore ALL loaded sessions to match CLI behavior.
            // This runs in a background worker so the UI thread stays responsive.
            candidates = Vec::with_capacity(self.sessions.len());
            for s in &self.sessions {
                candidates.push(Arc::clone(s));
            }
        }

        let id = self.next_job_id;
        self.next_job_id += 1;
        self.pending_job = Some(id);
        let job = ScoreJob { id, query: self.query.clone(), candidates, limit: self.limit };
        // best-effort: ignore send error if worker died
        let _ = self.worker_tx.send(job);

        // If nothing scheduled, keep message state
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
        }
        Ok(())
    }

    fn ingest_new_sessions(&mut self) -> bool {
        let mut updated = false;
        let mut processed = 0usize;
        loop {
            match self.session_rx.try_recv() {
                Ok(session) => {
                    if let Some(filter) = &self.filter_cwd {
                        if let Some(scwd) = &session.cwd {
                            if !paths_related(&normalize_path(scwd), &normalize_path(filter)) {
                                continue; // skip non-matching sessions
                            }
                        } else {
                            continue; // no cwd info: skip when filtering is on
                        }
                    }
                    let session_arc = Arc::new(session);
                    self.injector.push(session_arc.clone(), |item, columns| {
                        columns[0] = Utf32String::from(item.search_blob.as_str());
                    });
                    self.sessions.push(session_arc);
                    updated = true;
                    processed += 1;
                    if processed >= MAX_INGEST_PER_TICK {
                        break;
                    }
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
                "Updated",
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            )),
            Cell::from(Span::styled(
                "Preview",
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            )),
        ];
        let header = Row::new(header_cells).bottom_margin(1);

        let now = OffsetDateTime::now_utc();
        // Estimate available character width for the Preview column so we can split
        // the snippet across two visible lines contiguously.
        let table_inner_width = chunks[1].width.saturating_sub(2); // borders
        let updated_w: u16 = 12;
        let preview_w = table_inner_width
            .saturating_sub(updated_w)
            .saturating_sub(3) // spacing/margins
            .max(20);
        let mut rows: Vec<Row> = self
            .results
            .iter()
            .map(|result| {
                let preview_text = build_preview_text(result, preview_w as usize, &self.query);
                let updated_rel = format_relative(result.match_timestamp(), now);

                Row::new(vec![
                    Cell::from(updated_rel),
                    Cell::from(preview_text),
                ])
                .height(2)
            })
            .collect();

        // Insert horizontal separators between rows
        if !rows.is_empty() {
            let sep1 = "─".repeat(10);
            let sep2 = "─".repeat(preview_w as usize);
            let mut with_seps: Vec<Row> = Vec::with_capacity(rows.len() * 2);
            for (i, r) in rows.into_iter().enumerate() {
                with_seps.push(r);
                if i + 1 < self.results.len() {
                    with_seps.push(
                        Row::new(vec![
                            Cell::from(Span::styled(sep1.clone(), Style::default().fg(Color::DarkGray))),
                            Cell::from(Span::styled(sep2.clone(), Style::default().fg(Color::DarkGray))),
                        ])
                        .height(1),
                    );
                }
            }
            rows = with_seps;
        }

        let widths = [Constraint::Length(updated_w), Constraint::Min(40)];
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

        // When separator rows are present, each result occupies 2 rows (height=2),
        // plus a 1-row separator (except after the last). Map selection accordingly.
        let rendered_index = self.selected * 2 + self.selected.min(self.results.len().saturating_sub(1));
        self.table_state.select(Some(rendered_index));
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
                }
            }
            KeyCode::Up => {
                if !self.results.is_empty() {
                    self.selected = self.selected.saturating_sub(1);
                }
            }
            KeyCode::PageDown => {
                if !self.results.is_empty() {
                    let jump = (self.results.len() / 5).max(5);
                    self.selected = (self.selected + jump).min(self.results.len() - 1);
                }
            }
            KeyCode::PageUp => {
                if !self.results.is_empty() {
                    let jump = (self.results.len() / 5).max(5);
                    self.selected = self.selected.saturating_sub(jump);
                }
            }
            KeyCode::Home => {
                if !self.results.is_empty() {
                    self.selected = 0;
                }
            }
            KeyCode::End => {
                if !self.results.is_empty() {
                    self.selected = self.results.len() - 1;
                }
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match c {
                        'c' | 'C' => return Ok(Some(AppOutcome::Exit)),
                        'u' | 'U' => {
                            self.query.clear();
                            self.query_dirty = true;
                            self.results_dirty = true;
                            return Ok(None);
                        }
                        _ => return Ok(None),
                    }
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

fn normalize_path(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn paths_related(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn build_preview_text(result: &SearchResult, width_chars: usize, query: &str) -> Text<'static> {
    let width = width_chars.max(20);
    let window = width.saturating_mul(2).max(80);

    // Choose source text for the snippet
    let source = result
        .matched_message
        .as_ref()
        .map(|m| m.full_text.as_str())
        .unwrap_or_else(|| result.session.label.as_str());

    let lower = source.to_lowercase();
    let q = query.trim().to_lowercase();

    // Compute match window in chars
    let (start_char, end_char, match_start_char, match_end_char) = if !q.is_empty() {
        if let Some(byte_idx) = lower.find(&q) {
            let start_chars = lower[..byte_idx].chars().count();
            let match_len = q.chars().count();
            let end_chars = start_chars + match_len;
            let total_chars = source.chars().count();
            let desired_start = start_chars.saturating_sub((window.saturating_sub(match_len)) / 2);
            let mut start = desired_start.min(total_chars.saturating_sub(window));
            if window > total_chars { start = 0; }
            let end = (start + window).min(total_chars);
            (start, end, start_chars, end_chars)
        } else {
            (0, source.chars().count().min(window), 0, 0)
        }
    } else {
        (0, source.chars().count().min(window), 0, 0)
    };

    // Compose the full snippet string with ellipses and normalize whitespace
    let chars: Vec<char> = source.chars().collect();
    let mut full = String::new();
    if start_char > 0 { full.push('…'); }
    let match_start = match_start_char.max(start_char).min(end_char);
    let match_end = match_end_char.max(start_char).min(end_char);
    let mut hl_start_rel = None::<usize>;
    let mut hl_end_rel = None::<usize>;

    if start_char < match_start {
        full.extend(chars[start_char..match_start].iter());
    }
    if match_start < match_end {
        hl_start_rel = Some(full.chars().count());
        full.extend(chars[match_start..match_end].iter());
        hl_end_rel = Some(full.chars().count());
    }
    if match_end < end_char {
        full.extend(chars[match_end..end_char].iter());
    }
    if end_char < chars.len() { full.push('…'); }
    full = normalize_ws(&full);

    // Split into two visual lines and apply highlight spans across the split
    let (l1_str, l2_str) = split_visual_two_lines(&full, width);

    // Helper to build a line with highlight between [hstart, hend)
    let build_line = |text: &str, base_offset: usize| -> Line<'static> {
        if let (Some(hs), Some(he)) = (hl_start_rel, hl_end_rel) {
            let hs_rel = hs.saturating_sub(base_offset);
            let he_rel = he.saturating_sub(base_offset);
            let total = text.chars().count();
            let left_end = hs_rel.min(total);
            let right_start = he_rel.min(total);
            let mut segments: Vec<Span> = Vec::new();
            let mut idx = 0usize;
            for (part_start, part_end, style) in [
                (0usize, left_end, None),
                (left_end, right_start, Some(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))),
                (right_start, total, None),
            ] {
                if part_end > part_start {
                    let slice: String = text.chars().skip(part_start).take(part_end - part_start).collect();
                    segments.push(match style {
                        Some(st) => Span::styled(slice, st),
                        None => Span::raw(slice),
                    });
                }
                idx = part_end;
            }
            Line::from(segments)
        } else {
            Line::from(Span::raw(text.to_owned()))
        }
    };

    let line1 = build_line(&l1_str, 0);
    let line2 = build_line(&l2_str, l1_str.chars().count());
    Text::from(vec![line1, line2])
}

fn split_visual_two_lines(s: &str, width: usize) -> (String, String) {
    let total = s.chars().count();
    if total <= width {
        // Pad into two lines by taking up to width and the remainder empty
        return (s.to_owned(), String::new());
    }
    // Prefer breaking at whitespace near the width
    let mut count = 0usize;
    let mut split_idx = 0usize; // byte index
    let mut last_space_byte: Option<usize> = None;
    for (byte_idx, ch) in s.char_indices() {
        if ch.is_whitespace() { last_space_byte = Some(byte_idx); }
        if count >= width { break; }
        split_idx = byte_idx + ch.len_utf8();
        count += 1;
    }
    if let Some(space) = last_space_byte { split_idx = space + 1; }
    let (l1, l2) = s.split_at(split_idx);
    (l1.to_owned(), l2.to_owned())
}

fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last { out.push(' '); last = true; }
        } else {
            out.push(ch);
            last = false;
        }
    }
    out.trim().to_string()
}

fn split_by_chars(s: &str, n: usize) -> (String, String) {
    if n == 0 { return (String::new(), s.to_owned()); }
    let mut left = String::new();
    let mut count = 0usize;
    let mut last_space_idx: Option<usize> = None; // byte index
    for (byte_idx, ch) in s.char_indices() {
        if ch.is_whitespace() { last_space_idx = Some(byte_idx); }
        if count >= n { break; }
        left.push(ch);
        count += 1;
    }
    if count < n || count == s.chars().count() {
        return (s.to_owned(), String::new());
    }
    // Prefer breaking at last whitespace before limit when available.
    let split_byte = if let Some(space_idx) = last_space_idx { space_idx + 1 } else { left.len() };
    let (l, r) = s.split_at(split_byte);
    (l.to_owned(), r.to_owned())
}

fn style_span(text: String, highlighted: bool) -> Span<'static> {
    if highlighted {
        Span::styled(
            text,
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw(text)
    }
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
struct ScoreJob {
    id: u64,
    query: String,
    candidates: Vec<Arc<Session>>,
    limit: usize,
}

struct ScoreResult {
    id: u64,
    results: Vec<SearchResult>,
}

fn worker_loop(job_rx: Receiver<ScoreJob>, res_tx: Sender<ScoreResult>) {
    while let Ok(job) = job_rx.recv() {
        let mut scorer = Scorer::new(&job.query);
        let mut results: Vec<SearchResult> = Vec::new();
        if scorer.is_query_empty() {
            for session in job.candidates.into_iter() {
                if let Some(r) = scorer.score_session_arc(session) {
                    results.push(r);
                }
            }
        } else {
            for session in job.candidates.into_iter() {
                if let Some(r) = scorer.score_session_arc(session) {
                    results.push(r);
                }
            }
        }
        results.sort_by(|a, b| match b.match_timestamp().cmp(&a.match_timestamp()) {
            Ordering::Equal => b.score.cmp(&a.score),
            other => other,
        });
        if results.len() > job.limit { results.truncate(job.limit); }
        let _ = res_tx.send(ScoreResult { id: job.id, results });
    }
}
