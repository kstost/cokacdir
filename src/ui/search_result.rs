use chrono::{DateTime, Local};
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};
use std::fs;
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;

use super::theme::Theme;

/// 검색 결과 아이템
#[derive(Debug, Clone)]
pub struct SearchResultItem {
    pub full_path: PathBuf,    // 전체 경로
    pub relative_path: String, // 기준 폴더로부터의 상대 경로
    pub name: String,          // 파일/폴더 이름
    pub is_directory: bool,
    pub size: u64,
    pub modified: DateTime<Local>,
}

/// 검색 결과 상태
#[derive(Debug)]
pub struct SearchResultState {
    pub results: Vec<SearchResultItem>,
    pub selected_index: usize,
    pub scroll_offset: usize,
    pub search_term: String,
    pub base_path: PathBuf, // 검색 시작 경로
    pub active: bool,
}

impl Default for SearchResultState {
    fn default() -> Self {
        Self {
            results: Vec::new(),
            selected_index: 0,
            scroll_offset: 0,
            search_term: String::new(),
            base_path: PathBuf::new(),
            active: false,
        }
    }
}

impl SearchResultState {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    /// 현재 선택된 아이템 반환
    pub fn current_item(&self) -> Option<&SearchResultItem> {
        self.results.get(self.selected_index)
    }

    /// 커서 이동
    pub fn move_cursor(&mut self, delta: i32) {
        if self.results.is_empty() {
            return;
        }
        let new_index = (self.selected_index as i32 + delta)
            .max(0)
            .min(self.results.len().saturating_sub(1) as i32) as usize;
        self.selected_index = new_index;
    }

    /// 처음으로 이동
    pub fn cursor_to_start(&mut self) {
        self.selected_index = 0;
    }

    /// 끝으로 이동
    pub fn cursor_to_end(&mut self) {
        if !self.results.is_empty() {
            self.selected_index = self.results.len() - 1;
        }
    }

    /// 스크롤 오프셋 조정 (화면 높이에 맞게)
    pub fn adjust_scroll(&mut self, visible_height: usize) {
        if visible_height == 0 {
            return;
        }

        // 선택된 항목이 화면에 보이도록 스크롤 조정
        if self.selected_index < self.scroll_offset {
            self.scroll_offset = self.selected_index;
        } else if self.selected_index >= self.scroll_offset + visible_height {
            self.scroll_offset = self.selected_index - visible_height + 1;
        }
    }
}

/// Search a directory tree without consuming the call stack.
pub fn recursive_search(
    base_path: &PathBuf,
    current_path: &PathBuf,
    search_term: &str,
    results: &mut Vec<SearchResultItem>,
    max_results: usize,
) {
    if results.len() >= max_results || max_results == 0 {
        return;
    }

    let lower_term = search_term.to_lowercase();
    let mut pending_dirs = vec![current_path.clone()];

    while let Some(directory) = pending_dirs.pop() {
        if results.len() >= max_results {
            break;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            if results.len() >= max_results {
                break;
            }

            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            let metadata = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };

            // Symlink targets: don't follow into directories to avoid cycles
            let is_directory = metadata.is_dir() && !metadata.file_type().is_symlink();

            // 파일명이 검색어를 포함하는지 확인 (대소문자 무시)
            if name.to_lowercase().contains(&lower_term) {
                let relative_path = path
                    .strip_prefix(base_path)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| path.display().to_string());

                let size = if is_directory { 0 } else { metadata.len() };
                let modified = metadata
                    .modified()
                    .ok()
                    .map(DateTime::<Local>::from)
                    .unwrap_or_else(Local::now);

                results.push(SearchResultItem {
                    full_path: path.clone(),
                    relative_path,
                    name,
                    is_directory,
                    size,
                    modified,
                });
            }

            // Queue real directories only. `symlink_metadata` plus this check
            // prevents directory symlink cycles and traversal outside the
            // requested search root.
            if is_directory {
                pending_dirs.push(path);
            }
        }
    }
}

/// 검색 실행 및 결과 정렬
pub fn execute_recursive_search(
    base_path: &PathBuf,
    search_term: &str,
    max_results: usize,
) -> Vec<SearchResultItem> {
    let mut results = Vec::new();
    recursive_search(base_path, base_path, search_term, &mut results, max_results);

    // 결과 정렬: 디렉토리 우선, 그 다음 이름순
    results.sort_by(|a, b| match (a.is_directory, b.is_directory) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    results
}

/// 검색 결과 화면 그리기
pub fn draw(
    frame: &mut Frame,
    state: &mut SearchResultState,
    area: Rect,
    theme: &Theme,
    kb: &crate::keybindings::Keybindings,
) {
    let title = format!(
        " Search Results: \"{}\" ({} found) ",
        state.search_term,
        state.results.len()
    );

    let block = Block::default()
        .title(title)
        .title_style(theme.header_style())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.search_result.border));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.results.is_empty() {
        // 검색 결과 없음
        let no_result = Paragraph::new(Line::from(Span::styled(
            "No files found matching the search term.",
            theme.dim_style(),
        )));
        frame.render_widget(no_result, inner);
        return;
    }

    // 동적 경로 열 너비 계산: 전체 너비 - 인덱스(6) - 크기(11) - 날짜(17) - 여백(3)
    let path_width = inner.width.saturating_sub(37) as usize;

    // 헤더 행
    let header_line = Line::from(vec![
        Span::styled(
            format!("  {:3} ", "#"),
            Style::default().fg(theme.search_result.column_header_dim),
        ),
        Span::styled(
            format!("{:<width$} ", "Path", width = path_width),
            Style::default()
                .fg(theme.search_result.column_header)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>10} ", "Size"),
            Style::default()
                .fg(theme.search_result.column_header)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:16}", "Modified"),
            Style::default()
                .fg(theme.search_result.column_header)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let header_area = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(Paragraph::new(header_line), header_area);

    // 목록 영역 (헤더 제외)
    let list_area = Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.height.saturating_sub(2), // 헤더 + 도움말
    );

    let visible_height = list_area.height as usize;
    state.adjust_scroll(visible_height);

    // 결과 목록 그리기
    let visible_items: Vec<&SearchResultItem> = state
        .results
        .iter()
        .skip(state.scroll_offset)
        .take(visible_height)
        .collect();

    let mut lines: Vec<Line> = Vec::new();

    for (i, item) in visible_items.iter().enumerate() {
        let actual_index = state.scroll_offset + i;
        let is_selected = actual_index == state.selected_index;

        // 인덱스 번호
        let index_str = format!("{:3} ", actual_index + 1);

        // 경로 (디렉토리면 구분자 추가)
        let path_display = if item.is_directory {
            format!("{}{}", item.relative_path, std::path::MAIN_SEPARATOR)
        } else {
            item.relative_path.clone()
        };

        // 경로가 너무 길면 앞부분을 ...로 생략 (표시 너비 기준)
        let path_str = if path_display.width() > path_width {
            let suffix = crate::utils::format::display_width_suffix(
                &path_display,
                path_width.saturating_sub(3),
            );
            let with_ellipsis = format!("...{}", suffix);
            crate::utils::format::pad_to_display_width(&with_ellipsis, path_width)
        } else {
            crate::utils::format::pad_to_display_width(&path_display, path_width)
        };

        // 크기
        let size_str = if item.is_directory {
            format!("{:>10} ", "<DIR>")
        } else {
            format!("{:>10} ", crate::utils::format::format_size(item.size))
        };

        // 수정일
        let date_str = format!("{}", item.modified.format("%Y-%m-%d %H:%M"));

        // 스타일 결정
        let (index_style, path_style, size_style, date_style) = if is_selected {
            let sel_style = theme.selected_style();
            (sel_style, sel_style, sel_style, sel_style)
        } else if item.is_directory {
            let dir_style = Style::default().fg(theme.search_result.directory_text);
            let dim_style = Style::default().fg(theme.search_result.path_text);
            (dim_style, dir_style, dir_style, dim_style)
        } else {
            let file_style = Style::default().fg(theme.search_result.file_text);
            let dim_style = Style::default().fg(theme.search_result.path_text);
            (dim_style, file_style, file_style, dim_style)
        };

        let line = Line::from(vec![
            Span::styled(if is_selected { "> " } else { "  " }, path_style),
            Span::styled(index_str, index_style),
            Span::styled(format!("{} ", path_str), path_style),
            Span::styled(size_str, size_style),
            Span::styled(date_str, date_style),
        ]);

        lines.push(line);
    }

    let list_paragraph = Paragraph::new(lines);
    frame.render_widget(list_paragraph, list_area);

    // 스크롤바 (결과가 화면보다 많을 때)
    if state.results.len() > visible_height {
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));

        let mut scrollbar_state =
            ScrollbarState::new(state.results.len()).position(state.selected_index);

        let scrollbar_area = Rect::new(inner.x + inner.width - 1, inner.y + 1, 1, list_area.height);

        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }

    // 하단 도움말 (keybindings에서 동적으로)
    use crate::keybindings::SearchResultAction;
    let help_line = Line::from(vec![
        Span::styled(
            kb.search_result_first_key(SearchResultAction::MoveUp)
                .to_string(),
            theme.header_style(),
        ),
        Span::styled(":navigate ", theme.dim_style()),
        Span::styled(
            kb.search_result_first_key(SearchResultAction::Open)
                .to_string(),
            theme.header_style(),
        ),
        Span::styled(":go to path ", theme.dim_style()),
        Span::styled(
            kb.search_result_first_key(SearchResultAction::Close)
                .to_string(),
            theme.header_style(),
        ),
        Span::styled(":close", theme.dim_style()),
    ]);

    let help_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    frame.render_widget(Paragraph::new(help_line), help_area);
}

/// 입력 처리 - true 반환 시 화면 닫기
pub fn handle_input(
    state: &mut SearchResultState,
    code: KeyCode,
    modifiers: KeyModifiers,
    kb: &crate::keybindings::Keybindings,
) -> Option<crate::keybindings::SearchResultAction> {
    use crate::keybindings::SearchResultAction;

    if let Some(action) = kb.search_result_action(code, modifiers) {
        match action {
            SearchResultAction::Close => {
                state.active = false;
                return Some(SearchResultAction::Close);
            }
            SearchResultAction::MoveUp => {
                state.move_cursor(-1);
            }
            SearchResultAction::MoveDown => {
                state.move_cursor(1);
            }
            SearchResultAction::PageUp => {
                state.move_cursor(-10);
            }
            SearchResultAction::PageDown => {
                state.move_cursor(10);
            }
            SearchResultAction::GoHome => {
                state.cursor_to_start();
            }
            SearchResultAction::GoEnd => {
                state.cursor_to_end();
            }
            SearchResultAction::Open => {
                return Some(SearchResultAction::Open);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::execute_recursive_search;

    #[test]
    fn iterative_search_honors_the_result_limit() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..20 {
            std::fs::write(dir.path().join(format!("match-{index}.txt")), "x").unwrap();
        }

        let results = execute_recursive_search(&dir.path().to_path_buf(), "match", 3);

        assert_eq!(results.len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn iterative_search_handles_a_deep_directory_tree() {
        let dir = tempfile::tempdir().unwrap();
        let mut current = dir.path().to_path_buf();
        for _ in 0..1_000 {
            current.push("d");
            std::fs::create_dir(&current).unwrap();
        }
        std::fs::write(current.join("deep-needle.txt"), "found").unwrap();

        let results = execute_recursive_search(&dir.path().to_path_buf(), "needle", 10);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "deep-needle.txt");
    }

    #[cfg(unix)]
    #[test]
    fn iterative_search_does_not_follow_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret-needle.txt"), "outside").unwrap();
        symlink(outside.path(), root.path().join("gateway")).unwrap();

        let results = execute_recursive_search(&root.path().to_path_buf(), "needle", 10);

        assert!(results.is_empty());
    }
}
