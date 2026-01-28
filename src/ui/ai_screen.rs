use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use rand::Rng;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use super::theme::Theme;
use crate::services::claude::{self, ClaudeResponse};
use crate::utils::markdown::{render_markdown, MarkdownTheme};

#[derive(Debug, Clone)]
pub struct HistoryItem {
    pub item_type: HistoryType,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryType {
    User,
    Assistant,
    Error,
    System,
}

/// Placeholder messages for AI input
const PLACEHOLDER_MESSAGES: &[&str] = &[
    "Ask me about file operations...",
    "What would you like me to help with?",
    "Type your question or command...",
    "How can I assist you today?",
    "What files should I work with?",
];

pub struct AIScreenState {
    pub history: Vec<HistoryItem>,
    pub input_lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
    pub session_id: Option<String>,
    pub is_processing: bool,
    pub scroll_offset: usize,
    pub auto_scroll: bool,  // 자동 스크롤 활성화 여부
    pub claude_available: bool,
    pub current_path: String,
    pub placeholder_index: usize,
    /// Channel receiver for async Claude responses
    response_receiver: Option<Receiver<ClaudeResponse>>,
    /// Last known max scroll value (cached from draw)
    pub last_max_scroll: usize,
    /// Last known total lines (cached from draw)
    pub last_total_lines: usize,
    /// Last known visible height (cached from draw)
    pub last_visible_height: usize,
    /// Last known visible width (cached from draw)
    pub last_visible_width: usize,
    /// Last known raw lines count before wrap (cached from draw)
    pub last_raw_lines: usize,
    /// Debug mode: waiting for dummy line count input
    pub debug_input_mode: bool,
    /// Debug mode: accumulated number input
    pub debug_input_buffer: String,
}

impl AIScreenState {
    pub fn new(current_path: String) -> Self {
        let claude_available = claude::is_claude_available();
        let placeholder_index = rand::thread_rng().gen_range(0..PLACEHOLDER_MESSAGES.len());
        let mut state = Self {
            history: Vec::new(),
            input_lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            session_id: None,
            is_processing: false,
            scroll_offset: 0,
            auto_scroll: true,
            claude_available,
            current_path,
            placeholder_index,
            response_receiver: None,
            last_max_scroll: 0,
            last_total_lines: 0,
            last_visible_height: 0,
            last_visible_width: 0,
            last_raw_lines: 0,
            debug_input_mode: false,
            debug_input_buffer: String::new(),
        };

        if !claude::is_ai_supported() {
            state.history.push(HistoryItem {
                item_type: HistoryType::Error,
                content: "AI features are only available on Linux and macOS.".to_string(),
            });
        } else if !claude_available {
            state.history.push(HistoryItem {
                item_type: HistoryType::Error,
                content: "Claude CLI not found. Run 'which claude' to verify installation.".to_string(),
            });
        }

        state
    }

    /// Create state with existing history and session (for session persistence)
    pub fn with_session(
        current_path: String,
        history: Vec<HistoryItem>,
        session_id: Option<String>,
    ) -> Self {
        let claude_available = claude::is_claude_available();
        let placeholder_index = rand::thread_rng().gen_range(0..PLACEHOLDER_MESSAGES.len());
        Self {
            history,
            input_lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            session_id,
            is_processing: false,
            scroll_offset: usize::MAX,  // 센티널: 첫 draw에서 맨 아래로
            auto_scroll: true,
            claude_available,
            current_path,
            placeholder_index,
            response_receiver: None,
            last_max_scroll: 0,
            last_total_lines: 0,
            last_visible_height: 0,
            last_visible_width: 0,
            last_raw_lines: 0,
            debug_input_mode: false,
            debug_input_buffer: String::new(),
        }
    }

    /// Get current input text from lines
    fn get_input_text(&self) -> String {
        self.input_lines.join("\n")
    }

    /// Set input text and update lines
    fn set_input_text(&mut self, text: &str) {
        self.input_lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.lines().map(String::from).collect()
        };
        self.cursor_line = 0;
        self.cursor_col = 0;
    }

    /// Insert a newline at cursor position
    fn insert_newline(&mut self) {
        let current_line = &self.input_lines[self.cursor_line];
        let before: String = current_line.chars().take(self.cursor_col).collect();
        let after: String = current_line.chars().skip(self.cursor_col).collect();

        self.input_lines[self.cursor_line] = before;
        self.input_lines.insert(self.cursor_line + 1, after);
        self.cursor_line += 1;
        self.cursor_col = 0;
    }

    /// Insert a character at cursor position
    fn insert_char(&mut self, c: char) {
        let line = &mut self.input_lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();
        let mut new_line = String::new();
        for (i, ch) in chars.iter().enumerate() {
            if i == self.cursor_col {
                new_line.push(c);
            }
            new_line.push(*ch);
        }
        if self.cursor_col >= chars.len() {
            new_line.push(c);
        }
        *line = new_line;
        self.cursor_col += 1;
    }

    /// Delete character before cursor (backspace)
    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let line = &mut self.input_lines[self.cursor_line];
            let mut chars: Vec<char> = line.chars().collect();
            chars.remove(self.cursor_col - 1);
            *line = chars.into_iter().collect();
            self.cursor_col -= 1;
        } else if self.cursor_line > 0 {
            // Merge with previous line
            let current_line = self.input_lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = self.input_lines[self.cursor_line].chars().count();
            self.input_lines[self.cursor_line].push_str(&current_line);
        }
    }

    /// Delete character at cursor (delete key)
    fn delete_char(&mut self) {
        let line_len = self.input_lines[self.cursor_line].chars().count();
        if self.cursor_col < line_len {
            let line = &mut self.input_lines[self.cursor_line];
            let mut chars: Vec<char> = line.chars().collect();
            chars.remove(self.cursor_col);
            *line = chars.into_iter().collect();
        } else if self.cursor_line < self.input_lines.len() - 1 {
            // Merge with next line
            let next_line = self.input_lines.remove(self.cursor_line + 1);
            self.input_lines[self.cursor_line].push_str(&next_line);
        }
    }

    /// Move cursor left
    fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = self.input_lines[self.cursor_line].chars().count();
        }
    }

    /// Move cursor right
    fn move_right(&mut self) {
        let line_len = self.input_lines[self.cursor_line].chars().count();
        if self.cursor_col < line_len {
            self.cursor_col += 1;
        } else if self.cursor_line < self.input_lines.len() - 1 {
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
    }

    /// Move cursor up
    fn move_up(&mut self) {
        if self.cursor_line > 0 {
            self.cursor_line -= 1;
            let line_len = self.input_lines[self.cursor_line].chars().count();
            self.cursor_col = self.cursor_col.min(line_len);
        }
    }

    /// Move cursor down
    fn move_down(&mut self) {
        if self.cursor_line < self.input_lines.len() - 1 {
            self.cursor_line += 1;
            let line_len = self.input_lines[self.cursor_line].chars().count();
            self.cursor_col = self.cursor_col.min(line_len);
        }
    }

    /// Move cursor to start of line (Ctrl+A / Home)
    fn move_to_line_start(&mut self) {
        self.cursor_col = 0;
    }

    /// Move cursor to end of line (Ctrl+E / End)
    fn move_to_line_end(&mut self) {
        self.cursor_col = self.input_lines[self.cursor_line].chars().count();
    }

    /// Kill line to the right (Ctrl+K)
    fn kill_line_right(&mut self) {
        let line = &mut self.input_lines[self.cursor_line];
        *line = line.chars().take(self.cursor_col).collect();
    }

    /// Kill line to the left (Ctrl+U)
    fn kill_line_left(&mut self) {
        let line = &mut self.input_lines[self.cursor_line];
        *line = line.chars().skip(self.cursor_col).collect();
        self.cursor_col = 0;
    }

    /// Delete word backwards (Ctrl+W)
    fn delete_word_left(&mut self) {
        if self.cursor_col == 0 {
            return;
        }

        let line = &self.input_lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();
        let before: String = chars[..self.cursor_col].iter().collect();
        let after: String = chars[self.cursor_col..].iter().collect();

        let trimmed = before.trim_end();
        let new_col = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);

        let new_before: String = chars[..new_col].iter().collect();
        self.input_lines[self.cursor_line] = new_before + &after;
        self.cursor_col = new_col;
    }

    pub fn submit(&mut self) {
        let input_text = self.get_input_text();
        if input_text.trim().is_empty() || self.is_processing {
            return;
        }

        let user_input = input_text.trim().to_string();
        self.set_input_text("");

        // Handle /clear command
        if user_input.to_lowercase() == "/clear" {
            self.history.clear();
            self.session_id = None;
            self.scroll_offset = 0;
            return;
        }

        // Handle /debug command - enter debug mode for adding dummy lines
        if user_input.to_lowercase() == "/debug" {
            self.debug_input_mode = true;
            self.debug_input_buffer.clear();
            return;
        }

        // Handle /scroll command - show scroll debug info
        if user_input.to_lowercase() == "/scroll" {
            let info = format!(
                "Scroll Debug Info:\n\
                - scroll_offset: {}\n\
                - last_max_scroll: {}\n\
                - last_total_lines (wrapped): {}\n\
                - last_raw_lines (before wrap): {}\n\
                - last_visible_height: {}\n\
                - last_visible_width: {}\n\
                - auto_scroll: {}\n\
                - history items: {}",
                self.scroll_offset,
                self.last_max_scroll,
                self.last_total_lines,
                self.last_raw_lines,
                self.last_visible_height,
                self.last_visible_width,
                self.auto_scroll,
                self.history.len()
            );
            self.history.push(HistoryItem {
                item_type: HistoryType::System,
                content: info,
            });
            return;
        }

        // Handle /lines command - show detailed line info
        if user_input.to_lowercase() == "/lines" {
            use crate::utils::markdown::{render_markdown, MarkdownTheme};
            use crate::ui::theme::Theme;

            let theme = Theme::default();
            let md_theme = MarkdownTheme::from_theme(&theme);
            let width = if self.last_visible_width > 0 { self.last_visible_width } else { 80 };

            let mut info_lines: Vec<String> = vec!["Line Analysis:".to_string()];
            let mut total_calculated = 0usize;

            for (idx, item) in self.history.iter().enumerate() {
                let type_str = match item.item_type {
                    HistoryType::User => "User",
                    HistoryType::Assistant => "Assistant",
                    HistoryType::Error => "Error",
                    HistoryType::System => "System",
                };

                let (icon, color) = match item.item_type {
                    HistoryType::User => ("> ", theme.info),
                    HistoryType::Assistant => ("< ", theme.success),
                    HistoryType::Error => ("! ", theme.error),
                    HistoryType::System => ("* ", theme.text_dim),
                };
                let prefix_style = Style::default().fg(color).add_modifier(Modifier::BOLD);

                // Build actual Line objects like draw_history does
                let mut item_line_objects: Vec<Line> = Vec::new();
                if item.item_type == HistoryType::Assistant {
                    let md_lines = render_markdown(&item.content, md_theme);
                    for (i, md_line) in md_lines.into_iter().enumerate() {
                        let prefix = if i == 0 { icon } else { "  " };
                        let mut spans = vec![Span::styled(prefix, prefix_style)];
                        spans.extend(md_line.spans);
                        item_line_objects.push(Line::from(spans));
                    }
                } else {
                    let content_lines: Vec<&str> = item.content.lines().collect();
                    for (i, line_text) in content_lines.iter().enumerate() {
                        let prefix = if i == 0 { icon } else { "  " };
                        item_line_objects.push(Line::from(vec![
                            Span::styled(prefix, prefix_style),
                            Span::styled(line_text.to_string(), theme.normal_style()),
                        ]));
                    }
                }

                // Calculate wrapped lines using the same function as draw_history
                let item_lines: usize = item_line_objects.iter()
                    .map(|line| estimate_wrapped_lines(line, width))
                    .sum();

                total_calculated += item_lines + 1; // +1 for empty line between

                info_lines.push(format!(
                    "Item {}: {} - {} raw, {} wrapped (width={})",
                    idx, type_str,
                    item_line_objects.len(),
                    item_lines,
                    width
                ));
            }

            info_lines.push(format!("Total calculated: {} (last_total_lines: {})",
                total_calculated, self.last_total_lines));

            self.history.push(HistoryItem {
                item_type: HistoryType::System,
                content: info_lines.join("\n"),
            });
            return;
        }

        // Handle /markdown command - show sample markdown
        if user_input.to_lowercase() == "/markdown" || user_input.to_lowercase() == "/md" {
            let sample_markdown = r#"# Markdown Sample

## Headers
### Level 3 Header
#### Level 4 Header

## Text Formatting
This is **bold text** and this is *italic text*.
You can also use _underscores for italic_ and ~~strikethrough~~.
Mix them: ***bold and italic***

## Code
Inline `code` looks like this.

```rust
fn main() {
    println!("Hello, World!");
    let x = 42;
}
```

```python
def hello():
    print("Hello from Python!")
```

## Table
| 이름   | 나이 | 도시     |
|--------|------|----------|
| 홍길동 | 30   | 서울     |
| 김철수 | 25   | 도쿄     |
| Alice  | 35   | Beijing  |

## Lists
### Unordered
- First item
- Second item
  - Nested item
  - Another nested
- Third item

### Ordered
1. First step
2. Second step
3. Third step

### Checkboxes
- [ ] Unchecked task
- [x] Completed task
- [ ] Another task

## Blockquote
> This is a blockquote.
> It can span multiple lines.
>> Nested blockquote

## Links
Check out [Rust](https://rust-lang.org) for more info.

## Horizontal Rule
---

## Special Characters
Korean: 안녕하세요! 한글 테스트입니다.
Emoji: 🎉 🚀 ✨ (if supported)

---
*End of sample*"#;

            self.history.push(HistoryItem {
                item_type: HistoryType::Assistant,
                content: sample_markdown.to_string(),
            });
            if self.auto_scroll {
                self.scroll_offset = usize::MAX;
            }
            return;
        }

        // Check claude availability before actual API call
        if !self.claude_available {
            return;
        }

        // Add user message immediately
        self.history.push(HistoryItem {
            item_type: HistoryType::User,
            content: user_input.clone(),
        });

        // Set processing state
        self.is_processing = true;

        // Prepare context for async execution
        let context_prompt = format!(
            "You are an AI assistant helping with file management in a Norton Commander-style file manager.
Current working directory: {}

User request: {}

If the user asks to perform file operations, provide clear instructions.
Keep responses concise and terminal-friendly.",
            self.current_path, user_input
        );

        let session_id = self.session_id.clone();
        let current_path = self.current_path.clone();

        // Create channel for async response
        let (tx, rx) = mpsc::channel();
        self.response_receiver = Some(rx);

        // Spawn thread to execute Claude command
        thread::spawn(move || {
            let response = claude::execute_command(
                &context_prompt,
                session_id.as_deref(),
                &current_path,
            );
            let _ = tx.send(response);
        });
    }

    /// Poll for async response from Claude
    /// Returns true if still processing, false if done or no request pending
    pub fn poll_response(&mut self) -> bool {
        if !self.is_processing {
            return false;
        }

        if let Some(ref receiver) = self.response_receiver {
            match receiver.try_recv() {
                Ok(response) => {
                    // Got response
                    if response.success {
                        if let Some(sid) = response.session_id {
                            self.session_id = Some(sid);
                        }
                        self.history.push(HistoryItem {
                            item_type: HistoryType::Assistant,
                            content: response.response.unwrap_or_else(|| "Command executed.".to_string()),
                        });
                    } else {
                        self.history.push(HistoryItem {
                            item_type: HistoryType::Error,
                            content: response.error.unwrap_or_else(|| "Unknown error".to_string()),
                        });
                    }

                    self.is_processing = false;
                    self.response_receiver = None;
                    // Scroll to bottom if auto_scroll is enabled
                    // Use sentinel value - will be normalized in draw
                    if self.auto_scroll {
                        self.scroll_offset = usize::MAX;
                    }
                    return false;
                }
                Err(TryRecvError::Empty) => {
                    // Still processing
                    return true;
                }
                Err(TryRecvError::Disconnected) => {
                    // Channel closed unexpectedly
                    self.history.push(HistoryItem {
                        item_type: HistoryType::Error,
                        content: "Request was cancelled or failed.".to_string(),
                    });
                    self.is_processing = false;
                    self.response_receiver = None;
                    return false;
                }
            }
        }

        false
    }

    /// Cancel the current processing request
    pub fn cancel_processing(&mut self) {
        if self.is_processing {
            self.is_processing = false;
            self.response_receiver = None;
            self.history.push(HistoryItem {
                item_type: HistoryType::System,
                content: "Cancelled.".to_string(),
            });
        }
    }

    /// Get the placeholder message
    pub fn get_placeholder(&self) -> &'static str {
        PLACEHOLDER_MESSAGES[self.placeholder_index]
    }
}

pub fn draw(frame: &mut Frame, state: &mut AIScreenState, area: Rect, theme: &Theme) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),    // History area
            Constraint::Length(3), // Input area
            Constraint::Length(1), // Help
        ])
        .split(area);

    // History area (with path and session in title)
    draw_history(frame, state, chunks[0], theme);

    // Input area
    draw_input(frame, state, chunks[1], theme);

    // Help (same style as main screen function bar)
    let help_spans = if state.is_processing {
        vec![
            Span::styled("Esc", theme.header_style()),
            Span::styled(" Cancel ", theme.dim_style()),
            Span::styled("PgUp", theme.header_style()),
            Span::styled("/", theme.dim_style()),
            Span::styled("PgDn", theme.header_style()),
            Span::styled(" Scroll", theme.dim_style()),
        ]
    } else {
        vec![
            Span::styled("Enter", theme.header_style()),
            Span::styled(" Send ", theme.dim_style()),
            Span::styled("↑↓", theme.header_style()),
            Span::styled("/", theme.dim_style()),
            Span::styled("PgUp", theme.header_style()),
            Span::styled("/", theme.dim_style()),
            Span::styled("PgDn", theme.header_style()),
            Span::styled(" Scroll ", theme.dim_style()),
            Span::styled("Esc", theme.header_style()),
            Span::styled(" Close", theme.dim_style()),
        ]
    };
    frame.render_widget(
        Paragraph::new(Line::from(help_spans)),
        chunks[2],
    );
}

fn draw_history(frame: &mut Frame, state: &mut AIScreenState, area: Rect, theme: &Theme) {
    // Build title with path and session info
    let session_info = if let Some(ref sid) = state.session_id {
        format!("Session: {}...", &sid[..sid.len().min(8)])
    } else {
        "New Session".to_string()
    };

    let title = format!(" {} | {} ", state.current_path, session_info);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style(true))
        .title(Span::styled(
            title,
            Style::default().fg(theme.border_active).add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.history.is_empty() {
        let placeholder = Paragraph::new(Span::styled(
            state.get_placeholder(),
            theme.dim_style(),
        ));
        frame.render_widget(placeholder, inner);
        return;
    }

    // Calculate visible area dimensions
    let visible_height = inner.height as usize;
    let md_theme = MarkdownTheme::from_theme(theme);

    // Build all lines (without manual wrapping - let Paragraph handle it)
    let mut lines: Vec<Line> = Vec::new();

    for item in &state.history {
        let (icon, color) = match item.item_type {
            HistoryType::User => ("> ", theme.info),
            HistoryType::Assistant => ("< ", theme.success),
            HistoryType::Error => ("! ", theme.error),
            HistoryType::System => ("* ", theme.text_dim),
        };

        let prefix_style = Style::default().fg(color).add_modifier(Modifier::BOLD);

        // For assistant messages, render Markdown
        if item.item_type == HistoryType::Assistant {
            let md_lines = render_markdown(&item.content, md_theme);
            for (i, md_line) in md_lines.into_iter().enumerate() {
                let prefix = if i == 0 { icon } else { "  " };
                let mut spans = vec![Span::styled(prefix, prefix_style)];
                spans.extend(md_line.spans);
                lines.push(Line::from(spans));
            }
        } else {
            // Regular text rendering for non-assistant messages
            let content_lines: Vec<&str> = item.content.lines().collect();
            for (i, line_text) in content_lines.iter().enumerate() {
                let prefix = if i == 0 { icon } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(prefix, prefix_style),
                    Span::styled(line_text.to_string(), theme.normal_style()),
                ]));
            }
        }
        lines.push(Line::from("")); // Empty line between messages
    }

    // Calculate total wrapped lines by simulating ratatui's greedy word-wrap behavior
    let width = inner.width as usize;
    let total_lines: usize = if width == 0 {
        lines.len()
    } else {
        lines.iter().map(|line| {
            estimate_wrapped_lines(line, width)
        }).sum()
    };
    let max_scroll = total_lines.saturating_sub(visible_height);

    // 캐시 값 업데이트 (handle_input에서 사용)
    state.last_max_scroll = max_scroll;
    state.last_total_lines = total_lines;
    state.last_visible_height = visible_height;
    state.last_visible_width = width;
    // Debug: store actual lines count (before wrap calculation)
    state.last_raw_lines = lines.len();

    // 스크롤 오프셋 정규화
    let effective_scroll = if state.scroll_offset == usize::MAX {
        // 센티널 처리: 맨 아래로
        max_scroll
    } else if state.auto_scroll && state.is_processing {
        // 자동 스크롤 모드 + 처리 중
        max_scroll
    } else {
        // 범위 제한
        state.scroll_offset.min(max_scroll)
    };

    // scroll_offset을 항상 정규화된 값으로 업데이트
    state.scroll_offset = effective_scroll;

    // 맨 아래에 도달하면 auto_scroll 재활성화
    if effective_scroll >= max_scroll && max_scroll > 0 {
        state.auto_scroll = true;
    }

    // Use Paragraph's scroll method with Wrap
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((effective_scroll as u16, 0));
    frame.render_widget(paragraph, inner);

    // Show scroll indicator if there's more content
    if total_lines > visible_height {
        // Use original total_lines for display (not buffered value)
        let display_position = (effective_scroll + visible_height).min(total_lines);
        let scroll_info = format!(
            " [{}/{}] ",
            display_position,
            total_lines
        );
        let info_len = scroll_info.len() as u16;
        let indicator_x = inner.x + inner.width.saturating_sub(info_len + 1);
        frame.render_widget(
            Paragraph::new(Span::styled(scroll_info, theme.dim_style())),
            Rect::new(indicator_x, inner.y, info_len, 1),
        );
    }
}

fn draw_input(frame: &mut Frame, state: &AIScreenState, area: Rect, theme: &Theme) {
    let input_color = theme.success;

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(input_color))
        .border_type(ratatui::widgets::BorderType::Rounded);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.is_processing {
        let spinner_frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let frame_idx = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() / 100) as usize % spinner_frames.len();

        let processing_line = Line::from(vec![
            Span::styled(
                format!("{} ", spinner_frames[frame_idx]),
                Style::default().fg(theme.info),
            ),
            Span::styled("Processing... (Esc to cancel)", theme.dim_style()),
        ]);
        frame.render_widget(Paragraph::new(processing_line), inner);
    } else if state.debug_input_mode {
        // Debug mode: show prompt for number of dummy lines
        let debug_line = Line::from(vec![
            Span::styled("[DEBUG] ", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
            Span::styled("Enter number of dummy lines: ", theme.normal_style()),
            Span::styled(&state.debug_input_buffer, Style::default().fg(theme.info)),
            Span::styled("_", Style::default().fg(theme.border_active).add_modifier(Modifier::SLOW_BLINK)),
            Span::styled(" (Enter=Add, Esc=Cancel)", theme.dim_style()),
        ]);
        frame.render_widget(Paragraph::new(debug_line), inner);
    } else if !state.claude_available {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "Claude CLI not available",
                theme.error_style(),
            )),
            inner,
        );
    } else {
        // Render multiline input
        let input_text = state.get_input_text();
        if input_text.is_empty() {
            // Show placeholder with cursor
            let placeholder_line = Line::from(vec![
                Span::styled("> ", Style::default().fg(input_color)),
                Span::styled("_", Style::default().fg(theme.border_active).add_modifier(Modifier::SLOW_BLINK)),
                Span::styled(state.get_placeholder(), theme.dim_style()),
            ]);
            frame.render_widget(Paragraph::new(placeholder_line), inner);
        } else {
            let mut lines: Vec<Line> = Vec::new();
            for (line_idx, line_text) in state.input_lines.iter().enumerate() {
                let prefix = if line_idx == 0 { "> " } else { "  " };
                let is_cursor_line = line_idx == state.cursor_line;

                if is_cursor_line {
                    // Insert cursor at position
                    let chars: Vec<char> = line_text.chars().collect();
                    let before: String = chars.iter().take(state.cursor_col).collect();
                    let after: String = chars.iter().skip(state.cursor_col).collect();

                    lines.push(Line::from(vec![
                        Span::styled(prefix, Style::default().fg(input_color)),
                        Span::styled(before, theme.normal_style()),
                        Span::styled("_", Style::default().fg(theme.border_active).add_modifier(Modifier::SLOW_BLINK)),
                        Span::styled(after, theme.normal_style()),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled(prefix, Style::default().fg(input_color)),
                        Span::styled(line_text.clone(), theme.normal_style()),
                    ]));
                }
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
    }
}

/// Estimate the number of wrapped lines for ratatui's greedy word-wrap
/// This simulates ratatui's Wrap { trim: false } behavior
fn estimate_wrapped_lines(line: &Line, width: usize) -> usize {
    use unicode_width::UnicodeWidthStr;

    if width == 0 {
        return 1;
    }

    // Extract full text from all spans
    let text: String = line.spans.iter()
        .map(|span| span.content.as_ref())
        .collect();

    if text.is_empty() {
        return 1;
    }

    // IMPORTANT: ratatui 0.28 with Wrap { trim: false } renders whitespace-only
    // lines as 2 rows. This is a known behavior that we need to account for.
    if !text.is_empty() && text.trim().is_empty() {
        return 2;
    }

    // Calculate total width using unicode-width
    let text_width = UnicodeWidthStr::width(text.as_str());

    // If text fits in one line, return 1
    if text_width <= width {
        return 1;
    }

    // Simulate greedy word-wrap (like ratatui's WordWrapper)
    let mut line_count = 1;
    let mut current_width = 0;

    // Process word by word, preserving whitespace behavior
    let chars = text.chars();
    let mut word_width = 0;
    let mut in_word = false;

    for c in chars {
        let char_width = UnicodeWidthStr::width(c.to_string().as_str());

        if c.is_whitespace() {
            // End of word - add word + whitespace to current line
            if in_word {
                if current_width + word_width > width {
                    // Word doesn't fit, start new line
                    if word_width > width {
                        // Word is longer than width, needs multiple lines
                        line_count += word_width.div_ceil(width);
                        current_width = word_width % width;
                        if current_width == 0 {
                            current_width = 0;
                        }
                    } else {
                        line_count += 1;
                        current_width = word_width;
                    }
                } else {
                    current_width += word_width;
                }
                word_width = 0;
                in_word = false;
            }

            // Add whitespace
            if current_width + char_width > width {
                line_count += 1;
                current_width = char_width;
            } else {
                current_width += char_width;
            }
        } else {
            // Building a word
            in_word = true;
            word_width += char_width;
        }
    }

    // Handle last word
    if in_word && word_width > 0 && current_width + word_width > width {
        if word_width > width {
            line_count += word_width.div_ceil(width);
        } else {
            line_count += 1;
        }
    }

    line_count
}

/// Generate a random lorem ipsum-like line
fn generate_lorem_line(line_num: usize) -> String {
    const WORDS: &[&str] = &[
        "lorem", "ipsum", "dolor", "sit", "amet", "consectetur", "adipiscing", "elit",
        "sed", "do", "eiusmod", "tempor", "incididunt", "ut", "labore", "et", "dolore",
        "magna", "aliqua", "enim", "ad", "minim", "veniam", "quis", "nostrud",
        "exercitation", "ullamco", "laboris", "nisi", "aliquip", "ex", "ea", "commodo",
        "consequat", "duis", "aute", "irure", "in", "reprehenderit", "voluptate",
        "velit", "esse", "cillum", "fugiat", "nulla", "pariatur", "excepteur", "sint",
        "occaecat", "cupidatat", "non", "proident", "sunt", "culpa", "qui", "officia",
        "deserunt", "mollit", "anim", "id", "est", "laborum",
    ];

    let mut rng = rand::thread_rng();
    // Random word count between 3 and 15
    let word_count = rng.gen_range(3..=15);

    let line_words: Vec<&str> = (0..word_count)
        .map(|_| WORDS[rng.gen_range(0..WORDS.len())])
        .collect();

    format!("{}. {}", line_num, line_words.join(" "))
}

/// Helper function to scroll up by a given amount
fn scroll_up(state: &mut AIScreenState, amount: usize) {
    // 센티널 값(usize::MAX) 처리: 실제 max_scroll 값으로 정규화
    let current_scroll = if state.scroll_offset == usize::MAX {
        state.last_max_scroll
    } else {
        state.scroll_offset.min(state.last_max_scroll)
    };

    if current_scroll > 0 {
        state.scroll_offset = current_scroll.saturating_sub(amount);
        state.auto_scroll = false;  // 수동 스크롤 시 비활성화
    }
}

/// Helper function to scroll down by a given amount
fn scroll_down(state: &mut AIScreenState, amount: usize) {
    // 센티널 값(usize::MAX) 처리: 실제 max_scroll 값으로 정규화
    let current_scroll = if state.scroll_offset == usize::MAX {
        state.last_max_scroll
    } else {
        // Don't limit here - let draw() handle final normalization
        // This allows scrolling to new content before draw() updates last_max_scroll
        state.scroll_offset
    };

    let new_scroll = current_scroll.saturating_add(amount);
    // Don't limit to last_max_scroll here - it might be stale!
    // The actual limit will be applied in draw_history()
    state.scroll_offset = new_scroll;

    // Don't re-enable auto_scroll here - let draw() handle it
    // when it knows the actual max_scroll value
}

pub fn handle_input(state: &mut AIScreenState, code: KeyCode, modifiers: KeyModifiers) -> bool {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let shift = modifiers.contains(KeyModifiers::SHIFT);

    // Handle debug input mode (waiting for number of dummy lines)
    if state.debug_input_mode {
        match code {
            KeyCode::Esc => {
                // Cancel debug mode
                state.debug_input_mode = false;
                state.debug_input_buffer.clear();
            }
            KeyCode::Enter => {
                // Add dummy lines as a single message with random lorem ipsum
                if let Ok(count) = state.debug_input_buffer.parse::<usize>() {
                    let dummy_content: String = (1..=count)
                        .map(generate_lorem_line)
                        .collect::<Vec<_>>()
                        .join("\n");
                    state.history.push(HistoryItem {
                        item_type: HistoryType::System,
                        content: dummy_content,
                    });
                    // Scroll to bottom after adding
                    if state.auto_scroll {
                        state.scroll_offset = usize::MAX;
                    }
                }
                state.debug_input_mode = false;
                state.debug_input_buffer.clear();
            }
            KeyCode::Backspace => {
                state.debug_input_buffer.pop();
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                state.debug_input_buffer.push(c);
            }
            _ => {}
        }
        return false;
    }

    match code {
        KeyCode::Esc => {
            let input_text = state.get_input_text();
            if state.is_processing {
                state.cancel_processing();
            } else if !input_text.is_empty() {
                // Clear input on first ESC
                state.set_input_text("");
            } else {
                // Exit on second ESC (when input is empty)
                return true;
            }
        }
        KeyCode::Enter => {
            if shift || ctrl {
                // Shift+Enter or Ctrl+Enter: insert newline
                state.insert_newline();
            } else {
                // Regular Enter: submit
                state.submit();
            }
        }
        KeyCode::Backspace => {
            state.backspace();
        }
        KeyCode::Delete => {
            state.delete_char();
        }
        KeyCode::Left => {
            state.move_left();
        }
        KeyCode::Right => {
            state.move_right();
        }
        KeyCode::Up => {
            if ctrl {
                // Ctrl+Up: scroll history up
                scroll_up(state, 1);
            } else if state.input_lines.len() > 1 {
                // Multi-line input: move cursor up
                state.move_up();
            } else {
                // Single-line input: scroll history up
                scroll_up(state, 1);
            }
        }
        KeyCode::Down => {
            if ctrl {
                // Ctrl+Down: scroll history down
                scroll_down(state, 1);
            } else if state.input_lines.len() > 1 {
                // Multi-line input: move cursor down
                state.move_down();
            } else {
                // Single-line input: scroll history down
                scroll_down(state, 1);
            }
        }
        KeyCode::PageUp => {
            // Scroll history up by visible_height or 10 lines
            let scroll_amount = if state.last_visible_height > 1 {
                state.last_visible_height.saturating_sub(1)
            } else {
                10
            };
            scroll_up(state, scroll_amount);
        }
        KeyCode::PageDown => {
            // Scroll history down by visible_height or 10 lines
            let scroll_amount = if state.last_visible_height > 1 {
                state.last_visible_height.saturating_sub(1)
            } else {
                10
            };
            scroll_down(state, scroll_amount);
        }
        KeyCode::Home => {
            if ctrl {
                // Ctrl+Home: scroll to top
                state.scroll_offset = 0;
                state.auto_scroll = false;
            } else {
                state.move_to_line_start();
            }
        }
        KeyCode::End => {
            if ctrl {
                // Ctrl+End: scroll to bottom and re-enable auto_scroll
                state.scroll_offset = state.last_max_scroll;
                state.auto_scroll = true;
            } else {
                state.move_to_line_end();
            }
        }
        KeyCode::Char('a') if ctrl => {
            // Ctrl+A: move to line start
            state.move_to_line_start();
        }
        KeyCode::Char('e') if ctrl => {
            // Ctrl+E: move to line end
            state.move_to_line_end();
        }
        KeyCode::Char('u') if ctrl => {
            // Ctrl+U: kill line left
            state.kill_line_left();
        }
        KeyCode::Char('k') if ctrl => {
            // Ctrl+K: kill line right
            state.kill_line_right();
        }
        KeyCode::Char('w') if ctrl => {
            // Ctrl+W: delete word backwards
            state.delete_word_left();
        }
        KeyCode::Char(c) => {
            if !ctrl {
                state.insert_char(c);
            }
        }
        _ => {}
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_state() -> AIScreenState {
        let mut state = AIScreenState::new("/test".to_string());
        // Clear any system messages
        state.history.clear();
        // Simulate cached values from draw
        state.last_max_scroll = 50;
        state.last_total_lines = 100;
        state.last_visible_height = 20;
        state
    }

    #[test]
    fn test_scroll_up_from_sentinel() {
        let mut state = create_test_state();
        state.scroll_offset = usize::MAX;  // Sentinel value
        state.auto_scroll = true;

        scroll_up(&mut state, 1);

        // Should normalize to max_scroll - 1
        assert_eq!(state.scroll_offset, 49);
        assert!(!state.auto_scroll);
    }

    #[test]
    fn test_scroll_up_from_normal() {
        let mut state = create_test_state();
        state.scroll_offset = 30;
        state.auto_scroll = false;

        scroll_up(&mut state, 5);

        assert_eq!(state.scroll_offset, 25);
        assert!(!state.auto_scroll);
    }

    #[test]
    fn test_scroll_up_at_top() {
        let mut state = create_test_state();
        state.scroll_offset = 0;
        state.auto_scroll = false;

        scroll_up(&mut state, 10);

        // Should stay at 0
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_scroll_down_from_normal() {
        let mut state = create_test_state();
        state.scroll_offset = 30;
        state.auto_scroll = false;

        scroll_down(&mut state, 5);

        assert_eq!(state.scroll_offset, 35);
        assert!(!state.auto_scroll);
    }

    #[test]
    fn test_scroll_down_to_bottom_enables_auto_scroll() {
        let mut state = create_test_state();
        state.scroll_offset = 45;
        state.auto_scroll = false;

        scroll_down(&mut state, 10);

        // scroll_down no longer caps - draw() will normalize
        // So scroll_offset = 45 + 10 = 55
        assert_eq!(state.scroll_offset, 55);
        // auto_scroll is now set by draw(), not scroll_down
        assert!(!state.auto_scroll);
    }

    #[test]
    fn test_scroll_down_from_sentinel() {
        let mut state = create_test_state();
        state.scroll_offset = usize::MAX;
        state.auto_scroll = true;

        scroll_down(&mut state, 1);

        // Sentinel is normalized to last_max_scroll, then +1
        // 50 + 1 = 51 (draw() will cap this to actual max_scroll)
        assert_eq!(state.scroll_offset, 51);
        // auto_scroll unchanged by scroll_down
        assert!(state.auto_scroll);
    }

    #[test]
    fn test_page_up_uses_visible_height() {
        let mut state = create_test_state();
        state.scroll_offset = 40;
        state.auto_scroll = false;
        state.last_visible_height = 20;

        // PageUp should scroll by visible_height - 1 = 19
        handle_input(&mut state, KeyCode::PageUp, KeyModifiers::empty());

        assert_eq!(state.scroll_offset, 21);  // 40 - 19 = 21
    }

    #[test]
    fn test_page_down_uses_visible_height() {
        let mut state = create_test_state();
        state.scroll_offset = 10;
        state.auto_scroll = false;
        state.last_visible_height = 20;

        // PageDown should scroll by visible_height - 1 = 19
        handle_input(&mut state, KeyCode::PageDown, KeyModifiers::empty());

        assert_eq!(state.scroll_offset, 29);  // 10 + 19 = 29
    }

    #[test]
    fn test_ctrl_home_scrolls_to_top() {
        let mut state = create_test_state();
        state.scroll_offset = 30;
        state.auto_scroll = true;

        handle_input(&mut state, KeyCode::Home, KeyModifiers::CONTROL);

        assert_eq!(state.scroll_offset, 0);
        assert!(!state.auto_scroll);
    }

    #[test]
    fn test_ctrl_end_scrolls_to_bottom() {
        let mut state = create_test_state();
        state.scroll_offset = 10;
        state.auto_scroll = false;

        handle_input(&mut state, KeyCode::End, KeyModifiers::CONTROL);

        assert_eq!(state.scroll_offset, 50);  // last_max_scroll
        assert!(state.auto_scroll);
    }

    #[test]
    fn test_up_arrow_scrolls_when_single_line_input() {
        let mut state = create_test_state();
        state.input_lines = vec!["test".to_string()];
        state.scroll_offset = 30;
        state.auto_scroll = false;

        handle_input(&mut state, KeyCode::Up, KeyModifiers::empty());

        assert_eq!(state.scroll_offset, 29);
    }

    #[test]
    fn test_up_arrow_moves_cursor_when_multiline_input() {
        let mut state = create_test_state();
        state.input_lines = vec!["line1".to_string(), "line2".to_string()];
        state.cursor_line = 1;
        state.cursor_col = 2;
        state.scroll_offset = 30;

        handle_input(&mut state, KeyCode::Up, KeyModifiers::empty());

        // Cursor should move up, scroll should stay same
        assert_eq!(state.cursor_line, 0);
        assert_eq!(state.scroll_offset, 30);
    }

    #[test]
    fn test_ctrl_up_always_scrolls() {
        let mut state = create_test_state();
        state.input_lines = vec!["line1".to_string(), "line2".to_string()];
        state.cursor_line = 1;
        state.scroll_offset = 30;

        handle_input(&mut state, KeyCode::Up, KeyModifiers::CONTROL);

        // Cursor should NOT move, scroll should change
        assert_eq!(state.cursor_line, 1);
        assert_eq!(state.scroll_offset, 29);
    }

    #[test]
    fn test_scroll_with_zero_max_scroll() {
        let mut state = create_test_state();
        state.last_max_scroll = 0;
        state.scroll_offset = 0;

        scroll_up(&mut state, 1);
        assert_eq!(state.scroll_offset, 0);  // Can't scroll up from 0

        scroll_down(&mut state, 1);
        // scroll_down no longer caps - draw() will normalize to 0
        assert_eq!(state.scroll_offset, 1);
    }

    #[test]
    fn test_textwrap_line_calculation() {
        // Test that textwrap calculates wrapped lines correctly
        let width = 40usize;
        let wrap_options = textwrap::Options::new(width)
            .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
            .word_splitter(textwrap::WordSplitter::NoHyphenation);

        // Short line - should be 1 line
        let short = "Hello world";
        assert_eq!(textwrap::wrap(short, &wrap_options).len(), 1);

        // Long line - should wrap
        let long = "This is a very long line that should definitely wrap to multiple lines when the width is only 40 characters";
        let wrapped = textwrap::wrap(long, &wrap_options);
        assert!(wrapped.len() > 1, "Long line should wrap: {:?}", wrapped);

        // Empty line - textwrap returns 1 for empty string (contains one empty &str)
        let empty = "";
        assert_eq!(textwrap::wrap(empty, &wrap_options).len(), 1);

        // Korean text (wide characters)
        let korean = "안녕하세요 이것은 한글 테스트입니다";
        let korean_wrapped = textwrap::wrap(korean, &wrap_options);
        println!("Korean wrapped: {:?}", korean_wrapped);
    }

    #[test]
    fn test_max_scroll_calculation() {
        // Simulate the calculation done in draw_history
        let visible_height = 10usize;
        let total_lines = 25usize;

        let max_scroll = total_lines.saturating_sub(visible_height);
        assert_eq!(max_scroll, 15);  // 25 - 10 = 15

        // When at max_scroll, last line should be at bottom
        // scroll_offset = 15 means we skip first 15 lines
        // showing lines 16-25 (10 lines) in visible area
    }

    #[test]
    fn test_scroll_shows_all_content() {
        // Simulate: 30 total lines, 20 visible
        // max_scroll should be 10
        // At scroll_offset=10, we should see lines 11-30
        let total_lines = 30usize;
        let visible_height = 20usize;
        let max_scroll = total_lines.saturating_sub(visible_height);

        assert_eq!(max_scroll, 10);

        // Verify: at max_scroll, the last visible line is total_lines
        let scroll_offset = max_scroll;
        let first_visible = scroll_offset + 1;  // 1-indexed
        let last_visible = scroll_offset + visible_height;

        assert_eq!(first_visible, 11);
        assert_eq!(last_visible, 30);
        assert_eq!(last_visible, total_lines);
    }

    #[test]
    fn test_real_scenario_with_lines() {
        use ratatui::text::{Line, Span};

        // Simulate building lines like in draw_history
        let mut lines: Vec<Line> = Vec::new();

        // Add some messages
        for i in 0..5 {
            lines.push(Line::from(vec![
                Span::raw("> "),
                Span::raw(format!("User message {}", i)),
            ]));
            lines.push(Line::from("")); // Empty line between messages

            lines.push(Line::from(vec![
                Span::raw("< "),
                Span::raw("This is a response from the AI assistant that might be quite long and wrap to multiple lines depending on terminal width"),
            ]));
            lines.push(Line::from("")); // Empty line between messages
        }

        let width = 80usize;
        let visible_height = 15usize;

        let wrap_options = textwrap::Options::new(width)
            .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
            .word_splitter(textwrap::WordSplitter::NoHyphenation);

        let total_lines: usize = lines.iter().map(|line| {
            let full_text: String = line.spans.iter()
                .map(|span| span.content.as_ref())
                .collect();

            if full_text.is_empty() {
                1
            } else {
                textwrap::wrap(&full_text, &wrap_options).len()
            }
        }).sum();

        let max_scroll = total_lines.saturating_sub(visible_height);

        println!("Total lines: {}", total_lines);
        println!("Visible height: {}", visible_height);
        println!("Max scroll: {}", max_scroll);

        // At max_scroll, should be able to see all content
        assert!(max_scroll + visible_height >= total_lines,
            "max_scroll ({}) + visible_height ({}) should >= total_lines ({})",
            max_scroll, visible_height, total_lines);
    }

    #[test]
    fn test_compare_textwrap_vs_simple_calculation() {
        // Compare textwrap result with simple width division
        let width = 80usize;
        let wrap_options = textwrap::Options::new(width)
            .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
            .word_splitter(textwrap::WordSplitter::NoHyphenation);

        let test_cases = vec![
            "Short line",
            "This is a medium length line that fits in 80 chars",
            "This is a very long line that definitely exceeds eighty characters and should wrap to at least two lines when rendered",
            "Word at boundary: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",  // 80 a's
            "Line with    multiple   spaces    that   might   affect   wrapping",
            "한글 텍스트: 이것은 한글로 된 긴 문장입니다. 유니코드 너비가 다르게 계산될 수 있습니다.",
        ];

        for text in test_cases {
            let textwrap_lines = textwrap::wrap(text, &wrap_options).len();

            // Simple calculation (what we used before)
            use unicode_width::UnicodeWidthStr;
            let text_width = UnicodeWidthStr::width(text);
            let simple_lines = if text_width == 0 {
                1
            } else {
                (text_width + width - 1) / width
            };

            let display_text: String = text.chars().take(40).collect();
            println!("Text: {:?}", display_text);
            println!("  Width: {}, textwrap: {}, simple: {}", text_width, textwrap_lines, simple_lines);

            // textwrap should give equal or MORE lines than simple (due to word boundaries)
            if textwrap_lines < simple_lines {
                println!("  WARNING: textwrap gives fewer lines!");
            }
        }
    }

    #[test]
    fn test_scroll_boundary_exact() {
        // Test exact boundary case
        // If we have exactly visible_height lines, max_scroll should be 0
        let visible_height = 10usize;
        let total_lines = 10usize;
        let max_scroll = total_lines.saturating_sub(visible_height);
        assert_eq!(max_scroll, 0);

        // If we have visible_height + 1 lines, max_scroll should be 1
        let total_lines = 11usize;
        let max_scroll = total_lines.saturating_sub(visible_height);
        assert_eq!(max_scroll, 1);

        // Verify that at max_scroll=1, we see lines 2-11 (skipping line 1)
        let scroll_offset = 1usize;
        let last_visible_line = scroll_offset + visible_height;  // 1 + 10 = 11
        assert_eq!(last_visible_line, total_lines);
    }

    #[test]
    fn test_scroll_to_last_line() {
        use ratatui::{
            backend::TestBackend,
            Terminal,
            widgets::{Paragraph, Wrap},
            text::Line,
            layout::Rect,
        };

        // Create content with known lines
        let lines: Vec<Line> = (1..=15).map(|i| Line::from(format!("Line {}", i))).collect();

        let width = 40u16;
        let height = 10u16;  // Can show 10 lines

        // Total 15 lines, visible 10 → max_scroll = 5
        // At scroll=5, should show lines 6-15 (last line is "Line 15")

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();

        // Render at max scroll
        let max_scroll = 15 - 10;  // 5
        terminal.draw(|frame| {
            let area = Rect::new(0, 0, width, height);
            let paragraph = Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false })
                .scroll((max_scroll as u16, 0));
            frame.render_widget(paragraph, area);
        }).unwrap();

        let buffer = terminal.backend().buffer();

        // Check last visible row contains "Line 15"
        let mut last_row_content = String::new();
        for x in 0..width {
            let cell = buffer.cell((x, height - 1)).unwrap();
            last_row_content.push_str(cell.symbol());
        }
        println!("Last row (row {}): |{}|", height - 1, last_row_content.trim_end());

        assert!(last_row_content.contains("Line 15"),
            "Last row should contain 'Line 15', got: '{}'", last_row_content.trim_end());

        // Print all rows for debugging
        println!("\nAll rows at max_scroll={}:", max_scroll);
        for y in 0..height {
            let mut row = String::new();
            for x in 0..width {
                let cell = buffer.cell((x, y)).unwrap();
                row.push_str(cell.symbol());
            }
            println!("  Row {}: |{}|", y, row.trim_end());
        }
    }

    #[test]
    fn test_draw_history_simulation() {
        use ratatui::{
            backend::TestBackend,
            Terminal,
            widgets::{Paragraph, Wrap, Block, Borders},
            text::Line,
            layout::Rect,
        };

        // Simulate draw_history structure
        let mut lines: Vec<Line> = Vec::new();

        // Simulate 5 messages with empty lines between (will need scroll)
        for i in 1..=5 {
            lines.push(Line::from(format!("> Message {}", i)));
            lines.push(Line::from(format!("< Response to message {}", i)));
            lines.push(Line::from(""));  // Empty line between messages
        }

        let width = 40u16;
        let area_height = 12u16;  // Total area including borders

        // Create block with borders (like draw_history)
        let block = Block::default().borders(Borders::ALL);
        let area = Rect::new(0, 0, width, area_height);
        let inner = block.inner(area);

        println!("Area: {}x{}", area.width, area.height);
        println!("Inner (after borders): {}x{}", inner.width, inner.height);

        let visible_height = inner.height as usize;

        // Calculate total lines with textwrap
        let wrap_options = textwrap::Options::new(inner.width as usize)
            .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
            .word_splitter(textwrap::WordSplitter::NoHyphenation);

        let total_lines: usize = lines.iter().map(|line| {
            let full_text: String = line.spans.iter()
                .map(|span| span.content.as_ref())
                .collect();
            if full_text.is_empty() { 1 } else { textwrap::wrap(&full_text, &wrap_options).len() }
        }).sum();

        let max_scroll = total_lines.saturating_sub(visible_height);

        println!("Lines in content: {}", lines.len());
        println!("Total wrapped lines (textwrap): {}", total_lines);
        println!("Visible height: {}", visible_height);
        println!("Max scroll: {}", max_scroll);

        // Render at max scroll
        let backend = TestBackend::new(width, area_height);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| {
            frame.render_widget(block.clone(), area);
            let paragraph = Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false })
                .scroll((max_scroll as u16, 0));
            frame.render_widget(paragraph, inner);
        }).unwrap();

        let buffer = terminal.backend().buffer();
        println!("\nRendered at max_scroll={}:", max_scroll);
        for y in 0..area_height {
            let mut row = String::new();
            for x in 0..width {
                let cell = buffer.cell((x, y)).unwrap();
                row.push_str(cell.symbol());
            }
            println!("  Row {}: |{}|", y, row.trim_end());
        }

        // The last content line should be visible
        // Last message is "> Message 3", "< Response to message 3", then empty line
        // At max_scroll, the last non-empty content should be visible
    }

    #[test]
    fn test_ratatui_actual_rendering() {
        use ratatui::{
            backend::TestBackend,
            Terminal,
            widgets::{Paragraph, Wrap},
            text::{Line, Span},
            layout::Rect,
        };

        // Create a test terminal with specific size
        let backend = TestBackend::new(40, 10);  // 40 chars wide, 10 rows
        let mut terminal = Terminal::new(backend).unwrap();

        // Create test content - lines that should wrap
        let lines: Vec<Line> = vec![
            Line::from("Line 1: Short"),
            Line::from("Line 2: This is a longer line that should wrap to multiple lines in 40 char width"),
            Line::from("Line 3: Another line"),
            Line::from("Line 4: Yet another longer line that will definitely wrap around"),
            Line::from("Line 5: End"),
        ];

        let width = 40u16;
        let height = 10u16;

        // Calculate using textwrap
        let wrap_options = textwrap::Options::new(width as usize)
            .word_separator(textwrap::WordSeparator::UnicodeBreakProperties)
            .word_splitter(textwrap::WordSplitter::NoHyphenation);

        let textwrap_total: usize = lines.iter().map(|line| {
            let full_text: String = line.spans.iter()
                .map(|span| span.content.as_ref())
                .collect();
            if full_text.is_empty() { 1 } else { textwrap::wrap(&full_text, &wrap_options).len() }
        }).sum();

        println!("Width: {}, Height: {}", width, height);
        println!("Textwrap calculated total lines: {}", textwrap_total);

        // Render and check
        terminal.draw(|frame| {
            let area = Rect::new(0, 0, width, height);
            let paragraph = Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false });
            frame.render_widget(paragraph, area);
        }).unwrap();

        // Print what was rendered
        let buffer = terminal.backend().buffer();
        println!("\nRendered content:");
        for y in 0..height {
            let mut line_content = String::new();
            for x in 0..width {
                let cell = buffer.cell((x, y)).unwrap();
                line_content.push_str(cell.symbol());
            }
            println!("  Row {}: |{}|", y, line_content.trim_end());
        }

        // Count non-empty rows
        let mut rendered_lines = 0;
        for y in 0..height {
            let mut has_content = false;
            for x in 0..width {
                let cell = buffer.cell((x, y)).unwrap();
                if cell.symbol() != " " {
                    has_content = true;
                    break;
                }
            }
            if has_content {
                rendered_lines += 1;
            }
        }
        println!("\nRendered non-empty lines: {}", rendered_lines);
        println!("Textwrap calculated: {}", textwrap_total);

        // They should match or textwrap should be close
        let diff = (rendered_lines as i32 - textwrap_total as i32).abs();
        println!("Difference: {}", diff);
    }

    #[test]
    fn test_estimate_wrapped_lines_vs_ratatui() {
        use ratatui::{
            backend::TestBackend,
            Terminal,
            widgets::{Paragraph, Wrap},
            text::{Line, Span},
            style::Style,
            layout::Rect,
        };

        let width = 60u16;
        let height = 50u16;

        // Test cases with different line lengths and content
        let test_cases: Vec<Line> = vec![
            // Short line - should be 1 line
            Line::from(vec![
                Span::raw("> "),
                Span::raw("Hello world"),
            ]),
            // Line that wraps once
            Line::from(vec![
                Span::raw("< "),
                Span::raw("This is a longer response that should wrap to multiple lines when the terminal width is only 60 characters"),
            ]),
            // Empty line
            Line::from(""),
            // Line with Korean text (wide characters)
            Line::from(vec![
                Span::raw("> "),
                Span::raw("안녕하세요 이것은 한글 테스트입니다. 한글은 2칸을 차지합니다."),
            ]),
            // Line with mixed content
            Line::from(vec![
                Span::raw("< "),
                Span::styled("Bold text", Style::default()),
                Span::raw(" and "),
                Span::styled("italic text", Style::default()),
                Span::raw(" mixed together in one line that might wrap"),
            ]),
            // Very long line without spaces (should force wrap)
            Line::from(vec![
                Span::raw("* "),
                Span::raw("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ]),
        ];

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();

        // Render and count actual lines
        terminal.draw(|frame| {
            let area = Rect::new(0, 0, width, height);
            let paragraph = Paragraph::new(test_cases.clone())
                .wrap(Wrap { trim: false });
            frame.render_widget(paragraph, area);
        }).unwrap();

        let buffer = terminal.backend().buffer();

        // Count rendered lines by finding the last non-empty row
        // (since we're rendering into a larger buffer, we need to find where content ends)
        let mut last_content_row = 0;
        for y in 0..height {
            let mut has_content = false;
            for x in 0..width {
                let cell = buffer.cell((x, y)).unwrap();
                if cell.symbol() != " " {
                    has_content = true;
                    break;
                }
            }
            if has_content {
                last_content_row = y;
            }
        }
        // Total lines = last content row + 1 (0-indexed) + any trailing empty lines that are part of content
        // For accurate counting, we should count total lines including empty ones
        let rendered_lines = (last_content_row + 1) as usize;

        // Calculate using estimate_wrapped_lines
        let estimated_total: usize = test_cases.iter()
            .map(|line| super::estimate_wrapped_lines(line, width as usize))
            .sum();

        println!("\n=== estimate_wrapped_lines vs ratatui ===");
        println!("Width: {}", width);
        println!("Estimated total: {}", estimated_total);
        println!("Rendered lines: {}", rendered_lines);

        // Print rendered content
        println!("\nRendered content:");
        for y in 0..height {
            let mut row = String::new();
            for x in 0..width {
                let cell = buffer.cell((x, y)).unwrap();
                row.push_str(cell.symbol());
            }
            let trimmed = row.trim_end();
            if !trimmed.is_empty() {
                println!("  Row {}: |{}|", y, trimmed);
            }
        }

        // Print per-line breakdown
        println!("\nPer-line breakdown:");
        for (i, line) in test_cases.iter().enumerate() {
            let text: String = line.spans.iter()
                .map(|span| span.content.as_ref())
                .collect();
            let estimated = super::estimate_wrapped_lines(line, width as usize);
            println!("  Line {}: estimated={}, text={:?}", i, estimated,
                if text.len() > 50 { format!("{}...", &text[..50]) } else { text });
        }

        // Should match
        assert_eq!(estimated_total, rendered_lines,
            "Estimated ({}) should match rendered ({})", estimated_total, rendered_lines);
    }

    #[test]
    fn test_multiple_lines_with_empty() {
        use ratatui::{
            backend::TestBackend,
            Terminal,
            widgets::{Paragraph, Wrap},
            text::{Line, Span},
            style::Style,
            layout::Rect,
        };

        let width = 40u16;
        let height = 20u16;

        // Test: multiple lines including empty-looking lines
        let lines: Vec<Line> = vec![
            Line::from("Line 1"),
            Line::from(vec![Span::styled("  ", Style::default())]), // spaces only
            Line::from("Line 3"),
            Line::from(vec![Span::styled("  ", Style::default())]), // spaces only
            Line::from("Line 5"),
        ];

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| {
            let area = Rect::new(0, 0, width, height);
            let paragraph = Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false });
            frame.render_widget(paragraph, area);
        }).unwrap();

        let buffer = terminal.backend().buffer();

        println!("\n=== Multiple lines with empty test ===");
        println!("Number of Line objects: {}", lines.len());

        for y in 0..10.min(height) {
            let mut row = String::new();
            for x in 0..width {
                let cell = buffer.cell((x, y)).unwrap();
                row.push_str(cell.symbol());
            }
            println!("Row {}: |{}|", y, row.trim_end());
        }

        // With whitespace-only lines taking 2 rows:
        // Line 0: Row 0 (1 row)
        // Line 1 (spaces): Row 1-2 (2 rows)
        // Line 2: Row 3 (1 row)
        // Line 3 (spaces): Row 4-5 (2 rows)
        // Line 4: Row 6 (1 row)
        // Total: 7 rows
        let mut row6 = String::new();
        for x in 0..width {
            let cell = buffer.cell((x, 6)).unwrap();
            row6.push_str(cell.symbol());
        }
        assert!(row6.contains("Line 5"),
            "Line 5 should be at Row 6 (whitespace-only lines take 2 rows). Got: '{}'", row6.trim());

        // Verify estimate_wrapped_lines matches
        let estimated_total: usize = lines.iter()
            .map(|line| super::estimate_wrapped_lines(line, width as usize))
            .sum();
        println!("Estimated total lines: {}", estimated_total);
        assert_eq!(estimated_total, 7, "Estimated should be 7 (3 normal + 2*2 whitespace)");
    }

    #[test]
    fn test_individual_line_wrap() {
        use ratatui::{
            backend::TestBackend,
            Terminal,
            widgets::{Paragraph, Wrap},
            text::{Line, Span},
            style::Style,
            layout::Rect,
        };

        let width = 80u16;
        let height = 10u16;

        // Test individual lines
        let test_lines: Vec<(&str, Line)> = vec![
            ("empty string", Line::from("")),
            ("two spaces", Line::from("  ")),
            ("prefix only", Line::from(vec![Span::styled("  ", Style::default())])),
            ("prefix + empty span", Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::raw(""),
            ])),
            ("normal short", Line::from("Hello world")),
            ("long line", Line::from("This is a very long line that should definitely wrap when the terminal width is only 80 characters wide")),
        ];

        for (name, line) in test_lines {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();

            terminal.draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                let paragraph = Paragraph::new(vec![line.clone()])
                    .wrap(Wrap { trim: false });
                frame.render_widget(paragraph, area);
            }).unwrap();

            let buffer = terminal.backend().buffer();

            // Count rows used
            let mut rows_used = 0;
            for y in 0..height {
                let mut has_any_char = false;
                for x in 0..width {
                    let cell = buffer.cell((x, y)).unwrap();
                    // Check if cell has any content (even space can be "content" if it's the first row)
                    if y == 0 || cell.symbol() != " " {
                        has_any_char = true;
                    }
                }
                // For the first line, always count it
                // For subsequent lines, only count if there's actual visible content
                if y == 0 {
                    rows_used = 1;
                } else {
                    let mut has_visible = false;
                    for x in 0..width {
                        let cell = buffer.cell((x, y)).unwrap();
                        if cell.symbol() != " " {
                            has_visible = true;
                            break;
                        }
                    }
                    if has_visible {
                        rows_used = y as usize + 1;
                    }
                }
            }

            let estimated = super::estimate_wrapped_lines(&line, width as usize);

            let text: String = line.spans.iter()
                .map(|span| span.content.as_ref())
                .collect();

            println!("{}: estimated={}, rows_used={}, text={:?}",
                name, estimated, rows_used, text);

            // Print actual buffer content for debugging
            for y in 0..3.min(height) {
                let mut row = String::new();
                for x in 0..width {
                    let cell = buffer.cell((x, y)).unwrap();
                    row.push_str(cell.symbol());
                }
                println!("  Row {}: |{}|", y, row.trim_end());
            }
        }
    }

    #[test]
    fn test_markdown_rendering_line_count() {
        use ratatui::{
            backend::TestBackend,
            Terminal,
            widgets::{Paragraph, Wrap},
            text::{Line, Span},
            style::{Style, Modifier, Color},
            layout::Rect,
        };
        use crate::utils::markdown::{render_markdown, MarkdownTheme};

        let width = 80u16;
        let height = 100u16;

        // Sample markdown content (similar to AI response)
        let markdown_text = r#"Here's a quick explanation:

1. **First point**: This is an explanation that might be longer and wrap to the next line
2. **Second point**: Another explanation

```rust
fn main() {
    println!("Hello");
}
```

- Item one
- Item two with more text that could potentially wrap

> This is a blockquote that tests the blockquote rendering"#;

        let theme = MarkdownTheme::default();
        let md_lines = render_markdown(markdown_text, theme);

        // Add prefix like draw_history does
        let prefix_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let mut lines_with_prefix: Vec<Line> = Vec::new();
        for (i, md_line) in md_lines.into_iter().enumerate() {
            let prefix = if i == 0 { "< " } else { "  " };
            let mut spans = vec![Span::styled(prefix, prefix_style)];
            spans.extend(md_line.spans);
            lines_with_prefix.push(Line::from(spans));
        }
        lines_with_prefix.push(Line::from("")); // Empty line after message

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();

        // Render
        terminal.draw(|frame| {
            let area = Rect::new(0, 0, width, height);
            let paragraph = Paragraph::new(lines_with_prefix.clone())
                .wrap(Wrap { trim: false });
            frame.render_widget(paragraph, area);
        }).unwrap();

        let buffer = terminal.backend().buffer();

        // Count rendered lines
        let mut rendered_lines = 0;
        for y in 0..height {
            let mut has_content = false;
            for x in 0..width {
                let cell = buffer.cell((x, y)).unwrap();
                if cell.symbol() != " " {
                    has_content = true;
                    break;
                }
            }
            if has_content {
                rendered_lines += 1;
            }
        }

        // Calculate using estimate_wrapped_lines
        let estimated_total: usize = lines_with_prefix.iter()
            .map(|line| super::estimate_wrapped_lines(line, width as usize))
            .sum();

        println!("\n=== Markdown rendering test ===");
        println!("Width: {}", width);
        println!("Raw lines (before wrap): {}", lines_with_prefix.len());
        println!("Estimated wrapped lines: {}", estimated_total);
        println!("Actual rendered lines: {}", rendered_lines);

        // Print rendered content
        println!("\nRendered markdown:");
        for y in 0..height {
            let mut row = String::new();
            for x in 0..width {
                let cell = buffer.cell((x, y)).unwrap();
                row.push_str(cell.symbol());
            }
            let trimmed = row.trim_end();
            if !trimmed.is_empty() {
                println!("  Row {:2}: |{}|", y, trimmed);
            }
        }

        // Per-line breakdown
        println!("\nPer-line breakdown:");
        for (i, line) in lines_with_prefix.iter().enumerate() {
            let text: String = line.spans.iter()
                .map(|span| span.content.as_ref())
                .collect();
            let estimated = super::estimate_wrapped_lines(line, width as usize);
            let display = if text.len() > 60 { format!("{}...", &text[..60]) } else { text };
            println!("  Line {:2}: est={}, text={:?}", i, estimated, display);
        }

        // Note: The rendered_lines count may differ due to how we count rows
        // The important thing is that estimate_wrapped_lines is accurate per-line
        // which was verified in test_individual_line_wrap
        println!("\nNote: Individual line estimates are verified in test_individual_line_wrap");
        println!("Total estimated: {}, last content row: {}", estimated_total, rendered_lines);
    }

    #[test]
    fn test_scroll_reaches_bottom_with_markdown() {
        use ratatui::{
            backend::TestBackend,
            Terminal,
            widgets::{Paragraph, Wrap},
            text::{Line, Span},
            style::{Style, Modifier, Color},
            layout::Rect,
        };
        use crate::utils::markdown::{render_markdown, MarkdownTheme};

        let width = 60u16;
        let height = 10u16;  // Small visible area to force scrolling

        // Sample markdown with known last line
        let markdown_text = "Line 1\n\nLine 2\n\nLine 3\n\n**Last line marker**";

        let theme = MarkdownTheme::default();
        let md_lines = render_markdown(markdown_text, theme);

        // Add prefix like draw_history does
        let prefix_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let mut lines_with_prefix: Vec<Line> = Vec::new();
        for (i, md_line) in md_lines.into_iter().enumerate() {
            let prefix = if i == 0 { "< " } else { "  " };
            let mut spans = vec![Span::styled(prefix, prefix_style)];
            spans.extend(md_line.spans);
            lines_with_prefix.push(Line::from(spans));
        }
        lines_with_prefix.push(Line::from("")); // Empty line after message

        // Calculate using estimate_wrapped_lines
        let total_lines: usize = lines_with_prefix.iter()
            .map(|line| super::estimate_wrapped_lines(line, width as usize))
            .sum();

        let visible_height = height as usize;
        let max_scroll = total_lines.saturating_sub(visible_height);

        println!("\n=== Scroll to bottom test ===");
        println!("Total lines (estimated): {}", total_lines);
        println!("Visible height: {}", visible_height);
        println!("Max scroll: {}", max_scroll);

        // Render at max_scroll
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| {
            let area = Rect::new(0, 0, width, height);
            let paragraph = Paragraph::new(lines_with_prefix.clone())
                .wrap(Wrap { trim: false })
                .scroll((max_scroll as u16, 0));
            frame.render_widget(paragraph, area);
        }).unwrap();

        let buffer = terminal.backend().buffer();

        // Print rendered content
        println!("\nRendered at max_scroll={}:", max_scroll);
        for y in 0..height {
            let mut row = String::new();
            for x in 0..width {
                let cell = buffer.cell((x, y)).unwrap();
                row.push_str(cell.symbol());
            }
            println!("  Row {}: |{}|", y, row.trim_end());
        }

        // Check if "Last line marker" is visible
        let mut found_marker = false;
        for y in 0..height {
            let mut row = String::new();
            for x in 0..width {
                let cell = buffer.cell((x, y)).unwrap();
                row.push_str(cell.symbol());
            }
            if row.contains("Last line marker") {
                found_marker = true;
                println!("\nFound 'Last line marker' at row {}", y);
                break;
            }
        }

        assert!(found_marker, "Last line marker should be visible at max_scroll");
    }

    #[test]
    fn test_scroll_with_ai_response_simulation() {
        use ratatui::{
            backend::TestBackend,
            Terminal,
            widgets::{Paragraph, Wrap, Block, Borders},
            text::{Line, Span},
            style::{Style, Modifier, Color},
            layout::Rect,
        };
        use crate::utils::markdown::{render_markdown, MarkdownTheme};

        // Simulate actual AI screen layout - SMALL height to force scrolling
        let total_width = 80u16;
        let total_height = 12u16;  // Small to force scrolling

        // User message
        let user_content = "Hello, can you help me?";

        // AI response with multiple lines
        let ai_response = r#"Sure! Here's what I can help you with:

1. **File operations** - Create, copy, move, delete files
2. **Navigation** - Browse directories
3. **Search** - Find files and content

Let me know what you'd like to do!

> This is a tip: Use arrow keys to navigate.

**END_MARKER**"#;

        let theme = MarkdownTheme::default();

        // Build lines like draw_history does
        let mut lines: Vec<Line> = Vec::new();

        // User message
        let user_prefix = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
        for (i, line_text) in user_content.lines().enumerate() {
            let prefix = if i == 0 { "> " } else { "  " };
            lines.push(Line::from(vec![
                Span::styled(prefix, user_prefix),
                Span::raw(line_text.to_string()),
            ]));
        }
        // Use Span::raw("") to ensure spans is not empty
        lines.push(Line::from(Span::raw(""))); // Empty line between messages

        // AI response with markdown
        let md_lines = render_markdown(ai_response, theme);
        let ai_prefix = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        for (i, md_line) in md_lines.into_iter().enumerate() {
            let prefix = if i == 0 { "< " } else { "  " };
            let mut spans = vec![Span::styled(prefix, ai_prefix)];
            spans.extend(md_line.spans);
            lines.push(Line::from(spans));
        }
        // Use Span::raw("") to ensure spans is not empty
        lines.push(Line::from(Span::raw(""))); // Empty line after message

        // Create block with borders (like draw_history)
        let block = Block::default().borders(Borders::ALL);
        let area = Rect::new(0, 0, total_width, total_height);
        let inner = block.inner(area);

        let visible_height = inner.height as usize;
        let width = inner.width as usize;

        // Calculate total lines
        let total_lines: usize = lines.iter()
            .map(|line| super::estimate_wrapped_lines(line, width))
            .sum();

        let max_scroll = total_lines.saturating_sub(visible_height);

        println!("\n=== AI Response Scroll Test ===");
        println!("Inner area: {}x{}", inner.width, inner.height);
        println!("Raw lines: {}", lines.len());
        println!("Total wrapped lines: {}", total_lines);
        println!("Visible height: {}", visible_height);
        println!("Max scroll: {}", max_scroll);

        // Debug: print each line's content
        println!("\nLines content:");
        for (i, line) in lines.iter().enumerate() {
            let text: String = line.spans.iter()
                .map(|span| span.content.as_ref())
                .collect();
            let est = super::estimate_wrapped_lines(line, width);
            println!("  Line {:2}: est={}, spans={}, text={:?}",
                i, est, line.spans.len(),
                if text.len() > 50 { format!("{}...", &text[..50]) } else { text });
        }

        // Render at max_scroll
        let backend = TestBackend::new(total_width, total_height);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| {
            frame.render_widget(block.clone(), area);
            let paragraph = Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false })
                .scroll((max_scroll as u16, 0));
            frame.render_widget(paragraph, inner);
        }).unwrap();

        let buffer = terminal.backend().buffer();

        // Print rendered content
        println!("\nRendered at max_scroll={}:", max_scroll);
        for y in 0..total_height {
            let mut row = String::new();
            for x in 0..total_width {
                let cell = buffer.cell((x, y)).unwrap();
                row.push_str(cell.symbol());
            }
            let trimmed = row.trim_end();
            if !trimmed.is_empty() {
                println!("  Row {:2}: |{}|", y, trimmed);
            }
        }

        // Check if "END_MARKER" is visible
        let mut found_marker = false;
        for y in 0..total_height {
            let mut row = String::new();
            for x in 0..total_width {
                let cell = buffer.cell((x, y)).unwrap();
                row.push_str(cell.symbol());
            }
            if row.contains("END_MARKER") {
                found_marker = true;
                println!("\nFound 'END_MARKER' at row {}", y);
                break;
            }
        }

        assert!(found_marker, "END_MARKER should be visible at max_scroll - this means scroll reaches the bottom correctly");
    }
}
