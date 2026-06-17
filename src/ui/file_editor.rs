use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};
use regex::Regex;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_width::UnicodeWidthChar;

use super::{
    app::{App, Screen},
    syntax::{Language, SyntaxHighlighter},
    theme::Theme,
};
use crate::keybindings::EditorAction;

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
        line_ending: String,
    },
    DeleteLine {
        line: usize,
        content: String,
        line_ending: String,
    },
    MergeLine {
        line: usize,
        col: usize,
        line_ending: String,
    },
    SplitLine {
        line: usize,
        col: usize,
        line_ending: String,
    },
    SetLineEnding {
        line: usize,
        old_line_ending: String,
        new_line_ending: String,
    },
    Replace {
        line: usize,
        old_content: String,
        new_content: String,
    },
    SwapLines {
        line1: usize,
        line2: usize,
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

#[derive(Debug)]
pub struct RemoteEditOrigin {
    pub panel_index: usize,
    pub remote_path: String,
}

impl Selection {
    /// 블록 커서 선택: 현재 커서 위치의 문자를 포함하기 위해 end_col = col + 1
    pub fn new(line: usize, col: usize) -> Self {
        Self {
            start_line: line,
            start_col: col,
            end_line: line,
            end_col: col + 1,
        }
    }

    /// 정규화된 선택 영역 (시작이 항상 끝보다 앞)
    /// 블록 커서 선택: start_col = anchor, end_col = cursor + 1
    pub fn normalized(&self) -> (usize, usize, usize, usize) {
        if self.start_line == self.end_line {
            // 단일 줄: 블록 커서 선택
            // anchor와 cursor 사이의 모든 문자 선택 (양쪽 끝 포함)
            let anchor = self.start_col;
            let cursor = self.end_col.saturating_sub(1);
            let min_col = anchor.min(cursor);
            let max_col = anchor.max(cursor) + 1;
            (self.start_line, min_col, self.end_line, max_col)
        } else if self.start_line < self.end_line {
            // 정방향 (아래로 선택): anchor -> cursor
            (self.start_line, self.start_col, self.end_line, self.end_col)
        } else {
            // 역방향 (위로 선택): cursor <- anchor
            // cursor 위치 문자와 anchor 위치 문자 모두 포함
            (
                self.end_line,
                self.end_col.saturating_sub(1),
                self.start_line,
                self.start_col + 1,
            )
        }
    }
}

#[cfg(unix)]
struct UnixSaveMetadata {
    permissions: fs::Permissions,
    uid: u32,
    gid: u32,
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
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

/// Default maximum memory for undo/redo stacks (50MB)
const DEFAULT_MAX_UNDO_MEMORY: usize = 50 * 1024 * 1024;

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
    pub original_lines: Vec<String>,
    original_line_endings: Vec<String>,
    line_ending: String,
    trailing_newline: bool,
    line_endings: Vec<String>,
    pub(crate) remote_dirty: bool,
    pub(crate) remote_save_generation: u64,

    // Undo/Redo
    pub undo_stack: VecDeque<EditAction>,
    pub redo_stack: VecDeque<EditAction>,
    pub max_undo_size: usize,

    // Memory tracking for undo/redo
    undo_memory_usage: usize,
    redo_memory_usage: usize,
    max_undo_memory: usize,

    // 선택
    pub selection: Option<Selection>,
    pub clipboard: String,

    // 찾기/바꾸기
    pub find_mode: FindReplaceMode,
    pub find_input: String,
    pub find_cursor_pos: usize,
    pub replace_input: String,
    pub replace_cursor_pos: usize,
    pub find_term: String,
    pub find_options: FindReplaceOptions,
    pub match_positions: Vec<(usize, usize, usize)>,
    pub current_match: usize,
    pub input_focus: usize,         // 0: find, 1: replace
    pub find_error: Option<String>, // 정규식 에러 메시지

    // Goto
    pub goto_mode: bool,
    pub goto_input: String,

    // 문법 강조
    pub language: Language,
    pub highlighter: Option<SyntaxHighlighter>,
    pub syntax_colors: crate::ui::theme::SyntaxColors,

    // 설정
    pub auto_indent: bool,
    pub tab_size: usize,
    pub use_tabs: bool,
    #[allow(dead_code)]
    pub show_whitespace: bool,

    // 괄호 매칭
    pub matching_bracket: Option<(usize, usize)>,

    // 다중 커서 (Ctrl+D)
    pub cursors: Vec<(usize, usize)>, // (line, col) 추가 커서들
    pub last_word_selection: Option<String>, // 마지막 선택된 단어 (Ctrl+D용)

    // 저장되지 않은 변경사항 종료 확인
    pub exit_confirm_open: bool,
    pub exit_confirm_selected: usize, // 0: Save, 1: Don't Save, 2: Cancel

    // Word wrap 모드
    pub word_wrap: bool,
    wrap_scroll_offset: usize,

    // 화면 크기 (렌더링 시 업데이트)
    pub visible_height: usize,
    pub visible_width: usize,

    // 상태 메시지 (일시적으로 표시)
    pub message: Option<String>,
    pub message_timer: u8,

    // 원격 파일 편집 원본 정보
    pub remote_origin: Option<RemoteEditOrigin>,
}

impl EditorState {
    /// Estimate memory size of an EditAction
    fn estimate_action_size(action: &EditAction) -> usize {
        match action {
            EditAction::Insert { text, .. } => text.len() + 32,
            EditAction::Delete { text, .. } => text.len() + 32,
            EditAction::InsertLine {
                content,
                line_ending,
                ..
            } => content.len() + line_ending.len() + 24,
            EditAction::DeleteLine {
                content,
                line_ending,
                ..
            } => content.len() + line_ending.len() + 24,
            EditAction::MergeLine { line_ending, .. } => line_ending.len() + 24,
            EditAction::SplitLine { line_ending, .. } => line_ending.len() + 24,
            EditAction::SetLineEnding {
                old_line_ending,
                new_line_ending,
                ..
            } => old_line_ending.len() + new_line_ending.len() + 24,
            EditAction::Replace {
                old_content,
                new_content,
                ..
            } => old_content.len() + new_content.len() + 32,
            EditAction::SwapLines { .. } => 24,
            EditAction::Batch { actions } => {
                actions
                    .iter()
                    .map(Self::estimate_action_size)
                    .sum::<usize>()
                    + 24
            }
        }
    }

    pub fn new() -> Self {
        Self {
            file_path: PathBuf::new(),
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            scroll: 0,
            horizontal_scroll: 0,
            modified: false,
            original_lines: vec![String::new()],
            original_line_endings: vec![String::new()],
            line_ending: "\n".to_string(),
            trailing_newline: false,
            line_endings: vec![String::new()],
            remote_dirty: false,
            remote_save_generation: 0,
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            max_undo_size: 1000,
            undo_memory_usage: 0,
            redo_memory_usage: 0,
            max_undo_memory: DEFAULT_MAX_UNDO_MEMORY,
            selection: None,
            clipboard: String::new(),
            find_mode: FindReplaceMode::None,
            find_input: String::new(),
            find_cursor_pos: 0,
            replace_input: String::new(),
            replace_cursor_pos: 0,
            find_term: String::new(),
            find_options: FindReplaceOptions::default(),
            match_positions: Vec::new(),
            current_match: 0,
            input_focus: 0,
            find_error: None,
            goto_mode: false,
            goto_input: String::new(),
            language: Language::Plain,
            highlighter: None,
            syntax_colors: crate::ui::theme::Theme::default().syntax,
            auto_indent: true,
            tab_size: 4,
            use_tabs: false,
            show_whitespace: false,
            matching_bracket: None,
            cursors: Vec::new(),
            last_word_selection: None,
            exit_confirm_open: false,
            exit_confirm_selected: 2,
            word_wrap: false,
            wrap_scroll_offset: 0,
            visible_height: 20, // 기본값, 렌더링 시 업데이트됨
            visible_width: 80,  // 기본값, 렌더링 시 업데이트됨
            message: None,
            message_timer: 0,
            remote_origin: None,
        }
    }

    /// 메시지 설정 (지정된 프레임 수 동안 표시)
    pub fn set_message(&mut self, msg: impl Into<String>, duration: u8) {
        self.message = Some(msg.into());
        self.message_timer = duration;
    }

    /// 메시지 클리어
    pub fn clear_message(&mut self) {
        self.message = None;
        self.message_timer = 0;
    }

    /// 테마의 syntax colors 설정
    pub fn set_syntax_colors(&mut self, colors: crate::ui::theme::SyntaxColors) {
        self.syntax_colors = colors;
        // 하이라이터가 있으면 새 색상으로 재생성
        if self.highlighter.is_some() {
            self.highlighter = Some(SyntaxHighlighter::new(self.language, self.syntax_colors));
        }
    }

    /// 버퍼 위치(char index) -> Visual Column
    /// TAB 문자는 tab_size 단위로 정렬된 위치까지 확장됨
    /// Wide characters (한글 등)는 2칸으로 계산됨
    pub fn char_to_visual(&self, line: &str, char_idx: usize) -> usize {
        let mut visual_col = 0;
        for (i, c) in line.chars().enumerate() {
            if i >= char_idx {
                break;
            }
            if c == '\t' {
                // TAB은 다음 tab_size 배수 위치까지 확장
                visual_col = (visual_col / self.tab_size + 1) * self.tab_size;
            } else {
                visual_col += c.width().unwrap_or(1);
            }
        }
        visual_col
    }

    /// Visual Column -> 버퍼 위치(char index)
    /// 주어진 visual column에 해당하는 문자 인덱스를 반환
    /// Wide characters (한글 등)는 2칸으로 계산됨
    pub fn visual_to_char(&self, line: &str, visual_col: usize) -> usize {
        let mut current_visual = 0;
        for (i, c) in line.chars().enumerate() {
            if current_visual >= visual_col {
                return i;
            }
            if c == '\t' {
                current_visual = (current_visual / self.tab_size + 1) * self.tab_size;
            } else {
                current_visual += c.width().unwrap_or(1);
            }
        }
        line.chars().count()
    }

    /// 현재 커서의 visual column 반환
    pub fn cursor_visual_col(&self) -> usize {
        if self.cursor_line < self.lines.len() {
            self.char_to_visual(&self.lines[self.cursor_line], self.cursor_col)
        } else {
            self.cursor_col
        }
    }

    /// TAB을 visual column 기반으로 확장한 문자열 생성
    /// 각 TAB은 현재 visual column 위치에서 다음 tab_size 배수까지 스페이스로 확장됨
    /// Wide characters (한글 등)는 2칸으로 계산됨
    pub fn expand_tabs_visual(&self, line: &str) -> String {
        let mut result = String::new();
        let mut visual_col = 0;
        for c in line.chars() {
            if c == '\t' {
                let next_tab_stop = (visual_col / self.tab_size + 1) * self.tab_size;
                let spaces = next_tab_stop - visual_col;
                result.push_str(&" ".repeat(spaces));
                visual_col = next_tab_stop;
            } else {
                result.push(c);
                visual_col += c.width().unwrap_or(1);
            }
        }
        result
    }

    /// 원본 문자 인덱스 -> 확장된 visual 인덱스 매핑 생성
    /// 반환값: (확장된 문자열, 원본 인덱스 배열)
    /// 원본 인덱스 배열: expanded_line의 각 visual 위치에 해당하는 원본 char index
    /// Wide characters (한글 등)는 2칸으로 계산됨
    pub fn expand_tabs_with_mapping(&self, line: &str) -> (String, Vec<usize>) {
        let mut result = String::new();
        let mut visual_to_orig: Vec<usize> = Vec::new();
        let mut visual_col = 0;
        for (char_idx, c) in line.chars().enumerate() {
            if c == '\t' {
                let next_tab_stop = (visual_col / self.tab_size + 1) * self.tab_size;
                let spaces = next_tab_stop - visual_col;
                for _ in 0..spaces {
                    result.push(' ');
                    visual_to_orig.push(char_idx);
                }
                visual_col = next_tab_stop;
            } else {
                result.push(c);
                let char_width = c.width().unwrap_or(1);
                for _ in 0..char_width {
                    visual_to_orig.push(char_idx);
                }
                visual_col += char_width;
            }
        }
        (result, visual_to_orig)
    }

    /// Maximum file size for editing (50MB - more restrictive than viewer)
    const MAX_EDIT_FILE_SIZE: u64 = 50 * 1024 * 1024;

    fn detect_line_ending(content: &str) -> &'static str {
        let bytes = content.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\r' => {
                    return if bytes.get(i + 1) == Some(&b'\n') {
                        "\r\n"
                    } else {
                        "\r"
                    };
                }
                b'\n' => return "\n",
                _ => i += 1,
            }
        }
        "\n"
    }

    fn split_content_preserving_format(content: &str) -> (Vec<String>, Vec<String>, String, bool) {
        let mut lines = Vec::new();
        let mut line_endings = Vec::new();
        let mut start = 0usize;
        let bytes = content.as_bytes();
        let mut i = 0usize;

        while i < bytes.len() {
            match bytes[i] {
                b'\r' => {
                    lines.push(content[start..i].to_string());
                    if bytes.get(i + 1) == Some(&b'\n') {
                        line_endings.push("\r\n".to_string());
                        i += 2;
                    } else {
                        line_endings.push("\r".to_string());
                        i += 1;
                    }
                    start = i;
                }
                b'\n' => {
                    lines.push(content[start..i].to_string());
                    line_endings.push("\n".to_string());
                    i += 1;
                    start = i;
                }
                _ => i += 1,
            }
        }

        lines.push(content[start..].to_string());
        line_endings.push(String::new());

        let line_ending = line_endings
            .iter()
            .find(|ending| !ending.is_empty())
            .cloned()
            .unwrap_or_else(|| Self::detect_line_ending(content).to_string());
        let trailing_newline = line_endings
            .last()
            .map(|ending| !ending.is_empty())
            .unwrap_or(false);

        (lines, line_endings, line_ending, trailing_newline)
    }

    fn split_insert_text_preserving_endings(content: &str) -> (Vec<String>, Vec<String>) {
        let mut parts = Vec::new();
        let mut line_endings = Vec::new();
        let mut start = 0usize;
        let bytes = content.as_bytes();
        let mut i = 0usize;

        while i < bytes.len() {
            match bytes[i] {
                b'\r' => {
                    parts.push(content[start..i].to_string());
                    if bytes.get(i + 1) == Some(&b'\n') {
                        line_endings.push("\r\n".to_string());
                        i += 2;
                    } else {
                        line_endings.push("\r".to_string());
                        i += 1;
                    }
                    start = i;
                }
                b'\n' => {
                    parts.push(content[start..i].to_string());
                    line_endings.push("\n".to_string());
                    i += 1;
                    start = i;
                }
                _ => i += 1,
            }
        }

        parts.push(content[start..].to_string());
        (parts, line_endings)
    }

    fn serialize_content(&self) -> String {
        if self.line_endings.len() == self.lines.len() {
            let mut content = String::new();
            for (line, ending) in self.lines.iter().zip(self.line_endings.iter()) {
                content.push_str(line);
                content.push_str(ending);
            }
            return content;
        }

        let mut content = self.lines.join(&self.line_ending);
        if self.trailing_newline {
            content.push_str(&self.line_ending);
        }
        content
    }

    fn default_line_ending(&self) -> String {
        if self.line_ending.is_empty() {
            "\n".to_string()
        } else {
            self.line_ending.clone()
        }
    }

    fn sync_legacy_line_ending_flags(&mut self) {
        if let Some(first) = self.line_endings.iter().find(|ending| !ending.is_empty()) {
            self.line_ending = first.clone();
        }
        self.trailing_newline = self
            .line_endings
            .last()
            .map(|ending| !ending.is_empty())
            .unwrap_or(false);
    }

    fn ensure_line_endings(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }

        if self.line_endings.len() > self.lines.len() {
            self.line_endings.truncate(self.lines.len());
        }

        while self.line_endings.len() < self.lines.len() {
            let ending = if self.line_endings.len() + 1 < self.lines.len() {
                self.default_line_ending()
            } else {
                String::new()
            };
            self.line_endings.push(ending);
        }

        self.sync_legacy_line_ending_flags();
    }

    fn line_ending_at(&self, line: usize) -> String {
        self.line_endings.get(line).cloned().unwrap_or_else(|| {
            if line + 1 < self.lines.len() {
                self.default_line_ending()
            } else {
                String::new()
            }
        })
    }

    fn line_ending_for_clipboard(&self, line: usize) -> String {
        let ending = self.line_ending_at(line);
        if ending.is_empty() {
            self.default_line_ending()
        } else {
            ending
        }
    }

    fn set_line_ending_at(&mut self, line: usize, ending: String) {
        self.ensure_line_endings();
        if line < self.line_endings.len() {
            self.line_endings[line] = ending;
        }
        self.sync_legacy_line_ending_flags();
    }

    fn insert_line_with_ending(&mut self, line: usize, content: String, line_ending: String) {
        self.ensure_line_endings();
        let line = line.min(self.lines.len());
        self.lines.insert(line, content);
        self.line_endings.insert(line, line_ending);
        self.sync_legacy_line_ending_flags();
    }

    fn remove_line_with_ending(&mut self, line: usize) -> Option<(String, String)> {
        self.ensure_line_endings();
        if line >= self.lines.len() {
            return None;
        }

        let was_last = line + 1 == self.lines.len();
        let content = self.lines.remove(line);
        let ending = self.line_endings.remove(line);
        if self.lines.is_empty() {
            self.lines.push(String::new());
            self.line_endings.push(String::new());
        } else if was_last && line > 0 {
            let prev = line - 1;
            if prev < self.line_endings.len() {
                self.line_endings[prev] = ending.clone();
            }
        }
        self.sync_legacy_line_ending_flags();
        Some((content, ending))
    }

    fn create_save_temp_file(actual_path: &Path) -> Result<(PathBuf, fs::File), String> {
        let parent = actual_path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = actual_path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_else(|| "untitled".into());
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        for attempt in 0..1000u32 {
            let temp_path = parent.join(format!(
                ".{}.{}.{}.{}.tmp",
                file_name,
                std::process::id(),
                nonce,
                attempt
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)
            {
                Ok(file) => return Ok((temp_path, file)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("Failed to create temporary file: {}", e)),
            }
        }

        Err("Failed to create a unique temporary file".to_string())
    }

    #[cfg(windows)]
    fn unique_save_sidecar_path(actual_path: &Path, suffix: &str) -> Result<PathBuf, String> {
        let parent = actual_path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = actual_path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_else(|| "untitled".into());
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        for attempt in 0..1000u32 {
            let path = parent.join(format!(
                ".{}.{}.{}.{}.{}",
                file_name,
                std::process::id(),
                nonce,
                attempt,
                suffix
            ));
            if !path.exists() {
                return Ok(path);
            }
        }

        Err(format!("Failed to create a unique {} path", suffix))
    }

    #[cfg(not(windows))]
    fn replace_saved_file(temp_path: &Path, actual_path: &Path) -> Result<(), String> {
        fs::rename(temp_path, actual_path).map_err(|e| format!("Failed to save file: {}", e))
    }

    #[cfg(windows)]
    fn replace_saved_file(temp_path: &Path, actual_path: &Path) -> Result<(), String> {
        if !actual_path.exists() {
            return fs::rename(temp_path, actual_path)
                .map_err(|e| format!("Failed to save file: {}", e));
        }

        let backup_path = Self::unique_save_sidecar_path(actual_path, "bak")?;
        fs::rename(actual_path, &backup_path)
            .map_err(|e| format!("Failed to prepare existing file for replacement: {}", e))?;

        match fs::rename(temp_path, actual_path) {
            Ok(()) => {
                let _ = fs::remove_file(&backup_path);
                Ok(())
            }
            Err(e) => {
                let save_error = e.to_string();
                let rollback_result = if actual_path.exists() {
                    Err("target path already exists after failed replacement".to_string())
                } else {
                    fs::rename(&backup_path, actual_path).map_err(|rollback| rollback.to_string())
                };
                if let Err(rollback_error) = rollback_result {
                    return Err(format!(
                        "Failed to save file: {}. Rollback failed: {}",
                        save_error, rollback_error
                    ));
                }
                Err(format!("Failed to save file: {}", save_error))
            }
        }
    }

    #[cfg(unix)]
    fn capture_unix_save_metadata(path: &Path) -> Option<UnixSaveMetadata> {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata(path).ok()?;
        Some(UnixSaveMetadata {
            permissions: metadata.permissions(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            xattrs: Self::read_linux_xattrs(path),
        })
    }

    #[cfg(unix)]
    fn apply_unix_save_metadata(
        temp_path: &Path,
        temp_file: &fs::File,
        metadata: &UnixSaveMetadata,
    ) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::io::AsRawFd;

        let result = unsafe {
            libc::fchown(
                temp_file.as_raw_fd(),
                metadata.uid as libc::uid_t,
                metadata.gid as libc::gid_t,
            )
        };
        if result != 0 {
            if let Ok(temp_metadata) = temp_file.metadata() {
                if temp_metadata.uid() == metadata.uid && temp_metadata.gid() == metadata.gid {
                    temp_file
                        .set_permissions(metadata.permissions.clone())
                        .map_err(|e| format!("Failed to preserve file permissions: {}", e))?;
                    Self::write_linux_xattrs(temp_path, &metadata.xattrs)?;
                    return Ok(());
                }
            }
            // Saving should not fail only because the caller cannot chown the
            // replacement file. This can happen in group-writable directories
            // where editing is allowed but preserving ownership is not.
        }

        temp_file
            .set_permissions(metadata.permissions.clone())
            .map_err(|e| format!("Failed to preserve file permissions: {}", e))?;

        Self::write_linux_xattrs(temp_path, &metadata.xattrs)?;
        Ok(())
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn read_linux_xattrs(path: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let c_path = match CString::new(path.as_os_str().as_bytes()) {
            Ok(path) => path,
            Err(_) => return Vec::new(),
        };

        let size = unsafe { libc::listxattr(c_path.as_ptr(), std::ptr::null_mut(), 0) };
        if size <= 0 {
            return Vec::new();
        }

        let mut names = vec![0u8; size as usize];
        let size = unsafe {
            libc::listxattr(
                c_path.as_ptr(),
                names.as_mut_ptr() as *mut libc::c_char,
                names.len(),
            )
        };
        if size <= 0 {
            return Vec::new();
        }
        names.truncate(size as usize);

        let mut xattrs = Vec::new();
        for name in names
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
        {
            let c_name = match CString::new(name) {
                Ok(name) => name,
                Err(_) => continue,
            };

            let value_size = unsafe {
                libc::getxattr(c_path.as_ptr(), c_name.as_ptr(), std::ptr::null_mut(), 0)
            };
            if value_size < 0 {
                continue;
            }

            let mut value = vec![0u8; value_size as usize];
            let read = unsafe {
                libc::getxattr(
                    c_path.as_ptr(),
                    c_name.as_ptr(),
                    value.as_mut_ptr() as *mut libc::c_void,
                    value.len(),
                )
            };
            if read < 0 {
                continue;
            }
            value.truncate(read as usize);
            xattrs.push((name.to_vec(), value));
        }

        xattrs
    }

    #[cfg(not(all(unix, target_os = "linux")))]
    fn read_linux_xattrs(_path: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
        Vec::new()
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn write_linux_xattrs(path: &Path, xattrs: &[(Vec<u8>, Vec<u8>)]) -> Result<(), String> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let c_path = match CString::new(path.as_os_str().as_bytes()) {
            Ok(path) => path,
            Err(_) => return Ok(()),
        };

        for (name, value) in xattrs {
            let c_name = match CString::new(name.as_slice()) {
                Ok(name) => name,
                Err(_) => continue,
            };

            let result = unsafe {
                libc::setxattr(
                    c_path.as_ptr(),
                    c_name.as_ptr(),
                    value.as_ptr() as *const libc::c_void,
                    value.len(),
                    0,
                )
            };
            if result != 0 && Self::xattr_restore_failure_is_blocking(name) {
                return Err(format!(
                    "Failed to preserve extended attribute {}: {}",
                    String::from_utf8_lossy(name),
                    io::Error::last_os_error()
                ));
            }
        }

        Ok(())
    }

    #[cfg(not(all(unix, target_os = "linux")))]
    fn write_linux_xattrs(_path: &Path, _xattrs: &[(Vec<u8>, Vec<u8>)]) -> Result<(), String> {
        Ok(())
    }

    #[cfg(all(unix, target_os = "linux"))]
    fn xattr_restore_failure_is_blocking(name: &[u8]) -> bool {
        name.starts_with(b"user.")
            || name.starts_with(b"system.posix_acl_")
            || name == b"security.capability"
    }

    /// 파일 로드
    pub fn load_file(&mut self, path: &PathBuf) -> Result<(), String> {
        // Check file size before loading to prevent memory exhaustion
        if path.exists() {
            let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
            if metadata.is_dir() {
                return Err("Cannot edit a directory".to_string());
            }
            if metadata.len() > Self::MAX_EDIT_FILE_SIZE {
                return Err(format!(
                    "File too large for editing ({:.1} MB). Maximum size is {} MB. Use the viewer instead.",
                    metadata.len() as f64 / 1024.0 / 1024.0,
                    Self::MAX_EDIT_FILE_SIZE / 1024 / 1024
                ));
            }
        }

        self.file_path = path.clone();
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.scroll = 0;
        self.horizontal_scroll = 0;
        self.wrap_scroll_offset = 0;
        self.modified = false;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.undo_memory_usage = 0;
        self.redo_memory_usage = 0;
        self.selection = None;
        self.find_mode = FindReplaceMode::None;
        self.find_error = None;
        self.line_ending = "\n".to_string();
        self.trailing_newline = false;
        self.line_endings = vec![String::new()];
        self.remote_dirty = false;
        self.remote_save_generation = 0;

        // 파일 읽기
        if path.exists() {
            let content =
                fs::read_to_string(path).map_err(|e| format!("Failed to read file: {}", e))?;
            let (lines, line_endings, line_ending, trailing_newline) =
                Self::split_content_preserving_format(&content);
            self.lines = lines;
            self.line_endings = line_endings;
            self.line_ending = line_ending;
            self.trailing_newline = trailing_newline;
        } else {
            // 새 파일
            self.lines = vec![String::new()];
            self.line_endings = vec![String::new()];
        }

        // 원본 상태 저장
        self.ensure_line_endings();
        self.original_lines = self.lines.clone();
        self.original_line_endings = self.line_endings.clone();

        // 언어 감지
        self.language = Language::from_extension(path);
        self.highlighter = Some(SyntaxHighlighter::new(self.language, self.syntax_colors));

        Ok(())
    }

    /// 파일 저장
    /// Security: Preserves original file permissions and uses atomic write
    pub fn save_file(&mut self) -> Result<(), String> {
        // Resolve symlink to actual file path to avoid replacing symlink with regular file
        let is_symlink = fs::symlink_metadata(&self.file_path)
            .map(|m| m.is_symlink())
            .unwrap_or(false);

        let actual_path = if is_symlink {
            fs::canonicalize(&self.file_path)
                .map_err(|e| format!("Failed to resolve symlink: {}", e))?
        } else {
            self.file_path.clone()
        };

        // Save original metadata before writing
        #[cfg(unix)]
        let original_metadata = Self::capture_unix_save_metadata(&actual_path);

        let content = self.serialize_content();

        // Use atomic write: write to temp file, then rename
        let (temp_path, mut temp_file) = Self::create_save_temp_file(&actual_path)?;

        // Write to temporary file
        if let Err(e) = temp_file.write_all(content.as_bytes()) {
            let _ = fs::remove_file(&temp_path);
            return Err(format!("Failed to write temporary file: {}", e));
        }

        // Restore original metadata on temp file before rename
        #[cfg(unix)]
        if let Some(metadata) = &original_metadata {
            if let Err(e) = Self::apply_unix_save_metadata(&temp_path, &temp_file, metadata) {
                let _ = fs::remove_file(&temp_path);
                return Err(e);
            }
        }

        if let Err(e) = temp_file.sync_all() {
            let _ = fs::remove_file(&temp_path);
            return Err(format!("Failed to sync temporary file: {}", e));
        }
        drop(temp_file);

        Self::replace_saved_file(&temp_path, &actual_path).map_err(|e| {
            // Clean up temp file on failure
            let _ = fs::remove_file(&temp_path);
            e
        })?;

        #[cfg(unix)]
        if let Some(parent) = actual_path.parent() {
            if let Ok(dir) = fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }

        self.modified = false;
        self.remote_dirty = false;
        self.ensure_line_endings();
        self.original_lines = self.lines.clone();
        self.original_line_endings = self.line_endings.clone();
        Ok(())
    }

    /// 현재 상태와 원본을 비교하여 modified 플래그 업데이트
    pub fn update_modified(&mut self) {
        self.modified = self.remote_dirty
            || self.lines != self.original_lines
            || self.line_endings != self.original_line_endings;
    }

    fn clamp_cursor(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
            self.line_endings.push(String::new());
        }
        self.ensure_line_endings();

        self.cursor_line = self.cursor_line.min(self.lines.len() - 1);
        let line_len = self.lines[self.cursor_line].chars().count();
        self.cursor_col = self.cursor_col.min(line_len);
        self.scroll = self.scroll.min(self.lines.len().saturating_sub(1));
    }

    pub(crate) fn begin_remote_save(&mut self) -> u64 {
        self.remote_save_generation = self.remote_save_generation.wrapping_add(1);
        self.remote_dirty = true;
        self.update_modified();
        self.remote_save_generation
    }

    pub(crate) fn apply_remote_save_result(
        &mut self,
        panel_index: usize,
        remote_path: &str,
        generation: u64,
        failed: bool,
    ) -> bool {
        let is_current = generation == self.remote_save_generation
            && self
                .remote_origin
                .as_ref()
                .map(|origin| {
                    origin.panel_index == panel_index && origin.remote_path == remote_path
                })
                .unwrap_or(false);
        if is_current {
            self.remote_dirty = failed;
        }
        self.update_modified();
        is_current
    }

    pub fn has_selection_range(&self) -> bool {
        self.selection
            .and_then(|sel| self.clamped_selection_range(sel))
            .is_some()
    }

    fn clamped_selection_range(&self, sel: Selection) -> Option<(usize, usize, usize, usize)> {
        if self.lines.is_empty() {
            return None;
        }

        let (start_line, start_col, end_line, end_col) = sel.normalized();
        let start_line = start_line.min(self.lines.len() - 1);
        let end_line = end_line.min(self.lines.len() - 1);
        if start_line > end_line {
            return None;
        }

        let start_col = start_col.min(self.lines[start_line].chars().count());
        let end_col = end_col.min(self.lines[end_line].chars().count());

        if start_line == end_line && start_col >= end_col {
            return None;
        }

        Some((start_line, start_col, end_line, end_col))
    }

    /// Undo 액션 추가 (with memory limit enforcement)
    pub fn push_undo(&mut self, action: EditAction) {
        // Clear redo stack and its memory tracking
        self.redo_stack.clear();
        self.redo_memory_usage = 0;

        let action_size = Self::estimate_action_size(&action);

        // Enforce memory limit by removing oldest actions
        while self.undo_memory_usage + action_size > self.max_undo_memory
            && !self.undo_stack.is_empty()
        {
            if let Some(old_action) = self.undo_stack.pop_front() {
                self.undo_memory_usage = self
                    .undo_memory_usage
                    .saturating_sub(Self::estimate_action_size(&old_action));
            }
        }

        // Also enforce count limit
        while self.undo_stack.len() >= self.max_undo_size {
            if let Some(old_action) = self.undo_stack.pop_front() {
                self.undo_memory_usage = self
                    .undo_memory_usage
                    .saturating_sub(Self::estimate_action_size(&old_action));
            }
        }

        self.undo_memory_usage += action_size;
        self.undo_stack.push_back(action);
        self.modified = true;
    }

    /// Undo 실행
    pub fn undo(&mut self) {
        if let Some(action) = self.undo_stack.pop_back() {
            let action_size = Self::estimate_action_size(&action);
            self.undo_memory_usage = self.undo_memory_usage.saturating_sub(action_size);

            let reverse = self.reverse_action(&action);
            self.apply_action(&reverse, false);

            self.redo_memory_usage += action_size;
            self.redo_stack.push_back(action);
            self.selection = None;
            self.clear_multi_cursor_state();
            self.clamp_cursor();
            self.update_scroll();
            self.find_matching_bracket();
            self.update_modified();
        }
    }

    /// Redo 실행
    pub fn redo(&mut self) {
        if let Some(action) = self.redo_stack.pop_back() {
            let action_size = Self::estimate_action_size(&action);
            self.redo_memory_usage = self.redo_memory_usage.saturating_sub(action_size);

            self.apply_action(&action, false);

            self.undo_memory_usage += action_size;
            self.undo_stack.push_back(action);
            self.selection = None;
            self.clear_multi_cursor_state();
            self.clamp_cursor();
            self.update_scroll();
            self.find_matching_bracket();
            self.update_modified();
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
            EditAction::InsertLine {
                line,
                content,
                line_ending,
            } => EditAction::DeleteLine {
                line: *line,
                content: content.clone(),
                line_ending: line_ending.clone(),
            },
            EditAction::DeleteLine {
                line,
                content,
                line_ending,
            } => EditAction::InsertLine {
                line: *line,
                content: content.clone(),
                line_ending: line_ending.clone(),
            },
            EditAction::MergeLine {
                line,
                col,
                line_ending,
            } => EditAction::SplitLine {
                line: *line,
                col: *col,
                line_ending: line_ending.clone(),
            },
            EditAction::SplitLine {
                line,
                col,
                line_ending,
            } => EditAction::MergeLine {
                line: *line,
                col: *col,
                line_ending: line_ending.clone(),
            },
            EditAction::SetLineEnding {
                line,
                old_line_ending,
                new_line_ending,
            } => EditAction::SetLineEnding {
                line: *line,
                old_line_ending: new_line_ending.clone(),
                new_line_ending: old_line_ending.clone(),
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
            EditAction::SwapLines { line1, line2 } => EditAction::SwapLines {
                line1: *line1,
                line2: *line2,
            },
            EditAction::Batch { actions } => EditAction::Batch {
                actions: actions
                    .iter()
                    .rev()
                    .map(|a| self.reverse_action(a))
                    .collect(),
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
            EditAction::InsertLine {
                line,
                content,
                line_ending,
            } => {
                if *line <= self.lines.len() {
                    self.insert_line_with_ending(*line, content.clone(), line_ending.clone());
                }
            }
            EditAction::DeleteLine {
                line, line_ending, ..
            } => {
                if *line < self.lines.len() && self.lines.len() > 1 {
                    let was_last = *line + 1 == self.lines.len();
                    let _ = self.remove_line_with_ending(*line);
                    if was_last && *line > 0 {
                        self.set_line_ending_at(*line - 1, line_ending.clone());
                    }
                }
            }
            EditAction::MergeLine { line, .. } => {
                if *line + 1 < self.lines.len() {
                    let (next_line, next_ending) = self
                        .remove_line_with_ending(*line + 1)
                        .unwrap_or_else(|| (String::new(), String::new()));
                    self.lines[*line].push_str(&next_line);
                    self.set_line_ending_at(*line, next_ending);
                }
            }
            EditAction::SplitLine {
                line,
                col,
                line_ending,
            } => {
                if *line < self.lines.len() {
                    let content = &self.lines[*line];
                    let chars: Vec<char> = content.chars().collect();
                    let before: String = chars[..*col.min(&chars.len())].iter().collect();
                    let after: String = chars[*col.min(&chars.len())..].iter().collect();
                    let old_line_ending = self.line_ending_at(*line);
                    self.lines[*line] = before;
                    self.set_line_ending_at(*line, line_ending.clone());
                    self.insert_line_with_ending(*line + 1, after, old_line_ending);
                }
            }
            EditAction::SetLineEnding {
                line,
                new_line_ending,
                ..
            } => {
                if *line < self.lines.len() {
                    self.set_line_ending_at(*line, new_line_ending.clone());
                }
            }
            EditAction::Replace {
                line, new_content, ..
            } => {
                if *line < self.lines.len() {
                    self.lines[*line] = new_content.clone();
                }
            }
            EditAction::SwapLines { line1, line2 } => {
                if *line1 < self.lines.len() && *line2 < self.lines.len() {
                    self.lines.swap(*line1, *line2);
                }
            }
            EditAction::Batch { actions } => {
                for a in actions {
                    self.apply_action(a, false);
                }
            }
        }
    }

    fn has_multi_cursor(&self) -> bool {
        !self.cursors.is_empty()
    }

    fn active_multi_cursor_position(&self) -> (usize, usize) {
        if let Some(sel) = self.selection {
            let (_, _, end_line, end_col) = sel.normalized();
            (end_line, end_col)
        } else {
            (self.cursor_line, self.cursor_col)
        }
    }

    fn clamped_cursor_position(&self, line: usize, col: usize) -> Option<(usize, usize)> {
        if line >= self.lines.len() {
            return None;
        }
        Some((line, col.min(self.lines[line].chars().count())))
    }

    fn multi_cursor_positions(&self) -> Vec<(usize, usize)> {
        let mut positions = Vec::with_capacity(self.cursors.len() + 1);
        let active = self.active_multi_cursor_position();
        if let Some(active) = self.clamped_cursor_position(active.0, active.1) {
            positions.push(active);
        }
        for &(line, col) in &self.cursors {
            if let Some(pos) = self.clamped_cursor_position(line, col) {
                positions.push(pos);
            }
        }
        positions.sort_unstable();
        positions.dedup();
        positions
    }

    fn set_multi_cursor_positions(
        &mut self,
        active: (usize, usize),
        positions: Vec<(usize, usize)>,
    ) {
        let active = self
            .clamped_cursor_position(active.0, active.1)
            .unwrap_or((0, 0));
        let mut positions: Vec<(usize, usize)> = positions
            .into_iter()
            .filter_map(|(line, col)| self.clamped_cursor_position(line, col))
            .collect();
        positions.sort_unstable();
        positions.dedup();

        self.cursor_line = active.0;
        self.cursor_col = active.1;
        self.selection = None;
        self.last_word_selection = None;
        self.cursors = positions.into_iter().filter(|pos| *pos != active).collect();
    }

    fn clear_multi_cursor_state(&mut self) {
        self.cursors.clear();
        self.last_word_selection = None;
    }

    fn insertion_multi_cursor_active(&self) -> bool {
        self.selection.is_none() && self.last_word_selection.is_none() && self.has_multi_cursor()
    }

    fn is_extra_insert_cursor_at(&self, line: usize, col: usize) -> bool {
        self.insertion_multi_cursor_active()
            && self
                .cursors
                .iter()
                .any(|&(cursor_line, cursor_col)| cursor_line == line && cursor_col == col)
    }

    fn replace_ranges_with_text(
        &mut self,
        ranges: &[(usize, usize, usize)],
        replacement: &str,
        active_range: Option<(usize, usize, usize)>,
    ) -> bool {
        if ranges.is_empty() || replacement.contains('\n') || replacement.contains('\r') {
            return false;
        }

        let replacement_chars: Vec<char> = replacement.chars().collect();
        let replacement_len = replacement_chars.len();
        let mut ranges = ranges.to_vec();
        ranges.sort_unstable();
        ranges.dedup();

        let mut new_positions = Vec::with_capacity(ranges.len());
        let mut active_after = None;
        let mut actions = Vec::new();
        let mut idx = 0usize;

        while idx < ranges.len() {
            let line_idx = ranges[idx].0;
            if line_idx >= self.lines.len() {
                idx += 1;
                continue;
            }

            let old_content = self.lines[line_idx].clone();
            let mut chars: Vec<char> = old_content.chars().collect();
            let mut line_ranges = Vec::new();

            while idx < ranges.len() && ranges[idx].0 == line_idx {
                let (_, start, end) = ranges[idx];
                let start = start.min(chars.len());
                let end = end.min(chars.len());
                if start <= end {
                    line_ranges.push((line_idx, start, end));
                }
                idx += 1;
            }

            line_ranges.sort_unstable();
            line_ranges.dedup();

            let mut delta: isize = 0;
            for &(line, start, end) in &line_ranges {
                let adjusted_start = (start as isize + delta).max(0) as usize;
                let adjusted_end = adjusted_start + replacement_len;
                let pos = (line, adjusted_end);
                if active_range == Some((line, start, end)) {
                    active_after = Some(pos);
                }
                new_positions.push(pos);
                delta += replacement_len as isize - end.saturating_sub(start) as isize;
            }

            for &(_, start, end) in line_ranges.iter().rev() {
                chars.splice(start..end, replacement_chars.iter().copied());
            }

            let new_content: String = chars.into_iter().collect();
            if old_content != new_content {
                self.lines[line_idx] = new_content.clone();
                actions.push(EditAction::Replace {
                    line: line_idx,
                    old_content,
                    new_content,
                });
            }
        }

        if new_positions.is_empty() {
            return false;
        }

        let active = active_after.unwrap_or_else(|| *new_positions.last().unwrap());
        self.set_multi_cursor_positions(active, new_positions);

        if !actions.is_empty() {
            self.push_undo(EditAction::Batch { actions });
        }
        self.update_scroll();
        true
    }

    fn insert_at_multi_cursors(&mut self, text: &str) -> bool {
        if !self.insertion_multi_cursor_active() {
            return false;
        }
        if text.is_empty() {
            return true;
        }
        if text.contains('\n') || text.contains('\r') {
            self.set_message("Multi-cursor paste supports single-line text only", 50);
            return true;
        }

        let positions = self.multi_cursor_positions();
        let ranges: Vec<(usize, usize, usize)> = positions
            .iter()
            .map(|&(line, col)| (line, col, col))
            .collect();
        let active = (self.cursor_line, self.cursor_col);
        self.replace_ranges_with_text(&ranges, text, Some((active.0, active.1, active.1)))
    }

    fn delete_at_multi_cursors(&mut self, backward: bool) -> bool {
        if !self.insertion_multi_cursor_active() {
            return false;
        }

        let positions = self.multi_cursor_positions();
        let active = (self.cursor_line, self.cursor_col);
        let mut ranges = Vec::new();

        for &(line, col) in &positions {
            if line >= self.lines.len() {
                continue;
            }
            let line_len = self.lines[line].chars().count();
            if backward {
                if col > 0 {
                    ranges.push((line, col - 1, col));
                }
            } else if col < line_len {
                ranges.push((line, col, col + 1));
            }
        }

        let active_range = if backward {
            self.lines.get(active.0).map(|line| {
                let active_col = active.1.min(line.chars().count());
                if active_col > 0 {
                    (active.0, active_col - 1, active_col)
                } else {
                    (active.0, active_col, active_col)
                }
            })
        } else {
            self.lines.get(active.0).map(|line| {
                let line_len = line.chars().count();
                let active_col = active.1.min(line_len);
                if active_col < line_len {
                    (active.0, active_col, active_col + 1)
                } else {
                    (active.0, active_col, active_col)
                }
            })
        };

        if let Some(active_range) = active_range {
            if active_range.1 == active_range.2 && !ranges.contains(&active_range) {
                ranges.push(active_range);
            }
        }

        if ranges.is_empty() {
            return true;
        }

        self.replace_ranges_with_text(&ranges, "", active_range)
    }

    fn move_position(
        &self,
        line: usize,
        col: usize,
        line_delta: i32,
        col_delta: i32,
    ) -> (usize, usize) {
        let mut line = line.min(self.lines.len().saturating_sub(1));
        let mut col = col.min(self.lines[line].chars().count());

        if line_delta != 0 {
            line = (line as i32 + line_delta)
                .max(0)
                .min(self.lines.len().saturating_sub(1) as i32) as usize;
            col = col.min(self.lines[line].chars().count());
        }

        if col_delta != 0 {
            let line_len = self.lines[line].chars().count();
            let new_col = (col as i32 + col_delta).max(0) as usize;

            if new_col > line_len && col_delta > 0 && line + 1 < self.lines.len() {
                line += 1;
                col = 0;
            } else if col == 0 && col_delta < 0 && line > 0 {
                line -= 1;
                col = self.lines[line].chars().count();
            } else {
                col = new_col.min(line_len);
            }
        }

        (line, col)
    }

    fn move_all_cursors(&mut self, line_delta: i32, col_delta: i32) -> bool {
        if !self.has_multi_cursor() {
            return false;
        }

        let active_old = self.active_multi_cursor_position();
        let positions = self.multi_cursor_positions();
        let moved: Vec<(usize, usize)> = positions
            .iter()
            .map(|&(line, col)| self.move_position(line, col, line_delta, col_delta))
            .collect();
        let active = self.move_position(active_old.0, active_old.1, line_delta, col_delta);
        self.set_multi_cursor_positions(active, moved);
        self.update_scroll();
        self.find_matching_bracket();
        true
    }

    fn position_to_line_start(&self, line: usize, col: usize) -> (usize, usize) {
        let line = line.min(self.lines.len().saturating_sub(1));
        let col = col.min(self.lines[line].chars().count());
        let first_non_ws = self.lines[line]
            .chars()
            .position(|c| !c.is_whitespace())
            .unwrap_or(0);
        let new_col = if col == first_non_ws || col == 0 {
            if col == 0 {
                first_non_ws
            } else {
                0
            }
        } else {
            first_non_ws
        };
        (line, new_col)
    }

    fn position_to_line_end(&self, line: usize, _col: usize) -> (usize, usize) {
        let line = line.min(self.lines.len().saturating_sub(1));
        (line, self.lines[line].chars().count())
    }

    fn move_all_cursors_to_line_start(&mut self) -> bool {
        if !self.has_multi_cursor() {
            return false;
        }

        let active_old = self.active_multi_cursor_position();
        let positions = self.multi_cursor_positions();
        let moved: Vec<(usize, usize)> = positions
            .iter()
            .map(|&(line, col)| self.position_to_line_start(line, col))
            .collect();
        let active = self.position_to_line_start(active_old.0, active_old.1);
        self.set_multi_cursor_positions(active, moved);
        self.update_scroll();
        self.find_matching_bracket();
        true
    }

    fn move_all_cursors_to_line_end(&mut self) -> bool {
        if !self.has_multi_cursor() {
            return false;
        }

        let active_old = self.active_multi_cursor_position();
        let positions = self.multi_cursor_positions();
        let moved: Vec<(usize, usize)> = positions
            .iter()
            .map(|&(line, col)| self.position_to_line_end(line, col))
            .collect();
        let active = self.position_to_line_end(active_old.0, active_old.1);
        self.set_multi_cursor_positions(active, moved);
        self.update_scroll();
        self.find_matching_bracket();
        true
    }

    fn word_left_position(&self, line: usize, col: usize) -> (usize, usize) {
        let mut line = line.min(self.lines.len().saturating_sub(1));
        let mut col = col.min(self.lines[line].chars().count());

        if col == 0 {
            if line > 0 {
                line -= 1;
                col = self.lines[line].chars().count();
            }
            return (line, col);
        }

        let chars: Vec<char> = self.lines[line].chars().collect();
        while col > 0 && !Self::is_word_char(chars[col - 1]) {
            col -= 1;
        }
        while col > 0 && Self::is_word_char(chars[col - 1]) {
            col -= 1;
        }
        (line, col)
    }

    fn word_right_position(&self, line: usize, col: usize) -> (usize, usize) {
        let mut line = line.min(self.lines.len().saturating_sub(1));
        let mut col = col.min(self.lines[line].chars().count());
        let chars: Vec<char> = self.lines[line].chars().collect();
        let line_len = chars.len();

        if col >= line_len {
            if line + 1 < self.lines.len() {
                line += 1;
                col = 0;
            }
            return (line, col);
        }

        while col < line_len && Self::is_word_char(chars[col]) {
            col += 1;
        }
        while col < line_len && !Self::is_word_char(chars[col]) {
            col += 1;
        }
        (line, col)
    }

    fn move_all_cursors_by_word(&mut self, right: bool) -> bool {
        if !self.has_multi_cursor() {
            return false;
        }

        let active_old = self.active_multi_cursor_position();
        let positions = self.multi_cursor_positions();
        let move_one = |line, col| {
            if right {
                self.word_right_position(line, col)
            } else {
                self.word_left_position(line, col)
            }
        };
        let moved: Vec<(usize, usize)> = positions
            .iter()
            .map(|&(line, col)| move_one(line, col))
            .collect();
        let active = move_one(active_old.0, active_old.1);
        self.set_multi_cursor_positions(active, moved);
        self.update_scroll();
        self.find_matching_bracket();
        true
    }

    /// 문자 삽입
    pub fn insert_char(&mut self, c: char) {
        if self.replace_selected_occurrences_with(&c.to_string()) {
            return;
        }

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

    /// 문자열 삽입 (단일 Undo 액션으로 처리)
    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if self.replace_selected_occurrences_with(s) {
            return;
        }

        // 선택 영역 삭제 (별도 Undo 액션으로 처리됨)
        self.delete_selection();

        // 시작 위치 저장
        let start_line = self.cursor_line;
        let start_col = self.cursor_col;

        // 줄바꿈으로 분리
        let (parts, inserted_line_endings) = Self::split_insert_text_preserving_endings(s);

        if parts.len() == 1 {
            // 단일 줄 삽입 (줄바꿈 없음)
            let line = &mut self.lines[self.cursor_line];
            let mut chars: Vec<char> = line.chars().collect();
            for (i, c) in s.chars().enumerate() {
                chars.insert(self.cursor_col + i, c);
            }
            *line = chars.into_iter().collect();
            self.cursor_col += s.chars().count();

            self.push_undo(EditAction::Insert {
                line: start_line,
                col: start_col,
                text: s.to_string(),
            });
        } else {
            // 여러 줄 삽입
            let mut actions = Vec::new();
            self.ensure_line_endings();
            let old_current_ending = self.line_ending_at(self.cursor_line);

            // 현재 줄의 커서 이후 부분 저장
            let current_line = &self.lines[self.cursor_line];
            let chars: Vec<char> = current_line.chars().collect();
            let before: String = chars[..self.cursor_col].iter().collect();
            let after: String = chars[self.cursor_col..].iter().collect();

            // 첫 부분 + 첫 번째 삽입 텍스트
            self.lines[self.cursor_line] = format!("{}{}", before, parts[0]);
            self.set_line_ending_at(self.cursor_line, inserted_line_endings[0].clone());

            if !parts[0].is_empty() {
                actions.push(EditAction::Insert {
                    line: start_line,
                    col: start_col,
                    text: parts[0].clone(),
                });
            }
            actions.push(EditAction::SplitLine {
                line: start_line,
                col: start_col + parts[0].chars().count(),
                line_ending: inserted_line_endings[0].clone(),
            });

            // 중간 줄들 삽입
            for (i, part) in parts.iter().enumerate().skip(1).take(parts.len() - 2) {
                let new_line = part.to_string();
                self.insert_line_with_ending(
                    self.cursor_line + i,
                    new_line.clone(),
                    inserted_line_endings[i].clone(),
                );
                actions.push(EditAction::InsertLine {
                    line: self.cursor_line + i,
                    content: new_line,
                    line_ending: inserted_line_endings[i].clone(),
                });
            }

            // 마지막 줄 (마지막 삽입 텍스트 + 원래 커서 이후 부분)
            let last_idx = parts.len() - 1;
            let last_line = format!("{}{}", parts[last_idx], after);
            self.insert_line_with_ending(
                self.cursor_line + last_idx,
                last_line.clone(),
                old_current_ending.clone(),
            );
            if !parts[last_idx].is_empty() {
                actions.push(EditAction::Insert {
                    line: start_line + last_idx,
                    col: 0,
                    text: parts[last_idx].clone(),
                });
            }

            // 커서 위치 업데이트
            self.cursor_line = start_line + last_idx;
            self.cursor_col = parts[last_idx].chars().count();

            self.push_undo(EditAction::Batch { actions });
        }

        self.update_scroll();
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
        if self.has_multi_cursor() {
            self.set_message("Multi-cursor newline is not supported", 50);
            return;
        }

        self.delete_selection();

        let line = &self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();
        let split_col = self.cursor_col.min(chars.len());
        let before: String = chars[..split_col].iter().collect();
        let after: String = chars[split_col..].iter().collect();

        // 자동 들여쓰기
        let indent = if self.auto_indent {
            let leading_ws: String = before.chars().take_while(|c| c.is_whitespace()).collect();
            leading_ws
        } else {
            String::new()
        };

        let new_second_line = format!("{}{}", indent, after);
        let first_line = self.cursor_line;
        self.ensure_line_endings();
        let old_line_ending = self.line_ending_at(first_line);
        let inserted_ending = self.default_line_ending();

        self.lines[first_line] = before.clone();
        self.set_line_ending_at(first_line, inserted_ending.clone());
        self.insert_line_with_ending(
            first_line + 1,
            new_second_line.clone(),
            old_line_ending.clone(),
        );
        self.cursor_line += 1;
        self.cursor_col = indent.chars().count();

        let mut actions = vec![EditAction::SplitLine {
            line: first_line,
            col: split_col,
            line_ending: inserted_ending,
        }];
        if !indent.is_empty() {
            actions.push(EditAction::Insert {
                line: first_line + 1,
                col: 0,
                text: indent,
            });
        }

        self.push_undo(EditAction::Batch { actions });
        self.update_scroll();
    }

    /// 뒤로 삭제 (Backspace)
    pub fn delete_backward(&mut self) {
        if self.replace_selected_occurrences_with("") {
            return;
        }
        if self.delete_at_multi_cursors(true) {
            return;
        }

        if self.has_selection_range() {
            self.delete_selection();
            return;
        } else if self.selection.is_some() {
            self.selection = None;
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
            self.ensure_line_endings();
            let removed_separator = self.line_ending_at(self.cursor_line - 1);
            let (current_line, current_ending) = self
                .remove_line_with_ending(self.cursor_line)
                .unwrap_or_else(|| (String::new(), String::new()));
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].chars().count();
            self.lines[self.cursor_line].push_str(&current_line);
            self.set_line_ending_at(self.cursor_line, current_ending);

            let action = EditAction::MergeLine {
                line: self.cursor_line,
                col: self.cursor_col,
                line_ending: removed_separator,
            };

            self.push_undo(action);
        }
        self.update_scroll();
    }

    /// 앞으로 삭제 (Delete)
    pub fn delete_forward(&mut self) {
        if self.replace_selected_occurrences_with("") {
            return;
        }
        if self.delete_at_multi_cursors(false) {
            return;
        }

        if self.has_selection_range() {
            self.delete_selection();
            return;
        } else if self.selection.is_some() {
            self.selection = None;
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
            self.ensure_line_endings();
            let removed_separator = self.line_ending_at(self.cursor_line);
            let (next_line, next_ending) = self
                .remove_line_with_ending(self.cursor_line + 1)
                .unwrap_or_else(|| (String::new(), String::new()));
            self.lines[self.cursor_line].push_str(&next_line);
            self.set_line_ending_at(self.cursor_line, next_ending);

            let action = EditAction::MergeLine {
                line: self.cursor_line,
                col: self.cursor_col,
                line_ending: removed_separator,
            };

            self.push_undo(action);
        }
        self.update_scroll();
    }

    /// 선택 영역 삭제
    pub fn delete_selection(&mut self) {
        let sel = match self.selection.take() {
            Some(s) => s,
            _ => return,
        };

        let (start_line, start_col, end_line, end_col) = match self.clamped_selection_range(sel) {
            Some(range) => range,
            None => return,
        };

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
            self.ensure_line_endings();
            let start_line_ending = self.line_ending_at(start_line);
            let merged_line_ending = self.line_ending_at(end_line);

            // 시작 줄 원본 저장
            let first_line_original = self.lines[start_line].clone();

            // 시작 줄 처리
            let first_chars: Vec<char> = self.lines[start_line].chars().collect();
            let first_part: String = first_chars[..start_col].iter().collect();

            // 끝 줄 처리
            let last_chars: Vec<char> = self.lines[end_line].chars().collect();
            let last_part: String = last_chars[end_col..].iter().collect();

            // 줄 병합
            let new_first_line = format!("{}{}", first_part, last_part);

            // 시작 줄 수정을 Replace로 저장 (Undo 시 원본 복원을 위해)
            actions.push(EditAction::Replace {
                line: start_line,
                old_content: first_line_original,
                new_content: new_first_line.clone(),
            });
            actions.push(EditAction::SetLineEnding {
                line: start_line,
                old_line_ending: start_line_ending,
                new_line_ending: merged_line_ending.clone(),
            });

            // 삭제할 줄들 저장 (redo 시 인덱스가 유지되도록 역순으로)
            for i in (start_line + 1..=end_line).rev() {
                actions.push(EditAction::DeleteLine {
                    line: i,
                    content: self.lines[i].clone(),
                    line_ending: self.line_ending_at(i),
                });
            }

            self.lines[start_line] = new_first_line;
            self.set_line_ending_at(start_line, merged_line_ending);

            // 중간 줄들 제거
            for _ in start_line + 1..=end_line {
                if start_line + 1 < self.lines.len() {
                    let _ = self.remove_line_with_ending(start_line + 1);
                }
            }

            self.push_undo(EditAction::Batch { actions });
        }

        self.cursor_line = start_line;
        self.cursor_col = start_col;
        self.clear_multi_cursor_state();
        self.update_scroll();
    }

    /// 선택된 텍스트 가져오기
    pub fn get_selected_text(&self) -> String {
        let sel = match &self.selection {
            Some(s) => s,
            _ => return String::new(),
        };

        let (start_line, start_col, end_line, end_col) = match self.clamped_selection_range(*sel) {
            Some(range) => range,
            None => return String::new(),
        };

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
                result.push_str(&self.line_ending_for_clipboard(i - 1));
                result.push_str(&self.lines[i]);
            }

            // 마지막 줄
            result.push_str(&self.line_ending_for_clipboard(end_line - 1));
            let last_chars: Vec<char> = self.lines[end_line].chars().collect();
            result.push_str(&last_chars[..end_col].iter().collect::<String>());

            result
        }
    }

    /// 복사 (선택 없으면 줄 전체)
    pub fn copy(&mut self) {
        if self.has_selection_range() {
            self.clipboard = self.get_selected_text();
        } else {
            // 줄 전체 복사
            self.clipboard = self.lines[self.cursor_line].clone()
                + &self.line_ending_for_clipboard(self.cursor_line);
        }
    }

    /// 잘라내기
    #[allow(dead_code)]
    pub fn cut(&mut self) {
        if self.has_selection_range() {
            self.clipboard = self.get_selected_text();
            self.delete_selection();
        }
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
            self.clear_multi_cursor_state();
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
        let current_line = self.cursor_line;
        self.ensure_line_endings();
        let line_content = self.lines[current_line].clone();
        let old_line_ending = self.line_ending_at(current_line);
        let inserted_ending = self.default_line_ending();
        self.set_line_ending_at(current_line, inserted_ending.clone());
        self.insert_line_with_ending(
            current_line + 1,
            line_content.clone(),
            old_line_ending.clone(),
        );
        self.cursor_line += 1;

        self.push_undo(EditAction::Batch {
            actions: vec![
                EditAction::SetLineEnding {
                    line: current_line,
                    old_line_ending: old_line_ending.clone(),
                    new_line_ending: inserted_ending,
                },
                EditAction::InsertLine {
                    line: current_line + 1,
                    content: line_content,
                    line_ending: old_line_ending,
                },
            ],
        });
        self.selection = None;
        self.modified = true;
        self.update_scroll();
    }

    /// 줄 삭제
    pub fn delete_line(&mut self) {
        if self.lines.len() > 1 {
            self.ensure_line_endings();
            let deleted_line = self.cursor_line;
            let previous_line_ending = if deleted_line + 1 == self.lines.len() && deleted_line > 0 {
                Some(self.line_ending_at(deleted_line - 1))
            } else {
                None
            };
            let (content, line_ending) = self
                .remove_line_with_ending(deleted_line)
                .unwrap_or_else(|| (String::new(), String::new()));

            let delete_action = EditAction::DeleteLine {
                line: deleted_line,
                content,
                line_ending: line_ending.clone(),
            };
            if let Some(old_previous_line_ending) = previous_line_ending {
                self.push_undo(EditAction::Batch {
                    actions: vec![
                        delete_action,
                        EditAction::SetLineEnding {
                            line: deleted_line - 1,
                            old_line_ending: old_previous_line_ending,
                            new_line_ending: line_ending,
                        },
                    ],
                });
            } else {
                self.push_undo(delete_action);
            }

            if self.cursor_line >= self.lines.len() {
                self.cursor_line = self.lines.len() - 1;
            }
            self.cursor_col = self
                .cursor_col
                .min(self.lines[self.cursor_line].chars().count());
            self.selection = None;
            self.modified = true;
            self.update_scroll();
        } else {
            self.ensure_line_endings();
            let old_content = self.lines[0].clone();
            let old_line_ending = self.line_ending_at(0);
            if old_content.is_empty() && old_line_ending.is_empty() {
                return;
            }

            self.lines[0].clear();
            self.set_line_ending_at(0, String::new());
            self.cursor_col = 0;
            self.selection = None;

            self.push_undo(EditAction::Batch {
                actions: vec![
                    EditAction::Replace {
                        line: 0,
                        old_content,
                        new_content: String::new(),
                    },
                    EditAction::SetLineEnding {
                        line: 0,
                        old_line_ending,
                        new_line_ending: String::new(),
                    },
                ],
            });
            self.update_scroll();
        }
    }

    /// 줄 위로 이동
    pub fn move_line_up(&mut self) {
        if self.cursor_line > 0 {
            let line1 = self.cursor_line - 1;
            let line2 = self.cursor_line;
            self.lines.swap(line1, line2);
            self.push_undo(EditAction::SwapLines { line1, line2 });
            self.cursor_line -= 1;
            self.update_scroll();
        }
    }

    /// 줄 아래로 이동
    pub fn move_line_down(&mut self) {
        if self.cursor_line + 1 < self.lines.len() {
            let line1 = self.cursor_line;
            let line2 = self.cursor_line + 1;
            self.lines.swap(line1, line2);
            self.push_undo(EditAction::SwapLines { line1, line2 });
            self.cursor_line += 1;
            self.update_scroll();
        }
    }

    /// 커서 이동
    pub fn move_cursor(&mut self, line_delta: i32, col_delta: i32, extend_selection: bool) {
        if !extend_selection && self.move_all_cursors(line_delta, col_delta) {
            return;
        }

        let old_line = self.cursor_line;
        let old_col = self.cursor_col;
        let had_selection = self.selection.is_some();

        if !extend_selection {
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
            let shift_right_from_last_char = extend_selection
                && col_delta > 0
                && line_len > 0
                && self.cursor_col < line_len
                && self.cursor_col + 1 >= line_len;

            if (new_col > line_len || shift_right_from_last_char)
                && col_delta > 0
                && self.cursor_line + 1 < self.lines.len()
            {
                // 다음 줄로 이동
                self.cursor_line += 1;
                self.cursor_col = 0;
            } else if self.cursor_col == 0 && col_delta < 0 && self.cursor_line > 0 {
                // 줄 시작에서 왼쪽으로 이동 -> 이전 줄 끝으로
                self.cursor_line -= 1;
                self.cursor_col = self.lines[self.cursor_line].chars().count();
            } else {
                self.cursor_col = new_col.min(line_len);
            }
        }

        // 선택 영역 업데이트 (블록 커서 선택: 현재 커서 위치의 문자 포함)
        if extend_selection
            && !had_selection
            && (self.cursor_line != old_line || self.cursor_col != old_col)
        {
            self.selection = Some(Selection::new(old_line, old_col));
        }
        if extend_selection {
            if let Some(ref mut sel) = self.selection {
                sel.end_line = self.cursor_line;
                let line_len = self.lines[self.cursor_line].chars().count();
                sel.end_col = if self.cursor_line > sel.start_line {
                    self.cursor_col.min(line_len)
                } else {
                    (self.cursor_col + 1).min(line_len)
                };
            }
        }

        if !extend_selection {
            self.selection = None;
        }

        self.update_scroll();
        self.find_matching_bracket();
    }

    /// 줄 시작으로
    pub fn move_to_line_start(&mut self, extend_selection: bool) {
        if !extend_selection && self.move_all_cursors_to_line_start() {
            return;
        }

        let old_line = self.cursor_line;
        let old_col = self.cursor_col;
        let had_selection = self.selection.is_some();

        if !extend_selection {
            self.selection = None;
        }

        if extend_selection {
            // 선택 모드: 항상 줄 맨 처음으로
            self.cursor_col = 0;
        } else {
            // 이동 모드: 첫 번째 비공백 문자로 이동, 이미 거기 있으면 줄 시작으로
            let line = &self.lines[self.cursor_line];
            let first_non_ws = line.chars().position(|c| !c.is_whitespace()).unwrap_or(0);

            if self.cursor_col == first_non_ws || self.cursor_col == 0 {
                self.cursor_col = if self.cursor_col == 0 {
                    first_non_ws
                } else {
                    0
                };
            } else {
                self.cursor_col = first_non_ws;
            }
        }

        if extend_selection
            && !had_selection
            && (self.cursor_line != old_line || self.cursor_col != old_col)
        {
            self.selection = Some(Selection::new(old_line, old_col));
        }

        // 블록 커서 선택: 현재 커서 위치의 문자 포함
        if extend_selection {
            if let Some(ref mut sel) = self.selection {
                sel.end_line = self.cursor_line;
                let line_len = self.lines[self.cursor_line].chars().count();
                sel.end_col = (self.cursor_col + 1).min(line_len);
            }
        }
        self.update_scroll();
        self.find_matching_bracket();
    }

    /// 줄 끝으로
    pub fn move_to_line_end(&mut self, extend_selection: bool) {
        if !extend_selection && self.move_all_cursors_to_line_end() {
            return;
        }

        let old_line = self.cursor_line;
        let old_col = self.cursor_col;
        let had_selection = self.selection.is_some();

        if !extend_selection {
            self.selection = None;
        }

        self.cursor_col = self.lines[self.cursor_line].chars().count();

        if extend_selection
            && !had_selection
            && (self.cursor_line != old_line || self.cursor_col != old_col)
        {
            self.selection = Some(Selection::new(old_line, old_col));
        }

        if extend_selection {
            if let Some(ref mut sel) = self.selection {
                sel.end_line = self.cursor_line;
                sel.end_col = self.cursor_col;
            }
        }
        self.update_scroll();
        self.find_matching_bracket();
    }

    /// Word wrap 모드에서 논리적 줄이 차지하는 시각적 행 수 계산
    fn count_wrapped_rows(&self, line_idx: usize) -> usize {
        if line_idx >= self.lines.len() || self.visible_width == 0 {
            return 1;
        }
        let (expanded, _) = self.expand_tabs_with_mapping(&self.lines[line_idx]);
        if expanded.is_empty() {
            return 1;
        }
        Self::compute_wrap_segments(&expanded, self.visible_width).len()
    }

    /// 확장된 줄을 visual column 기준으로 세그먼트로 분할
    /// 반환: 각 세그먼트의 시작 visual column 위치 목록
    fn compute_wrap_segments(expanded_line: &str, max_width: usize) -> Vec<usize> {
        if expanded_line.is_empty() || max_width == 0 {
            return vec![0];
        }
        let mut segments = vec![0usize];
        let mut current_width = 0usize;
        let mut seg_start = 0usize;
        for ch in expanded_line.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(1);
            if current_width + w > max_width && current_width > 0 {
                seg_start += current_width;
                segments.push(seg_start);
                current_width = w;
            } else {
                current_width += w;
            }
        }
        segments
    }

    fn wrap_segment_index_for_visual_col(&self, line_idx: usize, visual_col: usize) -> usize {
        if line_idx >= self.lines.len() {
            return 0;
        }

        let (expanded, _) = self.expand_tabs_with_mapping(&self.lines[line_idx]);
        let segments = Self::compute_wrap_segments(&expanded, self.visible_width.max(1));
        segments
            .iter()
            .enumerate()
            .take_while(|(_, start)| **start <= visual_col)
            .map(|(idx, _)| idx)
            .last()
            .unwrap_or(0)
    }

    fn cursor_visual_row_from_wrap_top(&self, cursor_segment: usize) -> usize {
        if self.cursor_line < self.scroll || self.scroll >= self.lines.len() {
            return 0;
        }

        if self.cursor_line == self.scroll {
            return cursor_segment.saturating_sub(self.wrap_scroll_offset);
        }

        let mut rows = self
            .count_wrapped_rows(self.scroll)
            .saturating_sub(self.wrap_scroll_offset);
        for line_idx in self.scroll + 1..self.cursor_line {
            rows += self.count_wrapped_rows(line_idx);
        }
        rows + cursor_segment
    }

    fn advance_wrap_scroll_top(&mut self) {
        if self.scroll >= self.lines.len() {
            self.scroll = self.lines.len().saturating_sub(1);
            self.wrap_scroll_offset = 0;
            return;
        }

        let top_rows = self.count_wrapped_rows(self.scroll).max(1);
        if self.wrap_scroll_offset + 1 < top_rows {
            self.wrap_scroll_offset += 1;
        } else if self.scroll + 1 < self.lines.len() {
            self.scroll += 1;
            self.wrap_scroll_offset = 0;
        }
    }

    /// 스크롤 업데이트
    pub fn update_scroll(&mut self) {
        let visible_height = self.visible_height.max(1);
        let visible_width = self.visible_width.max(1);

        if self.word_wrap {
            self.horizontal_scroll = 0;

            if self.cursor_line < self.scroll {
                self.scroll = self.cursor_line;
                self.wrap_scroll_offset = 0;
            }
            if self.scroll >= self.lines.len() {
                self.scroll = self.lines.len().saturating_sub(1);
                self.wrap_scroll_offset = 0;
            }

            let top_rows = self.count_wrapped_rows(self.scroll).max(1);
            self.wrap_scroll_offset = self.wrap_scroll_offset.min(top_rows - 1);

            let cursor_segment =
                self.wrap_segment_index_for_visual_col(self.cursor_line, self.cursor_visual_col());

            if self.cursor_line == self.scroll && cursor_segment < self.wrap_scroll_offset {
                self.wrap_scroll_offset = cursor_segment;
            }

            let mut guard = 0usize;
            while self.cursor_visual_row_from_wrap_top(cursor_segment) >= visible_height
                && (self.scroll < self.cursor_line || self.wrap_scroll_offset < cursor_segment)
                && guard < self.lines.len().saturating_mul(1024).max(1024)
            {
                self.advance_wrap_scroll_top();
                guard += 1;
            }
        } else {
            self.wrap_scroll_offset = 0;
            if self.cursor_line < self.scroll {
                self.scroll = self.cursor_line;
            } else if self.cursor_line >= self.scroll + visible_height {
                self.scroll = self.cursor_line.saturating_sub(visible_height - 1);
            }

            // 수평 스크롤 (visual column 기반)
            let cursor_visual = self.cursor_visual_col();
            if cursor_visual < self.horizontal_scroll {
                self.horizontal_scroll = cursor_visual;
            } else if cursor_visual >= self.horizontal_scroll + visible_width {
                self.horizontal_scroll = cursor_visual.saturating_sub(visible_width - 1);
            }
        }

        self.find_matching_bracket();
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

    fn build_find_regex(&self) -> Result<Regex, regex::Error> {
        let pattern = if self.find_options.use_regex {
            self.find_term.clone()
        } else {
            regex::escape(&self.find_term)
        };

        let pattern = if self.find_options.whole_word {
            format!(r"\b(?:{})\b", pattern)
        } else {
            pattern
        };

        if self.find_options.case_sensitive {
            Regex::new(&pattern)
        } else {
            Regex::new(&format!("(?i){}", pattern))
        }
    }

    /// 검색 수행
    pub fn perform_find(&mut self) {
        self.match_positions.clear();
        self.find_error = None;
        self.clear_multi_cursor_state();

        if self.find_term.is_empty() {
            self.current_match = 0;
            self.selection = None;
            return;
        }

        match self.build_find_regex() {
            Ok(re) => {
                for (line_idx, line) in self.lines.iter().enumerate() {
                    for mat in re.find_iter(line) {
                        // 바이트 인덱스를 문자 인덱스로 변환
                        let byte_start = mat.start();
                        let byte_end = mat.end();
                        let char_start = line[..byte_start].chars().count();
                        let char_end = char_start + line[byte_start..byte_end].chars().count();
                        self.match_positions.push((line_idx, char_start, char_end));
                    }
                }
            }
            Err(e) => {
                self.find_error = Some(format!("Regex error: {}", e));
            }
        }

        self.current_match = 0;
        if self.match_positions.is_empty() {
            self.selection = None;
        } else {
            self.goto_current_match();
        }
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

    fn goto_first_match_at_or_after(&mut self, line: usize, col: usize) {
        if self.match_positions.is_empty() {
            return;
        }

        self.current_match = self
            .match_positions
            .iter()
            .position(|(match_line, match_start, _)| {
                *match_line > line || (*match_line == line && *match_start >= col)
            })
            .unwrap_or(0);
        self.goto_current_match();
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
            let matched_text: String = chars[start..end].iter().collect();
            let replacement = if self.find_options.use_regex {
                match self.build_find_regex() {
                    Ok(re) => re
                        .replace(&matched_text, self.replace_input.as_str())
                        .to_string(),
                    Err(_) => return,
                }
            } else {
                self.replace_input.clone()
            };

            let new_line: String = chars[..start]
                .iter()
                .chain(replacement.chars().collect::<Vec<_>>().iter())
                .chain(chars[end..].iter())
                .collect();

            let old_content = self.lines[line].clone();
            let next_col = start + replacement.chars().count();

            if old_content == new_line {
                self.find_next();
                return;
            }

            self.lines[line] = new_line;

            self.push_undo(EditAction::Replace {
                line,
                old_content,
                new_content: self.lines[line].clone(),
            });

            self.selection = None;
            self.perform_find();
            self.goto_first_match_at_or_after(line, next_col);
        }
    }

    /// 모두 바꾸기
    pub fn replace_all(&mut self) {
        if self.find_term.is_empty() {
            self.match_positions.clear();
            self.current_match = 0;
            self.selection = None;
            self.find_error = None;
            return;
        }

        match self.build_find_regex() {
            Ok(re) => {
                let mut actions = Vec::new();
                let use_regex = self.find_options.use_regex;
                let replace_input = self.replace_input.clone();

                for (line_idx, line) in self.lines.iter_mut().enumerate() {
                    let old_content = line.clone();
                    let new_content = if use_regex {
                        re.replace_all(line, replace_input.as_str()).to_string()
                    } else {
                        re.replace_all(line, regex::NoExpand(replace_input.as_str()))
                            .to_string()
                    };

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
                // Replacements may have shortened the cursor's line; clamp before any later edit.
                self.clamp_cursor();
                self.perform_find();
            }
            Err(e) => {
                self.match_positions.clear();
                self.current_match = 0;
                self.selection = None;
                self.find_error = Some(format!("Regex error: {}", e));
            }
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

    /// 문자가 단어 문자인지 확인
    fn is_word_char(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    fn find_whole_word_in_byte_range(
        line: &str,
        word: &str,
        start_byte: usize,
        end_byte: usize,
    ) -> Option<(usize, usize)> {
        if word.is_empty() {
            return None;
        }

        let start_byte = start_byte.min(line.len());
        let end_byte = end_byte.min(line.len());
        if start_byte >= end_byte {
            return None;
        }

        for (rel_byte_pos, _) in line[start_byte..end_byte].match_indices(word) {
            let byte_pos = start_byte + rel_byte_pos;
            let word_end_byte = byte_pos + word.len();
            let is_word_start = line[..byte_pos]
                .chars()
                .next_back()
                .map(|c| !Self::is_word_char(c))
                .unwrap_or(true);
            let is_word_end = line[word_end_byte..]
                .chars()
                .next()
                .map(|c| !Self::is_word_char(c))
                .unwrap_or(true);

            if is_word_start && is_word_end {
                let char_pos = line[..byte_pos].chars().count();
                let word_end = line[..word_end_byte].chars().count();
                return Some((char_pos, word_end));
            }
        }

        None
    }

    /// 단어 왼쪽으로 이동 (Ctrl+Left)
    pub fn move_word_left(&mut self, extend_selection: bool) {
        if !extend_selection && self.move_all_cursors_by_word(false) {
            return;
        }

        let old_line = self.cursor_line;
        let old_col = self.cursor_col;
        let had_selection = self.selection.is_some();

        if !extend_selection {
            self.selection = None;
        }

        let line = &self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();

        if self.cursor_col == 0 {
            // 이전 줄 끝으로 이동
            if self.cursor_line > 0 {
                self.cursor_line -= 1;
                self.cursor_col = self.lines[self.cursor_line].chars().count();
            }
        } else {
            let mut col = self.cursor_col;
            // 왼쪽이 non-word_char면 건너뛰기 (단어 시작에서 이전 단어로 이동하기 위해)
            // 선택 모드에서도 커서가 단어 시작에 있을 때 이전으로 이동 가능해야 함
            if col > 0 && !Self::is_word_char(chars[col - 1]) {
                while col > 0 && !Self::is_word_char(chars[col - 1]) {
                    col -= 1;
                }
            }
            // 단어 건너뛰기
            while col > 0 && Self::is_word_char(chars[col - 1]) {
                col -= 1;
            }
            self.cursor_col = col;
        }

        // 블록 커서 선택: 현재 커서 위치의 문자 포함
        if extend_selection
            && !had_selection
            && (self.cursor_line != old_line || self.cursor_col != old_col)
        {
            self.selection = Some(Selection::new(old_line, old_col));
        }
        if extend_selection {
            if let Some(ref mut sel) = self.selection {
                sel.end_line = self.cursor_line;
                let line_len = self.lines[self.cursor_line].chars().count();
                sel.end_col = (self.cursor_col + 1).min(line_len);
            }
        }

        self.update_scroll();
    }

    /// 단어 오른쪽으로 이동 (Ctrl+Right)
    pub fn move_word_right(&mut self, extend_selection: bool) {
        if !extend_selection && self.move_all_cursors_by_word(true) {
            return;
        }

        let old_line = self.cursor_line;
        let old_col = self.cursor_col;
        let had_selection = self.selection.is_some();

        if !extend_selection {
            self.selection = None;
        }

        let line = &self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();
        let line_len = chars.len();

        if self.cursor_col >= line_len {
            // 다음 줄 시작으로 이동
            if self.cursor_line + 1 < self.lines.len() {
                self.cursor_line += 1;
                self.cursor_col = 0;
            }
        } else {
            let mut col = self.cursor_col;

            // 현재 단어 끝까지 이동
            while col < line_len && Self::is_word_char(chars[col]) {
                col += 1;
            }

            // 선택 모드일 때: 마지막 word_char 위치에 멈춤 (공백 제외)
            // 단, 한 칸만 이동한 경우 (단어 끝에 있었던 경우) 다음 단어로 계속 이동
            if extend_selection && col > self.cursor_col {
                if col == self.cursor_col + 1 {
                    // 단어 끝에 있었음 - non-word_char 건너뛰고 다음 단어 끝까지
                    while col < line_len && !Self::is_word_char(chars[col]) {
                        col += 1;
                    }
                    let next_start = col;
                    while col < line_len && Self::is_word_char(chars[col]) {
                        col += 1;
                    }
                    if col > next_start {
                        col -= 1;
                    }
                } else {
                    col -= 1;
                }
            }

            // 이동하지 않았으면 (non-word_char 위에 있었으면) 건너뛰고 다음 단어로
            if col == self.cursor_col && col < line_len {
                while col < line_len && !Self::is_word_char(chars[col]) {
                    col += 1;
                }
                let next_start = col;
                while col < line_len && Self::is_word_char(chars[col]) {
                    col += 1;
                }
                if extend_selection && col > next_start {
                    col -= 1;
                }
            }

            self.cursor_col = col;
        }

        // 블록 커서 선택: 현재 커서 위치의 문자 포함
        if extend_selection
            && !had_selection
            && (self.cursor_line != old_line || self.cursor_col != old_col)
        {
            self.selection = Some(Selection::new(old_line, old_col));
        }
        if extend_selection {
            if let Some(ref mut sel) = self.selection {
                sel.end_line = self.cursor_line;
                let line_len = self.lines[self.cursor_line].chars().count();
                sel.end_col = (self.cursor_col + 1).min(line_len);
            }
        }

        self.update_scroll();
    }

    /// 단어 삭제 (뒤, Ctrl+Backspace)
    pub fn delete_word_backward(&mut self) {
        if self.has_selection_range() {
            self.delete_selection();
            return;
        } else if self.selection.is_some() {
            self.selection = None;
        }

        let line = &self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();

        if self.cursor_col == 0 {
            if self.cursor_line > 0 {
                // 이전 줄과 병합
                self.delete_backward();
            }
            return;
        }

        let start_col = self.cursor_col;
        let mut col = self.cursor_col;

        // 공백 건너뛰기
        while col > 0 && !Self::is_word_char(chars[col - 1]) {
            col -= 1;
        }
        // 단어 건너뛰기
        while col > 0 && Self::is_word_char(chars[col - 1]) {
            col -= 1;
        }

        let deleted_text: String = chars[col..start_col].iter().collect();
        let new_line: String = chars[..col]
            .iter()
            .chain(chars[start_col..].iter())
            .collect();

        self.push_undo(EditAction::Delete {
            line: self.cursor_line,
            col,
            text: deleted_text,
        });

        self.lines[self.cursor_line] = new_line;
        self.cursor_col = col;
        self.update_scroll();
    }

    /// 단어 삭제 (앞, Ctrl+Delete)
    pub fn delete_word_forward(&mut self) {
        if self.has_selection_range() {
            self.delete_selection();
            return;
        } else if self.selection.is_some() {
            self.selection = None;
        }

        let line = &self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();
        let line_len = chars.len();

        if self.cursor_col >= line_len {
            if self.cursor_line + 1 < self.lines.len() {
                // 다음 줄과 병합
                self.delete_forward();
            }
            return;
        }

        let start_col = self.cursor_col;
        let mut col = self.cursor_col;

        // 현재 단어 끝까지 이동
        while col < line_len && Self::is_word_char(chars[col]) {
            col += 1;
        }
        // 공백 건너뛰기
        while col < line_len && !Self::is_word_char(chars[col]) {
            col += 1;
        }

        let deleted_text: String = chars[start_col..col].iter().collect();
        let new_line: String = chars[..start_col]
            .iter()
            .chain(chars[col..].iter())
            .collect();

        self.push_undo(EditAction::Delete {
            line: self.cursor_line,
            col: start_col,
            text: deleted_text,
        });

        self.lines[self.cursor_line] = new_line;
        self.update_scroll();
    }

    /// 커서 위치의 단어 범위 찾기
    fn get_word_at_cursor(&self) -> Option<(usize, usize, String)> {
        let line = &self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();

        if chars.is_empty() {
            return None;
        }

        let col = self.cursor_col.min(chars.len().saturating_sub(1));

        // 현재 위치가 단어 문자가 아니면 None
        if !Self::is_word_char(chars[col]) {
            return None;
        }

        // 단어 시작 찾기
        let mut start = col;
        while start > 0 && Self::is_word_char(chars[start - 1]) {
            start -= 1;
        }

        // 단어 끝 찾기
        let mut end = col;
        while end < chars.len() && Self::is_word_char(chars[end]) {
            end += 1;
        }

        let word: String = chars[start..end].iter().collect();
        Some((start, end, word))
    }

    /// 커서 위치 단어 선택 (Ctrl+D 첫 번째)
    pub fn select_word_at_cursor(&mut self) {
        if let Some((start, end, word)) = self.get_word_at_cursor() {
            self.selection = Some(Selection {
                start_line: self.cursor_line,
                start_col: start,
                end_line: self.cursor_line,
                end_col: end,
            });
            self.cursor_col = end;
            self.last_word_selection = Some(word);
            self.cursors.clear();
        }
    }

    fn selected_occurrence_ranges(&self) -> Vec<(usize, usize, usize)> {
        let word = match &self.last_word_selection {
            Some(word) if !word.is_empty() => word,
            _ => return Vec::new(),
        };
        let word_len = word.chars().count();
        let mut ranges = Vec::new();

        if let Some(sel) = self.selection {
            if let Some((line, start, end_line, end)) = self.clamped_selection_range(sel) {
                if line == end_line && self.get_selected_text() == *word {
                    ranges.push((line, start, end));
                }
            }
        }

        for &(line, end) in &self.cursors {
            if line >= self.lines.len() || end < word_len {
                continue;
            }
            let start = end - word_len;
            let chars: Vec<char> = self.lines[line].chars().collect();
            if end <= chars.len() {
                let text: String = chars[start..end].iter().collect();
                if text == *word {
                    ranges.push((line, start, end));
                }
            }
        }

        ranges.sort_unstable();
        ranges.dedup();
        ranges
    }

    fn replace_selected_occurrences_with(&mut self, replacement: &str) -> bool {
        if self.insertion_multi_cursor_active() {
            if replacement.is_empty() {
                return false;
            }
            return self.insert_at_multi_cursors(replacement);
        }

        if self.cursors.is_empty() {
            return false;
        }
        if replacement.contains('\n') || replacement.contains('\r') {
            self.set_message("Multi-cursor paste supports single-line text only", 50);
            return true;
        }

        let ranges = self.selected_occurrence_ranges();
        if ranges.len() < 2 {
            return false;
        }

        let active_range = self
            .selection
            .and_then(|sel| self.clamped_selection_range(sel))
            .map(|(line, start, _, end)| (line, start, end));

        self.replace_ranges_with_text(&ranges, replacement, active_range)
    }

    fn push_cursor_once(&mut self, cursor: (usize, usize)) {
        if !self.cursors.contains(&cursor) {
            self.cursors.push(cursor);
        }
    }

    /// 다음 동일 단어 선택 (Ctrl+D 반복)
    pub fn select_next_occurrence(&mut self) {
        // 선택이 없으면 현재 단어 선택
        if self.selection.is_none() || self.last_word_selection.is_none() {
            self.select_word_at_cursor();
            return;
        }

        let selected_text = self.get_selected_text();
        if self.last_word_selection.as_deref() != Some(selected_text.as_str()) {
            if !selected_text.is_empty() && selected_text.chars().all(Self::is_word_char) {
                self.last_word_selection = Some(selected_text);
                self.cursors.clear();
            } else {
                self.select_word_at_cursor();
                return;
            }
        }

        let word = match &self.last_word_selection {
            Some(w) => w.clone(),
            None => return,
        };

        // 현재 커서 위치 이후에서 다음 occurrence 찾기
        let search_start_line = self.cursor_line;
        let search_start_col = self.cursor_col; // 문자 인덱스

        for line_idx in search_start_line..self.lines.len() {
            let line = &self.lines[line_idx];
            let start_char_col = if line_idx == search_start_line {
                search_start_col
            } else {
                0
            };

            // 문자 인덱스를 바이트 인덱스로 변환
            let start_byte: usize = line
                .chars()
                .take(start_char_col)
                .map(|c| c.len_utf8())
                .sum();

            if let Some((char_pos, word_end)) =
                Self::find_whole_word_in_byte_range(line, &word, start_byte, line.len())
            {
                // 현재 선택 위치를 다중 커서에 추가
                if let Some(sel) = &self.selection {
                    let (_, _, el, ec) = sel.normalized();
                    self.push_cursor_once((el, ec));
                }

                // 새 위치로 선택 이동
                self.cursor_line = line_idx;
                self.cursor_col = word_end;
                self.selection = Some(Selection {
                    start_line: line_idx,
                    start_col: char_pos,
                    end_line: line_idx,
                    end_col: word_end,
                });
                self.update_scroll();
                return;
            }
        }

        // 파일 끝에서 시작으로 wrap around
        for line_idx in 0..=search_start_line {
            let line = &self.lines[line_idx];

            // 검색 범위 끝 계산 (바이트 인덱스)
            let end_byte = if line_idx == search_start_line {
                // 원래 검색 시작 전까지만
                let sel = self.selection.as_ref().unwrap();
                let (sl, sc, _, _) = sel.normalized();
                let end_char = if line_idx == sl { sc } else { 0 };
                line.chars().take(end_char).map(|c| c.len_utf8()).sum()
            } else {
                line.len()
            };

            if let Some((char_pos, word_end)) =
                Self::find_whole_word_in_byte_range(line, &word, 0, end_byte)
            {
                if let Some(sel) = &self.selection {
                    let (_, _, el, ec) = sel.normalized();
                    self.push_cursor_once((el, ec));
                }

                self.cursor_line = line_idx;
                self.cursor_col = word_end;
                self.selection = Some(Selection {
                    start_line: line_idx,
                    start_col: char_pos,
                    end_line: line_idx,
                    end_col: word_end,
                });
                self.update_scroll();
                return;
            }
        }
    }

    /// 현재 줄 선택 (Ctrl+L)
    pub fn select_line(&mut self) {
        let line_len = self.lines[self.cursor_line].chars().count();
        self.selection = Some(Selection {
            start_line: self.cursor_line,
            start_col: 0,
            end_line: self.cursor_line,
            end_col: line_len,
        });
        self.cursor_col = line_len;
    }

    /// 아래에 빈 줄 삽입 (Ctrl+Enter)
    pub fn insert_line_below(&mut self) {
        let current_line = self.cursor_line;
        // 현재 줄의 들여쓰기 가져오기
        let indent = if self.auto_indent {
            let line = &self.lines[current_line];
            line.chars()
                .take_while(|c| c.is_whitespace())
                .collect::<String>()
        } else {
            String::new()
        };

        self.ensure_line_endings();
        let old_line_ending = self.line_ending_at(current_line);
        let inserted_ending = self.default_line_ending();
        self.set_line_ending_at(current_line, inserted_ending.clone());
        self.insert_line_with_ending(current_line + 1, indent.clone(), old_line_ending.clone());
        self.cursor_line += 1;
        self.cursor_col = indent.chars().count();

        self.push_undo(EditAction::Batch {
            actions: vec![
                EditAction::SetLineEnding {
                    line: current_line,
                    old_line_ending: old_line_ending.clone(),
                    new_line_ending: inserted_ending,
                },
                EditAction::InsertLine {
                    line: current_line + 1,
                    content: indent,
                    line_ending: old_line_ending,
                },
            ],
        });
        self.update_scroll();
    }

    /// 위에 빈 줄 삽입 (Ctrl+Shift+Enter)
    pub fn insert_line_above(&mut self) {
        // 현재 줄의 들여쓰기 가져오기
        let indent = if self.auto_indent {
            let line = &self.lines[self.cursor_line];
            line.chars()
                .take_while(|c| c.is_whitespace())
                .collect::<String>()
        } else {
            String::new()
        };

        let inserted_ending = self.default_line_ending();
        self.insert_line_with_ending(self.cursor_line, indent.clone(), inserted_ending.clone());
        self.cursor_col = indent.chars().count();

        self.push_undo(EditAction::InsertLine {
            line: self.cursor_line,
            content: indent,
            line_ending: inserted_ending,
        });
        self.update_scroll();
    }

    /// 줄 복사 위로 (Shift+Alt+Up)
    pub fn copy_line_up(&mut self) {
        let line_content = self.lines[self.cursor_line].clone();
        let inserted_ending = self.default_line_ending();
        self.insert_line_with_ending(
            self.cursor_line,
            line_content.clone(),
            inserted_ending.clone(),
        );

        self.push_undo(EditAction::InsertLine {
            line: self.cursor_line,
            content: line_content,
            line_ending: inserted_ending,
        });
        self.update_scroll();
    }

    /// 줄 복사 아래로 (Shift+Alt+Down)
    pub fn copy_line_down(&mut self) {
        let current_line = self.cursor_line;
        self.ensure_line_endings();
        let line_content = self.lines[current_line].clone();
        let old_line_ending = self.line_ending_at(current_line);
        let inserted_ending = self.default_line_ending();
        self.set_line_ending_at(current_line, inserted_ending.clone());
        self.insert_line_with_ending(
            current_line + 1,
            line_content.clone(),
            old_line_ending.clone(),
        );
        self.cursor_line += 1;

        self.push_undo(EditAction::Batch {
            actions: vec![
                EditAction::SetLineEnding {
                    line: current_line,
                    old_line_ending: old_line_ending.clone(),
                    new_line_ending: inserted_ending,
                },
                EditAction::InsertLine {
                    line: current_line + 1,
                    content: line_content,
                    line_ending: old_line_ending,
                },
            ],
        });
        self.update_scroll();
    }

    /// 잘라내기 (선택 없으면 줄 전체)
    pub fn cut_line_or_selection(&mut self) {
        if self.has_selection_range() {
            // 선택 영역 잘라내기
            self.clipboard = self.get_selected_text();
            self.delete_selection();
        } else {
            // 줄 전체 잘라내기
            if self.lines.len() > 1 {
                self.clipboard = self.lines[self.cursor_line].clone()
                    + &self.line_ending_for_clipboard(self.cursor_line);
                self.ensure_line_endings();
                let deleted_line = self.cursor_line;
                let previous_line_ending =
                    if deleted_line + 1 == self.lines.len() && deleted_line > 0 {
                        Some(self.line_ending_at(deleted_line - 1))
                    } else {
                        None
                    };
                let (content, line_ending) = self
                    .remove_line_with_ending(deleted_line)
                    .unwrap_or_else(|| (String::new(), String::new()));

                let delete_action = EditAction::DeleteLine {
                    line: deleted_line,
                    content,
                    line_ending: line_ending.clone(),
                };
                if let Some(old_previous_line_ending) = previous_line_ending {
                    self.push_undo(EditAction::Batch {
                        actions: vec![
                            delete_action,
                            EditAction::SetLineEnding {
                                line: deleted_line - 1,
                                old_line_ending: old_previous_line_ending,
                                new_line_ending: line_ending,
                            },
                        ],
                    });
                } else {
                    self.push_undo(delete_action);
                }

                if self.cursor_line >= self.lines.len() {
                    self.cursor_line = self.lines.len() - 1;
                }
                self.cursor_col = self
                    .cursor_col
                    .min(self.lines[self.cursor_line].chars().count());
            } else {
                // 유일한 줄이면 내용만 잘라내기
                self.ensure_line_endings();
                self.clipboard = self.lines[0].clone() + &self.line_ending_for_clipboard(0);
                let old_content = self.lines[0].clone();
                let old_line_ending = self.line_ending_at(0);
                if old_content.is_empty() && old_line_ending.is_empty() {
                    self.update_scroll();
                    return;
                }

                self.lines[0] = String::new();
                self.set_line_ending_at(0, String::new());
                self.cursor_col = 0;

                self.push_undo(EditAction::Batch {
                    actions: vec![
                        EditAction::Replace {
                            line: 0,
                            old_content,
                            new_content: String::new(),
                        },
                        EditAction::SetLineEnding {
                            line: 0,
                            old_line_ending,
                            new_line_ending: String::new(),
                        },
                    ],
                });
            }
            self.update_scroll();
        }
    }

    /// 들여쓰기 (Ctrl+])
    pub fn indent(&mut self) {
        let indent_str = if self.use_tabs {
            "\t".to_string()
        } else {
            " ".repeat(self.tab_size)
        };

        if let Some((start_line, _, end_line, _)) = self
            .selection
            .and_then(|sel| self.clamped_selection_range(sel))
        {
            let mut actions = Vec::new();

            for line_idx in start_line..=end_line {
                let old_content = self.lines[line_idx].clone();
                self.lines[line_idx] = format!("{}{}", indent_str, old_content);
                actions.push(EditAction::Replace {
                    line: line_idx,
                    old_content,
                    new_content: self.lines[line_idx].clone(),
                });
            }

            self.push_undo(EditAction::Batch { actions });
        } else {
            self.selection = None;
            let old_content = self.lines[self.cursor_line].clone();
            self.lines[self.cursor_line] = format!("{}{}", indent_str, old_content);
            self.cursor_col += indent_str.len();

            self.push_undo(EditAction::Replace {
                line: self.cursor_line,
                old_content,
                new_content: self.lines[self.cursor_line].clone(),
            });
        }
    }

    /// 내어쓰기 (Ctrl+[ 또는 Shift+Tab)
    pub fn outdent(&mut self) {
        let tab_size = self.tab_size;

        let remove_indent = |line: &str, tab_size: usize| -> (String, usize) {
            let chars: Vec<char> = line.chars().collect();

            if chars.first() == Some(&'\t') {
                (chars[1..].iter().collect(), 1)
            } else {
                let spaces_to_remove = chars
                    .iter()
                    .take(tab_size)
                    .take_while(|c| **c == ' ')
                    .count();
                (chars[spaces_to_remove..].iter().collect(), spaces_to_remove)
            }
        };

        if let Some((start_line, _, end_line, _)) = self
            .selection
            .and_then(|sel| self.clamped_selection_range(sel))
        {
            let mut actions = Vec::new();

            for line_idx in start_line..=end_line {
                let old_content = self.lines[line_idx].clone();
                let (new_content, removed) = remove_indent(&old_content, tab_size);
                if removed > 0 {
                    self.lines[line_idx] = new_content.clone();
                    actions.push(EditAction::Replace {
                        line: line_idx,
                        old_content,
                        new_content,
                    });
                }
            }

            if !actions.is_empty() {
                self.push_undo(EditAction::Batch { actions });
            }
            // Lines may have shrunk; keep cursor_col within the (possibly shorter) current line.
            self.clamp_cursor();
        } else {
            self.selection = None;
            let old_content = self.lines[self.cursor_line].clone();
            let (new_content, removed) = remove_indent(&old_content, tab_size);
            if removed > 0 {
                self.lines[self.cursor_line] = new_content.clone();
                self.cursor_col = self.cursor_col.saturating_sub(removed);

                self.push_undo(EditAction::Replace {
                    line: self.cursor_line,
                    old_content,
                    new_content,
                });
            }
        }
    }

    /// 언어별 주석 문자 가져오기
    fn get_comment_string(&self) -> Option<&'static str> {
        match self.language {
            Language::Rust
            | Language::C
            | Language::Cpp
            | Language::Java
            | Language::JavaScript
            | Language::TypeScript
            | Language::Go
            | Language::Swift
            | Language::Kotlin => Some("//"),

            Language::Python
            | Language::Shell
            | Language::Ruby
            | Language::Yaml
            | Language::Toml => Some("#"),

            Language::Sql => Some("--"),

            Language::Html | Language::Xml | Language::Css => None,

            Language::Php => Some("//"),

            Language::Markdown | Language::Json | Language::Plain => Some("//"),
        }
    }

    fn first_non_whitespace_byte(line: &str) -> usize {
        line.char_indices()
            .find(|(_, ch)| !ch.is_whitespace())
            .map(|(idx, _)| idx)
            .unwrap_or(line.len())
    }

    fn line_is_commented(line: &str, comment: &str, comment_with_space: &str) -> bool {
        let start = Self::first_non_whitespace_byte(line);
        let rest = &line[start..];
        rest.starts_with(comment_with_space) || rest.starts_with(comment)
    }

    fn toggle_comment_content(
        line: &str,
        comment: &str,
        comment_with_space: &str,
        remove_comment: bool,
    ) -> String {
        let start = Self::first_non_whitespace_byte(line);
        let (indent, rest) = line.split_at(start);

        if remove_comment {
            if let Some(uncommented) = rest.strip_prefix(comment_with_space) {
                format!("{}{}", indent, uncommented)
            } else if let Some(uncommented) = rest.strip_prefix(comment) {
                format!("{}{}", indent, uncommented)
            } else {
                line.to_string()
            }
        } else {
            format!("{}{}{}", indent, comment_with_space, rest)
        }
    }

    /// 주석 토글 (Ctrl+/)
    pub fn toggle_comment(&mut self) {
        let comment = match self.get_comment_string() {
            Some(c) => c,
            None => return,
        };
        let comment_with_space = format!("{} ", comment);

        if let Some((start_line, _, mut end_line, end_col)) = self
            .selection
            .and_then(|sel| self.clamped_selection_range(sel))
        {
            // 블록 커서: end_col = cursor + 1이므로 end_col <= 1이면 cursor가 라인 시작(col 0)
            // 여러 줄 선택에서 마지막 라인에 실제 선택된 문자가 없으므로 제외
            if end_col <= 1 && end_line > start_line {
                end_line -= 1;
            }
            let mut actions = Vec::new();

            // 모든 줄이 주석인지 확인 (들여쓰기 뒤 실제 코드 시작 기준)
            let all_commented = (start_line..=end_line)
                .all(|i| Self::line_is_commented(&self.lines[i], comment, &comment_with_space));

            for line_idx in start_line..=end_line {
                let old_content = self.lines[line_idx].clone();
                let new_content = Self::toggle_comment_content(
                    &old_content,
                    comment,
                    &comment_with_space,
                    all_commented,
                );

                self.lines[line_idx] = new_content.clone();
                actions.push(EditAction::Replace {
                    line: line_idx,
                    old_content,
                    new_content,
                });
            }

            self.push_undo(EditAction::Batch { actions });
        } else {
            self.selection = None;
            let old_content = self.lines[self.cursor_line].clone();
            let was_commented = Self::line_is_commented(&old_content, comment, &comment_with_space);
            let new_content = Self::toggle_comment_content(
                &old_content,
                comment,
                &comment_with_space,
                was_commented,
            );

            self.lines[self.cursor_line] = new_content.clone();

            self.push_undo(EditAction::Replace {
                line: self.cursor_line,
                old_content,
                new_content,
            });
        }
        // Uncommenting shortens the line; keep cursor_col within the current line.
        self.clamp_cursor();
    }
}

pub fn draw(
    frame: &mut Frame,
    state: &mut EditorState,
    area: Rect,
    theme: &Theme,
    kb: &crate::keybindings::Keybindings,
) {
    let border_color = if state.modified {
        theme.editor.modified_mark
    } else {
        theme.editor.border
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 3 {
        return;
    }

    // 화면 크기 업데이트 (스크롤 계산에 사용)
    state.visible_height = inner.height.saturating_sub(2) as usize; // 헤더와 푸터 제외
                                                                    // visible_width는 Content 섹션에서 동적 줄 번호 폭 기준으로 설정됨

    // Header
    let file_name = state
        .file_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "New File".to_string());

    let remote_span = if let Some(ref origin) = state.remote_origin {
        Span::styled(
            format!("[Remote: {}] ", origin.remote_path),
            Style::default().fg(theme.editor.remote_path_text),
        )
    } else {
        Span::raw("")
    };

    let header = Line::from(vec![
        Span::raw(" "),
        if state.modified {
            Span::styled("✻", Style::default().fg(theme.editor.modified_mark))
        } else {
            Span::raw("")
        },
        Span::styled(format!("{} ", file_name), theme.header_style()),
        remote_span,
        Span::styled(format!("[{}] ", state.language.name()), theme.dim_style()),
        Span::styled(
            format!(
                "Ln {}, Col {} ",
                state.cursor_line + 1,
                state.cursor_visual_col() + 1
            ),
            theme.dim_style(),
        ),
        if !state.undo_stack.is_empty() {
            Span::styled(
                format!("Undo:{} ", state.undo_stack.len()),
                theme.dim_style(),
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

    // 줄 번호 폭 동적 계산 (총 줄 수 기준)
    let total_lines = state.lines.len();
    let line_num_width = if total_lines == 0 {
        1
    } else {
        ((total_lines as f64).log10().floor() as usize) + 1
    }
    .max(4); // 최소 4자리
    let line_num_col_width = line_num_width + 1; // 공백 포함

    // visible_width 업데이트
    state.visible_width = inner.width.saturating_sub(line_num_col_width as u16 + 1) as usize;
    if state.word_wrap && state.scroll < state.lines.len() {
        let max_offset = state.count_wrapped_rows(state.scroll).saturating_sub(1);
        state.wrap_scroll_offset = state.wrap_scroll_offset.min(max_offset);
    }

    // 선택 영역 정규화
    let selection = state
        .selection
        .and_then(|sel| state.clamped_selection_range(sel));

    // 하이라이터
    let mut highlighter = state.highlighter.clone();
    if let Some(ref mut hl) = highlighter {
        hl.reset();
        for line in state.lines.iter().take(state.scroll) {
            hl.tokenize_line(line);
        }
    }

    if state.word_wrap {
        // Word wrap 모드: 논리적 줄을 시각적 세그먼트로 분할하여 렌더링
        let content_width = state.visible_width;
        let in_find_mode = state.find_mode != FindReplaceMode::None;
        let mut visual_row: usize = 0;
        let mut line_idx = state.scroll;

        while visual_row < content_height && line_idx < state.lines.len() {
            let original_line = &state.lines[line_idx];
            let (expanded_line, visual_to_orig) = state.expand_tabs_with_mapping(original_line);
            let is_cursor_line = line_idx == state.cursor_line;

            // 하이라이터로 토큰화 (논리적 줄당 1회만)
            let orig_chars: Vec<char> = original_line.chars().collect();
            let orig_styles: Vec<ratatui::style::Style> = if let Some(ref mut hl) = highlighter {
                let tokens = hl.tokenize_line(original_line);
                if !tokens.is_empty() {
                    let mut styles = vec![theme.normal_style(); orig_chars.len()];
                    let mut char_idx = 0;
                    for token in &tokens {
                        let token_len = token.text.chars().count();
                        let style = hl.style_for(token.token_type);
                        for i in char_idx..(char_idx + token_len).min(orig_chars.len()) {
                            styles[i] = style;
                        }
                        char_idx += token_len;
                    }
                    styles
                } else {
                    vec![]
                }
            } else {
                vec![]
            };

            // 줄을 visual column 기준 세그먼트로 분할
            let seg_starts = EditorState::compute_wrap_segments(&expanded_line, content_width);

            let first_segment = if line_idx == state.scroll {
                state.wrap_scroll_offset
            } else {
                0
            };

            for (seg_idx, &seg_start_visual) in seg_starts.iter().enumerate().skip(first_segment) {
                if visual_row >= content_height {
                    break;
                }

                let is_first = seg_idx == 0;

                // 줄 번호: 첫 세그먼트만 표시
                let line_num_style = if is_cursor_line {
                    Style::default()
                        .fg(theme.editor.line_number)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.editor.line_number)
                };

                let line_num_span = if is_first {
                    Span::styled(
                        format!("{:>width$} ", line_idx + 1, width = line_num_width),
                        line_num_style,
                    )
                } else {
                    Span::styled(
                        format!("{:>width$} ", "", width = line_num_width),
                        line_num_style,
                    )
                };

                let content_spans = render_editor_line(
                    &expanded_line,
                    original_line,
                    &visual_to_orig,
                    line_idx,
                    state,
                    &selection,
                    &mut None, // 하이라이터 건너뜀 (이미 토큰화됨)
                    theme,
                    is_cursor_line,
                    in_find_mode,
                    seg_start_visual,
                    content_width,
                    Some(&orig_styles),
                );

                let mut spans = vec![line_num_span];
                spans.extend(content_spans);

                frame.render_widget(
                    Paragraph::new(Line::from(spans)),
                    Rect::new(inner.x, inner.y + 1 + visual_row as u16, inner.width, 1),
                );

                visual_row += 1;
            }

            line_idx += 1;
        }
    } else {
        for (i, original_line) in state
            .lines
            .iter()
            .skip(state.scroll)
            .take(content_height)
            .enumerate()
        {
            // TAB을 visual column 기반으로 스페이스로 확장 (매핑 정보 포함)
            let (expanded_line, visual_to_orig) = state.expand_tabs_with_mapping(original_line);
            let line_num = state.scroll + i;
            let is_cursor_line = line_num == state.cursor_line;

            // 줄 번호
            let line_num_style = if is_cursor_line {
                Style::default()
                    .fg(theme.editor.line_number)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.editor.line_number)
            };

            let line_num_span = Span::styled(
                format!("{:>width$} ", line_num + 1, width = line_num_width),
                line_num_style,
            );

            // 라인 렌더링
            let in_find_mode = state.find_mode != FindReplaceMode::None;
            let content_spans = render_editor_line(
                &expanded_line,
                original_line,
                &visual_to_orig,
                line_num,
                state,
                &selection,
                &mut highlighter,
                theme,
                is_cursor_line,
                in_find_mode,
                state.horizontal_scroll,
                state.visible_width,
                None,
            );

            let mut spans = vec![line_num_span];
            spans.extend(content_spans);

            frame.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect::new(inner.x, inner.y + 1 + i as u16, inner.width, 1),
            );
        }
    }

    // 스크롤바
    let total_lines = state.lines.len();
    if total_lines > content_height {
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));

        let max_scroll = total_lines.saturating_sub(content_height);
        let mut scrollbar_state = ScrollbarState::new(max_scroll + 1).position(state.scroll);

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

                // Word wrap 표시자
                if state.word_wrap {
                    footer_spans.push(Span::styled(
                        "Wrap ",
                        Style::default().fg(theme.editor.wrap_indicator),
                    ));
                }

                // 단축키 안내 (keybindings에서 동적으로)
                let shortcuts: Vec<(String, &str)> = vec![
                    (kb.editor_first_key(EditorAction::Save).to_string(), "save "),
                    (
                        kb.editor_first_key(EditorAction::DeleteLine).to_string(),
                        "del ",
                    ),
                    (
                        kb.editor_first_key(EditorAction::DuplicateLine).to_string(),
                        "dup ",
                    ),
                    (
                        kb.editor_first_key(EditorAction::ToggleComment).to_string(),
                        "comment ",
                    ),
                    (
                        kb.editor_first_key(EditorAction::SelectNextOccurrence)
                            .to_string(),
                        "select ",
                    ),
                    (kb.editor_first_key(EditorAction::Find).to_string(), "find "),
                    (
                        kb.editor_first_key(EditorAction::Replace).to_string(),
                        "replace ",
                    ),
                    (
                        kb.editor_first_key(EditorAction::GotoLine).to_string(),
                        "goto ",
                    ),
                    (
                        kb.editor_first_key(EditorAction::ToggleWordWrap)
                            .to_string(),
                        "wrap ",
                    ),
                    (kb.editor_first_key(EditorAction::Exit).to_string(), "exit"),
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
        FindReplaceMode::Find | FindReplaceMode::Replace => {
            let find_opts = format!(
                "[{}{}{}]",
                if state.find_options.case_sensitive {
                    "Aa"
                } else {
                    "aa"
                },
                if state.find_options.use_regex {
                    " Re"
                } else {
                    ""
                },
                if state.find_options.whole_word {
                    " W"
                } else {
                    ""
                }
            );

            let (match_info, match_info_style) = if let Some(ref err) = state.find_error {
                // 정규식 에러 표시 (빨간색)
                let truncated = if err.chars().count() > 30 {
                    let t: String = err.chars().take(27).collect();
                    format!(" {}... ", t)
                } else {
                    format!(" {} ", err)
                };
                (truncated, Style::default().fg(Color::Red))
            } else if !state.match_positions.is_empty() {
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
            } else if !state.find_term.is_empty() {
                (" No matches ".to_string(), theme.dim_style())
            } else {
                (String::new(), theme.dim_style())
            };

            let cursor_style = Style::default()
                .fg(theme.editor.bg)
                .bg(theme.editor.selection_bg)
                .add_modifier(Modifier::SLOW_BLINK);
            let input_style = Style::default().fg(theme.editor.find_input_text);

            // Find 입력 필드
            let find_chars: Vec<char> = state.find_input.chars().collect();
            let find_cursor = state.find_cursor_pos.min(find_chars.len());
            let find_before: String = find_chars[..find_cursor].iter().collect();
            let find_cursor_char = if find_cursor < find_chars.len() {
                find_chars[find_cursor].to_string()
            } else {
                " ".to_string()
            };
            let find_after: String = if find_cursor < find_chars.len() {
                let mut s: String = find_chars[find_cursor + 1..].iter().collect();
                s.push(' '); // 끝에 공백 유지
                s
            } else {
                String::new()
            };

            let mut spans = vec![Span::styled("Find: ", theme.header_style())];
            if state.input_focus == 0 {
                spans.push(Span::styled(find_before, input_style));
                spans.push(Span::styled(find_cursor_char, cursor_style));
                spans.push(Span::styled(find_after, input_style));
            } else {
                spans.push(Span::styled(
                    format!("{} ", &state.find_input),
                    theme.dim_style(),
                ));
            }

            // Replace 입력 필드
            if state.find_mode == FindReplaceMode::Replace {
                let replace_chars: Vec<char> = state.replace_input.chars().collect();
                let replace_cursor = state.replace_cursor_pos.min(replace_chars.len());
                let replace_before: String = replace_chars[..replace_cursor].iter().collect();
                let replace_cursor_char = if replace_cursor < replace_chars.len() {
                    replace_chars[replace_cursor].to_string()
                } else {
                    " ".to_string()
                };
                let replace_after: String = if replace_cursor < replace_chars.len() {
                    let mut s: String = replace_chars[replace_cursor + 1..].iter().collect();
                    s.push(' '); // 끝에 공백 유지
                    s
                } else {
                    String::new()
                };

                spans.push(Span::styled(" Replace: ", theme.header_style()));
                if state.input_focus == 1 {
                    spans.push(Span::styled(replace_before, input_style));
                    spans.push(Span::styled(replace_cursor_char, cursor_style));
                    spans.push(Span::styled(replace_after, input_style));
                } else {
                    spans.push(Span::styled(
                        format!("{} ", &state.replace_input),
                        theme.dim_style(),
                    ));
                }
            }

            spans.push(Span::styled(match_info, match_info_style));
            spans.push(Span::styled(find_opts, theme.dim_style()));

            // 단축키 안내 (VSCode 스타일)
            spans.push(Span::styled("Enter", theme.header_style()));
            if state.find_mode == FindReplaceMode::Replace && state.input_focus == 1 {
                spans.push(Span::styled(" replace ", theme.dim_style()));
            } else {
                spans.push(Span::styled(" next/search ", theme.dim_style()));
            }
            spans.push(Span::styled("Up/Down", theme.header_style()));
            spans.push(Span::styled(" prev/next ", theme.dim_style()));
            spans.push(Span::styled(" ^C", theme.header_style()));
            spans.push(Span::styled("ase ", theme.dim_style()));
            spans.push(Span::styled("^R", theme.header_style()));
            spans.push(Span::styled("egex ", theme.dim_style()));
            spans.push(Span::styled("^W", theme.header_style()));
            spans.push(Span::styled("ord ", theme.dim_style()));
            if state.find_mode == FindReplaceMode::Replace {
                spans.push(Span::styled("^A", theme.header_style()));
                spans.push(Span::styled("ll ", theme.dim_style()));
                spans.push(Span::styled("Tab", theme.header_style()));
                spans.push(Span::styled(" ", theme.dim_style()));
            }
            spans.push(Span::styled("Esc", theme.header_style()));

            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(theme.status_bar_style()),
                Rect::new(inner.x, footer_y, inner.width, 1),
            );
        }
    }

    // 메시지 표시 (화면 상단에 오버레이)
    if let Some(ref msg) = state.message {
        let msg_width = (msg.len() + 4).min(inner.width as usize) as u16;
        let msg_x = inner.x + (inner.width.saturating_sub(msg_width)) / 2;
        let msg_y = inner.y + 1;
        let msg_area = Rect::new(msg_x, msg_y, msg_width, 1);
        // Clear the area first to ensure message is visible
        frame.render_widget(Clear, msg_area);
        frame.render_widget(
            Paragraph::new(format!(" {} ", msg))
                .style(Style::default().fg(theme.message.text).bg(theme.message.bg)),
            msg_area,
        );
    }

    // 메시지 타이머 업데이트
    if state.message_timer > 0 {
        state.message_timer -= 1;
        if state.message_timer == 0 {
            state.message = None;
        }
    }

    if state.exit_confirm_open {
        draw_unsaved_exit_dialog(frame, state, area, theme);
    }
}

fn draw_unsaved_exit_dialog(frame: &mut Frame, state: &EditorState, area: Rect, theme: &Theme) {
    if area.width < 24 || area.height < 7 {
        return;
    }

    let cd = &theme.confirm_dialog;
    let width = 62u16.min(area.width.saturating_sub(4)).max(24);
    let height = 8u16.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let dialog_area = Rect::new(x, y, width, height);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(cd.border))
        .title(" Unsaved Changes ")
        .title_style(Style::default().fg(cd.title).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(cd.bg));
    let inner = block.inner(dialog_area);

    frame.render_widget(Clear, dialog_area);
    frame.render_widget(block, dialog_area);

    let file_name = state
        .file_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "this file".to_string());

    let message = format!("Save changes to {} before closing?", file_name);
    frame.render_widget(
        Paragraph::new(message)
            .style(Style::default().fg(cd.message_text))
            .alignment(ratatui::layout::Alignment::Center),
        Rect::new(inner.x + 1, inner.y + 1, inner.width.saturating_sub(2), 1),
    );

    frame.render_widget(
        Paragraph::new("Esc cancels and returns to the editor")
            .style(Style::default().fg(cd.message_text))
            .alignment(ratatui::layout::Alignment::Center),
        Rect::new(inner.x + 1, inner.y + 2, inner.width.saturating_sub(2), 1),
    );

    let selected_style = Style::default()
        .fg(cd.button_selected_text)
        .bg(cd.button_selected_bg);
    let normal_style = Style::default().fg(cd.button_text);
    let button_style = |idx| {
        if state.exit_confirm_selected == idx {
            selected_style
        } else {
            normal_style
        }
    };

    let buttons = Line::from(vec![
        Span::styled(" Save ", button_style(0)),
        Span::styled("   ", Style::default().bg(cd.bg)),
        Span::styled(" Don't Save ", button_style(1)),
        Span::styled("   ", Style::default().bg(cd.bg)),
        Span::styled(" Cancel ", button_style(2)),
    ]);

    frame.render_widget(
        Paragraph::new(buttons).alignment(ratatui::layout::Alignment::Center),
        Rect::new(
            inner.x + 1,
            inner.y + inner.height.saturating_sub(2),
            inner.width.saturating_sub(2),
            1,
        ),
    );
}

/// 편집기 라인 렌더링
/// expanded_line: TAB이 스페이스로 확장된 문자열
/// original_line: 원본 문자열 (TAB 포함)
/// visual_to_orig: 확장된 문자열의 각 visual 위치에 해당하는 원본 char index
fn render_editor_line(
    expanded_line: &str,
    original_line: &str,
    visual_to_orig: &[usize],
    line_num: usize,
    state: &EditorState,
    selection: &Option<(usize, usize, usize, usize)>,
    highlighter: &mut Option<SyntaxHighlighter>,
    theme: &Theme,
    is_cursor_line: bool,
    in_find_mode: bool,
    horizontal_scroll: usize,
    visible_width: usize,
    pre_computed_styles: Option<&Vec<ratatui::style::Style>>,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = expanded_line.chars().collect();
    let orig_chars: Vec<char> = original_line.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();

    // 보이는 범위 (visual column 기준)
    let view_start = horizontal_scroll;
    let view_end = horizontal_scroll + visible_width;

    // 커서의 visual column 계산
    let cursor_visual = state.char_to_visual(original_line, state.cursor_col);

    // 선택 영역이 이 줄에 있는지 확인 (원본 인덱스 기준)
    let line_selection = if let Some((sl, sc, el, ec)) = selection {
        if *sl <= line_num && line_num <= *el {
            let start = if line_num == *sl { *sc } else { 0 };
            let end = if line_num == *el {
                *ec
            } else {
                orig_chars.len()
            };
            Some((start, end))
        } else {
            None
        }
    } else {
        None
    };

    // 문법 강조: pre_computed_styles가 있으면 토큰화 건너뜀
    let orig_styles: Vec<ratatui::style::Style> = if let Some(pcs) = pre_computed_styles {
        pcs.clone()
    } else {
        // 문법 강조 토큰 가져오기 (원본 라인에서 토큰화)
        let tokens = if let Some(ref mut hl) = highlighter {
            hl.tokenize_line(original_line)
        } else {
            vec![]
        };

        // 원본 char index -> token style 매핑 생성
        if !tokens.is_empty() {
            let mut styles = vec![theme.normal_style(); orig_chars.len()];
            let mut char_idx = 0;
            for token in &tokens {
                let token_len = token.text.chars().count();
                let style = if let Some(ref mut hl) = highlighter {
                    hl.style_for(token.token_type)
                } else {
                    theme.normal_style()
                };
                for i in char_idx..(char_idx + token_len).min(orig_chars.len()) {
                    styles[i] = style;
                }
                char_idx += token_len;
            }
            styles
        } else {
            vec![]
        }
    };

    // 문자를 순회하면서 visual column 누적 — CJK 전각 문자 올바르게 처리
    let mut visual_col = 0; // 현재 문자의 visual column 시작 위치
    let mut vis_idx = 0; // visual_to_orig 인덱스

    for (_char_idx, c) in chars.iter().enumerate() {
        let char_width = UnicodeWidthChar::width(*c).unwrap_or(1);
        let char_visual_start = visual_col;
        let char_visual_end = visual_col + char_width;

        // 이 문자가 보이는 영역을 벗어나면 종료
        if char_visual_start >= view_end {
            break;
        }

        // 이 문자가 보이는 영역과 겹치는 경우만 렌더링
        if char_visual_end > view_start {
            // visual_to_orig에서 원본 인덱스
            let orig_idx = if vis_idx < visual_to_orig.len() {
                visual_to_orig[vis_idx]
            } else {
                orig_chars.len()
            };

            // 기본 스타일 결정
            let mut style = if !orig_styles.is_empty() && orig_idx < orig_styles.len() {
                orig_styles[orig_idx]
            } else {
                theme.normal_style()
            };

            // 선택 영역 하이라이트 (원본 인덱스 기준)
            if let Some((sel_start, sel_end)) = line_selection {
                if orig_idx >= sel_start && orig_idx < sel_end {
                    style = style
                        .bg(theme.editor.selection_bg)
                        .fg(theme.editor.selection_text);
                }
            }

            if let Some(word) = &state.last_word_selection {
                let word_len = word.chars().count();
                for &(cursor_line, cursor_end) in &state.cursors {
                    if cursor_line == line_num && cursor_end >= word_len {
                        let cursor_start = cursor_end - word_len;
                        if orig_idx >= cursor_start && orig_idx < cursor_end {
                            style = style
                                .bg(theme.editor.selection_bg)
                                .fg(theme.editor.selection_text);
                        }
                    }
                }
            } else if state.is_extra_insert_cursor_at(line_num, orig_idx) {
                style = theme.selected_style();
            }

            // 검색 매치 하이라이트 (원본 인덱스 기준)
            for (idx, (ml, ms, me)) in state.match_positions.iter().enumerate() {
                if *ml == line_num && orig_idx >= *ms && orig_idx < *me {
                    if idx == state.current_match {
                        style = style.bg(theme.editor.match_current_bg).fg(theme.editor.bg);
                    } else {
                        style = style.bg(theme.editor.match_bg).fg(theme.editor.bg);
                    }
                }
            }

            // 매칭 괄호 하이라이트 (원본 인덱스 기준)
            if let Some((bl, bc)) = state.matching_bracket {
                if bl == line_num && bc == orig_idx {
                    style = style.bg(theme.editor.bracket_match).fg(Color::Black);
                }
            }

            // 커서 하이라이트 (visual column 기준)
            if is_cursor_line && char_visual_start == cursor_visual && state.selection.is_none() {
                if in_find_mode {
                    style = Style::default()
                        .fg(theme.editor.text)
                        .bg(theme.editor.footer_bg);
                } else {
                    style = theme.selected_style();
                }
            }

            // 전각 문자가 왼쪽 경계에 걸리는 경우: 공백으로 대체
            if char_visual_start < view_start && char_width == 2 {
                spans.push(Span::styled(" ", style));
            }
            // 전각 문자가 오른쪽 경계에 걸리는 경우: 공백으로 대체
            else if char_visual_end > view_end && char_width == 2 {
                spans.push(Span::styled(" ", style));
            } else {
                spans.push(Span::styled(c.to_string(), style));
            }
        }

        visual_col += char_width;
        vis_idx += char_width; // visual_to_orig는 visual column 단위
    }

    // 커서가 줄 끝에 있고 보이는 범위 내인 경우
    if is_cursor_line && state.cursor_col >= orig_chars.len() && state.selection.is_none() {
        if cursor_visual >= view_start && cursor_visual < view_end {
            let cursor_style = if in_find_mode {
                Style::default()
                    .fg(theme.editor.text)
                    .bg(theme.editor.footer_bg)
            } else {
                theme.selected_style()
            };
            spans.push(Span::styled(" ", cursor_style));
        }
    } else if state.is_extra_insert_cursor_at(line_num, orig_chars.len()) {
        let cursor_visual = state.char_to_visual(original_line, orig_chars.len());
        if cursor_visual >= view_start && cursor_visual < view_end {
            spans.push(Span::styled(" ", theme.selected_style()));
        }
    }

    if spans.is_empty() {
        // 빈 줄에 커서 표시 (수평 스크롤이 0일 때만)
        if is_cursor_line && state.selection.is_none() && horizontal_scroll == 0 {
            let cursor_style = if in_find_mode {
                Style::default()
                    .fg(theme.editor.text)
                    .bg(theme.status_bar.bg)
            } else {
                theme.selected_style()
            };
            spans.push(Span::styled(" ", cursor_style));
        } else if state.is_extra_insert_cursor_at(line_num, 0) && horizontal_scroll == 0 {
            spans.push(Span::styled(" ", theme.selected_style()));
        } else if horizontal_scroll == 0 {
            spans.push(Span::styled(" ", theme.normal_style()));
        }
    }

    spans
}

/// Handle paste event for file editor
pub fn handle_paste(app: &mut App, text: &str) {
    let state = match &mut app.editor_state {
        Some(s) => s,
        None => return,
    };
    if text.is_empty() || state.exit_confirm_open {
        return;
    }

    // Normalize line endings before routing paste to the active editor input.
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");

    if state.goto_mode {
        for ch in normalized.chars().filter(|ch| ch.is_ascii_digit()) {
            state.goto_input.push(ch);
        }
        return;
    }

    if state.find_mode != FindReplaceMode::None {
        let single_line_text: String = normalized.chars().filter(|ch| *ch != '\n').collect();
        if state.input_focus == 0 {
            let mut chars: Vec<char> = state.find_input.chars().collect();
            let pos = state.find_cursor_pos.min(chars.len());
            for (idx, ch) in single_line_text.chars().enumerate() {
                chars.insert(pos + idx, ch);
            }
            state.find_input = chars.into_iter().collect();
            state.find_cursor_pos = pos + single_line_text.chars().count();
        } else {
            let mut chars: Vec<char> = state.replace_input.chars().collect();
            let pos = state.replace_cursor_pos.min(chars.len());
            for (idx, ch) in single_line_text.chars().enumerate() {
                chars.insert(pos + idx, ch);
            }
            state.replace_input = chars.into_iter().collect();
            state.replace_cursor_pos = pos + single_line_text.chars().count();
        }
        return;
    }

    state.insert_str(&normalized);
}

fn close_file_editor(app: &mut App, reload_viewer: bool) {
    let scroll = app
        .editor_state
        .as_ref()
        .map(|state| state.scroll)
        .unwrap_or(0);

    if let Some(Screen::FileViewer) = app.previous_screen {
        if let Some(ref mut viewer) = app.viewer_state {
            if reload_viewer {
                let path = viewer.file_path.clone();
                let _ = viewer.load_file(&path);
            }
            viewer.scroll = scroll;
        }
        app.previous_screen = None;
        app.current_screen = Screen::FileViewer;
    } else {
        app.current_screen = Screen::FilePanel;
    }
}

fn save_current_editor(app: &mut App) -> bool {
    let (is_settings, remote_info, local_path, remote_save_generation) = {
        let state = match app.editor_state.as_mut() {
            Some(state) => state,
            None => return false,
        };

        let is_settings = App::is_settings_file(&state.file_path);
        let remote_info = state
            .remote_origin
            .as_ref()
            .map(|origin| (origin.panel_index, origin.remote_path.clone()));
        let is_remote_save = remote_info.is_some();
        let local_path = state.file_path.display().to_string();

        match state.save_file() {
            Ok(_) => {
                state.exit_confirm_open = false;
                let remote_save_generation = if is_remote_save {
                    state.set_message("Saved locally, uploading...", 30);
                    Some(state.begin_remote_save())
                } else {
                    if is_settings {
                        state.set_message("Settings saved and applied!", 30);
                    } else {
                        state.set_message("File saved!", 30);
                    }
                    None
                };
                (is_settings, remote_info, local_path, remote_save_generation)
            }
            Err(e) => {
                state.set_message(format!("Save error: {}", e), 50);
                return false;
            }
        }
    };

    if let Some((panel_idx, remote_path)) = remote_info {
        if app.remote_spinner.is_some() {
            if let Some(ref mut editor) = app.editor_state {
                editor.set_message("Saved locally, remote upload queued".to_string(), 50);
            }
        } else {
            let is_connected = app
                .panels
                .get(panel_idx)
                .and_then(|panel| panel.remote_ctx.as_ref())
                .map(|ctx| {
                    matches!(
                        ctx.status,
                        crate::services::remote::ConnectionStatus::Connected
                    )
                })
                .unwrap_or(false);

            if is_connected {
                let ctx = match app.panels[panel_idx].remote_ctx.take() {
                    Some(ctx) => ctx,
                    None => {
                        if let Some(ref mut editor) = app.editor_state {
                            editor.set_message(
                                "Saved locally, remote connection was disconnected".to_string(),
                                50,
                            );
                        }
                        if is_settings {
                            app.reload_settings();
                        }
                        app.refresh_panels();
                        return true;
                    }
                };
                let (tx, rx) = std::sync::mpsc::channel();

                std::thread::spawn(move || {
                    let msg = match ctx.session.upload_file(&local_path, &remote_path) {
                        Ok(_) => Ok("Saved & uploaded to remote!".to_string()),
                        Err(e) => Err(format!("Saved locally, upload failed: {}", e)),
                    };
                    let _ = tx.send(crate::ui::app::RemoteSpinnerResult::PanelOp {
                        ctx,
                        panel_idx,
                        outcome: crate::ui::app::PanelOpOutcome::RemoteSave {
                            message: msg,
                            remote_path,
                            generation: remote_save_generation.unwrap_or(0),
                            reload: true,
                        },
                    });
                });

                app.remote_spinner = Some(crate::ui::app::RemoteSpinner {
                    message: "Uploading...".to_string(),
                    started_at: std::time::Instant::now(),
                    receiver: rx,
                });
            } else {
                let msg = if app
                    .panels
                    .get(panel_idx)
                    .and_then(|panel| panel.remote_ctx.as_ref())
                    .is_some()
                {
                    "Saved locally, remote connection lost".to_string()
                } else {
                    "Saved locally, remote connection was disconnected".to_string()
                };
                if let Some(ref mut editor) = app.editor_state {
                    editor.set_message(msg, 50);
                }
            }
        }
    }

    if is_settings {
        app.reload_settings();
    }
    app.refresh_panels();
    true
}

fn cancel_exit_confirm(state: &mut EditorState) {
    state.exit_confirm_open = false;
    state.exit_confirm_selected = 2;
}

fn handle_exit_confirm_input(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    if code == KeyCode::Esc {
        if let Some(ref mut state) = app.editor_state {
            cancel_exit_confirm(state);
        }
        return;
    }

    if app.keybindings.editor_action(code, modifiers) == Some(EditorAction::Save) {
        if save_current_editor(app) {
            close_file_editor(app, true);
        }
        return;
    }

    match code {
        KeyCode::Left | KeyCode::BackTab => {
            if let Some(ref mut state) = app.editor_state {
                state.exit_confirm_selected = if state.exit_confirm_selected == 0 {
                    2
                } else {
                    state.exit_confirm_selected - 1
                };
            }
        }
        KeyCode::Right | KeyCode::Tab => {
            if let Some(ref mut state) = app.editor_state {
                state.exit_confirm_selected = (state.exit_confirm_selected + 1) % 3;
            }
        }
        KeyCode::Home => {
            if let Some(ref mut state) = app.editor_state {
                state.exit_confirm_selected = 0;
            }
        }
        KeyCode::End => {
            if let Some(ref mut state) = app.editor_state {
                state.exit_confirm_selected = 2;
            }
        }
        KeyCode::Enter => {
            let selected = app
                .editor_state
                .as_ref()
                .map(|state| state.exit_confirm_selected)
                .unwrap_or(2);
            match selected {
                0 => {
                    if save_current_editor(app) {
                        close_file_editor(app, true);
                    }
                }
                1 => {
                    close_file_editor(app, false);
                }
                _ => {
                    if let Some(ref mut state) = app.editor_state {
                        cancel_exit_confirm(state);
                    }
                }
            }
        }
        _ => {}
    }
}

pub fn handle_input(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    if app.editor_state.is_none() {
        return;
    }
    if app
        .editor_state
        .as_ref()
        .is_some_and(|state| state.exit_confirm_open)
    {
        handle_exit_confirm_input(app, code, modifiers);
        return;
    }

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
                state.match_positions.clear();
            }
            KeyCode::Tab if state.find_mode == FindReplaceMode::Replace => {
                state.input_focus = 1 - state.input_focus;
                if state.input_focus == 0 {
                    state.find_cursor_pos = state.find_input.chars().count();
                } else {
                    state.replace_cursor_pos = state.replace_input.chars().count();
                }
            }
            KeyCode::Enter => {
                if state.input_focus == 0 {
                    if state.find_term == state.find_input && !state.match_positions.is_empty() {
                        state.find_next();
                    } else {
                        state.find_term = state.find_input.clone();
                        state.perform_find();
                    }
                } else if state.find_mode == FindReplaceMode::Replace {
                    if state.find_term != state.find_input || state.match_positions.is_empty() {
                        state.find_term = state.find_input.clone();
                        state.perform_find();
                    }
                    state.replace_current();
                }
            }
            KeyCode::Backspace => {
                if state.input_focus == 0 {
                    if state.find_cursor_pos > 0 {
                        let mut chars: Vec<char> = state.find_input.chars().collect();
                        chars.remove(state.find_cursor_pos - 1);
                        state.find_input = chars.into_iter().collect();
                        state.find_cursor_pos -= 1;
                    }
                } else if state.replace_cursor_pos > 0 {
                    let mut chars: Vec<char> = state.replace_input.chars().collect();
                    chars.remove(state.replace_cursor_pos - 1);
                    state.replace_input = chars.into_iter().collect();
                    state.replace_cursor_pos -= 1;
                }
            }
            KeyCode::Delete => {
                if state.input_focus == 0 {
                    let char_count = state.find_input.chars().count();
                    if state.find_cursor_pos < char_count {
                        let mut chars: Vec<char> = state.find_input.chars().collect();
                        chars.remove(state.find_cursor_pos);
                        state.find_input = chars.into_iter().collect();
                    }
                } else {
                    let char_count = state.replace_input.chars().count();
                    if state.replace_cursor_pos < char_count {
                        let mut chars: Vec<char> = state.replace_input.chars().collect();
                        chars.remove(state.replace_cursor_pos);
                        state.replace_input = chars.into_iter().collect();
                    }
                }
            }
            KeyCode::Left => {
                if state.input_focus == 0 {
                    if state.find_cursor_pos > 0 {
                        state.find_cursor_pos -= 1;
                    }
                } else if state.replace_cursor_pos > 0 {
                    state.replace_cursor_pos -= 1;
                }
            }
            KeyCode::Right => {
                if state.input_focus == 0 {
                    if state.find_cursor_pos < state.find_input.chars().count() {
                        state.find_cursor_pos += 1;
                    }
                } else if state.replace_cursor_pos < state.replace_input.chars().count() {
                    state.replace_cursor_pos += 1;
                }
            }
            KeyCode::Home => {
                if state.input_focus == 0 {
                    state.find_cursor_pos = 0;
                } else {
                    state.replace_cursor_pos = 0;
                }
            }
            KeyCode::End => {
                if state.input_focus == 0 {
                    state.find_cursor_pos = state.find_input.chars().count();
                } else {
                    state.replace_cursor_pos = state.replace_input.chars().count();
                }
            }
            KeyCode::Char('c') | KeyCode::Char('C')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                state.find_options.case_sensitive = !state.find_options.case_sensitive;
                state.find_term = state.find_input.clone();
                state.perform_find();
            }
            KeyCode::Char('r') | KeyCode::Char('R')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                state.find_options.use_regex = !state.find_options.use_regex;
                state.find_term = state.find_input.clone();
                state.perform_find();
            }
            KeyCode::Char('w') | KeyCode::Char('W')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                state.find_options.whole_word = !state.find_options.whole_word;
                state.find_term = state.find_input.clone();
                state.perform_find();
            }
            KeyCode::Char('a') | KeyCode::Char('A')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                // 모두 바꾸기
                if state.find_mode == FindReplaceMode::Replace {
                    state.find_term = state.find_input.clone();
                    state.replace_all();
                }
            }
            KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
                // Shift 처리: 일부 터미널에서 Shift+문자가 소문자로 올 수 있음
                let ch = if modifiers.contains(KeyModifiers::SHIFT) && c.is_ascii_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                if state.input_focus == 0 {
                    let mut chars: Vec<char> = state.find_input.chars().collect();
                    chars.insert(state.find_cursor_pos, ch);
                    state.find_input = chars.into_iter().collect();
                    state.find_cursor_pos += 1;
                } else {
                    let mut chars: Vec<char> = state.replace_input.chars().collect();
                    chars.insert(state.replace_cursor_pos, ch);
                    state.replace_input = chars.into_iter().collect();
                    state.replace_cursor_pos += 1;
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

    // EditorAction 조회 (Ctrl/Alt 조합 및 Esc)
    if let Some(action) = app.keybindings.editor_action(code, modifiers) {
        match action {
            EditorAction::Save => {
                let save_result = state.save_file();
                let is_settings = App::is_settings_file(&state.file_path);
                let remote_info = state
                    .remote_origin
                    .as_ref()
                    .map(|o| (o.panel_index, o.remote_path.clone()));
                let is_remote_save = remote_info.is_some();
                let local_path = state.file_path.display().to_string();
                let mut remote_save_generation = None;
                match save_result {
                    Ok(_) => {
                        if is_remote_save {
                            remote_save_generation = Some(state.begin_remote_save());
                            state.set_message("Saved locally, uploading...", 30);
                        } else if is_settings {
                            state.set_message("Settings saved and applied!", 30);
                        } else {
                            state.set_message("File saved!", 30);
                        }
                    }
                    Err(e) => {
                        state.set_message(format!("Save error: {}", e), 50);
                        return;
                    }
                }
                // state borrow ends here due to NLL — now access app freely
                if let Some((panel_idx, remote_path)) = remote_info {
                    if app.remote_spinner.is_some() {
                        // Spinner already active — skip upload
                        if let Some(ref mut editor) = app.editor_state {
                            editor
                                .set_message("Saved locally, remote upload queued".to_string(), 50);
                        }
                    } else {
                        let is_connected = app
                            .panels
                            .get(panel_idx)
                            .and_then(|p| p.remote_ctx.as_ref())
                            .map(|ctx| {
                                matches!(
                                    ctx.status,
                                    crate::services::remote::ConnectionStatus::Connected
                                )
                            })
                            .unwrap_or(false);

                        if is_connected {
                            let ctx = match app.panels[panel_idx].remote_ctx.take() {
                                Some(ctx) => ctx,
                                None => {
                                    if let Some(ref mut editor) = app.editor_state {
                                        editor.set_message(
                                            "Saved locally, remote connection was disconnected"
                                                .to_string(),
                                            50,
                                        );
                                    }
                                    if is_settings {
                                        app.reload_settings();
                                    }
                                    app.refresh_panels();
                                    return;
                                }
                            };
                            let (tx, rx) = std::sync::mpsc::channel();

                            std::thread::spawn(move || {
                                let msg = match ctx.session.upload_file(&local_path, &remote_path) {
                                    Ok(_) => Ok("Saved & uploaded to remote!".to_string()),
                                    Err(e) => Err(format!("Saved locally, upload failed: {}", e)),
                                };
                                let _ = tx.send(crate::ui::app::RemoteSpinnerResult::PanelOp {
                                    ctx,
                                    panel_idx,
                                    outcome: crate::ui::app::PanelOpOutcome::RemoteSave {
                                        message: msg,
                                        remote_path,
                                        generation: remote_save_generation.unwrap_or(0),
                                        reload: true,
                                    },
                                });
                            });

                            app.remote_spinner = Some(crate::ui::app::RemoteSpinner {
                                message: "Uploading...".to_string(),
                                started_at: std::time::Instant::now(),
                                receiver: rx,
                            });
                        } else {
                            let msg = if app
                                .panels
                                .get(panel_idx)
                                .and_then(|p| p.remote_ctx.as_ref())
                                .is_some()
                            {
                                "Saved locally, remote connection lost".to_string()
                            } else {
                                "Saved locally, remote connection was disconnected".to_string()
                            };
                            if let Some(ref mut editor) = app.editor_state {
                                editor.set_message(msg, 50);
                            }
                        }
                    }
                }
                if is_settings {
                    app.reload_settings();
                }
                app.refresh_panels();
            }
            EditorAction::Cut => {
                state.cut_line_or_selection();
            }
            EditorAction::Undo => {
                state.undo();
            }
            EditorAction::Redo => {
                state.redo();
            }
            EditorAction::SelectAll => {
                state.select_all();
            }
            EditorAction::Copy => {
                state.copy();
            }
            EditorAction::Paste => {
                state.paste();
            }
            EditorAction::ToggleWordWrap => {
                state.word_wrap = !state.word_wrap;
                if state.word_wrap {
                    state.horizontal_scroll = 0;
                }
            }
            EditorAction::DeleteLine => {
                state.delete_line();
            }
            EditorAction::DuplicateLine => {
                state.duplicate_line();
            }
            EditorAction::SelectNextOccurrence => {
                state.select_next_occurrence();
            }
            EditorAction::SelectLine => {
                state.select_line();
            }
            EditorAction::ToggleComment => {
                state.toggle_comment();
            }
            EditorAction::Indent => {
                state.indent();
            }
            EditorAction::InsertLineBelow => {
                state.insert_line_below();
            }
            EditorAction::InsertLineAbove => {
                state.insert_line_above();
            }
            EditorAction::MoveWordLeft => {
                let extend_sel = modifiers.contains(KeyModifiers::SHIFT);
                state.move_word_left(extend_sel);
            }
            EditorAction::MoveWordRight => {
                let extend_sel = modifiers.contains(KeyModifiers::SHIFT);
                state.move_word_right(extend_sel);
            }
            EditorAction::DeleteWordBackward => {
                state.delete_word_backward();
            }
            EditorAction::DeleteWordForward => {
                state.delete_word_forward();
            }
            EditorAction::Find => {
                state.find_mode = FindReplaceMode::Find;
                state.input_focus = 0;
                state.find_cursor_pos = state.find_input.chars().count();
            }
            EditorAction::Replace => {
                state.find_mode = FindReplaceMode::Replace;
                state.input_focus = 0;
                state.find_cursor_pos = state.find_input.chars().count();
                state.replace_cursor_pos = state.replace_input.chars().count();
            }
            EditorAction::GotoLine => {
                state.goto_mode = true;
                state.goto_input.clear();
            }
            EditorAction::GoToFileStart => {
                let extend_sel = modifiers.contains(KeyModifiers::SHIFT);
                let old_line = state.cursor_line;
                let old_col = state.cursor_col;
                let had_selection = state.selection.is_some();
                if !extend_sel {
                    state.selection = None;
                }
                state.cursor_line = 0;
                state.cursor_col = 0;
                if extend_sel {
                    if !had_selection
                        && (state.cursor_line != old_line || state.cursor_col != old_col)
                    {
                        state.selection = Some(Selection::new(old_line, old_col));
                    }
                    if let Some(ref mut sel) = state.selection {
                        sel.end_line = state.cursor_line;
                        sel.end_col = state.cursor_col;
                    }
                }
                state.update_scroll();
            }
            EditorAction::GoToFileEnd => {
                let extend_sel = modifiers.contains(KeyModifiers::SHIFT);
                let old_line = state.cursor_line;
                let old_col = state.cursor_col;
                let had_selection = state.selection.is_some();
                if !extend_sel {
                    state.selection = None;
                }
                state.cursor_line = state.lines.len().saturating_sub(1);
                state.cursor_col = state.lines[state.cursor_line].chars().count();
                if extend_sel {
                    if !had_selection
                        && (state.cursor_line != old_line || state.cursor_col != old_col)
                    {
                        state.selection = Some(Selection::new(old_line, old_col));
                    }
                    if let Some(ref mut sel) = state.selection {
                        sel.end_line = state.cursor_line;
                        sel.end_col = state.cursor_col;
                    }
                }
                state.update_scroll();
            }
            EditorAction::MoveLineUp => {
                state.move_line_up();
            }
            EditorAction::MoveLineDown => {
                state.move_line_down();
            }
            EditorAction::Exit => {
                if state.selection.is_some() || state.has_multi_cursor() {
                    // 선택 해제 및 다중 커서 초기화
                    state.selection = None;
                    state.clear_multi_cursor_state();
                } else if state.modified {
                    // 변경사항이 있을 때는 명시적인 확인 다이얼로그를 표시
                    state.exit_confirm_open = true;
                    state.exit_confirm_selected = 2;
                } else {
                    // 변경사항 없으면 바로 종료
                    if let Some(Screen::FileViewer) = app.previous_screen {
                        if let Some(ref mut viewer) = app.viewer_state {
                            let scroll = state.scroll;
                            let path = viewer.file_path.clone();
                            let _ = viewer.load_file(&path);
                            viewer.scroll = scroll;
                        }
                        app.previous_screen = None;
                        app.current_screen = Screen::FileViewer;
                    } else {
                        app.current_screen = Screen::FilePanel;
                    }
                }
            }
        }
        return;
    }

    // 일반 모드 (화살표, Home/End, Enter, Tab, Backspace, Delete, 문자 입력)
    let extend_selection = modifiers.contains(KeyModifiers::SHIFT);

    match code {
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
            let page_size = state.visible_height.max(1) as i32;
            state.move_cursor(-page_size, 0, extend_selection);
        }
        KeyCode::PageDown => {
            let page_size = state.visible_height.max(1) as i32;
            state.move_cursor(page_size, 0, extend_selection);
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
            if modifiers.contains(KeyModifiers::SHIFT) {
                // Shift+Tab: 내어쓰기
                state.outdent();
            } else if state.has_selection_range() {
                // 선택 영역이 있으면 들여쓰기
                state.indent();
            } else {
                state.selection = None;
                state.insert_tab();
            }
        }
        KeyCode::BackTab => {
            // BackTab (일부 터미널에서 Shift+Tab): 내어쓰기
            state.outdent();
        }
        KeyCode::Char(c) => {
            if !modifiers.contains(KeyModifiers::CONTROL) && !modifiers.contains(KeyModifiers::ALT)
            {
                // 방어적 처리: 일부 터미널에서 Shift+문자가 소문자로 올 수 있음
                let ch = if modifiers.contains(KeyModifiers::SHIFT) && c.is_ascii_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                state.insert_char(ch);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor_with_lines(lines: &[&str]) -> EditorState {
        let mut editor = EditorState::new();
        editor.lines = lines.iter().map(|line| line.to_string()).collect();
        editor.ensure_line_endings();
        editor.original_lines = editor.lines.clone();
        editor.original_line_endings = editor.line_endings.clone();
        editor
    }

    #[test]
    fn load_missing_file_opens_empty_buffer() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("new.txt");
        let mut editor = EditorState::new();

        editor.load_file(&path).unwrap();

        assert_eq!(editor.lines, vec![""]);
        assert!(!editor.modified);
    }

    #[test]
    fn load_existing_invalid_utf8_reports_error() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("invalid.txt");
        std::fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
        let mut editor = EditorState::new();

        let err = editor.load_file(&path).unwrap_err();

        assert!(err.contains("Failed to read file"));
        assert_eq!(editor.lines, vec![""]);
    }

    #[test]
    fn load_trailing_newline_shows_empty_final_line() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("trailing.txt");
        std::fs::write(&path, "123\n234\n").unwrap();
        let mut editor = EditorState::new();

        editor.load_file(&path).unwrap();

        assert_eq!(editor.lines, vec!["123", "234", ""]);
        assert_eq!(editor.serialize_content(), "123\n234\n");
    }

    #[test]
    fn save_preserves_crlf_and_trailing_newline() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("crlf.txt");
        std::fs::write(&path, "alpha\r\nbeta\r\n").unwrap();
        let mut editor = EditorState::new();
        editor.load_file(&path).unwrap();

        assert_eq!(editor.lines, vec!["alpha", "beta", ""]);
        editor.lines[1].push('!');
        editor.save_file().unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "alpha\r\nbeta!\r\n"
        );
    }

    #[test]
    fn save_preserves_mixed_line_endings_after_content_edit() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("mixed.txt");
        std::fs::write(&path, "alpha\r\nbeta\ngamma\rdelta").unwrap();
        let mut editor = EditorState::new();
        editor.load_file(&path).unwrap();

        editor.lines[1].push('!');
        editor.save_file().unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "alpha\r\nbeta!\ngamma\rdelta"
        );
    }

    #[test]
    fn multiline_insert_preserves_inserted_line_endings_across_undo_redo() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("paste.txt");
        std::fs::write(&path, "ab\r\ncd").unwrap();
        let mut editor = EditorState::new();
        editor.load_file(&path).unwrap();
        editor.cursor_col = 1;

        editor.insert_str("X\nY\r\nZ");

        let edited = "aX\nY\r\nZb\r\ncd";
        assert_eq!(editor.serialize_content(), edited);

        editor.undo();
        assert_eq!(editor.serialize_content(), "ab\r\ncd");

        editor.redo();
        assert_eq!(editor.serialize_content(), edited);
    }

    #[test]
    fn moving_last_line_up_keeps_separator_between_lines() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("move.txt");
        std::fs::write(&path, "a\nb").unwrap();
        let mut editor = EditorState::new();
        editor.load_file(&path).unwrap();
        editor.cursor_line = 1;

        editor.move_line_up();

        assert_eq!(editor.serialize_content(), "b\na");

        editor.undo();
        assert_eq!(editor.serialize_content(), "a\nb");

        editor.redo();
        assert_eq!(editor.serialize_content(), "b\na");
    }

    #[test]
    fn delete_last_line_undo_redo_restores_previous_line_ending() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("delete-last.txt");
        std::fs::write(&path, "a\r\nb\nc").unwrap();
        let mut editor = EditorState::new();
        editor.load_file(&path).unwrap();
        editor.cursor_line = 2;

        editor.delete_line();

        assert_eq!(editor.serialize_content(), "a\r\nb");

        editor.undo();
        assert_eq!(editor.serialize_content(), "a\r\nb\nc");

        editor.redo();
        assert_eq!(editor.serialize_content(), "a\r\nb");
    }

    #[test]
    fn delete_only_line_clears_preserved_trailing_newline() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("only.txt");
        std::fs::write(&path, "only\n").unwrap();
        let mut editor = EditorState::new();
        editor.load_file(&path).unwrap();

        editor.delete_line();

        assert_eq!(editor.serialize_content(), "");

        editor.undo();
        assert_eq!(editor.serialize_content(), "only\n");

        editor.redo();
        editor.save_file().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");
    }

    #[test]
    fn cut_only_line_clears_preserved_trailing_newline() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("cut-only.txt");
        std::fs::write(&path, "only\n").unwrap();
        let mut editor = EditorState::new();
        editor.load_file(&path).unwrap();

        editor.cut_line_or_selection();

        assert_eq!(editor.clipboard, "only\n");
        assert_eq!(editor.serialize_content(), "");

        editor.undo();
        assert_eq!(editor.serialize_content(), "only\n");
    }

    #[test]
    fn duplicate_last_line_undo_redo_preserves_separator_layout() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("duplicate.txt");
        std::fs::write(&path, "a\rb").unwrap();
        let mut editor = EditorState::new();
        editor.load_file(&path).unwrap();
        editor.cursor_line = 1;

        editor.duplicate_line();

        let duplicated = "a\rb\rb";
        assert_eq!(editor.serialize_content(), duplicated);

        editor.undo();
        assert_eq!(editor.serialize_content(), "a\rb");

        editor.redo();
        assert_eq!(editor.serialize_content(), duplicated);
    }

    #[test]
    fn multiline_selection_delete_undo_redo_restores_mixed_endings() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("selection-delete.txt");
        std::fs::write(&path, "aa\r\nbb\ncc\rd").unwrap();
        let mut editor = EditorState::new();
        editor.load_file(&path).unwrap();
        editor.selection = Some(Selection {
            start_line: 0,
            start_col: 1,
            end_line: 2,
            end_col: 1,
        });

        editor.delete_selection();

        let deleted = "ac\rd";
        assert_eq!(editor.serialize_content(), deleted);

        editor.undo();
        assert_eq!(editor.serialize_content(), "aa\r\nbb\ncc\rd");

        editor.redo();
        assert_eq!(editor.serialize_content(), deleted);
    }

    #[test]
    fn multiline_selection_copy_preserves_mixed_endings() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("selection-copy.txt");
        std::fs::write(&path, "aa\r\nbb\ncc\rd").unwrap();
        let mut editor = EditorState::new();
        editor.load_file(&path).unwrap();
        editor.selection = Some(Selection {
            start_line: 0,
            start_col: 1,
            end_line: 2,
            end_col: 1,
        });

        assert_eq!(editor.get_selected_text(), "a\r\nbb\nc");

        editor.copy();
        assert_eq!(editor.clipboard, "a\r\nbb\nc");
    }

    #[test]
    fn whole_line_copy_and_cut_use_document_line_ending() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("whole-line-copy.txt");
        std::fs::write(&path, "a\r\nb").unwrap();
        let mut editor = EditorState::new();
        editor.load_file(&path).unwrap();

        editor.copy();
        assert_eq!(editor.clipboard, "a\r\n");

        editor.cursor_line = 1;
        editor.copy();
        assert_eq!(editor.clipboard, "b\r\n");

        editor.cut_line_or_selection();
        assert_eq!(editor.clipboard, "b\r\n");
    }

    #[test]
    fn save_does_not_overwrite_sibling_tmp_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("note.txt");
        let sibling_tmp = temp_dir.path().join("note.tmp");
        std::fs::write(&path, "old").unwrap();
        std::fs::write(&sibling_tmp, "keep").unwrap();
        let mut editor = EditorState::new();
        editor.load_file(&path).unwrap();

        editor.lines[0] = "new".to_string();
        editor.save_file().unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&sibling_tmp).unwrap(), "keep");
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_unix_file_mode() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("mode.txt");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o754)).unwrap();
        let mut editor = EditorState::new();
        editor.load_file(&path).unwrap();

        editor.lines[0] = "new".to_string();
        editor.save_file().unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o754);
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn save_preserves_linux_user_xattr_when_supported() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("xattr.txt");
        std::fs::write(&path, "old").unwrap();
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let c_name = CString::new("user.cokacdir_editor_test").unwrap();
        let original = b"metadata";

        let set_result = unsafe {
            libc::setxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                original.as_ptr() as *const libc::c_void,
                original.len(),
                0,
            )
        };
        if set_result != 0 {
            return;
        }

        let mut editor = EditorState::new();
        editor.load_file(&path).unwrap();
        editor.lines[0] = "new".to_string();
        editor.save_file().unwrap();

        let size =
            unsafe { libc::getxattr(c_path.as_ptr(), c_name.as_ptr(), std::ptr::null_mut(), 0) };
        assert!(size >= 0);
        let mut value = vec![0u8; size as usize];
        let read = unsafe {
            libc::getxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                value.as_mut_ptr() as *mut libc::c_void,
                value.len(),
            )
        };
        assert!(read >= 0);
        value.truncate(read as usize);
        assert_eq!(value, original);
    }

    #[test]
    fn line_end_selection_is_clamped_before_copy_and_delete() {
        let mut editor = editor_with_lines(&["abc"]);
        editor.cursor_col = 3;

        editor.move_cursor(0, -1, true);

        assert_eq!(editor.get_selected_text(), "c");
        editor.delete_selection();
        assert_eq!(editor.lines, vec!["ab"]);
    }

    #[test]
    fn auto_indent_newline_undo_restores_original_line() {
        let mut editor = editor_with_lines(&["    value"]);
        editor.cursor_col = 4;

        editor.insert_newline();
        assert_eq!(editor.lines, vec!["    ", "    value"]);

        editor.undo();
        assert_eq!(editor.lines, vec!["    value"]);

        editor.redo();
        assert_eq!(editor.lines, vec!["    ", "    value"]);
    }

    #[test]
    fn replace_current_selects_next_remaining_match_without_skipping() {
        let mut editor = editor_with_lines(&["foo foo foo"]);
        editor.find_input = "foo".to_string();
        editor.find_term = "foo".to_string();
        editor.replace_input = "bar".to_string();
        editor.perform_find();

        editor.replace_current();

        assert_eq!(editor.lines[0], "bar foo foo");
        let selection = editor.selection.unwrap().normalized();
        assert_eq!(selection, (0, 4, 0, 7));
    }

    #[test]
    fn enter_in_replace_field_uses_current_find_input_before_replacing() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        let mut editor = editor_with_lines(&["foo bar"]);
        editor.find_mode = FindReplaceMode::Replace;
        editor.input_focus = 1;
        editor.find_input = "foo".to_string();
        editor.find_term = "foo".to_string();
        editor.replace_input = "baz".to_string();
        editor.find_cursor_pos = 3;
        editor.replace_cursor_pos = 3;
        editor.perform_find();
        editor.find_input = "bar".to_string();
        app.editor_state = Some(editor);

        handle_input(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        let editor = app.editor_state.as_ref().unwrap();
        assert_eq!(editor.lines[0], "foo baz");
        assert_eq!(editor.find_term, "bar");
        assert!(editor.match_positions.is_empty());
        assert!(editor.selection.is_none());
    }

    #[test]
    fn enter_in_replace_field_searches_before_first_replace() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        let mut editor = editor_with_lines(&["foo"]);
        editor.find_mode = FindReplaceMode::Replace;
        editor.input_focus = 1;
        editor.find_input = "foo".to_string();
        editor.replace_input = "bar".to_string();
        editor.find_cursor_pos = 3;
        editor.replace_cursor_pos = 3;
        app.editor_state = Some(editor);

        handle_input(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        let editor = app.editor_state.as_ref().unwrap();
        assert_eq!(editor.lines[0], "bar");
        assert_eq!(editor.find_term, "foo");
        assert_eq!(editor.undo_stack.len(), 1);
        assert!(editor.modified);
    }

    #[test]
    fn unsaved_exit_opens_dialog_and_escape_cancels() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("note.txt");
        std::fs::write(&path, "old").unwrap();

        let mut app = App::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        let mut editor = editor_with_lines(&["changed"]);
        editor.file_path = path;
        editor.modified = true;
        app.current_screen = Screen::FileEditor;
        app.editor_state = Some(editor);

        handle_input(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        let editor = app.editor_state.as_ref().unwrap();
        assert_eq!(app.current_screen, Screen::FileEditor);
        assert!(editor.exit_confirm_open);
        assert_eq!(editor.exit_confirm_selected, 2);

        handle_input(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        let editor = app.editor_state.as_ref().unwrap();
        assert_eq!(app.current_screen, Screen::FileEditor);
        assert!(!editor.exit_confirm_open);
        assert!(editor.modified);
    }

    #[test]
    fn unsaved_exit_save_button_saves_then_closes() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("note.txt");
        std::fs::write(&path, "old").unwrap();

        let mut app = App::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        let mut editor = editor_with_lines(&["changed"]);
        editor.file_path = path.clone();
        editor.modified = true;
        app.current_screen = Screen::FileEditor;
        app.editor_state = Some(editor);

        handle_input(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        handle_input(&mut app, KeyCode::Right, KeyModifiers::NONE);
        handle_input(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(app.current_screen, Screen::FilePanel);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "changed");
        assert!(!app.editor_state.as_ref().unwrap().modified);
    }

    #[test]
    fn unsaved_exit_discard_button_closes_without_saving() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("note.txt");
        std::fs::write(&path, "old").unwrap();

        let mut app = App::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        let mut editor = editor_with_lines(&["changed"]);
        editor.file_path = path.clone();
        editor.modified = true;
        app.current_screen = Screen::FileEditor;
        app.editor_state = Some(editor);

        handle_input(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        handle_input(&mut app, KeyCode::Left, KeyModifiers::NONE);
        handle_input(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(app.current_screen, Screen::FilePanel);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "old");
    }

    #[test]
    fn enter_in_replace_field_with_empty_find_clears_stale_match_without_replacing() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        let mut editor = editor_with_lines(&["foo"]);
        editor.find_mode = FindReplaceMode::Replace;
        editor.input_focus = 1;
        editor.find_input = "foo".to_string();
        editor.find_term = "foo".to_string();
        editor.replace_input = "bar".to_string();
        editor.replace_cursor_pos = 3;
        editor.perform_find();
        assert!(editor.selection.is_some());

        editor.find_input.clear();
        editor.find_cursor_pos = 0;
        app.editor_state = Some(editor);

        handle_input(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        let editor = app.editor_state.as_ref().unwrap();
        assert_eq!(editor.lines[0], "foo");
        assert!(editor.find_term.is_empty());
        assert!(editor.match_positions.is_empty());
        assert!(editor.selection.is_none());
        assert!(editor.undo_stack.is_empty());
        assert!(!editor.modified);
    }

    #[test]
    fn replace_current_with_same_text_advances_without_marking_modified() {
        let mut editor = editor_with_lines(&["foo foo"]);
        editor.find_input = "foo".to_string();
        editor.find_term = "foo".to_string();
        editor.replace_input = "foo".to_string();
        editor.perform_find();

        editor.replace_current();

        assert_eq!(editor.lines[0], "foo foo");
        assert!(!editor.modified);
        assert!(editor.undo_stack.is_empty());
        assert_eq!(editor.current_match, 1);
        assert_eq!(editor.selection.unwrap().normalized(), (0, 4, 0, 7));
    }

    #[test]
    fn replace_all_invalid_regex_reports_error_and_clears_stale_match() {
        let mut editor = editor_with_lines(&["foo"]);
        editor.find_input = "foo".to_string();
        editor.find_term = "foo".to_string();
        editor.perform_find();
        assert!(editor.selection.is_some());

        editor.find_options.use_regex = true;
        editor.find_input = "(".to_string();
        editor.find_term = editor.find_input.clone();
        editor.replace_input = "bar".to_string();

        editor.replace_all();

        assert_eq!(editor.lines[0], "foo");
        assert!(editor
            .find_error
            .as_deref()
            .is_some_and(|e| e.starts_with("Regex error:")));
        assert!(editor.match_positions.is_empty());
        assert!(editor.selection.is_none());
        assert!(editor.undo_stack.is_empty());
        assert!(!editor.modified);
    }

    #[test]
    fn enter_in_find_mode_moves_to_next_existing_match() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        let mut editor = editor_with_lines(&["foo foo foo"]);
        editor.find_mode = FindReplaceMode::Find;
        editor.find_input = "foo".to_string();
        editor.find_term = "foo".to_string();
        editor.find_cursor_pos = 3;
        editor.perform_find();
        app.editor_state = Some(editor);

        handle_input(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        let editor = app.editor_state.as_ref().unwrap();
        assert_eq!(editor.current_match, 1);
        assert_eq!(editor.selection.unwrap().normalized(), (0, 4, 0, 7));
    }

    #[test]
    fn enter_in_find_mode_searches_when_input_changed() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        let mut editor = editor_with_lines(&["foo bar foo"]);
        editor.find_mode = FindReplaceMode::Find;
        editor.find_input = "foo".to_string();
        editor.find_term = "foo".to_string();
        editor.find_cursor_pos = 3;
        editor.perform_find();
        editor.find_input = "bar".to_string();
        editor.find_cursor_pos = 3;
        app.editor_state = Some(editor);

        handle_input(&mut app, KeyCode::Enter, KeyModifiers::NONE);

        let editor = app.editor_state.as_ref().unwrap();
        assert_eq!(editor.find_term, "bar");
        assert_eq!(editor.current_match, 0);
        assert_eq!(editor.selection.unwrap().normalized(), (0, 4, 0, 7));
    }

    #[test]
    fn replace_all_literal_mode_keeps_replacement_dollars_literal() {
        let mut editor = editor_with_lines(&["foo foo"]);
        editor.find_input = "foo".to_string();
        editor.find_term = "foo".to_string();
        editor.replace_input = "$1".to_string();

        editor.replace_all();

        assert_eq!(editor.lines[0], "$1 $1");
    }

    #[test]
    fn replace_current_regex_mode_expands_capture_groups() {
        let mut editor = editor_with_lines(&["foo123"]);
        editor.find_input = r"foo(\d+)".to_string();
        editor.find_term = editor.find_input.clone();
        editor.replace_input = "bar$1".to_string();
        editor.find_options.use_regex = true;
        editor.perform_find();

        editor.replace_current();

        assert_eq!(editor.lines[0], "bar123");
    }

    #[test]
    fn regex_whole_word_applies_to_entire_pattern() {
        let mut editor = editor_with_lines(&["foo xbar bar foo_bar"]);
        editor.find_input = "foo|bar".to_string();
        editor.find_term = editor.find_input.clone();
        editor.find_options.use_regex = true;
        editor.find_options.whole_word = true;

        editor.perform_find();

        assert_eq!(editor.match_positions, vec![(0, 0, 3), (0, 9, 12)]);
    }

    #[test]
    fn remote_dirty_keeps_modified_state_until_upload_success() {
        let mut editor = editor_with_lines(&["saved"]);
        editor.remote_origin = Some(RemoteEditOrigin {
            panel_index: 2,
            remote_path: "/remote/file.txt".to_string(),
        });

        editor.remote_dirty = true;
        editor.update_modified();
        assert!(editor.modified);

        editor.remote_dirty = false;
        editor.update_modified();
        assert!(!editor.modified);

        editor.lines[0] = "changed".to_string();
        editor.update_modified();
        assert!(editor.modified);
    }

    #[test]
    fn shift_right_at_line_end_does_not_select_previous_character() {
        let mut editor = editor_with_lines(&["abc"]);
        editor.cursor_col = 3;

        editor.move_cursor(0, 1, true);

        assert!(!editor.has_selection_range());
        assert_eq!(editor.get_selected_text(), "");
    }

    fn editor_with_ss4_selection() -> EditorState {
        let mut editor = editor_with_lines(&["abcde", "uf"]);
        editor.cursor_line = 0;
        editor.cursor_col = 3;
        editor.selection = Some(Selection {
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 4,
        });
        editor
    }

    #[test]
    fn shift_right_delete_from_ss4_selection_expands_through_line_boundary() {
        let mut editor = editor_with_ss4_selection();
        editor.move_cursor(0, 1, true);
        assert_eq!(editor.get_selected_text(), "abcde");
        editor.delete_forward();
        assert_eq!(editor.lines, vec!["", "uf"]);

        let mut editor = editor_with_ss4_selection();
        for _ in 0..2 {
            editor.move_cursor(0, 1, true);
        }
        assert_eq!(editor.get_selected_text(), "abcde\n");
        editor.delete_forward();
        assert_eq!(editor.lines, vec!["uf"]);

        let mut editor = editor_with_ss4_selection();
        for _ in 0..3 {
            editor.move_cursor(0, 1, true);
        }
        assert_eq!(editor.get_selected_text(), "abcde\nu");
        editor.delete_forward();
        assert_eq!(editor.lines, vec!["f"]);
    }

    #[test]
    fn shift_right_crossing_line_start_does_not_select_next_char_early() {
        let mut editor = editor_with_ss4_selection();
        for _ in 0..2 {
            editor.move_cursor(0, 1, true);
        }

        assert_eq!(editor.cursor_line, 1);
        assert_eq!(editor.cursor_col, 0);
        assert_eq!(editor.get_selected_text(), "abcde\n");
        editor.move_cursor(0, 1, true);
        assert_eq!(editor.get_selected_text(), "abcde\nu");
    }

    #[test]
    fn shift_home_at_line_start_does_not_select_first_character() {
        let mut editor = editor_with_lines(&["abc"]);
        editor.cursor_col = 0;

        editor.move_to_line_start(true);

        assert!(!editor.has_selection_range());
        assert_eq!(editor.get_selected_text(), "");
    }

    #[test]
    fn line_operations_use_clamped_selection_ranges() {
        let mut editor = editor_with_lines(&["first", "second"]);
        editor.selection = Some(Selection {
            start_line: 0,
            start_col: 0,
            end_line: 99,
            end_col: 99,
        });

        editor.indent();

        assert_eq!(editor.lines, vec!["    first", "    second"]);
    }

    #[test]
    fn select_next_occurrence_skips_embedded_non_word_candidate() {
        let mut editor = editor_with_lines(&["foo foobar foo"]);

        editor.select_next_occurrence();
        editor.select_next_occurrence();

        assert_eq!(editor.selection.unwrap().normalized(), (0, 11, 0, 14));
    }

    #[test]
    fn delete_line_on_only_line_clears_content_and_can_undo() {
        let mut editor = editor_with_lines(&["only"]);
        editor.cursor_col = 2;

        editor.delete_line();

        assert_eq!(editor.lines, vec![""]);
        assert_eq!(editor.cursor_col, 0);

        editor.undo();
        assert_eq!(editor.lines, vec!["only"]);
    }

    #[test]
    fn insert_line_below_uses_char_column_for_unicode_indent() {
        let mut editor = editor_with_lines(&["　abc"]);

        editor.insert_line_below();
        assert_eq!(editor.cursor_col, 1);
        editor.insert_char('x');

        assert_eq!(editor.lines[1], "　x");
    }

    #[test]
    fn insert_line_above_uses_char_column_for_unicode_indent() {
        let mut editor = editor_with_lines(&["　abc"]);

        editor.insert_line_above();
        assert_eq!(editor.cursor_col, 1);
        editor.insert_char('x');

        assert_eq!(editor.lines[0], "　x");
    }

    #[test]
    fn outdent_without_indent_is_noop_not_modified() {
        let mut editor = editor_with_lines(&["plain", "text"]);
        editor.outdent();

        assert_eq!(editor.lines, vec!["plain", "text"]);
        assert!(!editor.modified);
        assert!(editor.undo_stack.is_empty());

        editor.selection = Some(Selection {
            start_line: 0,
            start_col: 0,
            end_line: 1,
            end_col: 4,
        });
        editor.outdent();

        assert_eq!(editor.lines, vec!["plain", "text"]);
        assert!(!editor.modified);
        assert!(editor.undo_stack.is_empty());
    }

    #[test]
    fn undo_clamps_cursor_after_removing_current_line() {
        let mut editor = editor_with_lines(&["a"]);

        editor.duplicate_line();
        assert_eq!(editor.cursor_line, 1);

        editor.undo();

        assert_eq!(editor.lines, vec!["a"]);
        assert_eq!(editor.cursor_line, 0);
        editor.insert_char('x');
        assert_eq!(editor.lines, vec!["xa"]);
    }

    #[test]
    fn home_end_update_horizontal_scroll() {
        let mut editor = editor_with_lines(&["abcdefghij"]);
        editor.visible_width = 5;

        editor.move_to_line_end(false);
        assert_eq!(editor.cursor_col, 10);
        assert!(editor.horizontal_scroll > 0);

        editor.move_to_line_start(false);
        assert_eq!(editor.cursor_col, 0);
        assert_eq!(editor.horizontal_scroll, 0);
    }

    #[test]
    fn select_next_occurrence_resets_stale_word_selection() {
        let mut editor = editor_with_lines(&["foo bar bar"]);
        editor.select_next_occurrence();
        assert_eq!(editor.last_word_selection.as_deref(), Some("foo"));

        editor.selection = Some(Selection {
            start_line: 0,
            start_col: 4,
            end_line: 0,
            end_col: 7,
        });
        editor.cursor_col = 7;

        editor.select_next_occurrence();

        assert_eq!(editor.last_word_selection.as_deref(), Some("bar"));
        assert_eq!(editor.selection.unwrap().normalized(), (0, 8, 0, 11));
    }

    #[test]
    fn selected_occurrences_are_replaced_together_when_typing() {
        let mut editor = editor_with_lines(&["foo foo foo"]);

        editor.select_next_occurrence();
        editor.select_next_occurrence();
        assert_eq!(editor.cursors, vec![(0, 3)]);

        editor.insert_char('a');

        assert_eq!(editor.lines, vec!["a a foo"]);
        assert!(editor.selection.is_none());
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 3));
        assert_eq!(editor.cursors, vec![(0, 1)]);

        editor.insert_char('d');

        assert_eq!(editor.lines, vec!["ad ad foo"]);
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 5));
        assert_eq!(editor.cursors, vec![(0, 2)]);

        editor.undo();
        assert_eq!(editor.lines, vec!["a a foo"]);
        editor.undo();
        assert_eq!(editor.lines, vec!["foo foo foo"]);
    }

    #[test]
    fn insertion_multi_cursors_move_together_until_escape() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        let mut editor = editor_with_lines(&["foo foo foo"]);

        editor.select_next_occurrence();
        editor.select_next_occurrence();
        editor.insert_char('a');
        editor.move_cursor(0, -1, false);
        editor.insert_char('x');
        app.editor_state = Some(editor);
        app.current_screen = Screen::FileEditor;

        let editor = app.editor_state.as_ref().unwrap();
        assert_eq!(editor.lines, vec!["xa xa foo"]);
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 4));
        assert_eq!(editor.cursors, vec![(0, 1)]);

        handle_input(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        let editor = app.editor_state.as_ref().unwrap();
        assert_eq!(app.current_screen, Screen::FileEditor);
        assert!(editor.selection.is_none());
        assert!(editor.cursors.is_empty());
        assert!(editor.last_word_selection.is_none());
    }

    #[test]
    fn insertion_multi_cursors_backspace_together() {
        let mut editor = editor_with_lines(&["foo foo foo"]);

        editor.select_next_occurrence();
        editor.select_next_occurrence();
        editor.insert_char('a');
        editor.insert_char('d');
        editor.delete_backward();

        assert_eq!(editor.lines, vec!["a a foo"]);
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 3));
        assert_eq!(editor.cursors, vec![(0, 1)]);
    }

    #[test]
    fn insertion_multi_cursor_delete_preserves_active_boundary_cursor() {
        let mut editor = editor_with_lines(&["ab cd"]);
        editor.cursor_col = 5;
        editor.cursors = vec![(0, 2)];

        editor.delete_forward();

        assert_eq!(editor.lines, vec!["abcd"]);
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 4));
        assert_eq!(editor.cursors, vec![(0, 2)]);
    }

    #[test]
    fn selected_occurrences_are_deleted_together() {
        let mut editor = editor_with_lines(&["foo foo foo"]);

        editor.select_next_occurrence();
        editor.select_next_occurrence();
        editor.delete_forward();

        assert_eq!(editor.lines, vec!["  foo"]);
        editor.undo();
        assert_eq!(editor.lines, vec!["foo foo foo"]);
    }

    #[test]
    fn paste_in_find_mode_updates_find_input_without_editing_buffer() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        let mut editor = editor_with_lines(&["body"]);
        editor.find_mode = FindReplaceMode::Find;
        editor.find_input = "bo".to_string();
        editor.find_cursor_pos = 2;
        app.editor_state = Some(editor);

        handle_paste(&mut app, "dy");

        let editor = app.editor_state.as_ref().unwrap();
        assert_eq!(editor.find_input, "body");
        assert_eq!(editor.lines, vec!["body"]);
        assert!(!editor.modified);
    }

    #[test]
    fn paste_in_replace_mode_updates_replace_input_without_newlines() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        let mut editor = editor_with_lines(&["body"]);
        editor.find_mode = FindReplaceMode::Replace;
        editor.input_focus = 1;
        editor.replace_input = "new".to_string();
        editor.replace_cursor_pos = 3;
        app.editor_state = Some(editor);

        handle_paste(&mut app, "\r\nline");

        let editor = app.editor_state.as_ref().unwrap();
        assert_eq!(editor.replace_input, "newline");
        assert_eq!(editor.lines, vec!["body"]);
        assert!(!editor.modified);
    }

    #[test]
    fn paste_in_goto_mode_keeps_digits_in_goto_input() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        let mut editor = editor_with_lines(&["one", "two", "three"]);
        editor.goto_mode = true;
        app.editor_state = Some(editor);

        handle_paste(&mut app, "12x\n3");

        let editor = app.editor_state.as_ref().unwrap();
        assert_eq!(editor.goto_input, "123");
        assert_eq!(editor.lines, vec!["one", "two", "three"]);
        assert!(!editor.modified);
    }

    #[test]
    fn word_wrap_scrolls_within_long_logical_line_to_keep_cursor_visible() {
        let mut editor = editor_with_lines(&["abcdefghijklmnopqrstuvwxyz"]);
        editor.word_wrap = true;
        editor.visible_width = 5;
        editor.visible_height = 3;
        editor.cursor_col = editor.lines[0].chars().count();

        editor.update_scroll();

        let cursor_segment = editor
            .wrap_segment_index_for_visual_col(editor.cursor_line, editor.cursor_visual_col());
        assert_eq!(editor.scroll, 0);
        assert!(editor.wrap_scroll_offset > 0);
        assert!(editor.cursor_visual_row_from_wrap_top(cursor_segment) < editor.visible_height);

        editor.cursor_col = 0;
        editor.update_scroll();
        assert_eq!(editor.wrap_scroll_offset, 0);
    }

    #[test]
    fn toggle_comment_respects_indentation() {
        let mut editor = editor_with_lines(&["    // value"]);
        editor.language = Language::Rust;

        editor.toggle_comment();
        assert_eq!(editor.lines, vec!["    value"]);

        editor.toggle_comment();
        assert_eq!(editor.lines, vec!["    // value"]);
    }

    #[test]
    fn bracket_match_updates_after_editing_before_bracket() {
        let mut editor = editor_with_lines(&["(x)"]);
        editor.cursor_col = 0;
        editor.update_scroll();
        assert_eq!(editor.matching_bracket, Some((0, 2)));

        editor.insert_char('a');

        assert_eq!(editor.cursor_col, 1);
        assert_eq!(editor.matching_bracket, Some((0, 3)));
    }

    #[test]
    fn stale_remote_upload_result_does_not_clear_newer_local_save() {
        let mut editor = editor_with_lines(&["saved"]);
        editor.remote_origin = Some(RemoteEditOrigin {
            panel_index: 2,
            remote_path: "/remote/file.txt".to_string(),
        });

        let first_generation = editor.begin_remote_save();
        let second_generation = editor.begin_remote_save();

        assert!(editor.remote_dirty);
        assert!(!editor.apply_remote_save_result(2, "/remote/file.txt", first_generation, false));
        assert!(editor.remote_dirty);
        assert!(editor.modified);

        assert!(editor.apply_remote_save_result(2, "/remote/file.txt", second_generation, false));
        assert!(!editor.remote_dirty);
    }

    #[test]
    fn remote_save_result_ignores_same_generation_for_different_remote_file() {
        let mut editor = editor_with_lines(&["saved"]);
        editor.remote_origin = Some(RemoteEditOrigin {
            panel_index: 1,
            remote_path: "/remote/current.txt".to_string(),
        });
        let generation = editor.begin_remote_save();

        assert!(!editor.apply_remote_save_result(1, "/remote/previous.txt", generation, false));
        assert!(editor.remote_dirty);

        assert!(editor.apply_remote_save_result(1, "/remote/current.txt", generation, false));
        assert!(!editor.remote_dirty);
    }
}
