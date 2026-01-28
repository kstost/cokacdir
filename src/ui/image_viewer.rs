use crossterm::event::KeyCode;
use image::{GenericImageView, DynamicImage, Pixel};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use std::path::Path;

use super::{app::{App, Screen}, theme::Theme};

pub struct ImageViewerState {
    pub path: std::path::PathBuf,
    pub image: Option<DynamicImage>,
    pub error: Option<String>,
    pub zoom: f32,
    pub offset_x: i32,
    pub offset_y: i32,
}

impl ImageViewerState {
    pub fn new(path: &Path) -> Self {
        let (image, error) = match image::open(path) {
            Ok(img) => (Some(img), None),
            Err(e) => (None, Some(format!("Failed to load image: {}", e))),
        };

        Self {
            path: path.to_path_buf(),
            image,
            error,
            zoom: 1.0,
            offset_x: 0,
            offset_y: 0,
        }
    }

    pub fn zoom_in(&mut self) {
        self.zoom = (self.zoom * 1.2).min(10.0);
    }

    pub fn zoom_out(&mut self) {
        self.zoom = (self.zoom / 1.2).max(0.1);
    }

    pub fn reset_view(&mut self) {
        self.zoom = 1.0;
        self.offset_x = 0;
        self.offset_y = 0;
    }

    pub fn pan(&mut self, dx: i32, dy: i32) {
        self.offset_x += dx;
        self.offset_y += dy;
    }
}

/// Check if a file is a supported image format
pub fn is_image_file(path: &Path) -> bool {
    if let Some(ext) = path.extension() {
        let ext = ext.to_string_lossy().to_lowercase();
        matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "tiff" | "tif")
    } else {
        false
    }
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, theme: &Theme) {
    // Draw dual panel in background
    super::draw::draw_dual_panel_background(frame, app, area, theme);

    let state = match &app.image_viewer_state {
        Some(s) => s,
        None => return,
    };

    // Calculate viewer area (leave some margin)
    let margin = 2;
    let viewer_width = area.width.saturating_sub(margin * 2);
    let viewer_height = area.height.saturating_sub(margin * 2);

    if viewer_width < 20 || viewer_height < 10 {
        return;
    }

    let x = area.x + margin;
    let y = area.y + margin;
    let viewer_area = Rect::new(x, y, viewer_width, viewer_height);

    // Clear area
    frame.render_widget(ratatui::widgets::Clear, viewer_area);

    let filename = state.path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "Image".to_string());

    let title = if let Some(ref img) = state.image {
        format!(" {} ({}x{}) - {:.0}% ", filename, img.width(), img.height(), state.zoom * 100.0)
    } else {
        format!(" {} ", filename)
    };

    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(theme.border_active))
        .borders(Borders::ALL)
        .border_style(theme.border_style(true));

    let inner = block.inner(viewer_area);
    frame.render_widget(block, viewer_area);

    if let Some(ref error) = state.error {
        let error_lines = vec![
            Line::from(""),
            Line::from(Span::styled(error.clone(), Style::default().fg(theme.error))),
            Line::from(""),
            Line::from(Span::styled("Press ESC to close", theme.dim_style())),
        ];
        frame.render_widget(Paragraph::new(error_lines), inner);
        return;
    }

    if let Some(ref img) = state.image {
        render_image(frame, img, inner, state.zoom, state.offset_x, state.offset_y);
    }

    // Help line at bottom
    let help_area = Rect::new(inner.x, inner.y + inner.height.saturating_sub(1), inner.width, 1);
    let help = Line::from(vec![
        Span::styled("+", Style::default().fg(theme.success)),
        Span::styled("/", theme.dim_style()),
        Span::styled("-", Style::default().fg(theme.success)),
        Span::styled(" Zoom ", theme.dim_style()),
        Span::styled("Arrow", Style::default().fg(theme.success)),
        Span::styled(" Pan ", theme.dim_style()),
        Span::styled("r", Style::default().fg(theme.success)),
        Span::styled(" Reset ", theme.dim_style()),
        Span::styled("Esc", Style::default().fg(theme.success)),
        Span::styled(" Close", theme.dim_style()),
    ]);
    frame.render_widget(Paragraph::new(help), help_area);
}

fn render_image(frame: &mut Frame, img: &DynamicImage, area: Rect, zoom: f32, offset_x: i32, offset_y: i32) {
    // Each terminal cell can show 2 vertical pixels using half-block characters
    let term_width = area.width as u32;
    let term_height = (area.height.saturating_sub(1)) as u32 * 2; // -1 for help line, *2 for half-blocks

    let img_width = img.width();
    let img_height = img.height();

    // Calculate scaled dimensions
    let scale_x = (term_width as f32 / img_width as f32) * zoom;
    let scale_y = (term_height as f32 / img_height as f32) * zoom;
    let scale = scale_x.min(scale_y);

    let scaled_width = (img_width as f32 * scale) as u32;
    let scaled_height = (img_height as f32 * scale) as u32;

    // Center the image
    let start_x = ((term_width as i32 - scaled_width as i32) / 2 + offset_x).max(0) as u32;
    let start_y = ((term_height as i32 - scaled_height as i32) / 2 + offset_y).max(0) as u32;

    // Resize image for display
    let resized = img.resize_exact(
        scaled_width.max(1),
        scaled_height.max(1),
        image::imageops::FilterType::Triangle,
    );

    let mut lines: Vec<Line> = Vec::new();

    // Process 2 rows at a time for half-block rendering
    for row in 0..((area.height.saturating_sub(1)) as u32) {
        let mut spans: Vec<Span> = Vec::new();

        for col in 0..term_width {
            let img_x = col.saturating_sub(start_x);
            let img_y_top = (row * 2).saturating_sub(start_y / 2 * 2);
            let img_y_bottom = img_y_top + 1;

            let (top_color, bottom_color) = if col >= start_x
                && col < start_x + scaled_width
                && row * 2 >= start_y
                && row * 2 < start_y + scaled_height
            {
                let top = if img_y_top < resized.height() && img_x < resized.width() {
                    let pixel = resized.get_pixel(img_x, img_y_top);
                    let rgb = pixel.to_rgb();
                    Some(Color::Rgb(rgb[0], rgb[1], rgb[2]))
                } else {
                    None
                };

                let bottom = if img_y_bottom < resized.height() && img_x < resized.width() {
                    let pixel = resized.get_pixel(img_x, img_y_bottom);
                    let rgb = pixel.to_rgb();
                    Some(Color::Rgb(rgb[0], rgb[1], rgb[2]))
                } else {
                    None
                };

                (top, bottom)
            } else {
                (None, None)
            };

            let (ch, style) = match (top_color, bottom_color) {
                (Some(top), Some(bottom)) => {
                    // Use half-block: foreground = top, background = bottom
                    ('▀', Style::default().fg(top).bg(bottom))
                }
                (Some(top), None) => {
                    ('▀', Style::default().fg(top))
                }
                (None, Some(bottom)) => {
                    ('▄', Style::default().fg(bottom))
                }
                (None, None) => {
                    (' ', Style::default())
                }
            };

            spans.push(Span::styled(ch.to_string(), style));
        }

        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(lines), Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1)));
}

pub fn handle_input(app: &mut App, code: KeyCode) {
    let state = match &mut app.image_viewer_state {
        Some(s) => s,
        None => {
            app.current_screen = Screen::DualPanel;
            return;
        }
    };

    match code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
            app.current_screen = Screen::DualPanel;
            app.image_viewer_state = None;
        }
        KeyCode::Char('+') | KeyCode::Char('=') => {
            state.zoom_in();
        }
        KeyCode::Char('-') | KeyCode::Char('_') => {
            state.zoom_out();
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            state.reset_view();
        }
        KeyCode::Up => {
            state.pan(0, 5);
        }
        KeyCode::Down => {
            state.pan(0, -5);
        }
        KeyCode::Left => {
            state.pan(5, 0);
        }
        KeyCode::Right => {
            state.pan(-5, 0);
        }
        _ => {}
    }
}
