use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{
    app::{DirectorySizeStatus, PanelState, SortBy, SortOrder},
    theme::Theme,
};
use crate::utils::format::{format_size, pad_to_display_width, truncate_to_display_width};

pub fn draw(
    frame: &mut Frame,
    panel: &mut PanelState,
    area: Rect,
    is_active: bool,
    is_bookmarked: bool,
    diff_selected: bool,
    theme: &Theme,
) {
    let inner_width = area.width.saturating_sub(2) as usize;

    // Build path display (truncate if too long, using display width)
    let path_str = panel.display_path();
    let bookmark_marker = if is_bookmarked { "✻" } else { "" };
    let prefix = bookmark_marker.to_string();
    let path_display_width = path_str.width();
    let display_path =
        if inner_width > 4 && path_display_width + prefix.width() > inner_width.saturating_sub(4) {
            // Calculate how many characters to show from the end (by display width)
            let target_width = inner_width.saturating_sub(prefix.width() + 4); // prefix + "..."
            let mut suffix_width = 0;
            let mut start_char_idx = path_str.chars().count();
            for (i, c) in path_str.chars().rev().enumerate() {
                let cw = c.width().unwrap_or(1);
                if suffix_width + cw > target_width {
                    break;
                }
                suffix_width += cw;
                start_char_idx = path_str.chars().count() - i - 1;
            }
            let suffix: String = path_str.chars().skip(start_char_idx).collect();
            format!("{}...{}", prefix, suffix)
        } else {
            format!("{}{}", prefix, path_str)
        };

    let block = Block::default()
        .title(format!(" {} ", display_path))
        .title_style(if panel.is_remote() && is_active {
            Style::default()
                .fg(theme.panel.remote_indicator)
                .add_modifier(Modifier::BOLD)
        } else if is_active {
            Style::default()
                .fg(theme.panel.border_active)
                .add_modifier(Modifier::BOLD)
        } else if panel.is_remote() {
            Style::default().fg(theme.panel.remote_indicator)
        } else {
            Style::default().fg(theme.panel.file_text)
        })
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if diff_selected {
            theme.diff.panel_selected_border
        } else if is_active {
            theme.panel.border_active
        } else {
            theme.panel.border
        }));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Minimum dimensions check
    if inner.height < 3 || inner.width < 10 {
        return;
    }

    // Column widths - adapt to available space
    let min_columns: u16 = 10 + 12 + 4; // size + date + padding
    let type_col_total: usize = 10; // 2 + 6 + 2 (padding + type + padding)

    // Calculate max file name width (including marker and icon = 2 chars)
    let max_name_display_width = panel
        .files
        .iter()
        .map(|f| {
            let name = f.display_name.as_deref().unwrap_or(&f.name);
            name.width() + 2 // +2 for marker and icon
        })
        .max()
        .unwrap_or(0);

    let (name_col, type_col, size_col, date_col) = if inner.width > min_columns {
        let available_for_name = (inner.width - min_columns) as usize;

        // Check if we can show Type column:
        // All file names must fit without truncation AND
        // there must be at least type_col_total (10) extra chars of space
        let show_type = available_for_name >= max_name_display_width + type_col_total;

        if show_type {
            let name_width = available_for_name - type_col_total;
            (name_width, 6_usize, 10_usize, 12_usize)
        } else {
            (available_for_name, 0_usize, 10_usize, 12_usize)
        }
    } else {
        // Very narrow: use all available width for name only, hide size/date/type
        let name_width = inner.width.saturating_sub(2) as usize;
        (name_width, 0_usize, 0_usize, 0_usize)
    };

    // Header row
    let header = create_header_line(
        panel, name_col, type_col, size_col, date_col, is_active, theme,
    );
    let header_bg = if is_active {
        theme.panel.header_bg_active
    } else {
        theme.panel.header_bg
    };
    frame.render_widget(
        Paragraph::new(header).style(Style::default().bg(header_bg)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    // File list (visible area)
    let visible_height = inner.height.saturating_sub(2) as usize; // -2 for header and footer
    let total_files = panel.files.len();

    // 스크롤 오프셋 계산: 커서가 보이는 범위 내에 있으면 스크롤 유지
    let current_scroll = panel.scroll_offset;
    let start_index = if total_files <= visible_height {
        // 파일 개수가 화면보다 적으면 스크롤 없음
        0
    } else if panel.selected_index >= current_scroll
        && panel.selected_index < current_scroll + visible_height
    {
        // 커서가 현재 보이는 범위 내에 있으면 스크롤 유지
        // 단, 스크롤이 유효한 범위인지 확인
        if current_scroll + visible_height > total_files {
            total_files - visible_height
        } else {
            current_scroll
        }
    } else {
        // 커서가 범위 밖이면 center-locked로 조정
        let half_visible = visible_height / 2;
        let mut new_start = panel.selected_index.saturating_sub(half_visible);
        if new_start + visible_height > total_files {
            new_start = total_files - visible_height;
        }
        new_start
    };

    // scroll_offset 업데이트 (패널 전환 시 사용)
    panel.scroll_offset = start_index;

    let visible_files = panel.files.iter().skip(start_index).take(visible_height);
    let directory_size_spinner = directory_size_spinner_frame();

    for (i, file) in visible_files.enumerate() {
        let actual_index = start_index + i;
        let is_cursor = actual_index == panel.selected_index;
        let is_marked = panel.selected_files.contains(&file.name);
        let show_cursor = is_cursor && is_active;

        let line = create_file_line(
            file,
            show_cursor,
            is_marked,
            name_col,
            type_col,
            size_col,
            date_col,
            directory_size_spinner,
            theme,
        );

        let paragraph = if show_cursor {
            let cursor_bg = if is_marked {
                theme.panel.marked_text
            } else if file.is_symlink {
                theme.panel.symlink_text
            } else if file.is_directory {
                theme.panel.directory_text
            } else {
                theme.panel.file_text
            };
            Paragraph::new(line).style(Style::default().bg(cursor_bg))
        } else {
            Paragraph::new(line)
        };

        frame.render_widget(
            paragraph,
            Rect::new(inner.x, inner.y + 1 + i as u16, inner.width, 1),
        );
    }

    // 스크롤바 (파일이 화면보다 많을 때)
    if total_files > visible_height {
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));

        let mut scrollbar_state = ScrollbarState::new(total_files).position(panel.selected_index);

        let scrollbar_area = Rect::new(
            inner.x + inner.width - 1,
            inner.y + 1,
            1,
            visible_height as u16,
        );

        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }

    // Footer (폴더 정보 + 디스크 용량)
    let dir_count = panel
        .files
        .iter()
        .filter(|f| f.name != ".." && f.is_directory)
        .count();
    let file_count = panel.files.iter().filter(|f| !f.is_directory).count();
    let total_size: u64 = panel
        .files
        .iter()
        .filter(|f| !f.is_directory)
        .map(|f| f.size)
        .fold(0, u64::saturating_add);

    // 선택된 파일 정보 계산
    let selected_count = panel.selected_files.len();
    let selected_size: u64 = panel
        .files
        .iter()
        .filter(|f| panel.selected_files.contains(&f.name))
        .map(|f| f.size)
        .fold(0, u64::saturating_add);

    let number_style = Style::default().fg(theme.panel.directory_text);
    let label_style = theme.dim_style();
    let selected_style = Style::default().fg(theme.panel.marked_text);

    let mut spans = vec![
        Span::styled(format!("{}", dir_count), number_style),
        Span::styled("d ", label_style),
        Span::styled(format!("{}", file_count), number_style),
        Span::styled("f ", label_style),
        Span::styled(format_size(total_size), number_style),
    ];

    // 선택된 파일이 있으면 선택 정보 표시
    if selected_count > 0 {
        spans.push(Span::styled(" | ", label_style));
        spans.push(Span::styled(format!("{}", selected_count), selected_style));
        spans.push(Span::styled("sel ", label_style));
        spans.push(Span::styled(format_size(selected_size), selected_style));
    }

    if panel.is_remote() {
        // Show remote connection info instead of disk info
        let remote_info = if let Some(ref ctx) = panel.remote_ctx {
            Some((ctx.profile.user.as_str(), ctx.profile.host.as_str()))
        } else if let Some((ref user, ref host, _)) = panel.remote_display {
            Some((user.as_str(), host.as_str()))
        } else {
            None
        };
        if let Some((user, host)) = remote_info {
            let remote_style = Style::default().fg(theme.panel.remote_indicator);
            spans.push(Span::styled(" | ", label_style));
            spans.push(Span::styled(format!("{}@{}", user, host), remote_style));
        }
    } else if panel.disk_total > 0 {
        let disk_free = format_size(panel.disk_available);
        let disk_total = format_size(panel.disk_total);
        spans.push(Span::styled(" | ", label_style));
        spans.push(Span::styled(disk_free, number_style));
        spans.push(Span::styled("/", label_style));
        spans.push(Span::styled(disk_total, number_style));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(ratatui::layout::Alignment::Center),
        Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
    );
}

fn create_header_line(
    panel: &PanelState,
    name_width: usize,
    type_width: usize,
    size_width: usize,
    date_width: usize,
    is_active: bool,
    theme: &Theme,
) -> Line<'static> {
    let header_style = if is_active {
        Style::default()
            .fg(theme.panel.header_text_active)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.panel.header_text)
    };

    // Handle very narrow width
    if name_width == 0 {
        return Line::from(Span::styled("", header_style));
    }

    let name_indicator = match (panel.sort_by, panel.sort_order) {
        (SortBy::Name, SortOrder::Asc) => "Name\u{25B2}",
        (SortBy::Name, SortOrder::Desc) => "Name\u{25BC}",
        _ => "Name",
    };

    let type_indicator = match (panel.sort_by, panel.sort_order) {
        (SortBy::Type, SortOrder::Asc) => "Type\u{25B2}",
        (SortBy::Type, SortOrder::Desc) => "Type\u{25BC}",
        _ => "Type",
    };

    let size_indicator = match (panel.sort_by, panel.sort_order) {
        (SortBy::Size, SortOrder::Asc) => "Size\u{25B2}",
        (SortBy::Size, SortOrder::Desc) => "Size\u{25BC}",
        _ => "Size",
    };

    let date_indicator = match (panel.sort_by, panel.sort_order) {
        (SortBy::Modified, SortOrder::Asc) => "Modified\u{25B2}",
        (SortBy::Modified, SortOrder::Desc) => "Modified\u{25BC}",
        _ => "Modified",
    };

    // Use saturating_sub to prevent underflow in format width
    let name_col = format!(
        " {:width$}",
        name_indicator,
        width = name_width.saturating_sub(1)
    );
    let type_col_str = if type_width > 0 {
        format!("  {:^width$}  ", type_indicator, width = type_width)
    } else {
        String::new()
    };
    let size_col = if size_width > 2 {
        format!(
            "{:>width$}  ",
            size_indicator,
            width = size_width.saturating_sub(2)
        )
    } else {
        String::new()
    };
    let date_col = if date_width > 2 {
        format!(
            "{:>width$}  ",
            date_indicator,
            width = date_width.saturating_sub(2)
        )
    } else {
        String::new()
    };

    Line::from(vec![
        Span::styled(name_col, header_style),
        Span::styled(type_col_str, header_style),
        Span::styled(size_col, header_style),
        Span::styled(date_col, header_style),
    ])
}

fn create_file_line(
    file: &super::app::FileItem,
    is_cursor: bool,
    is_marked: bool,
    name_width: usize,
    type_width: usize,
    size_width: usize,
    date_width: usize,
    directory_size_spinner: char,
    theme: &Theme,
) -> Line<'static> {
    let marker = if is_marked { "✻" } else { " " };
    let icon = if file.is_symlink {
        theme.chars.symlink.to_string()
    } else if file.is_directory {
        theme.chars.folder.to_string()
    } else {
        theme.chars.file.to_string()
    };

    // Truncate name if needed using unicode display width
    let effective_name_width = name_width.saturating_sub(2);
    let name_str = file.display_name.as_deref().unwrap_or(&file.name);
    let display_name = if effective_name_width < 4 {
        String::new()
    } else {
        let name_display_width = name_str.width();
        if name_display_width > effective_name_width {
            let truncate_width = effective_name_width.saturating_sub(3);
            if truncate_width > 0 {
                let truncated = truncate_to_display_width(name_str, truncate_width);
                format!("{}...", truncated)
            } else {
                "...".to_string()
            }
        } else {
            name_str.to_string()
        }
    };

    // Pad name column to exact width using unicode-aware padding
    let name_with_prefix = format!("{}{}{}", marker, &icon, display_name);
    let name_col = pad_to_display_width(&name_with_prefix, name_width);

    // Type column: show file extension (max 6 chars, center aligned)
    let type_col_str = if type_width > 0 {
        let type_str = if file.is_directory || file.name == ".." {
            String::new()
        } else if file.name.ends_with(crate::enc::naming::EXT) {
            "\u{1F511}".to_string()
        } else {
            std::path::Path::new(name_str)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| {
                    let lower = e.to_lowercase();
                    if lower.chars().count() > type_width {
                        let take_n = type_width.saturating_sub(2);
                        let truncated: String = lower.chars().take(take_n).collect();
                        format!("{}..", truncated)
                    } else {
                        lower
                    }
                })
                .unwrap_or_default()
        };
        // Center align the type string
        format!("  {:^width$}  ", type_str, width = type_width)
    } else {
        String::new()
    };

    let size_str = if file.is_directory {
        match file.directory_size_status {
            Some(DirectorySizeStatus::Calculating) => {
                format!("{directory_size_spinner} calc")
            }
            Some(DirectorySizeStatus::Complete) => {
                format_size_for_column(file.size, size_width.saturating_sub(2))
            }
            Some(DirectorySizeStatus::Failed) => "<ERR>".to_string(),
            None => "<DIR>".to_string(),
        }
    } else {
        format_size_for_column(file.size, size_width.saturating_sub(2))
    };
    let size_col = if size_width > 2 {
        format!(
            "{:>width$}  ",
            size_str,
            width = size_width.saturating_sub(2)
        )
    } else {
        String::new()
    };

    let date_str = if file.name == ".." {
        String::new()
    } else {
        file.modified.format("%m-%d %H:%M").to_string()
    };
    let date_col = if date_width > 2 {
        format!(
            "{:>width$}  ",
            date_str,
            width = date_width.saturating_sub(2)
        )
    } else {
        String::new()
    };

    // Cursor style: 배경색을 항목의 원래 글자색으로 설정
    let name_style = if is_cursor {
        let cursor_bg = if is_marked {
            theme.panel.marked_text
        } else if file.is_symlink {
            theme.panel.symlink_text
        } else if file.is_directory {
            theme.panel.directory_text
        } else {
            theme.panel.file_text
        };
        Style::default().fg(theme.panel.selected_text).bg(cursor_bg)
    } else if is_marked {
        theme.marked_style()
    } else if file.is_symlink {
        theme.symlink_style()
    } else if file.is_directory {
        theme.directory_style()
    } else {
        theme.normal_style()
    };

    let other_style = if is_cursor {
        let cursor_bg = if is_marked {
            theme.panel.marked_text
        } else if file.is_symlink {
            theme.panel.symlink_text
        } else if file.is_directory {
            theme.panel.directory_text
        } else {
            theme.panel.file_text
        };
        Style::default().fg(theme.panel.selected_text).bg(cursor_bg)
    } else {
        theme.dim_style()
    };

    Line::from(vec![
        Span::styled(name_col, name_style),
        Span::styled(type_col_str, other_style),
        Span::styled(size_col, other_style),
        Span::styled(date_col, other_style),
    ])
}

fn directory_size_spinner_frame() -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    FRAMES[(elapsed.as_millis() / 100 % FRAMES.len() as u128) as usize]
}

fn format_size_for_column(bytes: u64, max_width: usize) -> String {
    let standard = format_size(bytes);
    if standard.width() <= max_width {
        return standard;
    }

    const UNITS: [&str; 7] = ["B", "K", "M", "G", "T", "P", "E"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let compact = if unit == 0 || value >= 100.0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    };
    if compact.width() <= max_width {
        compact
    } else {
        truncate_to_display_width(&compact, max_width)
            .trim_end()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use std::fs;

    #[test]
    fn calculating_folder_size_is_visible_in_an_80_column_panel() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("folder")).unwrap();
        let mut panel = PanelState::new(temp.path().to_path_buf());
        panel
            .files
            .iter_mut()
            .find(|file| file.name == "folder")
            .unwrap()
            .directory_size_status = Some(DirectorySizeStatus::Calculating);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &mut panel,
                    Rect::new(0, 0, 80, 24),
                    true,
                    false,
                    false,
                    &theme,
                );
            })
            .unwrap();

        let rendered = (0..24)
            .map(|y| {
                (0..80)
                    .map(|x| terminal.backend().buffer().cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("calc"),
            "folder size spinner should remain visible at 80 columns: {rendered:?}"
        );
        assert!(rendered.contains("Modified"));
    }

    #[test]
    fn very_large_folder_sizes_stay_inside_the_size_column() {
        let tebibyte = 1024_u64.pow(4);
        assert_eq!(format_size_for_column(tebibyte, 8), "1.0T");
        assert!(format_size_for_column(u64::MAX, 8).width() <= 8);
    }
}
