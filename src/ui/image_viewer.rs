use crossterm::event::{KeyCode, KeyModifiers};
use image::DynamicImage;
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use ratatui_image::protocol::StatefulProtocol;
use std::fs::OpenOptions;
use std::io::BufReader;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::OnceLock;
use std::thread;

use super::{
    app::{App, Screen},
    theme::Theme,
};

/// Result of async image loading
struct ImageLoadResult {
    image: Option<DynamicImage>,
    error: Option<String>,
}

struct ImageLoadJob {
    path: std::path::PathBuf,
    result_tx: mpsc::Sender<ImageLoadResult>,
}

fn image_loader() -> &'static SyncSender<ImageLoadJob> {
    static LOADER: OnceLock<SyncSender<ImageLoadJob>> = OnceLock::new();
    LOADER.get_or_init(|| {
        // One active decode plus one queued request. Image decoders cannot be
        // interrupted reliably; a single bounded worker prevents rapid
        // open/close cycles from creating an unbounded number of decoder
        // threads and their large pixel buffers.
        let (job_tx, job_rx) = mpsc::sync_channel::<ImageLoadJob>(1);
        thread::spawn(move || {
            while let Ok(job) = job_rx.recv() {
                let result = match decode_regular_image(&job.path) {
                    Ok(image) => ImageLoadResult {
                        image: Some(image),
                        error: None,
                    },
                    Err(error) => ImageLoadResult {
                        image: None,
                        error: Some(error),
                    },
                };
                let _ = job.result_tx.send(result);
            }
        });
        job_tx
    })
}

fn decode_regular_image(path: &Path) -> Result<DynamicImage, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Opening a FIFO for reading normally waits forever for a writer. Use a
        // nonblocking handle, then reject everything except a regular file.
        options.custom_flags(libc::O_NONBLOCK);
    }

    let file = options
        .open(path)
        .map_err(|e| format!("Failed to open image: {}", e))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("Failed to inspect image: {}", e))?;
    if !metadata.is_file() {
        return Err("Image path is not a regular file".to_string());
    }

    let reader = image::ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .map_err(|e| format!("Failed to detect image format: {}", e))?;
    reader
        .decode()
        .map_err(|e| format!("Failed to load image: {}", e))
}

/// Check if terminal supports true color (24-bit RGB)
pub fn supports_true_color() -> bool {
    // Check TERM_PROGRAM for known terminals
    if let Ok(term_program) = std::env::var("TERM_PROGRAM") {
        match term_program.as_str() {
            "Apple_Terminal" => return false,
            "iTerm.app" | "WezTerm" | "Hyper" | "vscode" | "Tabby" | "Alacritty" => return true,
            _ => {}
        }
    }

    // iTerm2 sets this
    if std::env::var("ITERM_SESSION_ID").is_ok() {
        return true;
    }

    // iTerm2 also sets LC_TERMINAL
    if let Ok(lc_term) = std::env::var("LC_TERMINAL") {
        if lc_term == "iTerm2" {
            return true;
        }
    }

    // Windows Terminal
    if std::env::var("WT_SESSION").is_ok() {
        return true;
    }

    // COLORTERM is the most reliable indicator
    if let Ok(colorterm) = std::env::var("COLORTERM") {
        if colorterm == "truecolor" || colorterm == "24bit" {
            return true;
        }
    }

    // If none of the above, assume no true color support
    // This is conservative but safer
    false
}

pub struct ImageViewerState {
    pub path: std::path::PathBuf,
    pub image: Option<DynamicImage>,
    pub error: Option<String>,
    pub zoom: f32,
    pub offset_x: i32,
    pub offset_y: i32,
    /// List of image files in the same directory
    image_list: Vec<std::path::PathBuf>,
    /// Current index in the image list
    current_index: usize,
    /// Whether image is currently loading
    pub is_loading: bool,
    /// Receiver for async image loading result
    receiver: Option<Receiver<ImageLoadResult>>,
    /// Inline image protocol state (Kitty/iTerm2/Sixel)
    pub inline_protocol: Option<Box<dyn StatefulProtocol>>,
    /// Whether using inline image protocol (vs halfblocks)
    pub use_inline: bool,
}

impl ImageViewerState {
    pub fn new(path: &Path) -> Self {
        // Scan for image files in the same directory
        let (image_list, current_index) = Self::scan_images_in_directory(path);

        let mut state = Self {
            path: path.to_path_buf(),
            image: None,
            error: None,
            zoom: 1.0,
            offset_x: 0,
            offset_y: 0,
            image_list,
            current_index,
            is_loading: true,
            receiver: None,
            inline_protocol: None,
            use_inline: false,
        };

        // Start async image loading
        state.start_loading(path);
        state
    }

    /// Start async loading of an image
    fn start_loading(&mut self, path: &Path) {
        self.is_loading = true;
        self.image = None;
        self.error = None;

        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);

        let job = ImageLoadJob {
            path: path.to_path_buf(),
            result_tx: tx,
        };
        if let Err(error) = image_loader().try_send(job) {
            let (job, message) = match error {
                TrySendError::Full(job) => (job, "Image loader is busy"),
                TrySendError::Disconnected(job) => (job, "Image loader stopped unexpectedly"),
            };
            let _ = job.result_tx.send(ImageLoadResult {
                image: None,
                error: Some(message.to_string()),
            });
        }
    }

    /// Poll for image loading result
    /// Returns true if still loading
    pub fn poll(&mut self) -> bool {
        if !self.is_loading {
            return false;
        }

        if let Some(ref receiver) = self.receiver {
            match receiver.try_recv() {
                Ok(result) => {
                    self.image = result.image;
                    self.error = result.error;
                    self.is_loading = false;
                    self.receiver = None;
                    return false;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    return true; // Still loading
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.is_loading = false;
                    self.receiver = None;
                    self.error = Some("Image loading failed".to_string());
                    return false;
                }
            }
        }
        false
    }

    /// Scan images in the same directory and find current image index
    fn scan_images_in_directory(path: &Path) -> (Vec<std::path::PathBuf>, usize) {
        let mut image_list = Vec::new();
        let mut current_index = 0;

        if let Some(parent) = path.parent() {
            if let Ok(entries) = std::fs::read_dir(parent) {
                let mut images: Vec<_> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| is_image_file(p))
                    .collect();

                // Sort by filename for consistent ordering
                images.sort_by(|a, b| {
                    a.file_name()
                        .map(|s| s.to_string_lossy().to_lowercase())
                        .cmp(&b.file_name().map(|s| s.to_string_lossy().to_lowercase()))
                });

                // Find current image index
                if let Ok(canonical_path) = path
                    .canonicalize()
                    .map(crate::utils::format::strip_unc_prefix)
                {
                    for (i, img_path) in images.iter().enumerate() {
                        if let Ok(canonical_img) = img_path
                            .canonicalize()
                            .map(crate::utils::format::strip_unc_prefix)
                        {
                            if canonical_img == canonical_path {
                                current_index = i;
                                break;
                            }
                        }
                    }
                } else {
                    // Fallback: compare by filename
                    if let Some(filename) = path.file_name() {
                        for (i, img_path) in images.iter().enumerate() {
                            if img_path.file_name() == Some(filename) {
                                current_index = i;
                                break;
                            }
                        }
                    }
                }

                image_list = images;
            }
        }

        (image_list, current_index)
    }

    /// Navigate to the previous image in the directory
    pub fn navigate_prev(&mut self) -> bool {
        if self.image_list.is_empty() {
            return false;
        }

        let new_index = if self.current_index == 0 {
            self.image_list.len() - 1 // Wrap to last
        } else {
            self.current_index - 1
        };

        self.load_image_at_index(new_index)
    }

    /// Navigate to the next image in the directory
    pub fn navigate_next(&mut self) -> bool {
        if self.image_list.is_empty() {
            return false;
        }

        let new_index = if self.current_index >= self.image_list.len() - 1 {
            0 // Wrap to first
        } else {
            self.current_index + 1
        };

        self.load_image_at_index(new_index)
    }

    /// Load image at given index (async)
    fn load_image_at_index(&mut self, index: usize) -> bool {
        // Replacing the receiver does not cancel the old decoder thread. Without
        // this guard, holding next/previous can launch many full image decodes in
        // parallel and exhaust memory even though only the newest result is used.
        if self.is_loading || index >= self.image_list.len() {
            return false;
        }

        let new_path = self.image_list[index].clone();
        self.path = new_path.clone();
        self.current_index = index;
        // Reset view when switching images
        self.reset_view();
        // Reset inline protocol for new image
        self.inline_protocol = None;
        // Start async loading
        self.start_loading(&new_path);
        true
    }

    /// Get current image position info (e.g., "3/10")
    pub fn get_position_info(&self) -> String {
        if self.image_list.is_empty() {
            String::new()
        } else {
            format!("{}/{}", self.current_index + 1, self.image_list.len())
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
        self.offset_x = self.offset_x.saturating_add(dx);
        self.offset_y = self.offset_y.saturating_add(dy);
    }
}

/// Check if a file is a supported image format
pub fn is_image_file(path: &Path) -> bool {
    if let Some(ext) = path.extension() {
        let ext = ext.to_string_lossy().to_lowercase();
        matches!(
            ext.as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "tiff" | "tif"
        )
    } else {
        false
    }
}

/// Get spinner frame character based on current time
fn get_spinner_frame() -> char {
    const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let frame_idx = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        / 100) as usize
        % SPINNER_FRAMES.len();
    SPINNER_FRAMES[frame_idx]
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, theme: &Theme) {
    // Draw panels in background (항상 그림 - AI 모드 포함)
    super::draw::draw_panel_background(frame, app, area, theme);

    let state = match &app.image_viewer_state {
        Some(s) => s,
        None => return,
    };

    // AI 모드에서는 파일 패널 영역에만 이미지 오버레이 표시
    let overlay_area = if app.is_ai_mode() {
        // 패널 영역 계산 (draw.rs의 draw_panels와 동일한 동적 레이아웃)
        let chunks = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([
                ratatui::layout::Constraint::Min(5),
                ratatui::layout::Constraint::Length(1),
                ratatui::layout::Constraint::Length(1),
            ])
            .split(area);
        let num_panels = app.panels.len();
        let constraints: Vec<ratatui::layout::Constraint> = (0..num_panels)
            .map(|_| ratatui::layout::Constraint::Ratio(1, num_panels as u32))
            .collect();
        let panel_chunks = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Horizontal)
            .constraints(constraints)
            .split(chunks[0]);
        // active_panel_index에 해당하는 패널 영역 사용
        panel_chunks[app
            .active_panel_index
            .min(panel_chunks.len().saturating_sub(1))]
    } else {
        area
    };

    // Calculate viewer area (margin 없이 전체 영역 사용)
    let margin = 0;
    let viewer_width = overlay_area.width.saturating_sub(margin * 2);
    let viewer_height = overlay_area.height.saturating_sub(margin * 2);

    if viewer_width < 20 || viewer_height < 10 {
        return;
    }

    let x = overlay_area.x + margin;
    let y = overlay_area.y + margin;
    let viewer_area = Rect::new(x, y, viewer_width, viewer_height);

    // Clear area
    frame.render_widget(ratatui::widgets::Clear, viewer_area);

    let filename = state
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "Image".to_string());

    let position_info = state.get_position_info();
    let use_inline = state.use_inline;
    let img_dimensions = state.image.as_ref().map(|img| (img.width(), img.height()));
    let font_size = app.image_picker.as_ref().map(|p| p.font_size);
    let title = if let Some(ref img) = state.image {
        if use_inline {
            // Inline protocol: no zoom info
            if position_info.is_empty() {
                format!(" {} ({}x{}) ", filename, img.width(), img.height())
            } else {
                format!(
                    " {} [{}] ({}x{}) ",
                    filename,
                    position_info,
                    img.width(),
                    img.height()
                )
            }
        } else if position_info.is_empty() {
            format!(
                " {} ({}x{}) - {:.0}% ",
                filename,
                img.width(),
                img.height(),
                state.zoom * 100.0
            )
        } else {
            format!(
                " {} [{}] ({}x{}) - {:.0}% ",
                filename,
                position_info,
                img.width(),
                img.height(),
                state.zoom * 100.0
            )
        }
    } else if position_info.is_empty() {
        format!(" {} ", filename)
    } else {
        format!(" {} [{}] ", filename, position_info)
    };

    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(theme.image_viewer.title_text))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.image_viewer.border))
        .style(Style::default().bg(theme.image_viewer.bg));

    let inner = block.inner(viewer_area);
    frame.render_widget(block, viewer_area);

    // Show loading spinner if image is being loaded
    if state.is_loading {
        let spinner = get_spinner_frame();
        let loading_lines = vec![
            Line::from(""),
            Line::from(""),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    format!(" {} ", spinner),
                    Style::default().fg(theme.image_viewer.loading_spinner),
                ),
                Span::styled(
                    "Loading image...",
                    Style::default().fg(theme.image_viewer.loading_text),
                ),
            ]),
        ];

        // Center the loading message
        let center_y = inner.y + inner.height / 2 - 2;
        let loading_area = Rect::new(inner.x, center_y, inner.width, 4);
        let paragraph = Paragraph::new(loading_lines).alignment(ratatui::layout::Alignment::Center);
        frame.render_widget(paragraph, loading_area);
        return;
    }

    if let Some(ref error) = state.error {
        use crate::keybindings::ImageViewerAction;
        let close_key = app
            .keybindings
            .image_viewer_first_key(ImageViewerAction::Close);
        let error_lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                error.clone(),
                Style::default().fg(theme.image_viewer.error_text),
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!("Press {} to close", close_key),
                Style::default().fg(theme.image_viewer.hint_text),
            )),
        ];
        frame.render_widget(Paragraph::new(error_lines), inner);
        return;
    }

    // Render image (need mutable borrow for inline protocol)
    // Release immutable state borrow, then re-borrow mutably
    let _ = state;
    if let Some(ref mut state) = app.image_viewer_state {
        if let Some(ref mut protocol) = state.inline_protocol {
            // Inline protocol rendering (Kitty/iTerm2/Sixel) — centered
            let render_area =
                if let (Some((img_w, img_h)), Some((fw, fh))) = (img_dimensions, font_size) {
                    // Natural cell size (image at 1:1 pixel mapping)
                    let natural_cols = img_w as f64 / fw as f64;
                    let natural_rows = img_h as f64 / fh as f64;
                    // Scale to fit area, but Resize::Fit won't upscale, so cap at 1.0
                    let scale = (inner.width as f64 / natural_cols)
                        .min(inner.height as f64 / natural_rows)
                        .min(1.0);
                    let fit_cols = (natural_cols * scale).floor().max(1.0) as u16;
                    let fit_rows = (natural_rows * scale).floor().max(1.0) as u16;
                    let fit_cols = fit_cols.min(inner.width);
                    let fit_rows = fit_rows.min(inner.height);
                    let off_x = (inner.width.saturating_sub(fit_cols)) / 2;
                    let off_y = (inner.height.saturating_sub(fit_rows)) / 2;
                    Rect::new(inner.x + off_x, inner.y + off_y, fit_cols, fit_rows)
                } else {
                    inner
                };
            let image_widget = ratatui_image::StatefulImage::new(None);
            frame.render_stateful_widget(image_widget, render_area, protocol);
        } else if let Some(ref img) = state.image {
            // Halfblock fallback rendering (existing code)
            render_image(
                frame,
                img,
                inner,
                state.zoom,
                state.offset_x,
                state.offset_y,
            );
        }
    }

    // Help line at bottom (keybindings에서 동적으로)
    use crate::keybindings::ImageViewerAction;
    let kb = &app.keybindings;
    let help_area = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    let fk = Style::default().fg(theme.image_viewer.footer_key);
    let ft = Style::default().fg(theme.image_viewer.footer_text);
    let shortcuts: Vec<(String, &str)> = if use_inline {
        // Inline mode: no zoom/pan shortcuts
        vec![
            (
                kb.image_viewer_first_key(ImageViewerAction::PrevImage)
                    .to_string(),
                "prev ",
            ),
            (
                kb.image_viewer_first_key(ImageViewerAction::NextImage)
                    .to_string(),
                "next ",
            ),
            (
                kb.image_viewer_first_key(ImageViewerAction::Close)
                    .to_string(),
                "close",
            ),
        ]
    } else {
        vec![
            (
                kb.image_viewer_first_key(ImageViewerAction::PrevImage)
                    .to_string(),
                "prev ",
            ),
            (
                kb.image_viewer_first_key(ImageViewerAction::NextImage)
                    .to_string(),
                "next ",
            ),
            (
                kb.image_viewer_first_key(ImageViewerAction::ZoomIn)
                    .to_string(),
                "zoom+ ",
            ),
            (
                kb.image_viewer_first_key(ImageViewerAction::ZoomOut)
                    .to_string(),
                "zoom- ",
            ),
            (
                kb.image_viewer_first_key(ImageViewerAction::ResetView)
                    .to_string(),
                "reset ",
            ),
            (
                kb.image_viewer_first_key(ImageViewerAction::Close)
                    .to_string(),
                "close",
            ),
        ]
    };
    let mut help_spans = Vec::new();
    for (key, label) in &shortcuts {
        help_spans.push(Span::styled(key.as_str(), fk));
        help_spans.push(Span::styled(":", ft));
        help_spans.push(Span::styled(*label, ft));
    }
    let help = Line::from(help_spans);
    frame.render_widget(Paragraph::new(help), help_area);
}

fn render_image(
    frame: &mut Frame,
    img: &DynamicImage,
    area: Rect,
    zoom: f32,
    offset_x: i32,
    offset_y: i32,
) {
    let term_width = area.width as u32;
    let term_height = area.height.saturating_sub(1) as u32;
    let pixel_height = term_height * 2;

    let img_width = img.width();
    let img_height = img.height();

    // Calculate scale to fit image in terminal area
    let scale_x = term_width as f32 / img_width as f32;
    let scale_y = pixel_height as f32 / img_height as f32;
    let base_scale = scale_x.min(scale_y);
    let scale = base_scale * zoom;

    let scaled_width = ((img_width as f32 * scale) as u32).max(1);
    let scaled_height = ((img_height as f32 * scale) as u32).max(1);

    // Resize image and convert to RGB8
    let resized = img
        .resize_exact(
            scaled_width,
            scaled_height,
            image::imageops::FilterType::Triangle,
        )
        .to_rgb8();

    // Calculate offset for centering (in pixels)
    let center_offset_x = (term_width as i32 - scaled_width as i32) / 2;
    let center_offset_y = (pixel_height as i32 - scaled_height as i32) / 2;

    // Apply user pan offset
    let view_offset_x = center_offset_x + offset_x;
    let view_offset_y = center_offset_y + offset_y;

    let mut lines: Vec<Line> = Vec::new();

    for term_row in 0..term_height {
        let mut spans: Vec<Span> = Vec::new();

        let pixel_row_top = (term_row * 2) as i32;
        let pixel_row_bottom = (term_row * 2 + 1) as i32;

        for term_col in 0..term_width {
            let img_x = term_col as i32 - view_offset_x;
            let img_y_top = pixel_row_top - view_offset_y;
            let img_y_bottom = pixel_row_bottom - view_offset_y;

            let top_color = if img_x >= 0
                && img_x < scaled_width as i32
                && img_y_top >= 0
                && img_y_top < scaled_height as i32
            {
                let rgb = resized.get_pixel(img_x as u32, img_y_top as u32);
                Some(Color::Rgb(rgb[0], rgb[1], rgb[2]))
            } else {
                None
            };

            let bottom_color = if img_x >= 0
                && img_x < scaled_width as i32
                && img_y_bottom >= 0
                && img_y_bottom < scaled_height as i32
            {
                let rgb = resized.get_pixel(img_x as u32, img_y_bottom as u32);
                Some(Color::Rgb(rgb[0], rgb[1], rgb[2]))
            } else {
                None
            };

            let (ch, style) = match (top_color, bottom_color) {
                (Some(top), Some(bottom)) => ('▀', Style::default().fg(top).bg(bottom)),
                (Some(top), None) => ('▀', Style::default().fg(top)),
                (None, Some(bottom)) => ('▄', Style::default().fg(bottom)),
                (None, None) => (' ', Style::default()),
            };

            spans.push(Span::styled(ch.to_string(), style));
        }

        lines.push(Line::from(spans));
    }

    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(area.x, area.y, area.width, term_height as u16),
    );
}

pub fn handle_input(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    use crate::keybindings::ImageViewerAction;

    let state = match &mut app.image_viewer_state {
        Some(s) => s,
        None => {
            app.current_screen = Screen::FilePanel;
            return;
        }
    };

    if let Some(action) = app.keybindings.image_viewer_action(code, modifiers) {
        match action {
            ImageViewerAction::Close => {
                let last_image_name = state
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string());

                if let Some(filename) = last_image_name {
                    app.active_panel_mut().pending_focus = Some(filename);
                    app.active_panel_mut().load_files();
                }

                app.current_screen = Screen::FilePanel;
                app.image_viewer_state = None;
            }
            ImageViewerAction::ZoomIn => {
                if !state.use_inline {
                    state.zoom_in();
                }
            }
            ImageViewerAction::ZoomOut => {
                if !state.use_inline {
                    state.zoom_out();
                }
            }
            ImageViewerAction::ResetView => {
                if !state.use_inline {
                    state.reset_view();
                }
            }
            ImageViewerAction::PanUp => {
                if !state.use_inline {
                    state.pan(0, 5);
                }
            }
            ImageViewerAction::PanDown => {
                if !state.use_inline {
                    state.pan(0, -5);
                }
            }
            ImageViewerAction::PanLeft => {
                if !state.use_inline {
                    state.pan(5, 0);
                }
            }
            ImageViewerAction::PanRight => {
                if !state.use_inline {
                    state.pan(-5, 0);
                }
            }
            ImageViewerAction::PrevImage => {
                state.navigate_prev();
            }
            ImageViewerAction::NextImage => {
                state.navigate_next();
            }
            ImageViewerAction::ToggleSelect => {
                let filename = state
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string());
                state.navigate_next();
                if let Some(name) = filename {
                    let panel = app.active_panel_mut();
                    if panel.selected_files.contains(&name) {
                        panel.selected_files.remove(&name);
                    } else {
                        panel.selected_files.insert(name);
                    }
                }
            }
            ImageViewerAction::Delete => {
                app.show_delete_dialog();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unloaded_state(paths: Vec<std::path::PathBuf>) -> ImageViewerState {
        ImageViewerState {
            path: paths[0].clone(),
            image: None,
            error: None,
            zoom: 1.0,
            offset_x: 0,
            offset_y: 0,
            image_list: paths,
            current_index: 0,
            is_loading: true,
            receiver: None,
            inline_protocol: None,
            use_inline: false,
        }
    }

    #[test]
    fn navigation_does_not_start_a_second_decode_while_loading() {
        let mut state = unloaded_state(vec!["one.png".into(), "two.png".into()]);

        assert!(!state.navigate_next());
        assert_eq!(state.current_index, 0);
        assert_eq!(state.path, std::path::PathBuf::from("one.png"));
    }

    #[test]
    fn pan_saturates_instead_of_overflowing() {
        let mut state = unloaded_state(vec!["one.png".into()]);
        state.offset_x = i32::MAX;
        state.offset_y = i32::MIN;

        state.pan(1, -1);

        assert_eq!(state.offset_x, i32::MAX);
        assert_eq!(state.offset_y, i32::MIN);
    }

    #[test]
    fn directory_with_image_extension_is_rejected() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("folder.png");
        std::fs::create_dir(&path).unwrap();

        let err = decode_regular_image(&path).unwrap_err();

        assert!(err.contains("regular file") || err.contains("open image"));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_with_image_extension_is_rejected_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("pipe.png");
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        let started = std::time::Instant::now();
        let err = decode_regular_image(&path).unwrap_err();

        assert!(err.contains("regular file"));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
}
