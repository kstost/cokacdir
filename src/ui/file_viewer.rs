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
    pub search_term: String,
    pub search_options: SearchOptions,
    pub match_lines: Vec<usize>,
    pub match_positions: Vec<(usize, usize, usize)>, // (line, start, end)
    pub current_match: usize,

    // 북마크
    pub bookmarks: HashSet<usize>,

    // Goto line
    pub goto_mode: bool,
    pub goto_input: String,

    // 문법 강조
    pub language: Language,
    pub highlighter: Option<SyntaxHighlighter>,

    // 인코딩
    pub encoding: String,
    pub is_binary: bool,

    // 파일 정보
    pub file_size: u64,
    pub total_lines: usize,
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
            search_term: String::new(),
            search_options: SearchOptions::default(),
            match_lines: Vec::new(),
            match_positions: Vec::new(),
            current_match: 0,
            bookmarks: HashSet::new(),
            goto_mode: false,
            goto_input: String::new(),
            language: Language::Plain,
            highlighter: None,
            encoding: "UTF-8".to_string(),
            is_binary: false,
            file_size: 0,
            total_lines: 0,
        }
    }

    /// 파일 로드
    pub fn load_file(&mut self, path: &PathBuf) -> Result<(), String> {
        self.file_path = path.clone();
        self.scroll = 0;
        self.horizontal_scroll = 0;
        self.bookmarks.clear();
        self.search_term.clear();
        self.match_lines.clear();
        self.match_positions.clear();

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
            self.highlighter = Some(SyntaxHighlighter::new(self.language));
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
                format!(
                    "{} {}",
                    hex[..8].join(" "),
                    hex[8..].join(" ")
                )
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

            lines.push(format!(
                "{:08X}  {:<48}  |{}|",
                offset,
                hex_str,
                ascii
            ));
        }

        lines
    }

    /// 검색 수행
    pub fn perform_search(&mut self) {
        self.match_lines.clear();
        self.match_positions.clear();

        if self.search_term.is_empty() {
            return;
        }

        let pattern = if self.search_options.use_regex {
            self.search_term.clone()
        } else {
            regex::escape(&self.search_term)
        };

        let pattern = if self.search_options.whole_word {
            format!(r"\b{}\b", pattern)
        } else {
            pattern
        };

        let regex = if self.search_options.case_sensitive {
            Regex::new(&pattern)
        } else {
            Regex::new(&format!("(?i){}", pattern))
        };

        if let Ok(re) = regex {
            for (line_idx, line) in self.lines.iter().enumerate() {
                let mut has_match = false;
                for mat in re.find_iter(line) {
                    self.match_positions.push((line_idx, mat.start(), mat.end()));
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

    /// 현재 매치로 스크롤
    pub fn scroll_to_current_match(&mut self) {
        if !self.match_lines.is_empty() && self.current_match < self.match_lines.len() {
            let line = self.match_lines[self.current_match];
            self.scroll = line.saturating_sub(5);
        }
    }

    /// 다음 매치
    pub fn next_match(&mut self) {
        if !self.match_lines.is_empty() {
            self.current_match = (self.current_match + 1) % self.match_lines.len();
            self.scroll_to_current_match();
        }
    }

    /// 이전 매치
    pub fn prev_match(&mut self) {
        if !self.match_lines.is_empty() {
            self.current_match = if self.current_match == 0 {
                self.match_lines.len() - 1
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
        let current_line = self.scroll + 5; // 화면 중앙 근처
        let mut sorted: Vec<_> = self.bookmarks.iter().copied().collect();
        sorted.sort();

        for &bm in &sorted {
            if bm > current_line {
                self.scroll = bm.saturating_sub(5);
                return;
            }
        }
        // 처음 북마크로 순환
        self.scroll = sorted[0].saturating_sub(5);
    }

    /// 이전 북마크로 이동
    pub fn goto_prev_bookmark(&mut self) {
        if self.bookmarks.is_empty() {
            return;
        }

        // 현재 화면에 보이는 첫 번째 줄 기준
        let current_line = self.scroll + 5; // 화면 중앙 근처
        let mut sorted: Vec<_> = self.bookmarks.iter().copied().collect();
        sorted.sort();
        sorted.reverse();

        for &bm in &sorted {
            if bm < current_line {
                self.scroll = bm.saturating_sub(5);
                return;
            }
        }
        // 마지막 북마크로 순환
        self.scroll = sorted[0].saturating_sub(5);
    }

    /// 줄 번호로 이동
    pub fn goto_line(&mut self, line_str: &str) {
        if let Ok(line_num) = line_str.parse::<usize>() {
            if line_num > 0 && line_num <= self.lines.len() {
                self.scroll = (line_num - 1).saturating_sub(5);
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
}

pub fn draw(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let state = match &app.viewer_state {
        Some(s) => s,
        None => return,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style(true));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 3 {
        return;
    }

    // Header
    let total_lines = state.lines.len();
    let visible_lines = (inner.height - 2) as usize;
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
                state.scroll + 1,
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
                    .fg(theme.info)
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
    let content_height = (inner.height - 2) as usize;
    let content_width = (inner.width - 5) as usize; // 줄 번호 공간 제외

    // 하이라이터 리셋
    let mut highlighter = state.highlighter.clone();
    if let Some(ref mut hl) = highlighter {
        hl.reset();
        // 스크롤 전까지 상태 업데이트
        for line in state.lines.iter().take(state.scroll) {
            hl.tokenize_line(line);
        }
    }

    // Word wrap 모드일 경우 표시할 줄들을 미리 계산
    if state.word_wrap {
        // wrapped 줄 목록 생성: (원본 줄 번호, 원본 줄 참조, 줄 내용, 첫 줄 여부)
        let mut wrapped_lines: Vec<(usize, String, bool)> = Vec::new();

        for (orig_idx, line) in state.lines.iter().enumerate() {
            if line.is_empty() {
                wrapped_lines.push((orig_idx, String::new(), true));
            } else if content_width > 0 {
                let wrapped = textwrap::wrap(line, content_width);
                for (wi, wline) in wrapped.iter().enumerate() {
                    wrapped_lines.push((orig_idx, wline.to_string(), wi == 0));
                }
            } else {
                wrapped_lines.push((orig_idx, line.clone(), true));
            }
        }

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
            let is_match = state.match_lines.contains(orig_line_num);
            let is_current_match = state.match_lines.get(state.current_match) == Some(orig_line_num);
            let is_bookmarked = state.bookmarks.contains(orig_line_num);

            // 줄 번호 (첫 줄만 표시)
            let line_num_style = if is_bookmarked {
                Style::default().fg(theme.info).add_modifier(Modifier::BOLD)
            } else {
                theme.dim_style()
            };

            let line_num_span = if *is_first {
                Span::styled(
                    format!("{:4} ", orig_line_num + 1),
                    line_num_style,
                )
            } else {
                Span::styled("     ", theme.dim_style()) // 연속 줄은 빈 줄번호
            };

            // 라인 배경 스타일
            let line_bg_style = if is_current_match {
                theme.selected_style()
            } else if is_match {
                theme.warning_style()
            } else {
                theme.normal_style()
            };

            // 콘텐츠 렌더링 (검색 하이라이트 또는 문법 강조)
            let content_spans = if state.mode == ViewerMode::Hex {
                vec![Span::styled(display_text.clone(), line_bg_style)]
            } else if !state.search_term.is_empty() {
                // 검색어 하이라이트 (wrapped 텍스트에 대해)
                highlight_search_in_wrapped_line(display_text, &state.search_term, line_bg_style, theme)
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

                // wrapped 텍스트에 대해 토큰화
                let tokens = hl.tokenize_line(display_text);
                if tokens.is_empty() {
                    vec![Span::styled(display_text.clone(), line_bg_style)]
                } else {
                    tokens
                        .into_iter()
                        .map(|token| {
                            let style = hl.style_for(token.token_type);
                            let final_style = match line_bg_style.bg {
                                Some(bg) => style.bg(bg),
                                None => style,
                            };
                            Span::styled(token.text, final_style)
                        })
                        .collect()
                }
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
        for (i, line) in state.lines.iter().skip(state.scroll).take(content_height).enumerate() {
            let line_num = state.scroll + i;
            let is_match = state.match_lines.contains(&line_num);
            let is_current_match = state.match_lines.get(state.current_match) == Some(&line_num);
            let is_bookmarked = state.bookmarks.contains(&line_num);

            // 줄 번호
            let line_num_style = if is_bookmarked {
                Style::default().fg(theme.info).add_modifier(Modifier::BOLD)
            } else {
                theme.dim_style()
            };

            let line_num_span = Span::styled(
                format!("{:4} ", line_num + 1),
                line_num_style,
            );

            // 라인 배경 스타일
            let line_bg_style = if is_current_match {
                theme.selected_style()
            } else if is_match {
                theme.warning_style()
            } else {
                theme.normal_style()
            };

            // 콘텐츠 렌더링
            let content_spans = if state.mode == ViewerMode::Hex {
                render_hex_line(line, theme)
            } else if !state.search_term.is_empty() {
                highlight_search_in_line(line, &state.match_positions, line_num, line_bg_style, theme)
            } else if let Some(ref mut hl) = highlighter {
                render_syntax_highlighted_line(line, hl, line_bg_style)
            } else {
                vec![Span::styled(line.clone(), line_bg_style)]
            };

            // 수평 스크롤 적용
            let final_spans = if state.horizontal_scroll > 0 {
                let display_line: String = line.chars().skip(state.horizontal_scroll).collect();
                if content_spans.len() == 1 {
                    vec![Span::styled(display_line, content_spans[0].style)]
                } else {
                    // 복잡한 spans의 경우 단순화
                    vec![Span::styled(display_line, line_bg_style)]
                }
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
        let mut scrollbar_state = ScrollbarState::new(max_scroll + 1)
            .position(state.scroll);

        let scrollbar_area = Rect::new(
            inner.x + inner.width - 1,
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
            if state.search_options.case_sensitive { "Aa" } else { "aa" },
            if state.search_options.use_regex { " Re" } else { "" },
            if state.search_options.whole_word { " W" } else { "" }
        );
        let search_line = Line::from(vec![
            Span::styled("Search: ", theme.header_style()),
            Span::styled(&state.search_input, theme.normal_style()),
            Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
            Span::styled(format!(" {}", search_opts), theme.dim_style()),
        ]);
        frame.render_widget(
            Paragraph::new(search_line).style(theme.status_bar_style()),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
    } else {
        let search_info = if !state.search_term.is_empty() {
            format!(
                "\"{}\" {} matches ({}/{}) ",
                if state.search_term.chars().count() > 20 {
                    let truncated: String = state.search_term.chars().take(17).collect();
                    format!("{}...", truncated)
                } else {
                    state.search_term.clone()
                },
                state.match_lines.len(),
                if state.match_lines.is_empty() {
                    0
                } else {
                    state.current_match + 1
                },
                state.match_lines.len()
            )
        } else {
            String::new()
        };

        let wrap_indicator = if state.word_wrap { "Wrap " } else { "" };

        // 단축키 spans 생성
        let mut footer_spans = vec![
            Span::styled(search_info, theme.header_style()),
        ];

        if !wrap_indicator.is_empty() {
            footer_spans.push(Span::styled(wrap_indicator, theme.info_style()));
        }

        // 단축키 표시: 첫 글자 강조
        let shortcuts = [
            ("q", "uit "),
            ("e", "dit "),
            ("/", "search "),
            ("g", "oto "),
            ("b", "mark "),
            ("w", "rap "),
            ("H", "ex"),
        ];

        for (key, rest) in shortcuts {
            footer_spans.push(Span::styled(key, theme.header_style()));
            footer_spans.push(Span::styled(rest, theme.dim_style()));
        }

        // 검색 중일 때만 n/N 표시
        if !state.search_term.is_empty() {
            footer_spans.push(Span::styled(" n", theme.header_style()));
            footer_spans.push(Span::styled("ext ", theme.dim_style()));
            footer_spans.push(Span::styled("N", theme.header_style()));
            footer_spans.push(Span::styled("prev", theme.dim_style()));
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
            Style::default().fg(theme.text_dim),
        ));
        spans.push(Span::styled("  ".to_string(), theme.normal_style()));

        let rest = &line[offset_end + 2..];
        if let Some(ascii_start) = rest.rfind("  |") {
            // 헥스 바이트
            spans.push(Span::styled(
                rest[..ascii_start].to_string(),
                Style::default().fg(theme.info),
            ));
            // ASCII
            spans.push(Span::styled(
                rest[ascii_start..].to_string(),
                Style::default().fg(theme.text_directory),
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
    base_style: Style,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let matches: Vec<_> = match_positions
        .iter()
        .filter(|(ln, _, _)| *ln == line_num)
        .map(|(_, s, e)| (*s, *e))
        .collect();

    if matches.is_empty() {
        return vec![Span::styled(line.to_string(), base_style)];
    }

    let mut spans = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut last_end = 0;

    for (start, end) in matches {
        let start = start.min(chars.len());
        let end = end.min(chars.len());

        if start > last_end {
            spans.push(Span::styled(
                chars[last_end..start].iter().collect::<String>(),
                base_style,
            ));
        }
        spans.push(Span::styled(
            chars[start..end].iter().collect::<String>(),
            Style::default()
                .fg(ratatui::style::Color::Black)
                .bg(theme.warning),
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

    let lower_line = line.to_lowercase();
    let lower_term = search_term.to_lowercase();

    let mut spans = Vec::new();
    let mut last_end = 0;

    for (start, _) in lower_line.match_indices(&lower_term) {
        let end = start + search_term.len();

        if start > last_end {
            spans.push(Span::styled(
                line[last_end..start].to_string(),
                base_style,
            ));
        }
        spans.push(Span::styled(
            line[start..end].to_string(),
            Style::default()
                .fg(ratatui::style::Color::Black)
                .bg(theme.warning),
        ));
        last_end = end;
    }

    if last_end < line.len() {
        spans.push(Span::styled(
            line[last_end..].to_string(),
            base_style,
        ));
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
                state.search_input.clear();
            }
            KeyCode::Enter => {
                state.search_term = state.search_input.clone();
                state.search_mode = false;
                state.perform_search();
            }
            KeyCode::Backspace => {
                state.search_input.pop();
            }
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+C: 대소문자 구분 토글
                state.search_options.case_sensitive = !state.search_options.case_sensitive;
            }
            KeyCode::Char('r') if modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+R: 정규식 토글
                state.search_options.use_regex = !state.search_options.use_regex;
            }
            KeyCode::Char('w') if modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+W: 단어 단위 검색 토글
                state.search_options.whole_word = !state.search_options.whole_word;
            }
            KeyCode::Char(c) => {
                state.search_input.push(c);
            }
            _ => {}
        }
        return;
    }

    let visible_lines = 20;

    match code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
            app.current_screen = Screen::DualPanel;
        }
        KeyCode::Char('e') | KeyCode::Char('E') => {
            // 편집기 모드로 전환
            if let Some(ref viewer_state) = app.viewer_state {
                if !viewer_state.is_binary {
                    let path = viewer_state.file_path.clone();
                    let viewer_scroll = viewer_state.scroll;
                    let mut editor = super::file_editor::EditorState::new();
                    if editor.load_file(&path).is_ok() {
                        // 뷰어의 스크롤 위치를 에디터에 전달
                        editor.scroll = viewer_scroll;
                        editor.cursor_line = viewer_scroll;
                        editor.cursor_col = 0;
                        app.editor_state = Some(editor);
                        app.previous_screen = Some(Screen::FileViewer);
                        app.current_screen = Screen::FileEditor;
                    }
                }
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.scroll = state.scroll.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if state.scroll + visible_lines < state.lines.len() {
                state.scroll += 1;
            }
        }
        KeyCode::Left | KeyCode::Char('h') if !modifiers.contains(KeyModifiers::SHIFT) => {
            // 헥스 모드 토글 (대문자 H만)
            if code == KeyCode::Char('h') {
                state.toggle_mode();
            } else {
                state.horizontal_scroll = state.horizontal_scroll.saturating_sub(10);
            }
        }
        KeyCode::Right | KeyCode::Char('l') => {
            if code == KeyCode::Right {
                state.horizontal_scroll += 10;
            }
        }
        KeyCode::Char('H') => {
            // 헥스 모드 토글
            state.toggle_mode();
        }
        KeyCode::PageUp => {
            state.scroll = state.scroll.saturating_sub(visible_lines);
        }
        KeyCode::PageDown => {
            let max = state.lines.len().saturating_sub(visible_lines);
            state.scroll = (state.scroll + visible_lines).min(max);
        }
        KeyCode::Home | KeyCode::Char('g') if !modifiers.contains(KeyModifiers::SHIFT) => {
            // Both Home and 'g' (without modifiers) scroll to top
            if code == KeyCode::Home || (code == KeyCode::Char('g') && modifiers.is_empty()) {
                state.scroll = 0;
            }
        }
        KeyCode::End | KeyCode::Char('G') => {
            state.scroll = state.lines.len().saturating_sub(visible_lines);
        }
        KeyCode::Char('/') => {
            state.search_mode = true;
            state.search_input.clear();
        }
        KeyCode::Char('n') => {
            state.next_match();
        }
        KeyCode::Char('N') => {
            state.prev_match();
        }
        KeyCode::Char('b') => {
            // 현재 줄 북마크 토글
            let current_line = state.scroll;
            state.toggle_bookmark(current_line);
        }
        KeyCode::Char('B') => {
            // 다음 북마크로 이동
            if modifiers.contains(KeyModifiers::SHIFT) {
                state.goto_prev_bookmark();
            } else {
                state.goto_next_bookmark();
            }
        }
        KeyCode::Char('[') => {
            // 이전 북마크
            state.goto_prev_bookmark();
        }
        KeyCode::Char(']') => {
            // 다음 북마크
            state.goto_next_bookmark();
        }
        KeyCode::Char('w') | KeyCode::Char('W') => {
            // Word wrap 토글
            state.word_wrap = !state.word_wrap;
        }
        KeyCode::Char('g') if modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl+G: Goto line
            state.goto_mode = true;
            state.goto_input.clear();
        }
        KeyCode::Char(':') => {
            // Vim 스타일 goto
            state.goto_mode = true;
            state.goto_input.clear();
        }
        _ => {}
    }
}

