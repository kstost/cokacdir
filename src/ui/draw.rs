use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use super::{
    app::{App, PanelSide, Screen},
    dialogs,
    file_editor,
    file_info,
    file_viewer,
    panel,
    process_manager,
    ai_screen,
    system_info,
    advanced_search,
    image_viewer,
    search_result,
    theme::Theme,
};

const APP_TITLE: &str = "COKACDIR v0.4.1";

pub fn draw(frame: &mut Frame, app: &mut App) {
    let theme = Theme::default();
    let area = frame.area();

    // Check if terminal is too large for ratatui buffer
    let frame_area = frame.area();
    if (frame_area.width as u32 * frame_area.height as u32) > 65534 {
        // Terminal too large, show warning message only
        let msg = Paragraph::new("Terminal too large. Please resize smaller.")
            .style(theme.warning_style());
        let safe_rect = Rect::new(0, 0, frame_area.width.min(80), 1);
        frame.render_widget(msg, safe_rect);
        return;
    }

    // Clear the entire screen first for full-screen views
    match app.current_screen {
        Screen::AIScreen | Screen::SystemInfo => {
            frame.render_widget(Clear, area);
        }
        _ => {}
    }

    match app.current_screen {
        Screen::DualPanel => draw_dual_panel(frame, app, area, &theme),
        Screen::FileViewer => file_viewer::draw(frame, app, area, &theme),
        Screen::FileEditor => file_editor::draw(frame, app, area, &theme),
        Screen::FileInfo => file_info::draw(frame, app, area, &theme),
        Screen::ProcessManager => process_manager::draw(frame, app, area, &theme),
        Screen::Help => draw_help(frame, app, area, &theme),
        Screen::AIScreen => {
            if let Some(ref mut state) = app.ai_state {
                ai_screen::draw(frame, state, area, &theme);
            }
        }
        Screen::SystemInfo => {
            system_info::draw(frame, &app.system_info_state, area, &theme);
        }
        Screen::ImageViewer => {
            image_viewer::draw(frame, app, area, &theme);
        }
        Screen::SearchResult => {
            search_result::draw(frame, &mut app.search_result_state, area, &theme);
        }
    }

    // Draw advanced search dialog overlay if active
    if app.advanced_search_state.active && app.current_screen == Screen::DualPanel {
        advanced_search::draw(frame, &app.advanced_search_state, area, &theme);
    }

    // Update message timer
    if app.message_timer > 0 {
        app.message_timer -= 1;
        if app.message_timer == 0 {
            app.message = None;
        }
    }
}

fn draw_dual_panel(frame: &mut Frame, app: &mut App, area: Rect, theme: &Theme) {
    // Layout: Header, Panels, Status Bar, Function Bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Header
            Constraint::Min(5),    // Panels
            Constraint::Length(1), // Status bar
            Constraint::Length(1), // Function bar / message
        ])
        .split(area);

    // Header (app title only)
    let header = Line::from(Span::styled(
        format!(" {} ", APP_TITLE),
        Style::default()
            .fg(theme.border_active)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(header), chunks[0]);

    // Dual panels
    let panel_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    panel::draw(
        frame,
        &mut app.left_panel,
        panel_chunks[0],
        app.active_panel == PanelSide::Left && app.dialog.is_none(),
        theme,
    );
    panel::draw(
        frame,
        &mut app.right_panel,
        panel_chunks[1],
        app.active_panel == PanelSide::Right && app.dialog.is_none(),
        theme,
    );

    // Status bar
    draw_status_bar(frame, app, chunks[2], theme);

    // Function bar or message
    draw_function_bar(frame, app, chunks[3], theme);

    // Dialog overlay
    if let Some(ref dialog) = app.dialog {
        dialogs::draw_dialog(frame, dialog, area, theme);
    }
}

/// Public function for drawing dual panel background (used by overlay screens)
pub fn draw_dual_panel_background(frame: &mut Frame, app: &mut App, area: Rect, theme: &Theme) {
    draw_dual_panel(frame, app, area, theme);
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let panel = app.active_panel();
    let current_file = panel.current_file();

    let left_text = if let Some(file) = current_file {
        if file.name != ".." {
            format!(
                "{} ({})",
                file.name,
                crate::utils::format::format_size(file.size)
            )
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let selected_count = panel.selected_files.len();
    let total_size: u64 = panel
        .files
        .iter()
        .filter(|f| !f.is_directory)
        .map(|f| f.size)
        .sum();

    let right_text = if selected_count > 0 {
        format!(
            "{} selected, Total: {}",
            selected_count,
            crate::utils::format::format_size(total_size)
        )
    } else {
        format!("Total: {}", crate::utils::format::format_size(total_size))
    };

    let status = Line::from(vec![
        Span::styled(format!(" {} ", left_text), theme.status_bar_style()),
        Span::styled(
            " ".repeat(area.width.saturating_sub(left_text.len() as u16 + right_text.len() as u16 + 4) as usize),
            theme.status_bar_style(),
        ),
        Span::styled(format!(" {} ", right_text), theme.status_bar_style()),
    ]);

    frame.render_widget(Paragraph::new(status).style(theme.status_bar_style()), area);
}

fn draw_function_bar(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    // Show message if present
    if let Some(ref msg) = app.message {
        let message = Paragraph::new(Span::styled(
            format!(" {} ", msg),
            theme.warning_style(),
        ));
        frame.render_widget(message, area);
        return;
    }

    // 단축키: 첫 글자 강조 스타일 (상단+하단 통합)
    let shortcuts = [
        ("h", "elp "),
        ("o", "info "),
        ("v", "iew "),
        ("e", "dit "),
        ("c", "opy "),
        ("m", "ove "),
        ("k", "mkdir "),
        ("x", "del "),
        ("r", "en "),
        ("f", "ind "),
        (".", "AI "),
        ("i", "nfo "),
        ("p", "roc "),
        ("q", "uit"),
    ];

    let mut spans = Vec::new();
    for (key, rest) in shortcuts {
        spans.push(Span::styled(key, theme.header_style()));
        spans.push(Span::styled(rest, theme.dim_style()));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_help(frame: &mut Frame, app: &mut App, area: Rect, theme: &Theme) {
    // First draw the dual panel in background
    draw_dual_panel(frame, app, area, theme);

    // Build styled help content
    let mut lines: Vec<Line> = Vec::new();

    // Helper to create section header
    let section = |title: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled("── ".to_string(), Style::default().fg(theme.text_dim)),
            Span::styled(title.to_string(), Style::default().fg(theme.info).add_modifier(Modifier::BOLD)),
            Span::styled(" ──".to_string(), Style::default().fg(theme.text_dim)),
        ])
    };

    // Helper to create key-description line
    let key_line = |key: &str, desc: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {:14}", key), Style::default().fg(theme.success)),
            Span::styled(desc.to_string(), theme.normal_style()),
        ])
    };

    // Navigation section
    lines.push(section("Navigation"));
    lines.push(key_line("↑/↓", "Move cursor"));
    lines.push(key_line("PgUp/PgDn", "Move page"));
    lines.push(key_line("Home/End", "Go to start/end"));
    lines.push(key_line("Enter", "Open directory"));
    lines.push(key_line("Esc", "Go to parent"));
    lines.push(key_line("Tab", "Switch panel"));
    lines.push(Line::from(""));

    // Selection & Search section
    lines.push(section("Selection & Search"));
    lines.push(key_line("Space", "Select/deselect"));
    lines.push(key_line("*", "Select/deselect all"));
    lines.push(key_line("f", "Quick find"));
    lines.push(Line::from(""));

    // Sorting section
    lines.push(section("Sorting"));
    lines.push(key_line("n / s / d", "Name / Size / Date"));
    lines.push(Line::from(""));

    // Tools section
    lines.push(section("Tools"));
    lines.push(key_line(".", "AI Assistant"));
    lines.push(key_line("i", "System/Disk Info"));
    lines.push(key_line("p", "Process Manager"));
    lines.push(Line::from(""));

    // File Operations section
    lines.push(section("File Operations"));
    lines.push(Line::from(vec![
        Span::styled("  h", Style::default().fg(theme.success)),
        Span::styled("elp ", theme.dim_style()),
        Span::styled("o", Style::default().fg(theme.success)),
        Span::styled("info ", theme.dim_style()),
        Span::styled("v", Style::default().fg(theme.success)),
        Span::styled("iew ", theme.dim_style()),
        Span::styled("e", Style::default().fg(theme.success)),
        Span::styled("dit ", theme.dim_style()),
        Span::styled("c", Style::default().fg(theme.success)),
        Span::styled("opy ", theme.dim_style()),
        Span::styled("m", Style::default().fg(theme.success)),
        Span::styled("ove", theme.dim_style()),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  k", Style::default().fg(theme.success)),
        Span::styled("mkdir ", theme.dim_style()),
        Span::styled("x", Style::default().fg(theme.success)),
        Span::styled("del ", theme.dim_style()),
        Span::styled("r", Style::default().fg(theme.success)),
        Span::styled("ename ", theme.dim_style()),
        Span::styled("q", Style::default().fg(theme.success)),
        Span::styled("uit", theme.dim_style()),
    ]));
    lines.push(Line::from(""));

    // Footer
    lines.push(Line::from(Span::styled(
        "Press any key to close",
        theme.dim_style(),
    )));

    // Calculate dialog size based on content
    let content_height = (lines.len() + 2) as u16;
    let width = 50u16.min(area.width.saturating_sub(4));
    let height = content_height.min(area.height.saturating_sub(2));

    // Need minimum size to display anything useful
    if width < 30 || height < 10 {
        return;
    }

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    // Clear the area
    frame.render_widget(Clear, dialog_area);

    let block = Block::default()
        .title(" Help ")
        .title_style(Style::default().fg(theme.border_active).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(theme.border_style(true));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, dialog_area);
}
