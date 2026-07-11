use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};
use regex::Regex;
use std::collections::HashSet;
use std::path::PathBuf;
use unicode_width::UnicodeWidthChar;

use super::{
    app::{App, Screen},
    syntax::{Language, SyntaxHighlighter},
    theme::Theme,
};

/// 뷰어 모드
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerMode {
    Text,
    Hex,
}

/// 검색 옵션
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub use_regex: bool,
    pub whole_word: bool,
}

/// 뷰어 상태
#[derive(Debug)]
pub struct ViewerState {
    pub file_path: PathBuf,
    pub lines: Vec<String>,
    pub raw_bytes: Vec<u8>,
    pub scroll: usize,
    pub horizontal_scroll: usize,
    pub mode: ViewerMode,
    pub word_wrap: bool,

    // 검색
    pub search_mode: bool,
    pub search_input: String,
    pub search_cursor_pos: usize,
    pub search_term: String,
    pub search_options: SearchOptions,
    pub match_lines: Vec<usize>,
    pub match_positions: Vec<(usize, usize, usize)>, // (line, start, end)
    pub current_match: usize,

    // 검색 Regex 캐싱 (성능 최적화)
    cached_regex: Option<Regex>,
    cached_pattern: String,
    cached_case_sensitive: bool,

    // 북마크
    pub bookmarks: HashSet<usize>,

    // Goto line
    pub goto_mode: bool,
    pub goto_input: String,

    // 문법 강조
    pub language: Language,
    pub highlighter: Option<SyntaxHighlighter>,
    pub syntax_colors: crate::ui::theme::SyntaxColors,

    // 인코딩
    pub encoding: String,
    pub is_binary: bool,

    // 파일 정보
    pub file_size: u64,
    pub total_lines: usize,

    // 화면 크기 (렌더링 시 업데이트)
    pub visible_height: usize,

    // Word-wrap navigation uses visual rows, not logical source lines.
    wrapped_line_origins: Vec<usize>,
    wrap_width: usize,
    pending_wrap_source_line: Option<usize>,
}

impl ViewerState {
    pub fn new() -> Self {
        Self {
            file_path: PathBuf::new(),
            lines: Vec::new(),
            raw_bytes: Vec::new(),
            scroll: 0,
            horizontal_scroll: 0,
            mode: ViewerMode::Text,
            word_wrap: false,
            search_mode: false,
            search_input: String::new(),
            search_cursor_pos: 0,
            search_term: String::new(),
            search_options: SearchOptions::default(),
            match_lines: Vec::new(),
            match_positions: Vec::new(),
            current_match: 0,
            cached_regex: None,
            cached_pattern: String::new(),
            cached_case_sensitive: false,
            bookmarks: HashSet::new(),
            goto_mode: false,
            goto_input: String::new(),
            language: Language::Plain,
            highlighter: None,
            syntax_colors: crate::ui::theme::Theme::default().syntax,
            encoding: "UTF-8".to_string(),
            is_binary: false,
            file_size: 0,
            total_lines: 0,
            visible_height: 20, // 기본값, 렌더링 시 업데이트됨
            wrapped_line_origins: Vec::new(),
            wrap_width: 0,
            pending_wrap_source_line: None,
        }
    }

    /// 테마의 syntax colors 설정
    pub fn set_syntax_colors(&mut self, colors: crate::ui::theme::SyntaxColors) {
        self.syntax_colors = colors;
        // 하이라이터가 있으면 새 색상으로 재생성
        if self.highlighter.is_some() {
            self.highlighter = Some(SyntaxHighlighter::new(self.language, self.syntax_colors));
        }
    }

    /// Maximum file size for viewing (100MB)
    const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

    /// 파일 로드
    pub fn load_file(&mut self, path: &PathBuf) -> Result<(), String> {
        self.file_path = path.clone();
        self.scroll = 0;
        self.horizontal_scroll = 0;
        self.bookmarks.clear();
        self.search_term.clear();
        self.match_lines.clear();
        self.match_positions.clear();
        self.wrapped_line_origins.clear();
        self.wrap_width = 0;
        self.pending_wrap_source_line = None;

        // Check file size before loading to prevent memory exhaustion
        let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
        if metadata.len() > Self::MAX_FILE_SIZE {
            return Err(format!(
                "File too large ({:.1} MB). Maximum size is {} MB.",
                metadata.len() as f64 / 1024.0 / 1024.0,
                Self::MAX_FILE_SIZE / 1024 / 1024
            ));
        }

        // 파일 읽기
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        self.file_size = bytes.len() as u64;

        // 바이너리 파일 감지
        self.is_binary = self.detect_binary(&bytes);

        if self.is_binary {
            self.mode = ViewerMode::Hex;
            self.lines = self.format_hex_view(&bytes);
            self.encoding = "Binary".to_string();
            self.raw_bytes = bytes;
        } else {
            self.mode = ViewerMode::Text;
            // UTF-8로 시도
            match String::from_utf8(bytes) {
                Ok(content) => {
                    self.encoding = "UTF-8".to_string();
                    self.raw_bytes = content.as_bytes().to_vec();
                    self.lines = content.lines().map(String::from).collect();
                }
                Err(e) => {
                    // Latin-1 (ISO-8859-1)로 시도
                    self.encoding = "ISO-8859-1".to_string();
                    let bytes = e.into_bytes();
                    let content: String = bytes.iter().map(|&b| b as char).collect();
                    self.raw_bytes = bytes;
                    self.lines = content.lines().map(String::from).collect();
                }
            }
        }

        self.total_lines = self.lines.len();

        // 언어 감지 및 하이라이터 초기화
        self.language = Language::from_extension(path);
        if !self.is_binary {
            self.highlighter = Some(SyntaxHighlighter::new(self.language, self.syntax_colors));
        }

        Ok(())
    }

    /// 바이너리 파일 감지
    fn detect_binary(&self, bytes: &[u8]) -> bool {
        // 처음 8KB를 검사
        let check_size = bytes.len().min(8192);
        let null_count = bytes[..check_size].iter().filter(|&&b| b == 0).count();
        let non_text = bytes[..check_size]
            .iter()
            .filter(|&&b| b < 0x09 || (b > 0x0d && b < 0x20 && b != 0x1b))
            .count();

        // NULL 문자가 많거나 비텍스트 문자가 30% 이상이면 바이너리
        null_count > 0 || non_text as f32 / check_size as f32 > 0.3
    }

    /// 헥스 뷰 포맷
    fn format_hex_view(&self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        let bytes_per_line = 16;

        for (i, chunk) in bytes.chunks(bytes_per_line).enumerate() {
            let offset = i * bytes_per_line;
            let hex: Vec<String> = chunk.iter().map(|b| format!("{:02X}", b)).collect();
            let hex_str = if hex.len() <= 8 {
                format!("{:<24}", hex.join(" "))
            } else {
                format!("{} {}", hex[..8].join(" "), hex[8..].join(" "))
            };

            let ascii: String = chunk
                .iter()
                .map(|&b| {
                    if b.is_ascii_graphic() || b == b' ' {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect();

            lines.push(format!("{:08X}  {:<48}  |{}|", offset, hex_str, ascii));
        }

        lines
    }

    /// 검색 수행 (with regex caching for performance)
    pub fn perform_search(&mut self) {
        self.match_lines.clear();
        self.match_positions.clear();

        if self.search_term.is_empty() {
            self.cached_regex = None;
            self.cached_pattern.clear();
            return;
        }

        // Build the full pattern with options
        let base_pattern = if self.search_options.use_regex {
            self.search_term.clone()
        } else {
            regex::escape(&self.search_term)
        };

        let full_pattern = if self.search_options.whole_word {
            format!(r"\b{}\b", base_pattern)
        } else {
            base_pattern
        };

        // Check if we need to recompile the regex (caching optimization)
        let needs_recompile = self.cached_regex.is_none()
            || self.cached_pattern != full_pattern
            || self.cached_case_sensitive != self.search_options.case_sensitive;

        if needs_recompile {
            let regex_pattern = if self.search_options.case_sensitive {
                full_pattern.clone()
            } else {
                format!("(?i){}", full_pattern)
            };

            match Regex::new(&regex_pattern) {
                Ok(re) => {
                    self.cached_regex = Some(re);
                    self.cached_pattern = full_pattern;
                    self.cached_case_sensitive = self.search_options.case_sensitive;
                }
                Err(_) => {
                    self.cached_regex = None;
                    self.cached_pattern.clear();
                    return;
                }
            }
        }

        // Use cached regex for search
        if let Some(ref re) = self.cached_regex {
            for (line_idx, line) in self.lines.iter().enumerate() {
                let mut has_match = false;
                for mat in re.find_iter(line) {
                    // 바이트 인덱스를 문자 인덱스로 변환
                    let byte_start = mat.start();
                    let byte_end = mat.end();
                    let char_start = line[..byte_start].chars().count();
                    let char_end = char_start + line[byte_start..byte_end].chars().count();
                    self.match_positions.push((line_idx, char_start, char_end));
                    has_match = true;
                }
                if has_match {
                    self.match_lines.push(line_idx);
                }
            }
        }

        self.current_match = 0;
        self.scroll_to_current_match();
    }

    /// 현재 매치로 스크롤 (match_positions 기준)
    pub fn scroll_to_current_match(&mut self) {
        if !self.match_positions.is_empty() && self.current_match < self.match_positions.len() {
            let (line, _, _) = self.match_positions[self.current_match];
            self.scroll_to_source_line(line, 5);
        }
    }

    /// 다음 매치
    pub fn next_match(&mut self) {
        if !self.match_positions.is_empty() {
            self.current_match = (self.current_match + 1) % self.match_positions.len();
            self.scroll_to_current_match();
        }
    }

    /// 이전 매치
    pub fn prev_match(&mut self) {
        if !self.match_positions.is_empty() {
            self.current_match = if self.current_match == 0 {
                self.match_positions.len() - 1
            } else {
                self.current_match - 1
            };
            self.scroll_to_current_match();
        }
    }

    /// 북마크 토글
    pub fn toggle_bookmark(&mut self, line: usize) {
        if self.bookmarks.contains(&line) {
            self.bookmarks.remove(&line);
        } else {
            self.bookmarks.insert(line);
        }
    }

    /// 다음 북마크로 이동
    pub fn goto_next_bookmark(&mut self) {
        if self.bookmarks.is_empty() {
            return;
        }

        // 현재 화면에 보이는 첫 번째 줄 기준
        let current_line = self.source_line_for_visual_row(self.scroll.saturating_add(5));
        let mut sorted: Vec<_> = self.bookmarks.iter().copied().collect();
        sorted.sort();

        for &bm in &sorted {
            if bm > current_line {
                self.scroll_to_source_line(bm, 5);
                return;
            }
        }
        // 처음 북마크로 순환
        self.scroll_to_source_line(sorted[0], 5);
    }

    /// 이전 북마크로 이동
    pub fn goto_prev_bookmark(&mut self) {
        if self.bookmarks.is_empty() {
            return;
        }

        // 현재 화면에 보이는 첫 번째 줄 기준
        let current_line = self.source_line_for_visual_row(self.scroll.saturating_add(5));
        let mut sorted: Vec<_> = self.bookmarks.iter().copied().collect();
        sorted.sort();
        sorted.reverse();

        for &bm in &sorted {
            if bm < current_line {
                self.scroll_to_source_line(bm, 5);
                return;
            }
        }
        // 마지막 북마크로 순환
        self.scroll_to_source_line(sorted[0], 5);
    }

    /// 줄 번호로 이동
    pub fn goto_line(&mut self, line_str: &str) {
        if let Ok(line_num) = line_str.parse::<usize>() {
            if line_num > 0 && line_num <= self.lines.len() {
                self.scroll_to_source_line(line_num - 1, 5);
            }
        }
    }

    /// 모드 토글 (텍스트/헥스)
    pub fn toggle_mode(&mut self) {
        if self.is_binary {
            return; // 바이너리 파일은 항상 헥스 모드
        }

        match self.mode {
            ViewerMode::Text => {
                self.mode = ViewerMode::Hex;
                self.lines = self.format_hex_view(&self.raw_bytes);
            }
            ViewerMode::Hex => {
                self.mode = ViewerMode::Text;
                if let Ok(content) = String::from_utf8(self.raw_bytes.clone()) {
                    self.lines = content.lines().map(String::from).collect();
                }
            }
        }
        self.scroll = 0;
    }

    fn build_wrapped_lines(&self, content_width: usize) -> Vec<(usize, String, bool)> {
        let mut wrapped_lines = Vec::new();
        for (orig_idx, original_line) in self.lines.iter().enumerate() {
            let line = original_line.replace('\t', "    ");
            if line.is_empty() {
                wrapped_lines.push((orig_idx, String::new(), true));
            } else if content_width > 0 {
                for (segment, wrapped) in textwrap::wrap(&line, content_width).iter().enumerate() {
                    wrapped_lines.push((orig_idx, wrapped.to_string(), segment == 0));
                }
            } else {
                wrapped_lines.push((orig_idx, line, true));
            }
        }
        wrapped_lines
    }

    fn effective_line_count(&self) -> usize {
        if self.word_wrap && !self.wrapped_line_origins.is_empty() {
            self.wrapped_line_origins.len()
        } else {
            self.lines.len()
        }
    }

    fn source_line_for_visual_row(&self, row: usize) -> usize {
        if self.word_wrap && !self.wrapped_line_origins.is_empty() {
            self.wrapped_line_origins
                .get(row.min(self.wrapped_line_origins.len() - 1))
                .copied()
                .unwrap_or(0)
        } else {
            row.min(self.lines.len().saturating_sub(1))
        }
    }

    fn scroll_to_source_line(&mut self, source_line: usize, leading_rows: usize) {
        let row = if self.word_wrap && !self.wrapped_line_origins.is_empty() {
            self.wrapped_line_origins
                .iter()
                .position(|line| *line == source_line)
                .unwrap_or(source_line)
        } else {
            source_line
        };
        self.scroll = row.saturating_sub(leading_rows);
    }

    fn toggle_word_wrap(&mut self) {
        if self.word_wrap {
            let source_line = self.source_line_for_visual_row(self.scroll);
            self.word_wrap = false;
            self.wrapped_line_origins.clear();
            self.wrap_width = 0;
            self.pending_wrap_source_line = None;
            self.scroll = source_line;
        } else {
            self.pending_wrap_source_line = Some(self.scroll);
            self.word_wrap = true;
            self.horizontal_scroll = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_from_viewer_clamps_cursor_after_reload() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("note.txt");
        std::fs::write(&path, "one\ntwo\nthree\nfour\nfive").unwrap();

        let mut viewer = ViewerState::new();
        viewer.load_file(&path).unwrap();
        viewer.scroll = 4;

        std::fs::write(&path, "one").unwrap();

        let mut app = App::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        app.viewer_state = Some(viewer);
        app.current_screen = Screen::FileViewer;

        handle_input(&mut app, KeyCode::Char('e'), KeyModifiers::NONE);

        assert_eq!(app.current_screen, Screen::FileEditor);
        let editor = app.editor_state.as_ref().unwrap();
        assert_eq!(editor.cursor_line, 0);
        assert_eq!(editor.scroll, 0);
    }

    #[test]
    fn wrapped_viewer_scrolls_visual_rows_of_a_long_logical_line() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut viewer = ViewerState::new();
        viewer.lines = vec!["abcdefghijklmnopqrstuvwxyz".to_string()];
        viewer.word_wrap = true;
        viewer.visible_height = 2;
        viewer.wrapped_line_origins = viewer
            .build_wrapped_lines(5)
            .iter()
            .map(|row| row.0)
            .collect();
        assert!(viewer.effective_line_count() > viewer.lines.len());

        let mut app = App::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        app.viewer_state = Some(viewer);
        handle_input(&mut app, KeyCode::Down, KeyModifiers::NONE);

        assert_eq!(app.viewer_state.as_ref().unwrap().scroll, 1);
    }

    #[test]
    fn bookmark_in_wrapped_view_targets_the_source_line() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut viewer = ViewerState::new();
        viewer.lines = vec!["abcdefghijklmnopqrstuvwxyz".to_string()];
        viewer.word_wrap = true;
        viewer.visible_height = 2;
        viewer.wrapped_line_origins = viewer
            .build_wrapped_lines(5)
            .iter()
            .map(|row| row.0)
            .collect();
        viewer.scroll = 3;

        let mut app = App::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        app.viewer_state = Some(viewer);
        handle_input(&mut app, KeyCode::Char('b'), KeyModifiers::NONE);

        let viewer = app.viewer_state.as_ref().unwrap();
        assert!(viewer.bookmarks.contains(&0));
        assert!(!viewer.bookmarks.contains(&3));
    }
}

pub fn draw(
    frame: &mut Frame,
    state: &mut ViewerState,
    area: Rect,
    theme: &Theme,
    kb: &crate::keybindings::Keybindings,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.viewer.border));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 3 {
        return;
    }

    // 화면 크기 업데이트 (스크롤 계산에 사용)
    let visible_lines = (inner.height - 2) as usize;
    state.visible_height = visible_lines;

    // Content dimensions are also needed to build the visual-row map used by
    // word-wrap navigation.
    let content_height = visible_lines;
    let content_width = inner.width.saturating_sub(5) as usize;
    let previous_source_line = state.source_line_for_visual_row(state.scroll);
    let restore_source_line = state.pending_wrap_source_line.take().or_else(|| {
        (state.word_wrap && state.wrap_width != content_width).then_some(previous_source_line)
    });
    let wrapped_lines = if state.word_wrap {
        state.build_wrapped_lines(content_width)
    } else {
        Vec::new()
    };
    if state.word_wrap {
        state.wrapped_line_origins = wrapped_lines.iter().map(|row| row.0).collect();
        state.wrap_width = content_width;
        if let Some(source_line) = restore_source_line {
            state.scroll_to_source_line(source_line, 0);
        }
    } else {
        state.wrapped_line_origins.clear();
        state.wrap_width = 0;
    }

    // Header
    let total_lines = state.effective_line_count();
    state.scroll = state.scroll.min(total_lines.saturating_sub(content_height));
    let end_line = (state.scroll + visible_lines).min(total_lines);
    let percentage = if total_lines > 0 {
        ((end_line as f32 / total_lines as f32) * 100.0) as u32
    } else {
        100
    };

    let mode_str = match state.mode {
        ViewerMode::Text => state.language.name(),
        ViewerMode::Hex => "Hex",
    };

    let header = Line::from(vec![
        Span::styled(" File Viewer ", theme.header_style()),
        Span::styled(
            format!(
                "[{}] {} | {}-{}/{} ({}%) ",
                mode_str,
                state.encoding,
                if total_lines == 0 {
                    0
                } else {
                    state.scroll + 1
                },
                end_line,
                total_lines,
                percentage
            ),
            theme.dim_style(),
        ),
        if !state.bookmarks.is_empty() {
            Span::styled(
                format!(" [{}]", state.bookmarks.len()),
                Style::default()
                    .fg(theme.viewer.bookmark_indicator)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("")
        },
    ]);
    frame.render_widget(
        Paragraph::new(header).style(theme.status_bar_style()),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    // Content
    // 하이라이터 리셋
    let mut highlighter = state.highlighter.clone();
    if let Some(ref mut hl) = highlighter {
        hl.reset();
        // 스크롤 전까지 상태 업데이트
        if !state.word_wrap {
            for line in state.lines.iter().take(state.scroll) {
                hl.tokenize_line(line);
            }
        }
    }

    // Word wrap 모드일 경우 표시할 줄들을 미리 계산
    if state.word_wrap {
        // 하이라이터 리셋 for word wrap mode
        let mut hl_for_wrap = state.highlighter.clone();
        if let Some(ref mut hl) = hl_for_wrap {
            hl.reset();
        }
        let mut last_orig_line: Option<usize> = None;

        // 스크롤 위치부터 렌더링
        for (i, (orig_line_num, display_text, is_first)) in wrapped_lines
            .iter()
            .skip(state.scroll)
            .take(content_height)
            .enumerate()
        {
            let is_bookmarked = state.bookmarks.contains(orig_line_num);

            // 줄 번호 (첫 줄만 표시)
            let line_num_style = if is_bookmarked {
                Style::default()
                    .fg(theme.viewer.bookmark_indicator)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.viewer.line_number)
            };

            let line_num_span = if *is_first {
                Span::styled(format!("{:4} ", orig_line_num + 1), line_num_style)
            } else {
                Span::styled("     ", theme.dim_style()) // 연속 줄은 빈 줄번호
            };

            // 라인 배경 스타일 (에디터와 동일하게 매치된 텍스트만 하이라이트)
            let line_bg_style = theme.normal_style();

            // 콘텐츠 렌더링 (검색 하이라이트 또는 문법 강조)
            let content_spans = if state.mode == ViewerMode::Hex {
                render_hex_line(display_text, theme)
            } else if let Some(ref mut hl) = hl_for_wrap {
                // 새로운 원본 줄이면 하이라이터 상태 업데이트
                if last_orig_line != Some(*orig_line_num) {
                    // 이전에 처리하지 않은 줄들의 상태 업데이트
                    if let Some(last) = last_orig_line {
                        for skip_idx in (last + 1)..*orig_line_num {
                            if skip_idx < state.lines.len() {
                                hl.tokenize_line(&state.lines[skip_idx]);
                            }
                        }
                    } else {
                        // 처음 시작 시 스크롤 전까지의 줄들 처리
                        for skip_idx in 0..*orig_line_num {
                            if skip_idx < state.lines.len() {
                                hl.tokenize_line(&state.lines[skip_idx]);
                            }
                        }
                    }
                    last_orig_line = Some(*orig_line_num);
                }

                // 문법 강조와 검색 하이라이트를 함께 처리 (wrapped 모드)
                render_wrapped_line_with_syntax_and_search(
                    display_text,
                    hl,
                    &state.search_term,
                    line_bg_style,
                    theme,
                )
            } else if !state.match_positions.is_empty() {
                // 검색어 하이라이트 (wrapped 텍스트에 대해)
                highlight_search_in_wrapped_line(
                    display_text,
                    &state.search_term,
                    line_bg_style,
                    theme,
                )
            } else {
                vec![Span::styled(display_text.clone(), line_bg_style)]
            };

            let mut spans = vec![line_num_span];
            spans.extend(content_spans);

            frame.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect::new(inner.x, inner.y + 1 + i as u16, inner.width, 1),
            );
        }
    } else {
        // 일반 모드 (word wrap 없음)
        for (i, original_line) in state
            .lines
            .iter()
            .skip(state.scroll)
            .take(content_height)
            .enumerate()
        {
            // TAB을 4칸 스페이스로 변환 (잔상 방지)
            let line = original_line.replace('\t', "    ");
            let line_num = state.scroll + i;
            let is_bookmarked = state.bookmarks.contains(&line_num);

            // 줄 번호
            let line_num_style = if is_bookmarked {
                Style::default()
                    .fg(theme.viewer.bookmark_indicator)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.viewer.line_number)
            };

            let line_num_span = Span::styled(format!("{:4} ", line_num + 1), line_num_style);

            // 라인 배경 스타일 (에디터와 동일하게 매치된 텍스트만 하이라이트)
            let line_bg_style = theme.normal_style();

            // 콘텐츠 렌더링
            let content_spans = if state.mode == ViewerMode::Hex {
                render_hex_line(&line, theme)
            } else if let Some(ref mut hl) = highlighter {
                // 문법 강조와 검색 하이라이트를 함께 처리
                render_line_with_syntax_and_search(
                    &line,
                    hl,
                    &state.match_positions,
                    line_num,
                    state.current_match,
                    line_bg_style,
                    theme,
                )
            } else if !state.match_positions.is_empty() {
                highlight_search_in_line(
                    &line,
                    &state.match_positions,
                    line_num,
                    state.current_match,
                    line_bg_style,
                    theme,
                )
            } else {
                vec![Span::styled(line.clone(), line_bg_style)]
            };

            // 수평 스크롤 적용 (display width 기준, span별 개별 처리)
            let final_spans = if state.horizontal_scroll > 0 {
                let mut result_spans: Vec<Span> = Vec::new();
                let mut remaining_skip = state.horizontal_scroll;
                let mut total_rendered_width = 0usize;

                for span in &content_spans {
                    if total_rendered_width >= content_width {
                        break;
                    }
                    let span_text = &span.content;
                    let mut visible_text = String::new();

                    for c in span_text.chars() {
                        let cw = c.width().unwrap_or(1);
                        if remaining_skip > 0 {
                            if cw <= remaining_skip {
                                remaining_skip -= cw;
                            } else {
                                // 전각 문자가 스크롤 경계에 걸림 — 공백으로 대체
                                visible_text.push(' ');
                                total_rendered_width += 1;
                                remaining_skip = 0;
                            }
                            continue;
                        }
                        if total_rendered_width + cw > content_width {
                            // 전각 문자가 우측 경계에 걸리면 공백 패딩
                            if total_rendered_width + 1 <= content_width {
                                visible_text.push(' ');
                                total_rendered_width += 1;
                            }
                            break;
                        }
                        visible_text.push(c);
                        total_rendered_width += cw;
                    }

                    if !visible_text.is_empty() {
                        result_spans.push(Span::styled(visible_text, span.style));
                    }
                }
                result_spans
            } else {
                content_spans
            };

            let mut spans = vec![line_num_span];
            spans.extend(final_spans);

            frame.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect::new(inner.x, inner.y + 1 + i as u16, inner.width, 1),
            );
        }
    }

    // 스크롤바
    if total_lines > content_height {
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));

        let max_scroll = total_lines.saturating_sub(content_height);
        let mut scrollbar_state = ScrollbarState::new(max_scroll + 1).position(state.scroll);

        let scrollbar_area = Rect::new(
            inner.x + inner.width.saturating_sub(1),
            inner.y + 1,
            1,
            content_height as u16,
        );

        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }

    // Footer / Search bar / Goto bar
    let footer_y = inner.y + inner.height - 1;

    if state.goto_mode {
        let goto_line = Line::from(vec![
            Span::styled("Go to line: ", theme.header_style()),
            Span::styled(&state.goto_input, theme.normal_style()),
            Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ]);
        frame.render_widget(
            Paragraph::new(goto_line).style(theme.status_bar_style()),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
    } else if state.search_mode {
        let search_opts = format!(
            "[{}{}{}]",
            if state.search_options.case_sensitive {
                "Aa"
            } else {
                "aa"
            },
            if state.search_options.use_regex {
                " Re"
            } else {
                ""
            },
            if state.search_options.whole_word {
                " W"
            } else {
                ""
            }
        );

        let cursor_style = Style::default()
            .fg(theme.viewer.search_cursor_fg)
            .bg(theme.viewer.search_cursor_bg)
            .add_modifier(Modifier::SLOW_BLINK);
        let input_style = Style::default().fg(theme.viewer.search_input_text);

        // 커서 위치 기반 텍스트 분리
        let search_chars: Vec<char> = state.search_input.chars().collect();
        let cursor_pos = state.search_cursor_pos.min(search_chars.len());
        let before: String = search_chars[..cursor_pos].iter().collect();
        let cursor_char = if cursor_pos < search_chars.len() {
            search_chars[cursor_pos].to_string()
        } else {
            " ".to_string()
        };
        let after: String = if cursor_pos < search_chars.len() {
            search_chars[cursor_pos + 1..].iter().collect()
        } else {
            String::new()
        };

        // 매치 정보
        let (match_info, match_info_style) = if !state.match_positions.is_empty() {
            let count = state.match_positions.len();
            (
                format!(
                    " {}/{} ({} matches) ",
                    state.current_match + 1,
                    count,
                    count
                ),
                theme.dim_style(),
            )
        } else if !state.search_term.is_empty() {
            (" No matches ".to_string(), theme.dim_style())
        } else {
            (String::new(), theme.dim_style())
        };

        let mut spans = vec![
            Span::styled("Find: ", theme.header_style()),
            Span::styled(before, input_style),
            Span::styled(cursor_char, cursor_style),
            Span::styled(after, input_style),
            Span::styled(match_info, match_info_style),
            Span::styled(format!("{} ", search_opts), theme.dim_style()),
        ];

        // 단축키 안내 (에디터와 동일)
        spans.push(Span::styled("^N", theme.header_style()));
        spans.push(Span::styled("ext ", theme.dim_style()));
        spans.push(Span::styled("^P", theme.header_style()));
        spans.push(Span::styled("rev ", theme.dim_style()));
        spans.push(Span::styled("^C", theme.header_style()));
        spans.push(Span::styled("ase ", theme.dim_style()));
        spans.push(Span::styled("^R", theme.header_style()));
        spans.push(Span::styled("egex ", theme.dim_style()));
        spans.push(Span::styled("^W", theme.header_style()));
        spans.push(Span::styled("ord ", theme.dim_style()));
        spans.push(Span::styled("Esc", theme.header_style()));

        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(theme.status_bar_style()),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
    } else {
        let wrap_indicator = if state.word_wrap { "Wrap " } else { "" };

        // 단축키 spans 생성
        let mut footer_spans = vec![];

        if !wrap_indicator.is_empty() {
            footer_spans.push(Span::styled(
                wrap_indicator,
                Style::default().fg(theme.viewer.wrap_indicator),
            ));
        }

        // 단축키 표시: keybindings에서 동적으로
        use crate::keybindings::ViewerAction;
        let vkb = kb;
        let shortcuts: Vec<(String, &str)> = vec![
            (
                vkb.viewer_first_key(ViewerAction::Quit).to_string(),
                "quit ",
            ),
            (
                vkb.viewer_first_key(ViewerAction::Find).to_string(),
                "find ",
            ),
            (
                vkb.viewer_first_key(ViewerAction::GotoLine).to_string(),
                "goto ",
            ),
            (
                vkb.viewer_first_key(ViewerAction::Edit).to_string(),
                "edit ",
            ),
            (
                vkb.viewer_first_key(ViewerAction::ToggleWrap).to_string(),
                "wrap ",
            ),
            (
                vkb.viewer_first_key(ViewerAction::ToggleHex).to_string(),
                "hex ",
            ),
            (
                vkb.viewer_first_key(ViewerAction::ToggleBookmark)
                    .to_string(),
                "bmark",
            ),
        ];

        for (key, rest) in &shortcuts {
            footer_spans.push(Span::styled(key.as_str(), theme.header_style()));
            footer_spans.push(Span::styled(":", theme.dim_style()));
            footer_spans.push(Span::styled(*rest, theme.dim_style()));
        }

        let footer = Line::from(footer_spans);
        frame.render_widget(
            Paragraph::new(footer).style(theme.status_bar_style()),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
    }
}

/// 헥스 라인 렌더링
fn render_hex_line(line: &str, theme: &Theme) -> Vec<Span<'static>> {
    // 헥스 뷰: offset | hex bytes | ascii
    let mut spans = Vec::new();

    // 간단한 파싱
    if let Some(offset_end) = line.find("  ") {
        // 오프셋
        spans.push(Span::styled(
            line[..offset_end].to_string(),
            Style::default().fg(theme.viewer.hex_offset),
        ));
        spans.push(Span::styled("  ".to_string(), theme.normal_style()));

        let rest = &line[offset_end + 2..];
        if let Some(ascii_start) = rest.rfind("  |") {
            // 헥스 바이트
            spans.push(Span::styled(
                rest[..ascii_start].to_string(),
                Style::default().fg(theme.viewer.hex_bytes),
            ));
            // ASCII
            spans.push(Span::styled(
                rest[ascii_start..].to_string(),
                Style::default().fg(theme.viewer.hex_ascii),
            ));
        } else {
            spans.push(Span::styled(rest.to_string(), theme.normal_style()));
        }
    } else {
        spans.push(Span::styled(line.to_string(), theme.normal_style()));
    }

    spans
}

/// 검색 하이라이트
fn highlight_search_in_line(
    line: &str,
    match_positions: &[(usize, usize, usize)],
    line_num: usize,
    current_match: usize,
    base_style: Style,
    theme: &Theme,
) -> Vec<Span<'static>> {
    // 매치 인덱스와 함께 수집
    let matches: Vec<_> = match_positions
        .iter()
        .enumerate()
        .filter(|(_, (ln, _, _))| *ln == line_num)
        .map(|(idx, (_, s, e))| (idx, *s, *e))
        .collect();

    if matches.is_empty() {
        return vec![Span::styled(line.to_string(), base_style)];
    }

    let mut spans = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut last_end = 0;

    for (match_idx, start, end) in matches {
        let start = start.min(chars.len());
        let end = end.min(chars.len());

        if start > last_end {
            spans.push(Span::styled(
                chars[last_end..start].iter().collect::<String>(),
                base_style,
            ));
        }

        // 현재 매치와 다른 매치 구분
        let match_style = if match_idx == current_match {
            Style::default()
                .bg(theme.viewer.search_match_current_bg)
                .fg(theme.viewer.search_match_current_fg)
        } else {
            Style::default()
                .bg(theme.viewer.search_match_other_bg)
                .fg(theme.viewer.search_match_other_fg)
        };

        spans.push(Span::styled(
            chars[start..end].iter().collect::<String>(),
            match_style,
        ));
        last_end = end;
    }

    if last_end < chars.len() {
        spans.push(Span::styled(
            chars[last_end..].iter().collect::<String>(),
            base_style,
        ));
    }

    if spans.is_empty() {
        spans.push(Span::styled(line.to_string(), base_style));
    }

    spans
}

/// 원본 문자열에서 대소문자 무시 검색하여 (byte_start, byte_end) 쌍 반환
fn case_insensitive_find_all(line: &str, term: &str) -> Vec<(usize, usize)> {
    let term_lower: Vec<char> = term.to_lowercase().chars().collect();
    if term_lower.is_empty() {
        return Vec::new();
    }
    let mut results = Vec::new();
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    for start_idx in 0..chars.len() {
        let mut matched = true;
        let mut ti = 0;
        let mut ci = start_idx;
        while ti < term_lower.len() {
            if ci >= chars.len() {
                matched = false;
                break;
            }
            let lc: Vec<char> = chars[ci].1.to_lowercase().collect();
            if lc.len() == 1 && lc[0] == term_lower[ti] {
                ci += 1;
                ti += 1;
            } else {
                matched = false;
                break;
            }
        }
        if matched && ti == term_lower.len() {
            let byte_start = chars[start_idx].0;
            let byte_end = if ci < chars.len() {
                chars[ci].0
            } else {
                line.len()
            };
            results.push((byte_start, byte_end));
        }
    }
    results
}

/// Wrapped 텍스트에서 검색어 하이라이트
fn highlight_search_in_wrapped_line(
    line: &str,
    search_term: &str,
    base_style: Style,
    theme: &Theme,
) -> Vec<Span<'static>> {
    if search_term.is_empty() {
        return vec![Span::styled(line.to_string(), base_style)];
    }

    let matches = case_insensitive_find_all(line, search_term);

    let mut spans = Vec::new();
    let mut last_end = 0;

    // Wrapped 모드에서는 현재 매치 구분이 어려우므로 동일한 스타일 사용
    let match_style = Style::default()
        .bg(theme.viewer.search_match_other_bg)
        .fg(theme.viewer.search_match_other_fg);

    for (byte_start, byte_end) in &matches {
        if *byte_start < last_end {
            continue; // 겹침 매치 건너뛰기
        }
        if *byte_start > last_end {
            spans.push(Span::styled(
                line[last_end..*byte_start].to_string(),
                base_style,
            ));
        }
        spans.push(Span::styled(
            line[*byte_start..*byte_end].to_string(),
            match_style,
        ));
        last_end = *byte_end;
    }

    if last_end < line.len() {
        spans.push(Span::styled(line[last_end..].to_string(), base_style));
    }

    if spans.is_empty() {
        spans.push(Span::styled(line.to_string(), base_style));
    }

    spans
}

/// 문법 강조와 검색 하이라이트를 함께 처리하는 라인 렌더링
fn render_line_with_syntax_and_search(
    line: &str,
    highlighter: &mut SyntaxHighlighter,
    match_positions: &[(usize, usize, usize)],
    line_num: usize,
    current_match: usize,
    base_style: Style,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let tokens = highlighter.tokenize_line(line);
    let chars: Vec<char> = line.chars().collect();

    // 이 라인의 매치 정보 수집
    let line_matches: Vec<_> = match_positions
        .iter()
        .enumerate()
        .filter(|(_, (ln, _, _))| *ln == line_num)
        .map(|(idx, (_, s, e))| (idx, *s, *e))
        .collect();

    // 매치가 없으면 일반 문법 강조만 적용
    if line_matches.is_empty() {
        if tokens.is_empty() {
            return vec![Span::styled(line.to_string(), base_style)];
        }
        return tokens
            .into_iter()
            .map(|token| {
                let style = highlighter.style_for(token.token_type);
                let final_style = match base_style.bg {
                    Some(bg) => style.bg(bg),
                    None => style,
                };
                Span::styled(token.text, final_style)
            })
            .collect();
    }

    // 문자 단위로 처리하여 문법 강조 + 검색 하이라이트 적용
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut char_idx = 0;

    for token in &tokens {
        let token_chars: Vec<char> = token.text.chars().collect();
        let token_style = highlighter.style_for(token.token_type);

        for c in &token_chars {
            let mut style = token_style;

            // 검색 매치 확인
            for &(match_idx, start, end) in &line_matches {
                if char_idx >= start && char_idx < end {
                    // 매치된 부분: 배경색 적용, 문법 강조의 modifier(이탤릭 등) 유지
                    if match_idx == current_match {
                        style = style
                            .bg(theme.viewer.search_match_current_bg)
                            .fg(theme.viewer.search_match_current_fg);
                    } else {
                        style = style
                            .bg(theme.viewer.search_match_other_bg)
                            .fg(theme.viewer.search_match_other_fg);
                    }
                    break;
                }
            }

            spans.push(Span::styled(c.to_string(), style));
            char_idx += 1;
        }
    }

    // 토큰이 없는 경우 (빈 줄 등)
    if spans.is_empty() && !chars.is_empty() {
        for (i, c) in chars.iter().enumerate() {
            let mut style = base_style;

            for &(match_idx, start, end) in &line_matches {
                if i >= start && i < end {
                    if match_idx == current_match {
                        style = style
                            .bg(theme.viewer.search_match_current_bg)
                            .fg(theme.viewer.search_match_current_fg);
                    } else {
                        style = style
                            .bg(theme.viewer.search_match_other_bg)
                            .fg(theme.viewer.search_match_other_fg);
                    }
                    break;
                }
            }

            spans.push(Span::styled(c.to_string(), style));
        }
    }

    if spans.is_empty() {
        spans.push(Span::styled(line.to_string(), base_style));
    }

    spans
}

/// Wrapped 모드에서 문법 강조와 검색 하이라이트를 함께 처리
fn render_wrapped_line_with_syntax_and_search(
    line: &str,
    highlighter: &mut SyntaxHighlighter,
    search_term: &str,
    base_style: Style,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let tokens = highlighter.tokenize_line(line);
    let chars: Vec<char> = line.chars().collect();

    // 검색어가 없으면 일반 문법 강조만 적용
    if search_term.is_empty() {
        if tokens.is_empty() {
            return vec![Span::styled(line.to_string(), base_style)];
        }
        return tokens
            .into_iter()
            .map(|token| {
                let style = highlighter.style_for(token.token_type);
                let final_style = match base_style.bg {
                    Some(bg) => style.bg(bg),
                    None => style,
                };
                Span::styled(token.text, final_style)
            })
            .collect();
    }

    // 검색 매치 위치 찾기 (대소문자 무시, 바이트 인덱스를 문자 인덱스로 변환)
    let lower_line = line.to_lowercase();
    let lower_term = search_term.to_lowercase();
    let match_ranges: Vec<(usize, usize)> = lower_line
        .match_indices(&lower_term)
        .map(|(byte_start, matched)| {
            let char_start = lower_line[..byte_start].chars().count();
            let char_end = char_start + matched.chars().count();
            (char_start, char_end)
        })
        .collect();

    if match_ranges.is_empty() {
        if tokens.is_empty() {
            return vec![Span::styled(line.to_string(), base_style)];
        }
        return tokens
            .into_iter()
            .map(|token| {
                let style = highlighter.style_for(token.token_type);
                let final_style = match base_style.bg {
                    Some(bg) => style.bg(bg),
                    None => style,
                };
                Span::styled(token.text, final_style)
            })
            .collect();
    }

    // 문자 단위로 처리하여 문법 강조 + 검색 하이라이트 적용
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut char_idx = 0;

    for token in &tokens {
        let token_chars: Vec<char> = token.text.chars().collect();
        let token_style = highlighter.style_for(token.token_type);

        for c in &token_chars {
            let mut style = token_style;

            // 검색 매치 확인
            for &(start, end) in &match_ranges {
                if char_idx >= start && char_idx < end {
                    // 매치된 부분: 배경색 적용, 문법 강조의 modifier(이탤릭 등) 유지
                    style = style
                        .bg(theme.viewer.search_match_other_bg)
                        .fg(theme.viewer.search_match_other_fg);
                    break;
                }
            }

            spans.push(Span::styled(c.to_string(), style));
            char_idx += 1;
        }
    }

    // 토큰이 없는 경우
    if spans.is_empty() && !chars.is_empty() {
        for (i, c) in chars.iter().enumerate() {
            let mut style = base_style;

            for &(start, end) in &match_ranges {
                if i >= start && i < end {
                    style = style
                        .bg(theme.viewer.search_match_other_bg)
                        .fg(theme.viewer.search_match_other_fg);
                    break;
                }
            }

            spans.push(Span::styled(c.to_string(), style));
        }
    }

    if spans.is_empty() {
        spans.push(Span::styled(line.to_string(), base_style));
    }

    spans
}

/// 문법 강조된 라인 렌더링
fn render_syntax_highlighted_line(
    line: &str,
    highlighter: &mut SyntaxHighlighter,
    base_style: Style,
) -> Vec<Span<'static>> {
    let tokens = highlighter.tokenize_line(line);

    if tokens.is_empty() {
        return vec![Span::styled(line.to_string(), base_style)];
    }

    tokens
        .into_iter()
        .map(|token| {
            let style = highlighter.style_for(token.token_type);
            // 배경색이 있는 경우 (선택된 라인 등) 배경색 유지
            let final_style = match base_style.bg {
                Some(bg) => style.bg(bg),
                None => style,
            };
            Span::styled(token.text, final_style)
        })
        .collect()
}

pub fn handle_input(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    let state = match &mut app.viewer_state {
        Some(s) => s,
        None => return,
    };

    // Goto 모드
    if state.goto_mode {
        match code {
            KeyCode::Esc => {
                state.goto_mode = false;
                state.goto_input.clear();
            }
            KeyCode::Enter => {
                state.goto_line(&state.goto_input.clone());
                state.goto_mode = false;
                state.goto_input.clear();
            }
            KeyCode::Backspace => {
                state.goto_input.pop();
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                state.goto_input.push(c);
            }
            _ => {}
        }
        return;
    }

    // 검색 모드
    if state.search_mode {
        match code {
            KeyCode::Esc => {
                state.search_mode = false;
                // 에디터와 동일하게 검색 결과 초기화 (검색어는 유지)
                state.match_positions.clear();
                state.match_lines.clear();
            }
            KeyCode::Enter => {
                // 항상 새로 검색
                state.search_term = state.search_input.clone();
                state.perform_search();
                // 검색 모드 유지 (에디터와 동일)
            }
            KeyCode::Backspace => {
                if state.search_cursor_pos > 0 {
                    let mut chars: Vec<char> = state.search_input.chars().collect();
                    chars.remove(state.search_cursor_pos - 1);
                    state.search_input = chars.into_iter().collect();
                    state.search_cursor_pos -= 1;
                }
            }
            KeyCode::Delete => {
                let char_count = state.search_input.chars().count();
                if state.search_cursor_pos < char_count {
                    let mut chars: Vec<char> = state.search_input.chars().collect();
                    chars.remove(state.search_cursor_pos);
                    state.search_input = chars.into_iter().collect();
                }
            }
            KeyCode::Left => {
                if state.search_cursor_pos > 0 {
                    state.search_cursor_pos -= 1;
                }
            }
            KeyCode::Right => {
                if state.search_cursor_pos < state.search_input.chars().count() {
                    state.search_cursor_pos += 1;
                }
            }
            KeyCode::Home => {
                state.search_cursor_pos = 0;
            }
            KeyCode::End => {
                state.search_cursor_pos = state.search_input.chars().count();
            }
            KeyCode::Char('c') | KeyCode::Char('C')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                state.search_options.case_sensitive = !state.search_options.case_sensitive;
            }
            KeyCode::Char('r') | KeyCode::Char('R')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                state.search_options.use_regex = !state.search_options.use_regex;
            }
            KeyCode::Char('w') | KeyCode::Char('W')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                state.search_options.whole_word = !state.search_options.whole_word;
            }
            KeyCode::Char('n') | KeyCode::Char('N')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                state.next_match();
            }
            KeyCode::Char('p') | KeyCode::Char('P')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                state.prev_match();
            }
            KeyCode::Down => {
                state.next_match();
            }
            KeyCode::Up => {
                state.prev_match();
            }
            KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
                // Shift 처리: 일부 터미널에서 Shift+문자가 소문자로 올 수 있음
                let ch = if modifiers.contains(KeyModifiers::SHIFT) && c.is_ascii_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                let mut chars: Vec<char> = state.search_input.chars().collect();
                chars.insert(state.search_cursor_pos, ch);
                state.search_input = chars.into_iter().collect();
                state.search_cursor_pos += 1;
            }
            _ => {}
        }
        return;
    }

    let visible_lines = state.visible_height;
    let total_lines = state.effective_line_count();

    use crate::keybindings::ViewerAction;
    if let Some(action) = app.keybindings.viewer_action(code, modifiers) {
        match action {
            ViewerAction::Quit => {
                app.current_screen = Screen::FilePanel;
            }
            ViewerAction::Edit => {
                if let Some(ref viewer_state) = app.viewer_state {
                    if !viewer_state.is_binary {
                        let path = viewer_state.file_path.clone();
                        let viewer_scroll = viewer_state.scroll;
                        let mut editor = super::file_editor::EditorState::new();
                        editor.set_syntax_colors(app.theme.syntax);
                        match editor.load_file(&path) {
                            Ok(_) => {
                                let cursor_line =
                                    viewer_scroll.min(editor.lines.len().saturating_sub(1));
                                editor.scroll = cursor_line;
                                editor.cursor_line = cursor_line;
                                editor.cursor_col = 0;
                                app.editor_state = Some(editor);
                                app.previous_screen = Some(Screen::FileViewer);
                                app.current_screen = Screen::FileEditor;
                            }
                            Err(e) => {
                                app.show_message(&format!("Cannot edit file: {}", e));
                            }
                        }
                    }
                }
            }
            ViewerAction::ScrollUp => {
                state.scroll = state.scroll.saturating_sub(1);
            }
            ViewerAction::ScrollDown => {
                if state.scroll.saturating_add(visible_lines) < total_lines {
                    state.scroll += 1;
                }
            }
            ViewerAction::ScrollLeft => {
                state.horizontal_scroll = state.horizontal_scroll.saturating_sub(10);
            }
            ViewerAction::ScrollRight => {
                state.horizontal_scroll += 10;
            }
            ViewerAction::PageUp => {
                state.scroll = state.scroll.saturating_sub(visible_lines);
            }
            ViewerAction::PageDown => {
                let max = total_lines.saturating_sub(visible_lines);
                state.scroll = state.scroll.saturating_add(visible_lines).min(max);
            }
            ViewerAction::GoTop => {
                state.scroll = 0;
            }
            ViewerAction::GoBottom => {
                state.scroll = total_lines.saturating_sub(visible_lines);
            }
            ViewerAction::Find => {
                state.search_mode = true;
                state.search_input.clear();
                state.search_cursor_pos = 0;
            }
            ViewerAction::ToggleBookmark => {
                let current_line = state.source_line_for_visual_row(state.scroll);
                state.toggle_bookmark(current_line);
            }
            ViewerAction::NextBookmark => {
                state.goto_next_bookmark();
            }
            ViewerAction::PrevBookmark => {
                state.goto_prev_bookmark();
            }
            ViewerAction::ToggleWrap => {
                state.toggle_word_wrap();
            }
            ViewerAction::ToggleHex => {
                state.toggle_mode();
            }
            ViewerAction::GotoLine => {
                state.goto_mode = true;
                state.goto_input.clear();
            }
        }
    }
}
