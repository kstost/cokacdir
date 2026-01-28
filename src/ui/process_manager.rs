use crossterm::event::KeyCode;
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

use super::{app::{App, Screen}, theme::Theme};
use crate::services::process::{self, SortField};

pub fn draw(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style(true));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 5 {
        return;
    }

    // Header
    let header = Line::from(vec![
        Span::styled(" Process Manager ", theme.header_style()),
        Span::styled(
            format!(" [{} processes]", app.processes.len()),
            theme.dim_style(),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    // Column headers
    let sort_indicator = |field: SortField| -> &str {
        if app.process_sort_field == field {
            if app.process_sort_asc { "\u{2191}" } else { "\u{2193}" }
        } else {
            " "
        }
    };

    let pid_width = 7;
    let user_width = 10;
    let cpu_width = 6;
    let mem_width = 6;
    let command_width = inner.width.saturating_sub(pid_width + user_width + cpu_width + mem_width + 4) as usize;

    let col_header = Line::from(vec![
        Span::styled(
            format!("PID{:width$}", sort_indicator(SortField::Pid), width = pid_width as usize - 3),
            theme.header_style(),
        ),
        Span::styled(format!("{:width$}", "USER", width = user_width as usize), theme.header_style()),
        Span::styled(
            format!("{:>width$}", format!("CPU{}", sort_indicator(SortField::Cpu)), width = cpu_width as usize),
            theme.header_style(),
        ),
        Span::styled(
            format!("{:>width$}", format!("MEM{}", sort_indicator(SortField::Mem)), width = mem_width as usize),
            theme.header_style(),
        ),
        Span::styled(
            format!("  COMMAND{}", sort_indicator(SortField::Command)),
            theme.header_style(),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(col_header),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );

    // Process list
    let list_height = (inner.height - 5) as usize;
    let start_index = app.process_selected_index.saturating_sub(list_height / 2);
    let start_index = start_index.min(app.processes.len().saturating_sub(list_height));

    for (i, proc) in app.processes.iter().skip(start_index).take(list_height).enumerate() {
        let actual_index = start_index + i;
        let is_cursor = actual_index == app.process_selected_index;

        let style = if is_cursor {
            theme.selected_style()
        } else {
            theme.normal_style()
        };

        let line = Line::from(vec![
            Span::styled(format!("{:width$}", proc.pid, width = pid_width as usize), style),
            Span::styled(
                format!("{:width$}", truncate(&proc.user, user_width as usize - 1), width = user_width as usize),
                style,
            ),
            Span::styled(format!("{:>width$.1}", proc.cpu, width = cpu_width as usize), style),
            Span::styled(format!("{:>width$.1}", proc.mem, width = mem_width as usize), style),
            Span::styled(
                format!("  {}", truncate(&proc.command, command_width)),
                style,
            ),
        ]);

        frame.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, inner.y + 2 + i as u16, inner.width, 1),
        );
    }

    // 스크롤바
    let total_processes = app.processes.len();
    if total_processes > list_height {
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));

        let mut scrollbar_state = ScrollbarState::new(total_processes)
            .position(app.process_selected_index);

        let scrollbar_area = Rect::new(
            inner.x + inner.width - 1,
            inner.y + 2,
            1,
            list_height as u16,
        );

        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }

    // Confirm dialog
    if let Some(pid) = app.process_confirm_kill {
        let confirm_text = if app.process_force_kill {
            format!("Force kill process {}? (y/n)", pid)
        } else {
            format!("Kill process {}? (y/n)", pid)
        };
        let confirm_line = Line::from(Span::styled(confirm_text, theme.warning_style()));
        frame.render_widget(
            Paragraph::new(confirm_line).alignment(ratatui::layout::Alignment::Center),
            Rect::new(inner.x, inner.y + inner.height - 3, inner.width, 1),
        );
    }

    // Footer - 첫 글자 강조 스타일
    let mut footer_spans = vec![];

    // 기본 명령어
    let commands = [
        ("k", "ill "),
        ("K", "ill! "),
        ("r", "efresh "),
        ("q", "uit "),
    ];

    for (key, rest) in commands {
        footer_spans.push(Span::styled(key, theme.header_style()));
        footer_spans.push(Span::styled(rest, theme.dim_style()));
    }

    // 구분자
    footer_spans.push(Span::styled("| sort: ", theme.dim_style()));

    // 정렬 옵션
    let sort_options = [
        ("p", "id "),
        ("c", "pu "),
        ("m", "em "),
        ("n", "ame"),
    ];

    for (key, rest) in sort_options {
        footer_spans.push(Span::styled(key, theme.header_style()));
        footer_spans.push(Span::styled(rest, theme.dim_style()));
    }

    let footer = Line::from(footer_spans);
    frame.render_widget(
        Paragraph::new(footer),
        Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
    );
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() > max_len {
        let truncated: String = s.chars().take(max_len.saturating_sub(3)).collect();
        format!("{}...", truncated)
    } else {
        s.to_string()
    }
}

pub fn handle_input(app: &mut App, code: KeyCode) {
    // Handle confirm dialog
    if app.process_confirm_kill.is_some() {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(pid) = app.process_confirm_kill {
                    let result = if app.process_force_kill {
                        process::force_kill_process(pid)
                    } else {
                        process::kill_process(pid)
                    };
                    match result {
                        Ok(_) => app.show_message(&format!("Process {} killed", pid)),
                        Err(e) => app.show_message(&format!("Error: {}", e)),
                    }
                    // Refresh process list
                    app.processes = process::get_process_list();
                    sort_processes(app);
                }
                app.process_confirm_kill = None;
                app.process_force_kill = false;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.process_confirm_kill = None;
                app.process_force_kill = false;
            }
            _ => {}
        }
        return;
    }

    match code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
            app.current_screen = Screen::DualPanel;
        }
        KeyCode::Up => {
            if app.process_selected_index > 0 {
                app.process_selected_index -= 1;
            }
        }
        KeyCode::Down => {
            if app.process_selected_index < app.processes.len().saturating_sub(1) {
                app.process_selected_index += 1;
            }
        }
        KeyCode::PageUp => {
            app.process_selected_index = app.process_selected_index.saturating_sub(10);
        }
        KeyCode::PageDown => {
            app.process_selected_index = (app.process_selected_index + 10)
                .min(app.processes.len().saturating_sub(1));
        }
        KeyCode::Home => {
            app.process_selected_index = 0;
        }
        KeyCode::End => {
            app.process_selected_index = app.processes.len().saturating_sub(1);
        }
        KeyCode::Char('p') | KeyCode::Char('P') => {
            toggle_sort(app, SortField::Pid);
        }
        KeyCode::Char('c') | KeyCode::Char('C') => {
            toggle_sort(app, SortField::Cpu);
        }
        KeyCode::Char('m') | KeyCode::Char('M') => {
            toggle_sort(app, SortField::Mem);
        }
        KeyCode::Char('n') | KeyCode::Char('N') => {
            toggle_sort(app, SortField::Command);
        }
        KeyCode::Char('k') => {
            // 일반 kill (SIGTERM)
            if let Some(proc) = app.processes.get(app.process_selected_index) {
                app.process_confirm_kill = Some(proc.pid);
                app.process_force_kill = false;
            }
        }
        KeyCode::Char('K') => {
            // Force kill (SIGKILL)
            if let Some(proc) = app.processes.get(app.process_selected_index) {
                app.process_confirm_kill = Some(proc.pid);
                app.process_force_kill = true;
            }
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            app.processes = process::get_process_list();
            sort_processes(app);
            app.show_message("Refreshed");
        }
        _ => {}
    }
}

fn toggle_sort(app: &mut App, field: SortField) {
    if app.process_sort_field == field {
        app.process_sort_asc = !app.process_sort_asc;
    } else {
        app.process_sort_field = field;
        app.process_sort_asc = field == SortField::Pid || field == SortField::Command;
    }
    sort_processes(app);
}

fn sort_processes(app: &mut App) {
    let field = app.process_sort_field;
    let asc = app.process_sort_asc;

    app.processes.sort_by(|a, b| {
        let cmp = match field {
            SortField::Pid => a.pid.cmp(&b.pid),
            SortField::Cpu => a.cpu.partial_cmp(&b.cpu).unwrap_or(std::cmp::Ordering::Equal),
            SortField::Mem => a.mem.partial_cmp(&b.mem).unwrap_or(std::cmp::Ordering::Equal),
            SortField::Command => a.command.cmp(&b.command),
        };
        if asc { cmp } else { cmp.reverse() }
    });
}
