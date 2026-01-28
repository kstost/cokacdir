use std::fs;
use std::path::PathBuf;
use std::collections::VecDeque;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};
use regex::Regex;

use super::{
    app::{App, Screen},
    syntax::{Language, SyntaxHighlighter},
    theme::Theme,
};

/// Undo/Redo 액션 유형
#[derive(Debug, Clone)]
pub enum EditAction {
    Insert {
        line: usize,
        col: usize,
        text: String,
    },
    Delete {
        line: usize,
        col: usize,
        text: String,
    },
    InsertLine {
        line: usize,
        content: String,
    },
    DeleteLine {
        line: usize,
        content: String,
    },
    MergeLine {
        line: usize,
        col: usize,
    },
    SplitLine {
        line: usize,
        col: usize,
    },
    Replace {
        line: usize,
        old_content: String,
        new_content: String,
    },
    Batch {
        actions: Vec<EditAction>,
    },
}

/// 선택 영역
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

impl Selection {
    pub fn new(line: usize, col: usize) -> Self {
        Self {
            start_line: line,
            start_col: col,
            end_line: line,
            end_col: col,
        }
    }

    /// 정규화된 선택 영역 (시작이 항상 끝보다 앞)
    pub fn normalized(&self) -> (usize, usize, usize, usize) {
        if self.start_line < self.end_line
            || (self.start_line == self.end_line && self.start_col <= self.end_col)
        {
            (self.start_line, self.start_col, self.end_line, self.end_col)
        } else {
            (self.end_line, self.end_col, self.start_line, self.start_col)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.start_line == self.end_line && self.start_col == self.end_col
    }
}

/// 찾기/바꾸기 모드
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindReplaceMode {
    None,
    Find,
    Replace,
}

/// 찾기/바꾸기 옵션
#[derive(Debug, Clone, Default)]
pub struct FindReplaceOptions {
    pub case_sensitive: bool,
    pub use_regex: bool,
    pub whole_word: bool,
}

/// 편집기 상태
#[derive(Debug)]
pub struct EditorState {
    pub file_path: PathBuf,
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
    pub scroll: usize,
    pub horizontal_scroll: usize,
    pub modified: bool,

    // Undo/Redo
    pub undo_stack: VecDeque<EditAction>,
    pub redo_stack: VecDeque<EditAction>,
    pub max_undo_size: usize,

    // 선택
    pub selection: Option<Selection>,
    pub clipboard: String,

    // 찾기/바꾸기
    pub find_mode: FindReplaceMode,
    pub find_input: String,
    pub replace_input: String,
    pub find_term: String,
    pub find_options: FindReplaceOptions,
    pub match_positions: Vec<(usize, usize, usize)>,
    pub current_match: usize,
    pub input_focus: usize, // 0: find, 1: replace

    // Goto
    pub goto_mode: bool,
    pub goto_input: String,

    // 문법 강조
    pub language: Language,
    pub highlighter: Option<SyntaxHighlighter>,

    // 설정
    pub auto_indent: bool,
    pub tab_size: usize,
    pub use_tabs: bool,
    #[allow(dead_code)]
    pub show_whitespace: bool,

    // 괄호 매칭
    pub matching_bracket: Option<(usize, usize)>,
}

impl EditorState {
    pub fn new() -> Self {
        Self {
            file_path: PathBuf::new(),
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            scroll: 0,
            horizontal_scroll: 0,
            modified: false,
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            max_undo_size: 1000,
            selection: None,
            clipboard: String::new(),
            find_mode: FindReplaceMode::None,
            find_input: String::new(),
            replace_input: String::new(),
            find_term: String::new(),
            find_options: FindReplaceOptions::default(),
            match_positions: Vec::new(),
            current_match: 0,
            input_focus: 0,
            goto_mode: false,
            goto_input: String::new(),
            language: Language::Plain,
            highlighter: None,
            auto_indent: true,
            tab_size: 4,
            use_tabs: false,
            show_whitespace: false,
            matching_bracket: None,
        }
    }

    /// 파일 로드
    pub fn load_file(&mut self, path: &PathBuf) -> Result<(), String> {
        self.file_path = path.clone();
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.scroll = 0;
        self.horizontal_scroll = 0;
        self.modified = false;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.selection = None;
        self.find_mode = FindReplaceMode::None;

        // 파일 읽기
        match fs::read_to_string(path) {
            Ok(content) => {
                self.lines = content.lines().map(String::from).collect();
                if self.lines.is_empty() {
                    self.lines.push(String::new());
                }
            }
            Err(_) => {
                // 새 파일
                self.lines = vec![String::new()];
            }
        }

        // 언어 감지
        self.language = Language::from_extension(path);
        self.highlighter = Some(SyntaxHighlighter::new(self.language));

        Ok(())
    }

    /// 파일 저장
    pub fn save_file(&mut self) -> Result<(), String> {
        let content = self.lines.join("\n");
        fs::write(&self.file_path, content).map_err(|e| e.to_string())?;
        self.modified = false;
        Ok(())
    }

    /// Undo 액션 추가
    pub fn push_undo(&mut self, action: EditAction) {
        self.redo_stack.clear();
        self.undo_stack.push_back(action);
        while self.undo_stack.len() > self.max_undo_size {
            self.undo_stack.pop_front();
        }
        self.modified = true;
    }

    /// Undo 실행
    pub fn undo(&mut self) {
        if let Some(action) = self.undo_stack.pop_back() {
            let reverse = self.reverse_action(&action);
            self.apply_action(&reverse, false);
            self.redo_stack.push_back(action);
        }
    }

    /// Redo 실행
    pub fn redo(&mut self) {
        if let Some(action) = self.redo_stack.pop_back() {
            self.apply_action(&action, false);
            self.undo_stack.push_back(action);
        }
    }

    /// 액션 역순 생성
    fn reverse_action(&self, action: &EditAction) -> EditAction {
        match action {
            EditAction::Insert { line, col, text } => EditAction::Delete {
                line: *line,
                col: *col,
                text: text.clone(),
            },
            EditAction::Delete { line, col, text } => EditAction::Insert {
                line: *line,
                col: *col,
                text: text.clone(),
            },
            EditAction::InsertLine { line, content } => EditAction::DeleteLine {
                line: *line,
                content: content.clone(),
            },
            EditAction::DeleteLine { line, content } => EditAction::InsertLine {
                line: *line,
                content: content.clone(),
            },
            EditAction::MergeLine { line, col } => EditAction::SplitLine {
                line: *line,
                col: *col,
            },
            EditAction::SplitLine { line, col } => EditAction::MergeLine {
                line: *line,
                col: *col,
            },
            EditAction::Replace {
                line,
                old_content,
                new_content,
            } => EditAction::Replace {
                line: *line,
                old_content: new_content.clone(),
                new_content: old_content.clone(),
            },
            EditAction::Batch { actions } => EditAction::Batch {
                actions: actions.iter().rev().map(|a| self.reverse_action(a)).collect(),
            },
        }
    }

    /// 액션 적용
    fn apply_action(&mut self, action: &EditAction, _record: bool) {
        match action {
            EditAction::Insert { line, col, text } => {
                if *line < self.lines.len() {
                    let line_content = &mut self.lines[*line];
                    let mut chars: Vec<char> = line_content.chars().collect();
                    for (i, c) in text.chars().enumerate() {
                        if *col + i <= chars.len() {
                            chars.insert(*col + i, c);
                        }
                    }
                    *line_content = chars.into_iter().collect();
                }
            }
            EditAction::Delete { line, col, text } => {
                if *line < self.lines.len() {
                    let line_content = &mut self.lines[*line];
                    let mut chars: Vec<char> = line_content.chars().collect();
                    for _ in 0..text.chars().count() {
                        if *col < chars.len() {
                            chars.remove(*col);
                        }
                    }
                    *line_content = chars.into_iter().collect();
                }
            }
            EditAction::InsertLine { line, content } => {
                if *line <= self.lines.len() {
                    self.lines.insert(*line, content.clone());
                }
            }
            EditAction::DeleteLine { line, .. } => {
                if *line < self.lines.len() && self.lines.len() > 1 {
                    self.lines.remove(*line);
                }
            }
            EditAction::MergeLine { line, .. } => {
                if *line + 1 < self.lines.len() {
                    let next_line = self.lines.remove(*line + 1);
                    self.lines[*line].push_str(&next_line);
                }
            }
            EditAction::SplitLine { line, col } => {
                if *line < self.lines.len() {
                    let content = &self.lines[*line];
                    let chars: Vec<char> = content.chars().collect();
                    let before: String = chars[..*col.min(&chars.len())].iter().collect();
                    let after: String = chars[*col.min(&chars.len())..].iter().collect();
                    self.lines[*line] = before;
                    self.lines.insert(*line + 1, after);
                }
            }
            EditAction::Replace {
                line,
                new_content,
                ..
            } => {
                if *line < self.lines.len() {
                    self.lines[*line] = new_content.clone();
                }
            }
            EditAction::Batch { actions } => {
                for a in actions {
                    self.apply_action(a, false);
                }
            }
        }
    }

    /// 문자 삽입
    pub fn insert_char(&mut self, c: char) {
        self.delete_selection();

        let action = EditAction::Insert {
            line: self.cursor_line,
            col: self.cursor_col,
            text: c.to_string(),
        };

        let line = &mut self.lines[self.cursor_line];
        let mut chars: Vec<char> = line.chars().collect();
        chars.insert(self.cursor_col, c);
        *line = chars.into_iter().collect();
        self.cursor_col += 1;

        self.push_undo(action);
        self.update_scroll();
    }

    /// 문자열 삽입
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                self.insert_newline();
            } else {
                self.insert_char(c);
            }
        }
    }

    /// 탭 삽입
    pub fn insert_tab(&mut self) {
        let indent = if self.use_tabs {
            "\t".to_string()
        } else {
            " ".repeat(self.tab_size)
        };
        self.insert_str(&indent);
    }

    /// 새 줄 삽입
    pub fn insert_newline(&mut self) {
        self.delete_selection();

        let line = &self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();
        let before: String = chars[..self.cursor_col.min(chars.len())].iter().collect();
        let after: String = chars[self.cursor_col.min(chars.len())..].iter().collect();

        // 자동 들여쓰기
        let indent = if self.auto_indent {
            let leading_ws: String = before.chars().take_while(|c| c.is_whitespace()).collect();
            leading_ws
        } else {
            String::new()
        };

        let action = EditAction::SplitLine {
            line: self.cursor_line,
            col: self.cursor_col,
        };

        self.lines[self.cursor_line] = before;
        self.lines.insert(self.cursor_line + 1, format!("{}{}", indent, after));
        self.cursor_line += 1;
        self.cursor_col = indent.len();

        self.push_undo(action);
        self.update_scroll();
    }

    /// 뒤로 삭제 (Backspace)
    pub fn delete_backward(&mut self) {
        if self.selection.is_some() {
            self.delete_selection();
            return;
        }

        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_line];
            let mut chars: Vec<char> = line.chars().collect();
            let deleted = chars.remove(self.cursor_col - 1);
            *line = chars.into_iter().collect();

            let action = EditAction::Delete {
                line: self.cursor_line,
                col: self.cursor_col - 1,
                text: deleted.to_string(),
            };

            self.cursor_col -= 1;
            self.push_undo(action);
        } else if self.cursor_line > 0 {
            // 이전 줄과 병합
            let current_line = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].chars().count();
            self.lines[self.cursor_line].push_str(&current_line);

            let action = EditAction::MergeLine {
                line: self.cursor_line,
                col: self.cursor_col,
            };

            self.push_undo(action);
        }
        self.update_scroll();
    }

    /// 앞으로 삭제 (Delete)
    pub fn delete_forward(&mut self) {
        if self.selection.is_some() {
            self.delete_selection();
            return;
        }

        let line_len = self.lines[self.cursor_line].chars().count();
        if self.cursor_col < line_len {
            let line = &mut self.lines[self.cursor_line];
            let mut chars: Vec<char> = line.chars().collect();
            let deleted = chars.remove(self.cursor_col);
            *line = chars.into_iter().collect();

            let action = EditAction::Delete {
                line: self.cursor_line,
                col: self.cursor_col,
                text: deleted.to_string(),
            };

            self.push_undo(action);
        } else if self.cursor_line + 1 < self.lines.len() {
            // 다음 줄과 병합
            let next_line = self.lines.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].push_str(&next_line);

            let action = EditAction::MergeLine {
                line: self.cursor_line,
                col: self.cursor_col,
            };

            self.push_undo(action);
        }
    }

    /// 선택 영역 삭제
    pub fn delete_selection(&mut self) {
        let sel = match self.selection.take() {
            Some(s) if !s.is_empty() => s,
            _ => return,
        };

        let (start_line, start_col, end_line, end_col) = sel.normalized();

        if start_line == end_line {
            // 같은 줄 내 삭제
            let line = &mut self.lines[start_line];
            let chars: Vec<char> = line.chars().collect();
            let deleted: String = chars[start_col..end_col].iter().collect();
            let new_line: String = chars[..start_col]
                .iter()
                .chain(chars[end_col..].iter())
                .collect();
            *line = new_line;

            self.push_undo(EditAction::Delete {
                line: start_line,
                col: start_col,
                text: deleted,
            });
        } else {
            // 여러 줄 삭제
            let mut actions = Vec::new();

            // 시작 줄 처리
            let first_chars: Vec<char> = self.lines[start_line].chars().collect();
            let first_part: String = first_chars[..start_col].iter().collect();

            // 끝 줄 처리
            let last_chars: Vec<char> = self.lines[end_line].chars().collect();
            let last_part: String = last_chars[end_col..].iter().collect();

            // 중간 줄들 저장 (undo용)
            for i in (start_line + 1..=end_line).rev() {
                actions.push(EditAction::DeleteLine {
                    line: i,
                    content: self.lines[i].clone(),
                });
            }

            // 줄 병합
            self.lines[start_line] = format!("{}{}", first_part, last_part);

            // 중간 줄들 제거
            for _ in start_line + 1..=end_line {
                if start_line + 1 < self.lines.len() {
                    self.lines.remove(start_line + 1);
                }
            }

            self.push_undo(EditAction::Batch { actions });
        }

        self.cursor_line = start_line;
        self.cursor_col = start_col;
        self.update_scroll();
    }

    /// 선택된 텍스트 가져오기
    pub fn get_selected_text(&self) -> String {
        let sel = match &self.selection {
            Some(s) if !s.is_empty() => s,
            _ => return String::new(),
        };

        let (start_line, start_col, end_line, end_col) = sel.normalized();

        if start_line == end_line {
            let chars: Vec<char> = self.lines[start_line].chars().collect();
            chars[start_col..end_col].iter().collect()
        } else {
            let mut result = String::new();

            // 첫 줄
            let first_chars: Vec<char> = self.lines[start_line].chars().collect();
            result.push_str(&first_chars[start_col..].iter().collect::<String>());

            // 중간 줄
            for i in start_line + 1..end_line {
                result.push('\n');
                result.push_str(&self.lines[i]);
            }

            // 마지막 줄
            result.push('\n');
            let last_chars: Vec<char> = self.lines[end_line].chars().collect();
            result.push_str(&last_chars[..end_col].iter().collect::<String>());

            result
        }
    }

    /// 복사
    pub fn copy(&mut self) {
        self.clipboard = self.get_selected_text();
    }

    /// 잘라내기
    #[allow(dead_code)]
    pub fn cut(&mut self) {
        self.clipboard = self.get_selected_text();
        self.delete_selection();
    }

    /// 붙여넣기
    pub fn paste(&mut self) {
        if !self.clipboard.is_empty() {
            let text = self.clipboard.clone();
            self.insert_str(&text);
        }
    }

    /// 전체 선택
    pub fn select_all(&mut self) {
        if !self.lines.is_empty() {
            let last_line = self.lines.len() - 1;
            let last_col = self.lines[last_line].chars().count();
            self.selection = Some(Selection {
                start_line: 0,
                start_col: 0,
                end_line: last_line,
                end_col: last_col,
            });
            self.cursor_line = last_line;
            self.cursor_col = last_col;
        }
    }

    /// 줄 복제
    pub fn duplicate_line(&mut self) {
        let line_content = self.lines[self.cursor_line].clone();
        self.lines.insert(self.cursor_line + 1, line_content.clone());
        self.cursor_line += 1;

        self.push_undo(EditAction::InsertLine {
            line: self.cursor_line,
            content: line_content,
        });
        self.update_scroll();
    }

    /// 줄 삭제
    pub fn delete_line(&mut self) {
        if self.lines.len() > 1 {
            let content = self.lines.remove(self.cursor_line);

            self.push_undo(EditAction::DeleteLine {
                line: self.cursor_line,
                content,
            });

            if self.cursor_line >= self.lines.len() {
                self.cursor_line = self.lines.len() - 1;
            }
            self.cursor_col = self.cursor_col.min(self.lines[self.cursor_line].chars().count());
            self.update_scroll();
        }
    }

    /// 줄 위로 이동
    pub fn move_line_up(&mut self) {
        if self.cursor_line > 0 {
            self.lines.swap(self.cursor_line, self.cursor_line - 1);
            self.cursor_line -= 1;
            self.modified = true;
            self.update_scroll();
        }
    }

    /// 줄 아래로 이동
    pub fn move_line_down(&mut self) {
        if self.cursor_line + 1 < self.lines.len() {
            self.lines.swap(self.cursor_line, self.cursor_line + 1);
            self.cursor_line += 1;
            self.modified = true;
            self.update_scroll();
        }
    }

    /// 커서 이동
    pub fn move_cursor(&mut self, line_delta: i32, col_delta: i32, extend_selection: bool) {
        if extend_selection {
            if self.selection.is_none() {
                self.selection = Some(Selection::new(self.cursor_line, self.cursor_col));
            }
        } else {
            self.selection = None;
        }

        // 줄 이동
        let new_line = (self.cursor_line as i32 + line_delta)
            .max(0)
            .min(self.lines.len().saturating_sub(1) as i32) as usize;

        if new_line != self.cursor_line {
            self.cursor_line = new_line;
            let line_len = self.lines[self.cursor_line].chars().count();
            self.cursor_col = self.cursor_col.min(line_len);
        }

        // 열 이동
        if col_delta != 0 {
            let line_len = self.lines[self.cursor_line].chars().count();
            let new_col = (self.cursor_col as i32 + col_delta).max(0) as usize;

            if new_col > line_len && col_delta > 0 && self.cursor_line + 1 < self.lines.len() {
                // 다음 줄로 이동
                self.cursor_line += 1;
                self.cursor_col = 0;
            } else if new_col > self.cursor_col && col_delta < 0 && self.cursor_line > 0 {
                // 이전 줄 끝으로 이동
                self.cursor_line -= 1;
                self.cursor_col = self.lines[self.cursor_line].chars().count();
            } else {
                self.cursor_col = new_col.min(line_len);
            }
        }

        // 선택 영역 업데이트
        if let Some(ref mut sel) = self.selection {
            sel.end_line = self.cursor_line;
            sel.end_col = self.cursor_col;
        }

        self.update_scroll();
        self.find_matching_bracket();
    }

    /// 줄 시작으로
    pub fn move_to_line_start(&mut self, extend_selection: bool) {
        if extend_selection {
            if self.selection.is_none() {
                self.selection = Some(Selection::new(self.cursor_line, self.cursor_col));
            }
        } else {
            self.selection = None;
        }

        // 첫 번째 비공백 문자로 이동, 이미 거기 있으면 줄 시작으로
        let line = &self.lines[self.cursor_line];
        let first_non_ws = line.chars().position(|c| !c.is_whitespace()).unwrap_or(0);

        if self.cursor_col == first_non_ws || self.cursor_col == 0 {
            self.cursor_col = if self.cursor_col == 0 { first_non_ws } else { 0 };
        } else {
            self.cursor_col = first_non_ws;
        }

        if let Some(ref mut sel) = self.selection {
            sel.end_col = self.cursor_col;
        }
    }

    /// 줄 끝으로
    pub fn move_to_line_end(&mut self, extend_selection: bool) {
        if extend_selection {
            if self.selection.is_none() {
                self.selection = Some(Selection::new(self.cursor_line, self.cursor_col));
            }
        } else {
            self.selection = None;
        }

        self.cursor_col = self.lines[self.cursor_line].chars().count();

        if let Some(ref mut sel) = self.selection {
            sel.end_col = self.cursor_col;
        }
    }

    /// 스크롤 업데이트
    pub fn update_scroll(&mut self) {
        let visible_height = 20;
        if self.cursor_line < self.scroll {
            self.scroll = self.cursor_line;
        } else if self.cursor_line >= self.scroll + visible_height {
            self.scroll = self.cursor_line - visible_height + 1;
        }

        // 수평 스크롤
        let visible_width = 80;
        if self.cursor_col < self.horizontal_scroll {
            self.horizontal_scroll = self.cursor_col;
        } else if self.cursor_col >= self.horizontal_scroll + visible_width {
            self.horizontal_scroll = self.cursor_col - visible_width + 1;
        }
    }

    /// 괄호 매칭 찾기
    fn find_matching_bracket(&mut self) {
        self.matching_bracket = None;

        if self.cursor_line >= self.lines.len() {
            return;
        }

        let line = &self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();

        if self.cursor_col >= chars.len() {
            return;
        }

        let current_char = chars[self.cursor_col];
        let (opening, closing, forward) = match current_char {
            '(' => ('(', ')', true),
            ')' => ('(', ')', false),
            '[' => ('[', ']', true),
            ']' => ('[', ']', false),
            '{' => ('{', '}', true),
            '}' => ('{', '}', false),
            '<' => ('<', '>', true),
            '>' => ('<', '>', false),
            _ => return,
        };

        let mut depth = 1;

        if forward {
            // 앞으로 검색
            let mut line_idx = self.cursor_line;
            let mut col_idx = self.cursor_col + 1;

            while line_idx < self.lines.len() {
                let line_chars: Vec<char> = self.lines[line_idx].chars().collect();
                while col_idx < line_chars.len() {
                    if line_chars[col_idx] == closing {
                        depth -= 1;
                        if depth == 0 {
                            self.matching_bracket = Some((line_idx, col_idx));
                            return;
                        }
                    } else if line_chars[col_idx] == opening {
                        depth += 1;
                    }
                    col_idx += 1;
                }
                line_idx += 1;
                col_idx = 0;
            }
        } else {
            // 뒤로 검색
            let mut line_idx = self.cursor_line;
            let mut col_idx = self.cursor_col.saturating_sub(1);

            loop {
                let line_chars: Vec<char> = self.lines[line_idx].chars().collect();
                loop {
                    if col_idx < line_chars.len() {
                        if line_chars[col_idx] == opening {
                            depth -= 1;
                            if depth == 0 {
                                self.matching_bracket = Some((line_idx, col_idx));
                                return;
                            }
                        } else if line_chars[col_idx] == closing {
                            depth += 1;
                        }
                    }
                    if col_idx == 0 {
                        break;
                    }
                    col_idx -= 1;
                }
                if line_idx == 0 {
                    break;
                }
                line_idx -= 1;
                col_idx = self.lines[line_idx].chars().count().saturating_sub(1);
            }
        }
    }

    /// 검색 수행
    pub fn perform_find(&mut self) {
        self.match_positions.clear();

        if self.find_term.is_empty() {
            return;
        }

        let pattern = if self.find_options.use_regex {
            self.find_term.clone()
        } else {
            regex::escape(&self.find_term)
        };

        let pattern = if self.find_options.whole_word {
            format!(r"\b{}\b", pattern)
        } else {
            pattern
        };

        let regex = if self.find_options.case_sensitive {
            Regex::new(&pattern)
        } else {
            Regex::new(&format!("(?i){}", pattern))
        };

        if let Ok(re) = regex {
            for (line_idx, line) in self.lines.iter().enumerate() {
                for mat in re.find_iter(line) {
                    self.match_positions.push((line_idx, mat.start(), mat.end()));
                }
            }
        }

        self.current_match = 0;
        self.goto_current_match();
    }

    /// 현재 매치로 이동
    fn goto_current_match(&mut self) {
        if !self.match_positions.is_empty() && self.current_match < self.match_positions.len() {
            let (line, start, end) = self.match_positions[self.current_match];
            self.cursor_line = line;
            self.cursor_col = start;
            self.selection = Some(Selection {
                start_line: line,
                start_col: start,
                end_line: line,
                end_col: end,
            });
            self.update_scroll();
        }
    }

    /// 다음 매치
    pub fn find_next(&mut self) {
        if !self.match_positions.is_empty() {
            self.current_match = (self.current_match + 1) % self.match_positions.len();
            self.goto_current_match();
        }
    }

    /// 이전 매치
    pub fn find_prev(&mut self) {
        if !self.match_positions.is_empty() {
            self.current_match = if self.current_match == 0 {
                self.match_positions.len() - 1
            } else {
                self.current_match - 1
            };
            self.goto_current_match();
        }
    }

    /// 바꾸기
    pub fn replace_current(&mut self) {
        if self.match_positions.is_empty() || self.current_match >= self.match_positions.len() {
            return;
        }

        let (line, start, end) = self.match_positions[self.current_match];

        // 선택 영역이 현재 매치와 일치하는지 확인
        let sel = self.selection.as_ref();
        if sel.is_some_and(|s| {
            let (sl, sc, el, ec) = s.normalized();
            sl == line && sc == start && el == line && ec == end
        }) {
            // 바꾸기 실행
            let line_content = &self.lines[line];
            let chars: Vec<char> = line_content.chars().collect();
            let new_line: String = chars[..start]
                .iter()
                .chain(self.replace_input.chars().collect::<Vec<_>>().iter())
                .chain(chars[end..].iter())
                .collect();

            let old_content = self.lines[line].clone();
            self.lines[line] = new_line;

            self.push_undo(EditAction::Replace {
                line,
                old_content,
                new_content: self.lines[line].clone(),
            });

            self.selection = None;
            self.perform_find();
            self.find_next();
        }
    }

    /// 모두 바꾸기
    pub fn replace_all(&mut self) {
        if self.find_term.is_empty() {
            return;
        }

        let pattern = if self.find_options.use_regex {
            self.find_term.clone()
        } else {
            regex::escape(&self.find_term)
        };

        let pattern = if self.find_options.whole_word {
            format!(r"\b{}\b", pattern)
        } else {
            pattern
        };

        let regex = if self.find_options.case_sensitive {
            Regex::new(&pattern)
        } else {
            Regex::new(&format!("(?i){}", pattern))
        };

        if let Ok(re) = regex {
            let mut actions = Vec::new();

            for (line_idx, line) in self.lines.iter_mut().enumerate() {
                let old_content = line.clone();
                let new_content = re.replace_all(line, self.replace_input.as_str()).to_string();

                if old_content != new_content {
                    actions.push(EditAction::Replace {
                        line: line_idx,
                        old_content,
                        new_content: new_content.clone(),
                    });
                    *line = new_content;
                }
            }

            if !actions.is_empty() {
                self.push_undo(EditAction::Batch { actions });
            }

            self.selection = None;
            self.perform_find();
        }
    }

    /// 줄 번호로 이동
    pub fn goto_line(&mut self, line_str: &str) {
        if let Ok(line_num) = line_str.parse::<usize>() {
            if line_num > 0 && line_num <= self.lines.len() {
                self.cursor_line = line_num - 1;
                self.cursor_col = 0;
                self.selection = None;
                self.update_scroll();
            }
        }
    }
}

pub fn draw(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let state = match &app.editor_state {
        Some(s) => s,
        None => return,
    };

    let border_color = if state.modified {
        theme.warning
    } else {
        theme.border_active
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 3 {
        return;
    }

    // Header
    let file_name = state
        .file_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "New File".to_string());

    let header = Line::from(vec![
        Span::styled(
            format!(
                " {}{} ",
                if state.modified { "*" } else { "" },
                file_name
            ),
            theme.header_style(),
        ),
        Span::styled(
            format!("[{}] ", state.language.name()),
            theme.dim_style(),
        ),
        Span::styled(
            format!("Ln {}, Col {} ", state.cursor_line + 1, state.cursor_col + 1),
            theme.dim_style(),
        ),
        if !state.undo_stack.is_empty() {
            Span::styled(
                format!("[Undo:{}] ", state.undo_stack.len()),
                Style::default().fg(theme.info),
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
    let _content_width = (inner.width - 6) as usize;

    // 선택 영역 정규화
    let selection = state.selection.as_ref().map(|s| s.normalized());

    // 하이라이터
    let mut highlighter = state.highlighter.clone();
    if let Some(ref mut hl) = highlighter {
        hl.reset();
        for line in state.lines.iter().take(state.scroll) {
            hl.tokenize_line(line);
        }
    }

    for (i, line) in state.lines.iter().skip(state.scroll).take(content_height).enumerate() {
        let line_num = state.scroll + i;
        let is_cursor_line = line_num == state.cursor_line;

        // 줄 번호
        let line_num_style = if is_cursor_line {
            Style::default()
                .fg(theme.text_header)
                .add_modifier(Modifier::BOLD)
        } else {
            theme.dim_style()
        };

        let line_num_span = Span::styled(format!("{:4} ", line_num + 1), line_num_style);

        // 라인 렌더링
        let content_spans = render_editor_line(
            line,
            line_num,
            state,
            &selection,
            &mut highlighter,
            theme,
            is_cursor_line,
        );

        let mut spans = vec![line_num_span];
        spans.extend(content_spans);

        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(inner.x, inner.y + 1 + i as u16, inner.width, 1),
        );
    }

    // 스크롤바
    let total_lines = state.lines.len();
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

    // Footer
    let footer_y = inner.y + inner.height - 1;

    match state.find_mode {
        FindReplaceMode::None => {
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
            } else {
                let mut footer_spans = vec![];

                if state.modified {
                    footer_spans.push(Span::styled("Modified ", theme.warning_style()));
                }

                // Ctrl 단축키: ^키 강조 + 나머지 dim
                let shortcuts = [
                    ("^S", "ave "),
                    ("^Q", "uit "),
                    ("^X", "discard "),
                    ("^Z", "undo "),
                    ("^Y", "redo "),
                    ("^F", "ind "),
                    ("^G", "oto "),
                    ("^D", "up"),
                ];

                for (key, rest) in shortcuts {
                    footer_spans.push(Span::styled(key, theme.header_style()));
                    footer_spans.push(Span::styled(rest, theme.dim_style()));
                }

                let footer = Line::from(footer_spans);
                frame.render_widget(
                    Paragraph::new(footer).style(theme.status_bar_style()),
                    Rect::new(inner.x, footer_y, inner.width, 1),
                );
            }
        }
        FindReplaceMode::Find | FindReplaceMode::Replace => {
            let find_opts = format!(
                "[{}{}{}]",
                if state.find_options.case_sensitive { "Aa" } else { "aa" },
                if state.find_options.use_regex { " Re" } else { "" },
                if state.find_options.whole_word { " W" } else { "" }
            );

            let match_info = if !state.match_positions.is_empty() {
                format!(
                    " {}/{} ",
                    state.current_match + 1,
                    state.match_positions.len()
                )
            } else {
                " 0/0 ".to_string()
            };

            let mut spans = vec![
                Span::styled("Find: ", theme.header_style()),
                Span::styled(
                    &state.find_input,
                    if state.input_focus == 0 {
                        theme.normal_style()
                    } else {
                        theme.dim_style()
                    },
                ),
            ];

            if state.input_focus == 0 {
                spans.push(Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)));
            }

            if state.find_mode == FindReplaceMode::Replace {
                spans.push(Span::styled(" Replace: ", theme.header_style()));
                spans.push(Span::styled(
                    &state.replace_input,
                    if state.input_focus == 1 {
                        theme.normal_style()
                    } else {
                        theme.dim_style()
                    },
                ));
                if state.input_focus == 1 {
                    spans.push(Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)));
                }
            }

            spans.push(Span::styled(match_info, theme.success_style()));
            spans.push(Span::styled(find_opts, theme.dim_style()));

            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(theme.status_bar_style()),
                Rect::new(inner.x, footer_y, inner.width, 1),
            );
        }
    }
}

/// 편집기 라인 렌더링
fn render_editor_line(
    line: &str,
    line_num: usize,
    state: &EditorState,
    selection: &Option<(usize, usize, usize, usize)>,
    highlighter: &mut Option<SyntaxHighlighter>,
    theme: &Theme,
    is_cursor_line: bool,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();

    // 선택 영역이 이 줄에 있는지 확인
    let line_selection = if let Some((sl, sc, el, ec)) = selection {
        if *sl <= line_num && line_num <= *el {
            let start = if line_num == *sl { *sc } else { 0 };
            let end = if line_num == *el { *ec } else { chars.len() };
            Some((start, end))
        } else {
            None
        }
    } else {
        None
    };

    // 문법 강조 토큰 가져오기
    let tokens = if let Some(ref mut hl) = highlighter {
        hl.tokenize_line(line)
    } else {
        vec![]
    };

    // 토큰이 있으면 토큰 기반 렌더링
    if !tokens.is_empty() {
        let mut char_idx = 0;

        for token in tokens {
            let token_chars: Vec<char> = token.text.chars().collect();
            let token_start = char_idx;
            let token_end = char_idx + token_chars.len();

            for (i, c) in token_chars.iter().enumerate() {
                let pos = token_start + i;
                let mut style = if let Some(ref mut hl) = highlighter {
                    hl.style_for(token.token_type)
                } else {
                    theme.normal_style()
                };

                // 선택 영역 하이라이트
                if let Some((sel_start, sel_end)) = line_selection {
                    if pos >= sel_start && pos < sel_end {
                        style = style.bg(theme.bg_selected);
                    }
                }

                // 검색 매치 하이라이트
                for (ml, ms, me) in &state.match_positions {
                    if *ml == line_num && pos >= *ms && pos < *me {
                        style = style.bg(theme.warning);
                    }
                }

                // 매칭 괄호 하이라이트
                if let Some((bl, bc)) = state.matching_bracket {
                    if bl == line_num && bc == pos {
                        style = style.bg(theme.info).fg(Color::Black);
                    }
                }

                // 커서 하이라이트
                if is_cursor_line && pos == state.cursor_col && state.selection.is_none() {
                    style = theme.selected_style();
                }

                spans.push(Span::styled(c.to_string(), style));
            }

            char_idx = token_end;
        }

        // 커서가 줄 끝에 있는 경우
        if is_cursor_line && state.cursor_col >= chars.len() && state.selection.is_none() {
            spans.push(Span::styled(" ", theme.selected_style()));
        }
    } else {
        // 토큰 없이 문자 단위 렌더링
        for (i, c) in chars.iter().enumerate() {
            let mut style = theme.normal_style();

            // 선택 영역
            if let Some((sel_start, sel_end)) = line_selection {
                if i >= sel_start && i < sel_end {
                    style = style.bg(theme.bg_selected);
                }
            }

            // 검색 매치
            for (ml, ms, me) in &state.match_positions {
                if *ml == line_num && i >= *ms && i < *me {
                    style = style.bg(theme.warning);
                }
            }

            // 매칭 괄호
            if let Some((bl, bc)) = state.matching_bracket {
                if bl == line_num && bc == i {
                    style = style.bg(theme.info).fg(Color::Black);
                }
            }

            // 커서
            if is_cursor_line && i == state.cursor_col && state.selection.is_none() {
                style = theme.selected_style();
            }

            spans.push(Span::styled(c.to_string(), style));
        }

        // 커서가 줄 끝에 있는 경우
        if is_cursor_line && state.cursor_col >= chars.len() && state.selection.is_none() {
            spans.push(Span::styled(" ", theme.selected_style()));
        }
    }

    if spans.is_empty() {
        // 빈 줄에 커서 표시
        if is_cursor_line && state.selection.is_none() {
            spans.push(Span::styled(" ", theme.selected_style()));
        } else {
            spans.push(Span::styled(" ", theme.normal_style()));
        }
    }

    spans
}

pub fn handle_input(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    let state = match &mut app.editor_state {
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

    // Find/Replace 모드
    if state.find_mode != FindReplaceMode::None {
        match code {
            KeyCode::Esc => {
                state.find_mode = FindReplaceMode::None;
                state.selection = None;
            }
            KeyCode::Tab if state.find_mode == FindReplaceMode::Replace => {
                state.input_focus = 1 - state.input_focus;
            }
            KeyCode::Enter => {
                if state.input_focus == 0 {
                    state.find_term = state.find_input.clone();
                    state.perform_find();
                } else if state.find_mode == FindReplaceMode::Replace {
                    state.replace_current();
                }
            }
            KeyCode::Backspace => {
                if state.input_focus == 0 {
                    state.find_input.pop();
                } else {
                    state.replace_input.pop();
                }
            }
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                state.find_options.case_sensitive = !state.find_options.case_sensitive;
            }
            KeyCode::Char('r') if modifiers.contains(KeyModifiers::CONTROL) => {
                state.find_options.use_regex = !state.find_options.use_regex;
            }
            KeyCode::Char('w') if modifiers.contains(KeyModifiers::CONTROL) => {
                state.find_options.whole_word = !state.find_options.whole_word;
            }
            KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => {
                // 모두 바꾸기
                if state.find_mode == FindReplaceMode::Replace {
                    state.find_term = state.find_input.clone();
                    state.replace_all();
                }
            }
            KeyCode::Char('n') if modifiers.contains(KeyModifiers::CONTROL) => {
                state.find_next();
            }
            KeyCode::Char('p') if modifiers.contains(KeyModifiers::CONTROL) => {
                state.find_prev();
            }
            KeyCode::Char(c) => {
                if state.input_focus == 0 {
                    state.find_input.push(c);
                } else {
                    state.replace_input.push(c);
                }
            }
            KeyCode::Down => {
                state.find_next();
            }
            KeyCode::Up => {
                state.find_prev();
            }
            _ => {}
        }
        return;
    }

    // Ctrl 조합
    if modifiers.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('s') => {
                match state.save_file() {
                    Ok(_) => {
                        app.show_message("File saved!");
                        app.refresh_panels();
                    }
                    Err(e) => {
                        app.show_message(&format!("Save error: {}", e));
                    }
                }
                return;
            }
            KeyCode::Char('q') => {
                if !state.modified {
                    app.current_screen = Screen::DualPanel;
                } else {
                    app.show_message("Unsaved changes! ^S to save, ^X to discard");
                }
                return;
            }
            KeyCode::Char('x') => {
                // Discard changes - go back to previous screen (viewer) or dual panel
                if let Some(Screen::FileViewer) = app.previous_screen {
                    // 에디터의 커서 위치를 뷰어에 전달
                    if let Some(ref mut viewer) = app.viewer_state {
                        viewer.scroll = state.scroll;
                    }
                    app.previous_screen = None;
                    app.current_screen = Screen::FileViewer;
                } else {
                    app.current_screen = Screen::DualPanel;
                }
                return;
            }
            KeyCode::Char('z') => {
                state.undo();
                return;
            }
            KeyCode::Char('y') => {
                state.redo();
                return;
            }
            KeyCode::Char('a') => {
                state.select_all();
                return;
            }
            KeyCode::Char('c') => {
                state.copy();
                return;
            }
            KeyCode::Char('v') => {
                state.paste();
                return;
            }
            KeyCode::Char('d') => {
                state.duplicate_line();
                return;
            }
            KeyCode::Char('k') => {
                state.delete_line();
                return;
            }
            KeyCode::Char('f') => {
                state.find_mode = FindReplaceMode::Find;
                state.input_focus = 0;
                return;
            }
            KeyCode::Char('h') => {
                state.find_mode = FindReplaceMode::Replace;
                state.input_focus = 0;
                return;
            }
            KeyCode::Char('g') => {
                state.goto_mode = true;
                state.goto_input.clear();
                return;
            }
            KeyCode::Home => {
                // 파일 시작으로
                state.cursor_line = 0;
                state.cursor_col = 0;
                state.selection = None;
                state.update_scroll();
                return;
            }
            KeyCode::End => {
                // 파일 끝으로
                state.cursor_line = state.lines.len().saturating_sub(1);
                state.cursor_col = state.lines[state.cursor_line].chars().count();
                state.selection = None;
                state.update_scroll();
                return;
            }
            _ => {}
        }
    }

    // Alt 조합
    if modifiers.contains(KeyModifiers::ALT) {
        match code {
            KeyCode::Up => {
                state.move_line_up();
                return;
            }
            KeyCode::Down => {
                state.move_line_down();
                return;
            }
            _ => {}
        }
    }

    // Shift 선택
    let extend_selection = modifiers.contains(KeyModifiers::SHIFT);

    match code {
        KeyCode::Esc => {
            if state.selection.is_some() {
                state.selection = None;
            } else if state.modified {
                app.show_message("Unsaved changes! ^S to save, ^X to discard");
            } else {
                // Go back to previous screen (viewer) or dual panel
                if let Some(Screen::FileViewer) = app.previous_screen {
                    // 에디터의 스크롤 위치를 뷰어에 전달
                    if let Some(ref mut viewer) = app.viewer_state {
                        viewer.scroll = state.scroll;
                    }
                    app.previous_screen = None;
                    app.current_screen = Screen::FileViewer;
                } else {
                    app.current_screen = Screen::DualPanel;
                }
            }
        }
        KeyCode::Up => {
            state.move_cursor(-1, 0, extend_selection);
        }
        KeyCode::Down => {
            state.move_cursor(1, 0, extend_selection);
        }
        KeyCode::Left => {
            state.move_cursor(0, -1, extend_selection);
        }
        KeyCode::Right => {
            state.move_cursor(0, 1, extend_selection);
        }
        KeyCode::Home => {
            state.move_to_line_start(extend_selection);
        }
        KeyCode::End => {
            state.move_to_line_end(extend_selection);
        }
        KeyCode::PageUp => {
            state.move_cursor(-20, 0, extend_selection);
        }
        KeyCode::PageDown => {
            state.move_cursor(20, 0, extend_selection);
        }
        KeyCode::Backspace => {
            state.delete_backward();
        }
        KeyCode::Delete => {
            state.delete_forward();
        }
        KeyCode::Enter => {
            state.insert_newline();
        }
        KeyCode::Tab => {
            state.insert_tab();
        }
        KeyCode::Char(c) => {
            if !modifiers.contains(KeyModifiers::CONTROL) && !modifiers.contains(KeyModifiers::ALT)
            {
                state.insert_char(c);
            }
        }
        _ => {}
    }
}
