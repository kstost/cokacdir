use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use super::{
    advanced_search, ai_screen,
    app::{App, Screen},
    dedup_screen, dialogs, diff_file_view, diff_screen, file_editor, file_info, file_viewer,
    git_screen, help, image_viewer, panel, process_manager, search_result, system_info,
    theme::Theme,
};
use crate::keybindings::PanelAction;
use crate::services::remote;

const APP_TITLE: &str = concat!("COKACDIR v", env!("CARGO_PKG_VERSION"));

pub fn draw(frame: &mut Frame, app: &mut App) {
    // Clone theme to avoid borrow conflict with mutable app
    let theme = app.theme.clone();
    let area = frame.area();

    // Check if terminal is too large for ratatui buffer
    let frame_area = frame.area();
    if (frame_area.width as u32 * frame_area.height as u32) > 65534 {
        // Terminal too large, show warning message only
        let msg = Paragraph::new("Terminal too large. Please resize smaller.").style(
            Style::default()
                .fg(theme.message.text)
                .add_modifier(Modifier::BOLD),
        );
        let safe_rect = Rect::new(0, 0, frame_area.width.min(80), 1);
        frame.render_widget(msg, safe_rect);
        return;
    }

    // Fill entire screen with background color first
    let background = Block::default().style(Style::default().bg(theme.palette.bg));
    frame.render_widget(background, area);

    // Clear the entire screen first for full-screen views
    match app.current_screen {
        Screen::AIScreen | Screen::SystemInfo => {
            frame.render_widget(Clear, area);
        }
        _ => {}
    }

    match app.current_screen {
        Screen::FilePanel => draw_panels(frame, app, area, &theme),
        Screen::FileViewer => {
            if app.is_ai_mode() {
                // AI 모드: 뷰어와 AI 화면을 나란히 표시
                draw_viewer_with_ai(frame, app, area, &theme);
            } else if let Some(ref mut state) = app.viewer_state {
                file_viewer::draw(frame, state, area, &theme, &app.keybindings);
            }
        }
        Screen::FileEditor => {
            if app.is_ai_mode() {
                // AI 모드: 에디터와 AI 화면을 나란히 표시
                draw_editor_with_ai(frame, app, area, &theme);
            } else if let Some(ref mut state) = app.editor_state {
                file_editor::draw(frame, state, area, &theme, &app.keybindings);
            }
        }
        Screen::FileInfo => file_info::draw(frame, app, area, &theme),
        Screen::ProcessManager => process_manager::draw(frame, app, area, &theme),
        Screen::Help => help::draw(frame, app, area, &theme),
        Screen::AIScreen => {
            if let Some(ref mut state) = app.ai_state {
                ai_screen::draw(frame, state, area, &theme);
            }
        }
        Screen::SystemInfo => {
            system_info::draw(
                frame,
                &app.system_info_state,
                area,
                &theme,
                &app.keybindings,
            );
        }
        Screen::ImageViewer => {
            // 이미지 뷰어는 항상 배경(패널) 위에 오버레이로 표시
            // AI 모드여도 draw_panel_background가 AI 패널을 포함해서 그림
            image_viewer::draw(frame, app, area, &theme);
        }
        Screen::SearchResult => {
            search_result::draw(
                frame,
                &mut app.search_result_state,
                area,
                &theme,
                &app.keybindings,
            );
        }
        Screen::DiffScreen => {
            if let Some(ref mut state) = app.diff_state {
                diff_screen::draw(frame, state, area, &theme, &app.keybindings);
            }
        }
        Screen::DiffFileView => {
            if let Some(ref mut state) = app.diff_file_view_state {
                diff_file_view::draw(frame, state, area, &theme, &app.keybindings);
            }
        }
        Screen::GitScreen => {
            if let Some(ref mut state) = app.git_screen_state {
                git_screen::draw(frame, state, area, &theme);
            }
        }
        Screen::DedupScreen => {
            if let Some(ref mut state) = app.dedup_screen_state {
                dedup_screen::draw(frame, state, area, &theme);
            }
        }
    }

    // Draw advanced search dialog overlay if active
    if app.advanced_search_state.active && app.current_screen == Screen::FilePanel {
        advanced_search::draw(
            frame,
            &app.advanced_search_state,
            area,
            &theme,
            &app.keybindings,
        );
    }

    // Draw dialog overlay on top of everything (모든 화면 위에 다이얼로그 표시)
    if let Some(ref dialog) = app.dialog {
        dialogs::draw_dialog(frame, app, dialog, area, &theme);

        // Update git log diff visible height for scroll calculation
        if dialog.dialog_type == crate::ui::app::DialogType::GitLogDiff {
            if let Some(ref mut state) = app.git_log_diff_state {
                let dialog_h = area.height.saturating_sub(6).max(15).min(30);
                let inner_h = dialog_h.saturating_sub(2); // borders
                state.visible_height = inner_h.saturating_sub(3) as usize; // header + gap + buttons
            }
        }
    }

    // Draw remote spinner overlay on top of everything
    if let Some(ref spinner) = app.remote_spinner {
        dialogs::draw_remote_spinner(frame, &spinner.message, area, &theme);
    }

    // Update message timer
    if app.message_timer > 0 {
        app.message_timer -= 1;
        if app.message_timer == 0 {
            app.message = None;
        }
    }
}

fn draw_panels(frame: &mut Frame, app: &mut App, area: Rect, theme: &Theme) {
    // Layout: Panels, Status Bar, Function Bar (no header - saves 1 line)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),    // Panels
            Constraint::Length(1), // Status bar
            Constraint::Length(1), // Function bar / message
        ])
        .split(area);

    // Dynamic N-panel layout
    let num_panels = app.panels.len();
    let constraints: Vec<Constraint> = (0..num_panels)
        .map(|_| Constraint::Ratio(1, num_panels as u32))
        .collect();
    let panel_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(chunks[0]);

    // AI fullscreen 체크: fullscreen이면 AI만 전체 영역에 렌더링
    let ai_fullscreen = app.ai_state.as_ref().map_or(false, |s| s.ai_fullscreen);
    if ai_fullscreen && app.ai_panel_index.is_some() {
        if let Some(ref mut state) = app.ai_state {
            ai_screen::draw_with_focus(frame, state, chunks[0], theme, true);
        }
        draw_status_bar(frame, app, chunks[1], theme);
        draw_function_bar(frame, app, chunks[2], theme);
        return;
    }

    let is_ai_mode = app.is_ai_mode();
    let has_dialog = app.dialog.is_some();
    let active_idx = app.active_panel_index;
    let ai_panel_index = app.ai_panel_index;
    let diff_first_panel = app.diff_first_panel;

    // 각 패널을 루프로 렌더링
    for i in 0..num_panels {
        if ai_panel_index == Some(i) {
            // AI 화면 렌더링
            if let Some(ref mut state) = app.ai_state {
                let ai_focused = active_idx == i && !has_dialog;
                ai_screen::draw_with_focus(frame, state, panel_chunks[i], theme, ai_focused);
            }
        } else {
            let path_str = app.panels[i].path.display().to_string();
            let bookmarked = if app.panels[i].is_remote() {
                if let Some(ref ctx) = app.panels[i].remote_ctx {
                    let remote_str = remote::format_remote_display(&ctx.profile, &path_str);
                    app.settings.bookmarked_path.contains(&remote_str)
                } else if let Some((ref user, ref host, port)) = app.panels[i].remote_display {
                    let remote_str = if port != 22 {
                        format!("{}@{}:{}:{}", user, host, port, path_str)
                    } else {
                        format!("{}@{}:{}", user, host, path_str)
                    };
                    app.settings.bookmarked_path.contains(&remote_str)
                } else {
                    false
                }
            } else {
                app.settings.bookmarked_path.contains(&path_str)
            };
            let focused =
                active_idx == i && !has_dialog && (!is_ai_mode || ai_panel_index != Some(i));
            let diff_selected = diff_first_panel == Some(i);
            panel::draw(
                frame,
                &mut app.panels[i],
                panel_chunks[i],
                focused,
                bookmarked,
                diff_selected,
                theme,
            );
        }
    }

    // Status bar
    draw_status_bar(frame, app, chunks[1], theme);

    // Function bar or message
    draw_function_bar(frame, app, chunks[2], theme);
}

/// Public function for drawing panel background (used by overlay screens)
pub fn draw_panel_background(frame: &mut Frame, app: &mut App, area: Rect, theme: &Theme) {
    draw_panels(frame, app, area, theme);
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let panel = app.active_panel();
    let current_file = panel.current_file();

    let left_text = if let Some(file) = current_file {
        if file.name != ".." {
            let name = file.display_name.as_deref().unwrap_or(&file.name);
            format!(
                "{} ({})",
                name,
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
            " ".repeat(
                area.width
                    .saturating_sub(left_text.width() as u16 + right_text.width() as u16 + 4)
                    as usize,
            ),
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
            Style::default()
                .fg(theme.message.text)
                .add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(message, area);
        return;
    }

    let kb = &app.keybindings;

    // (action, short label) — key display comes from keybindings
    let mut items: Vec<(PanelAction, &str)> = vec![
        (PanelAction::Help, "help "),
        (PanelAction::FileInfo, "info "),
        (PanelAction::Edit, "edit "),
        (PanelAction::Mkdir, "mkdir "),
        (PanelAction::Mkfile, "mkfile "),
        (PanelAction::Delete, "del "),
        (PanelAction::Rename, "ren "),
        (PanelAction::Tar, "tar "),
        (PanelAction::SetHandler, "hnd "),
        (PanelAction::Search, "find "),
        (PanelAction::AIScreen, "AI "),
        (PanelAction::ProcessManager, "proc "),
        (PanelAction::ToggleSelect, "sel "),
        (PanelAction::SelectUp, "sel "),
        (PanelAction::SelectByExtension, "ext "),
        (PanelAction::SelectAll, "all "),
        (PanelAction::Copy, "cpy "),
        (PanelAction::Cut, "cut "),
        (PanelAction::Paste, "pst "),
        (PanelAction::SortByName, "name "),
        (PanelAction::SortBySize, "size "),
        (PanelAction::SortByDate, "date "),
        (PanelAction::SortByType, "type "),
        (PanelAction::GoHomeDir, "home "),
        (PanelAction::Refresh, "ref "),
        (PanelAction::ToggleBookmark, "bmk "),
        (PanelAction::GitScreen, "git "),
        (PanelAction::GitLogDiff, "glog "),
        (PanelAction::StartDiff, "diff "),
        (PanelAction::AddPanel, "+pan "),
        (PanelAction::ClosePanel, "-pan "),
    ];

    #[cfg(target_os = "macos")]
    {
        items.push((PanelAction::OpenInFinder, "finder "));
        items.push((PanelAction::OpenInVSCode, "vscode "));
    }
    #[cfg(target_os = "windows")]
    {
        items.push((PanelAction::OpenInExplorer, "explorer "));
        items.push((PanelAction::OpenInVSCode, "vscode "));
    }

    items.push((PanelAction::Settings, "set "));
    items.push((PanelAction::Quit, "quit"));

    let key_style = Style::default().fg(theme.function_bar.key);
    let label_style = Style::default().fg(theme.function_bar.label);

    // Build display strings for width calculation
    let shortcuts: Vec<(String, &str)> = items
        .iter()
        .filter_map(|(action, label)| {
            let key = kb.panel_first_key(*action).to_string();
            if key.is_empty() {
                None
            } else {
                Some((key, *label))
            }
        })
        .collect();

    let mut spans = Vec::new();
    for (key, label) in &shortcuts {
        spans.push(Span::styled(key.as_str(), key_style));
        spans.push(Span::styled(":", label_style));
        spans.push(Span::styled(*label, label_style));
    }

    // Calculate shortcuts width and add padding + version
    let shortcuts_width: usize = shortcuts
        .iter()
        .map(|(k, l)| k.width() + 1 + l.width()) // +1 for ":"
        .sum();
    let version_text = format!(" {}", APP_TITLE);
    let padding_width =
        (area.width as usize).saturating_sub(shortcuts_width + version_text.width());

    spans.push(Span::styled(" ".repeat(padding_width), theme.dim_style()));
    spans.push(Span::styled(version_text, theme.dim_style()));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// AI 모드에서 에디터와 AI 화면을 나란히 표시
fn draw_editor_with_ai(frame: &mut Frame, app: &mut App, area: Rect, theme: &Theme) {
    // Layout: Panels, Status Bar, Function Bar (same as draw_panels)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),    // Panels
            Constraint::Length(1), // Status bar
            Constraint::Length(1), // Function bar
        ])
        .split(area);

    let ai_fullscreen = app.ai_state.as_ref().map_or(false, |s| s.ai_fullscreen);
    if ai_fullscreen {
        // AI만 전체 영역에 렌더링
        if let Some(ref mut state) = app.ai_state {
            ai_screen::draw_with_focus(frame, state, chunks[0], theme, true);
        }
    } else {
        let panel_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[0]);

        let ai_on_left = app
            .ai_panel_index
            .map(|i| i < app.active_panel_index)
            .unwrap_or(false);

        if ai_on_left {
            // AI 왼쪽, 에디터 오른쪽
            if let Some(ref mut state) = app.ai_state {
                ai_screen::draw_with_focus(frame, state, panel_chunks[0], theme, false);
            }
            if let Some(ref mut state) = app.editor_state {
                file_editor::draw(frame, state, panel_chunks[1], theme, &app.keybindings);
            }
        } else {
            // 에디터 왼쪽, AI 오른쪽
            if let Some(ref mut state) = app.editor_state {
                file_editor::draw(frame, state, panel_chunks[0], theme, &app.keybindings);
            }
            if let Some(ref mut state) = app.ai_state {
                ai_screen::draw_with_focus(frame, state, panel_chunks[1], theme, false);
            }
        }
    }

    // Status bar and Function bar
    draw_status_bar(frame, app, chunks[1], theme);
    draw_function_bar(frame, app, chunks[2], theme);
}

/// AI 모드에서 뷰어와 AI 화면을 나란히 표시
fn draw_viewer_with_ai(frame: &mut Frame, app: &mut App, area: Rect, theme: &Theme) {
    // Layout: Panels, Status Bar, Function Bar (same as draw_panels)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),    // Panels
            Constraint::Length(1), // Status bar
            Constraint::Length(1), // Function bar
        ])
        .split(area);

    let ai_fullscreen = app.ai_state.as_ref().map_or(false, |s| s.ai_fullscreen);
    if ai_fullscreen {
        // AI만 전체 영역에 렌더링
        if let Some(ref mut state) = app.ai_state {
            ai_screen::draw_with_focus(frame, state, chunks[0], theme, true);
        }
    } else {
        let panel_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[0]);

        let ai_on_left = app
            .ai_panel_index
            .map(|i| i < app.active_panel_index)
            .unwrap_or(false);

        if ai_on_left {
            // AI 왼쪽, 뷰어 오른쪽
            if let Some(ref mut state) = app.ai_state {
                ai_screen::draw_with_focus(frame, state, panel_chunks[0], theme, false);
            }
            if let Some(ref mut state) = app.viewer_state {
                file_viewer::draw(frame, state, panel_chunks[1], theme, &app.keybindings);
            }
        } else {
            // 뷰어 왼쪽, AI 오른쪽
            if let Some(ref mut state) = app.viewer_state {
                file_viewer::draw(frame, state, panel_chunks[0], theme, &app.keybindings);
            }
            if let Some(ref mut state) = app.ai_state {
                ai_screen::draw_with_focus(frame, state, panel_chunks[1], theme, false);
            }
        }
    }

    // Status bar and Function bar
    draw_status_bar(frame, app, chunks[1], theme);
    draw_function_bar(frame, app, chunks[2], theme);
}
