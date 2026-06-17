use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::services::dedup::{self, DedupMessage, DedupPhase};
use crate::ui::theme::Theme;

const MAX_LOG_LINES: usize = 10_000;

pub struct DedupScreenState {
    pub target_path: PathBuf,
    pub phase: DedupPhase,
    pub current_file: String,
    pub progress: u8,
    pub log_lines: Vec<String>,
    pub log_scroll: usize,
    pub scanned: usize,
    pub duplicates: usize,
    pub freed: u64,
    pub is_complete: bool,
    pub receiver: Option<Receiver<DedupMessage>>,
    pub cancel_flag: Arc<AtomicBool>,
}

impl DedupScreenState {
    pub fn new(path: PathBuf) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let flag_clone = cancel_flag.clone();
        let path_clone = path.clone();

        std::thread::spawn(move || {
            dedup::run_dedup(path_clone, tx, flag_clone);
        });

        Self {
            target_path: path,
            phase: DedupPhase::Scanning,
            current_file: String::new(),
            progress: 0,
            log_lines: Vec::new(),
            log_scroll: 0,
            scanned: 0,
            duplicates: 0,
            freed: 0,
            is_complete: false,
            receiver: Some(rx),
            cancel_flag,
        }
    }

    fn push_log(&mut self, line: String) {
        if self.log_lines.len() >= MAX_LOG_LINES {
            self.log_lines.remove(0);
            self.log_scroll = self.log_scroll.saturating_sub(1);
        }
        self.log_lines.push(line);
        let max_scroll = self.log_lines.len().saturating_sub(1);
        self.log_scroll = max_scroll;
    }

    fn poll(&mut self) {
        // Collect messages first to avoid borrow conflict
        let messages: Vec<DedupMessage> = if let Some(ref rx) = self.receiver {
            let mut msgs = Vec::new();
            while let Ok(msg) = rx.try_recv() {
                msgs.push(msg);
            }
            msgs
        } else {
            return;
        };

        for msg in messages {
            match msg {
                DedupMessage::Phase(phase) => {
                    self.phase = phase;
                }
                DedupMessage::Scanning(path) => {
                    self.current_file = path;
                }
                DedupMessage::Hashing(path, pct) => {
                    self.current_file = path;
                    self.progress = pct;
                }
                DedupMessage::Deleting(path) => {
                    self.current_file = path;
                }
                DedupMessage::Log(msg) => {
                    self.push_log(msg);
                }
                DedupMessage::Stats {
                    scanned,
                    duplicates,
                    freed,
                } => {
                    self.scanned = scanned;
                    self.duplicates = duplicates;
                    self.freed = freed;
                }
                DedupMessage::Error(msg) => {
                    self.push_log(format!("[ERROR] {}", msg));
                }
                DedupMessage::Complete => {
                    self.is_complete = true;
                    self.receiver = None;
                }
            }
        }
    }
}

pub fn draw(frame: &mut Frame, state: &mut DedupScreenState, area: Rect, theme: &Theme) {
    // Poll messages before drawing
    state.poll();

    let colors = &theme.dedup_screen;

    // Main layout: info(5) + log(min) + footer(1)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // unified info box (header + stats)
            Constraint::Min(3),    // log area
            Constraint::Length(1), // footer
        ])
        .split(area);

    // ── Unified info box ──
    let phase_text = match state.phase {
        DedupPhase::Scanning => "Scanning...",
        DedupPhase::Hashing => "Computing Hashes...",
        DedupPhase::Deleting => "Removing Duplicates...",
        DedupPhase::Complete => "Complete",
    };

    let info_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(colors.border))
        .title(Span::styled(
            " Remove Duplicates ",
            Style::default()
                .fg(colors.title)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(colors.bg));

    // Line 1: target path + phase
    let line1 = Line::from(vec![
        Span::styled("Target: ", Style::default().fg(colors.stats_text)),
        Span::styled(
            state.target_path.display().to_string(),
            Style::default().fg(colors.phase_text),
        ),
        Span::raw("  "),
        Span::styled(
            format!("[{}]", phase_text),
            Style::default()
                .fg(colors.phase_text)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    // Line 2: stats
    let line2 = Line::from(vec![
        Span::styled("Scanned: ", Style::default().fg(colors.stats_text)),
        Span::styled(
            format!("{}", state.scanned),
            Style::default()
                .fg(colors.phase_text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  |  Duplicates removed: ",
            Style::default().fg(colors.stats_text),
        ),
        Span::styled(
            format!("{}", state.duplicates),
            Style::default()
                .fg(colors.log_deleted)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  |  Freed: ", Style::default().fg(colors.stats_text)),
        Span::styled(
            dedup::format_size(state.freed),
            Style::default()
                .fg(colors.phase_text)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let info = Paragraph::new(vec![line1, line2]).block(info_block);
    frame.render_widget(info, chunks[0]);

    // ── Log area ──
    let log_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(colors.border))
        .title(Span::styled(" Log ", Style::default().fg(colors.title)))
        .style(Style::default().bg(colors.bg));

    let inner_height = chunks[1].height.saturating_sub(2) as usize; // borders

    let skip_count = state
        .log_scroll
        .saturating_sub(inner_height.saturating_sub(1));
    let ca = Style::default().fg(colors.log_text);
    let cb = Style::default().fg(colors.log_text_alt);
    let log_lines: Vec<Line> = state
        .log_lines
        .iter()
        .skip(skip_count)
        .take(inner_height)
        .map(|line| {
            if line.starts_with("[ERROR]") {
                Line::from(Span::styled(
                    line.as_str(),
                    Style::default().fg(colors.log_error),
                ))
            } else if line.starts_with("REMOVE ") {
                // REMOVE {hash} {path}
                let rest = &line[7..];
                if let Some(sp) = rest.find(' ') {
                    Line::from(vec![
                        Span::styled("REMOVE ", Style::default().fg(colors.log_deleted)),
                        Span::styled(&rest[..sp], ca),
                        Span::styled(&rest[sp..], cb),
                    ])
                } else {
                    Line::from(Span::styled(
                        line.as_str(),
                        Style::default().fg(colors.log_deleted),
                    ))
                }
            } else if line.starts_with("READING ") {
                // READING {path}
                Line::from(vec![
                    Span::styled("READING ", ca),
                    Span::styled(&line[8..], cb),
                ])
            } else if let Some(pct_pos) = line.find(" % ") {
                // {hash} {pct} % {size} {path}
                let before = &line[..pct_pos];
                if let Some(hash_end) = before.rfind(' ') {
                    let after = &line[pct_pos + 3..];
                    if let Some(size_end_rel) = after.find(' ') {
                        let size_end = pct_pos + 3 + size_end_rel;
                        Line::from(vec![
                            Span::styled(&line[..hash_end], ca),
                            Span::styled(&line[hash_end..pct_pos + 3], cb),
                            Span::styled(&line[pct_pos + 3..size_end], ca),
                            Span::styled(&line[size_end..], cb),
                        ])
                    } else {
                        Line::from(Span::styled(line.as_str(), ca))
                    }
                } else {
                    Line::from(Span::styled(line.as_str(), ca))
                }
            } else {
                Line::from(Span::styled(line.as_str(), ca))
            }
        })
        .collect();

    let log = Paragraph::new(log_lines)
        .block(log_block)
        .wrap(Wrap { trim: false });
    frame.render_widget(log, chunks[1]);

    // ── Footer ──
    let footer_items = if state.is_complete {
        vec![
            Span::styled(
                " Esc",
                Style::default()
                    .fg(colors.footer_key)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Close  ", Style::default().fg(colors.footer_text)),
            Span::styled(
                "Up/Down",
                Style::default()
                    .fg(colors.footer_key)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Scroll  ", Style::default().fg(colors.footer_text)),
            Span::styled(
                "PgUp/PgDn",
                Style::default()
                    .fg(colors.footer_key)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Page scroll", Style::default().fg(colors.footer_text)),
        ]
    } else {
        vec![
            Span::styled(
                " Esc",
                Style::default()
                    .fg(colors.footer_key)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Cancel  ", Style::default().fg(colors.footer_text)),
            Span::styled(
                "Up/Down",
                Style::default()
                    .fg(colors.footer_key)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Scroll  ", Style::default().fg(colors.footer_text)),
            Span::styled(
                "PgUp/PgDn",
                Style::default()
                    .fg(colors.footer_key)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Page scroll", Style::default().fg(colors.footer_text)),
        ]
    };

    let footer = Paragraph::new(Line::from(footer_items)).style(Style::default().bg(colors.bg));
    frame.render_widget(footer, chunks[2]);
}

/// Handle input. Returns true if screen should close.
pub fn handle_input(state: &mut DedupScreenState, code: KeyCode, modifiers: KeyModifiers) -> bool {
    let shift = modifiers.contains(KeyModifiers::SHIFT);
    match code {
        KeyCode::Esc => {
            if state.is_complete {
                return true; // Close screen
            } else {
                // Cancel the operation
                state.cancel_flag.store(true, Ordering::Relaxed);
                // Don't close immediately; wait for Complete message
                // But if receiver is already gone, close now
                if state.receiver.is_none() {
                    return true;
                }
            }
        }
        KeyCode::Up if shift => {
            state.log_scroll = state.log_scroll.saturating_sub(10);
        }
        KeyCode::Down if shift => {
            let max = state.log_lines.len().saturating_sub(1);
            state.log_scroll = (state.log_scroll + 10).min(max);
        }
        KeyCode::Up => {
            state.log_scroll = state.log_scroll.saturating_sub(1);
        }
        KeyCode::Down => {
            let max = state.log_lines.len().saturating_sub(1);
            if state.log_scroll < max {
                state.log_scroll += 1;
            }
        }
        KeyCode::PageUp => {
            state.log_scroll = state.log_scroll.saturating_sub(10);
        }
        KeyCode::PageDown => {
            let max = state.log_lines.len().saturating_sub(1);
            state.log_scroll = (state.log_scroll + 10).min(max);
        }
        _ => {}
    }
    false
}
