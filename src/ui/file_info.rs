use std::fs;
use crossterm::event::KeyCode;
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use super::{app::{App, Screen}, theme::Theme};
use crate::utils::format::{format_size, format_permissions};

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, theme: &Theme) {
    // Draw dual panel in background first
    super::draw::draw_dual_panel_background(frame, app, area, theme);

    // Build content first to calculate required height
    let path = &app.info_file_path;
    let metadata = fs::metadata(path);

    let mut lines: Vec<Line> = Vec::new();

    if let Ok(meta) = metadata {
        let name = path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        lines.push(info_line("Name", &name, theme));
        lines.push(info_line("Path", &path.display().to_string(), theme));

        let file_type = if meta.is_dir() {
            "Directory"
        } else if meta.is_symlink() {
            "Symbolic Link"
        } else {
            "File"
        };
        lines.push(info_line("Type", file_type, theme));
        lines.push(info_line("Size", &format_size(meta.len()), theme));

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            lines.push(Line::from(Span::raw("")));
            lines.push(info_line("Permissions", &format_permissions(meta.mode()), theme));
            lines.push(info_line("Owner/Group", &format!("{}/{}", meta.uid(), meta.gid()), theme));
            lines.push(info_line("Links", &meta.nlink().to_string(), theme));
            lines.push(info_line("Inode", &meta.ino().to_string(), theme));
        }

        lines.push(Line::from(Span::raw("")));

        if let Ok(created) = meta.created() {
            let datetime: chrono::DateTime<chrono::Local> = created.into();
            lines.push(info_line("Created", &datetime.format("%Y-%m-%d %H:%M:%S").to_string(), theme));
        }

        if let Ok(modified) = meta.modified() {
            let datetime: chrono::DateTime<chrono::Local> = modified.into();
            lines.push(info_line("Modified", &datetime.format("%Y-%m-%d %H:%M:%S").to_string(), theme));
        }

        if let Ok(accessed) = meta.accessed() {
            let datetime: chrono::DateTime<chrono::Local> = accessed.into();
            lines.push(info_line("Accessed", &datetime.format("%Y-%m-%d %H:%M:%S").to_string(), theme));
        }

        // Directory-specific info
        if meta.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                let count = entries.count();
                lines.push(Line::from(Span::raw("")));
                lines.push(info_line("Items", &count.to_string(), theme));
            }
        }
    } else {
        lines.push(Line::from(Span::styled(
            "Error reading file information",
            theme.error_style(),
        )));
    }

    lines.push(Line::from(Span::raw("")));
    lines.push(Line::from(Span::styled(
        "Press any key to close",
        theme.dim_style(),
    )));

    // Calculate dialog size based on content (+2 for borders)
    let content_height = lines.len() as u16 + 2;
    let width = 60u16.min(area.width.saturating_sub(4));
    let height = content_height.min(area.height.saturating_sub(4));

    // Need minimum size to display anything useful
    if width < 20 || height < 5 {
        return;
    }

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    // Clear the area
    frame.render_widget(Clear, dialog_area);

    let block = Block::default()
        .title(" File Information ")
        .title_style(theme.header_style())
        .borders(Borders::ALL)
        .border_style(theme.border_style(true));

    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

fn info_line<'a>(label: &str, value: &str, theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{:12}", label), theme.dim_style()),
        Span::styled(value.to_string(), theme.normal_style()),
    ])
}

pub fn handle_input(app: &mut App, _code: KeyCode) {
    // Any key closes the info dialog
    app.current_screen = Screen::DualPanel;
}
