use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use chrono::{DateTime, Local};

use crate::services::file_ops;
use crate::ui::file_viewer::ViewerState;
use crate::ui::file_editor::EditorState;

/// Get a valid directory path, falling back to parent directories if needed
pub fn get_valid_path(target_path: &Path, fallback: &Path) -> PathBuf {
    let mut current = target_path.to_path_buf();

    loop {
        if current.is_dir() {
            // Check if we can actually read the directory
            if fs::read_dir(&current).is_ok() {
                return current;
            }
        }

        // Try parent directory
        if let Some(parent) = current.parent() {
            if parent == current {
                // Reached root, use fallback
                break;
            }
            current = parent.to_path_buf();
        } else {
            break;
        }
    }

    // Fallback path validation
    if fallback.is_dir() && fs::read_dir(fallback).is_ok() {
        return fallback.to_path_buf();
    }

    // Ultimate fallback to root
    PathBuf::from("/")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelSide {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    Name,
    Size,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum Screen {
    DualPanel,
    FileViewer,
    FileEditor,
    FileInfo,
    ProcessManager,
    Help,
    AIScreen,
    SystemInfo,
    ImageViewer,
    SearchResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogType {
    Copy,
    Move,
    Delete,
    Mkdir,
    Rename,
    Search,
    Goto,
}

#[derive(Debug, Clone, Default)]
pub struct PathCompletion {
    pub suggestions: Vec<String>,  // 자동완성 후보 목록
    pub selected_index: usize,     // 선택된 후보 인덱스
    pub visible: bool,             // 목록 표시 여부
}

#[derive(Debug, Clone)]
pub struct Dialog {
    pub dialog_type: DialogType,
    pub input: String,
    pub message: String,
    pub completion: Option<PathCompletion>,  // 경로 자동완성용
    pub selected_button: usize,  // 버튼 선택 인덱스 (0: Yes, 1: No)
}

#[derive(Debug, Clone)]
pub struct FileItem {
    pub name: String,
    pub is_directory: bool,
    pub size: u64,
    pub modified: DateTime<Local>,
    #[allow(dead_code)]
    pub permissions: String,
}

#[derive(Debug)]
pub struct PanelState {
    pub path: PathBuf,
    pub files: Vec<FileItem>,
    pub selected_index: usize,
    pub selected_files: HashSet<String>,
    pub sort_by: SortBy,
    pub sort_order: SortOrder,
    pub scroll_offset: usize,
    pub pending_focus: Option<String>,
}

impl PanelState {
    pub fn new(path: PathBuf) -> Self {
        // Validate path and get a valid one
        let fallback = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let valid_path = get_valid_path(&path, &fallback);

        let mut state = Self {
            path: valid_path,
            files: Vec::new(),
            selected_index: 0,
            selected_files: HashSet::new(),
            sort_by: SortBy::Name,
            sort_order: SortOrder::Asc,
            scroll_offset: 0,
            pending_focus: None,
        };
        state.load_files();
        state
    }

    pub fn load_files(&mut self) {
        self.files.clear();

        // Add parent directory entry if not at root
        if self.path.parent().is_some() {
            self.files.push(FileItem {
                name: "..".to_string(),
                is_directory: true,
                size: 0,
                modified: Local::now(),
                permissions: String::new(),
            });
        }

        if let Ok(entries) = fs::read_dir(&self.path) {
            // Estimate capacity based on typical directory size
            let entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
            let mut items: Vec<FileItem> = Vec::with_capacity(entries.len());

            items.extend(entries.into_iter().filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let metadata = entry.metadata().ok()?;
                    let is_directory = metadata.is_dir();
                    let size = if is_directory { 0 } else { metadata.len() };
                    let modified = metadata.modified().ok()
                        .map(DateTime::<Local>::from)
                        .unwrap_or_else(Local::now);

                    #[cfg(unix)]
                    let permissions = {
                        use std::os::unix::fs::PermissionsExt;
                        let mode = metadata.permissions().mode();
                        crate::utils::format::format_permissions_short(mode)
                    };
                    #[cfg(not(unix))]
                    let permissions = String::new();

                    Some(FileItem {
                        name,
                        is_directory,
                        size,
                        modified,
                        permissions,
                    })
                }));

            // Sort files
            items.sort_by(|a, b| {
                // Directories always first
                if a.is_directory && !b.is_directory {
                    return std::cmp::Ordering::Less;
                }
                if !a.is_directory && b.is_directory {
                    return std::cmp::Ordering::Greater;
                }

                let cmp = match self.sort_by {
                    SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    SortBy::Size => a.size.cmp(&b.size),
                    SortBy::Modified => a.modified.cmp(&b.modified),
                };

                match self.sort_order {
                    SortOrder::Asc => cmp,
                    SortOrder::Desc => cmp.reverse(),
                }
            });

            self.files.reserve(items.len());
            self.files.extend(items);
        }

        // Handle pending focus (when going to parent directory)
        if let Some(focus_name) = self.pending_focus.take() {
            if let Some(idx) = self.files.iter().position(|f| f.name == focus_name) {
                self.selected_index = idx;
            }
        }

        // Ensure selected_index is within bounds
        if self.selected_index >= self.files.len() && !self.files.is_empty() {
            self.selected_index = self.files.len() - 1;
        }
    }

    pub fn current_file(&self) -> Option<&FileItem> {
        self.files.get(self.selected_index)
    }

    pub fn toggle_sort(&mut self, sort_by: SortBy) {
        if self.sort_by == sort_by {
            self.sort_order = match self.sort_order {
                SortOrder::Asc => SortOrder::Desc,
                SortOrder::Desc => SortOrder::Asc,
            };
        } else {
            self.sort_by = sort_by;
            self.sort_order = SortOrder::Asc;
        }
        self.selected_index = 0;
        self.load_files();
    }
}

pub struct App {
    pub left_panel: PanelState,
    pub right_panel: PanelState,
    pub active_panel: PanelSide,
    pub current_screen: Screen,
    pub dialog: Option<Dialog>,
    pub message: Option<String>,
    pub message_timer: u8,

    // File viewer state (새로운 고급 상태)
    pub viewer_state: Option<ViewerState>,

    // File viewer state (레거시 호환용 - 제거 예정)
    #[allow(dead_code)]
    pub viewer_lines: Vec<String>,
    #[allow(dead_code)]
    pub viewer_scroll: usize,
    #[allow(dead_code)]
    pub viewer_search_term: String,
    #[allow(dead_code)]
    pub viewer_search_mode: bool,
    #[allow(dead_code)]
    pub viewer_search_input: String,
    #[allow(dead_code)]
    pub viewer_match_lines: Vec<usize>,
    #[allow(dead_code)]
    pub viewer_current_match: usize,

    // File editor state (새로운 고급 상태)
    pub editor_state: Option<EditorState>,

    // File editor state (레거시 호환용 - 제거 예정)
    #[allow(dead_code)]
    pub editor_lines: Vec<String>,
    #[allow(dead_code)]
    pub editor_cursor_line: usize,
    #[allow(dead_code)]
    pub editor_cursor_col: usize,
    #[allow(dead_code)]
    pub editor_scroll: usize,
    #[allow(dead_code)]
    pub editor_modified: bool,
    #[allow(dead_code)]
    pub editor_file_path: PathBuf,

    // File info state
    pub info_file_path: PathBuf,

    // Process manager state
    pub processes: Vec<crate::services::process::ProcessInfo>,
    pub process_selected_index: usize,
    pub process_sort_field: crate::services::process::SortField,
    pub process_sort_asc: bool,
    pub process_confirm_kill: Option<i32>,
    pub process_force_kill: bool,

    // AI screen state
    pub ai_state: Option<crate::ui::ai_screen::AIScreenState>,

    // Saved AI session (for persistence between AI screen visits)
    pub saved_ai_history: Vec<crate::ui::ai_screen::HistoryItem>,
    pub saved_ai_session_id: Option<String>,

    // System info state
    pub system_info_state: crate::ui::system_info::SystemInfoState,

    // Advanced search state
    pub advanced_search_state: crate::ui::advanced_search::AdvancedSearchState,

    // Image viewer state
    pub image_viewer_state: Option<crate::ui::image_viewer::ImageViewerState>,

    // Search result state (재귀 검색 결과)
    pub search_result_state: crate::ui::search_result::SearchResultState,

    // Track previous screen for back navigation
    pub previous_screen: Option<Screen>,
}

impl App {
    pub fn new(left_path: PathBuf, right_path: PathBuf) -> Self {
        Self {
            left_panel: PanelState::new(left_path),
            right_panel: PanelState::new(right_path),
            active_panel: PanelSide::Left,
            current_screen: Screen::DualPanel,
            dialog: None,
            message: None,
            message_timer: 0,

            // 새로운 고급 상태
            viewer_state: None,
            editor_state: None,

            // 레거시 호환용
            viewer_lines: Vec::new(),
            viewer_scroll: 0,
            viewer_search_term: String::new(),
            viewer_search_mode: false,
            viewer_search_input: String::new(),
            viewer_match_lines: Vec::new(),
            viewer_current_match: 0,

            editor_lines: vec![String::new()],
            editor_cursor_line: 0,
            editor_cursor_col: 0,
            editor_scroll: 0,
            editor_modified: false,
            editor_file_path: PathBuf::new(),

            info_file_path: PathBuf::new(),

            processes: Vec::new(),
            process_selected_index: 0,
            process_sort_field: crate::services::process::SortField::Cpu,
            process_sort_asc: false,
            process_confirm_kill: None,
            process_force_kill: false,

            ai_state: None,
            saved_ai_history: Vec::new(),
            saved_ai_session_id: None,
            system_info_state: crate::ui::system_info::SystemInfoState::default(),
            advanced_search_state: crate::ui::advanced_search::AdvancedSearchState::default(),
            image_viewer_state: None,
            search_result_state: crate::ui::search_result::SearchResultState::default(),
            previous_screen: None,
        }
    }

    pub fn active_panel_mut(&mut self) -> &mut PanelState {
        match self.active_panel {
            PanelSide::Left => &mut self.left_panel,
            PanelSide::Right => &mut self.right_panel,
        }
    }

    pub fn active_panel(&self) -> &PanelState {
        match self.active_panel {
            PanelSide::Left => &self.left_panel,
            PanelSide::Right => &self.right_panel,
        }
    }

    pub fn target_panel(&self) -> &PanelState {
        match self.active_panel {
            PanelSide::Left => &self.right_panel,
            PanelSide::Right => &self.left_panel,
        }
    }

    pub fn switch_panel(&mut self) {
        self.active_panel = match self.active_panel {
            PanelSide::Left => PanelSide::Right,
            PanelSide::Right => PanelSide::Left,
        };
    }

    /// 패널 전환 시 화면에서의 상대적 위치(줄 번호) 유지, 새 패널의 스크롤은 변경하지 않음
    pub fn switch_panel_keep_index(&mut self) {
        // 현재 패널의 스크롤 오프셋과 선택 인덱스로 화면 내 상대 위치 계산
        let current_scroll = self.active_panel().scroll_offset;
        let current_index = self.active_panel().selected_index;
        let relative_pos = current_index.saturating_sub(current_scroll);

        // 패널 전환
        self.active_panel = match self.active_panel {
            PanelSide::Left => PanelSide::Right,
            PanelSide::Right => PanelSide::Left,
        };

        // 새 패널의 기존 스크롤 오프셋 유지, 같은 화면 위치에 커서 설정
        let new_panel = self.active_panel_mut();
        if !new_panel.files.is_empty() {
            let new_scroll = new_panel.scroll_offset;
            let new_total = new_panel.files.len();

            // 새 패널의 스크롤 오프셋 + 화면 내 상대 위치 = 새 선택 인덱스
            let new_index = new_scroll + relative_pos;
            new_panel.selected_index = new_index.min(new_total.saturating_sub(1));
        }
    }

    pub fn move_cursor(&mut self, delta: i32) {
        let panel = self.active_panel_mut();
        let new_index = (panel.selected_index as i32 + delta)
            .max(0)
            .min(panel.files.len().saturating_sub(1) as i32) as usize;
        panel.selected_index = new_index;
    }

    pub fn cursor_to_start(&mut self) {
        self.active_panel_mut().selected_index = 0;
    }

    pub fn cursor_to_end(&mut self) {
        let panel = self.active_panel_mut();
        if !panel.files.is_empty() {
            panel.selected_index = panel.files.len() - 1;
        }
    }

    pub fn enter_selected(&mut self) {
        let panel = self.active_panel_mut();
        if let Some(file) = panel.current_file().cloned() {
            if file.is_directory {
                if file.name == ".." {
                    // Go to parent - remember current directory name
                    if let Some(current_name) = panel.path.file_name() {
                        panel.pending_focus = Some(current_name.to_string_lossy().to_string());
                    }
                    if let Some(parent) = panel.path.parent() {
                        panel.path = parent.to_path_buf();
                        panel.selected_index = 0;
                        panel.selected_files.clear();
                        panel.load_files();
                    }
                } else {
                    panel.path = panel.path.join(&file.name);
                    panel.selected_index = 0;
                    panel.selected_files.clear();
                    panel.load_files();
                }
            } else {
                // It's a file - open viewer (text or image)
                self.view_file()
            }
        }
    }

    pub fn go_to_parent(&mut self) {
        let panel = self.active_panel_mut();
        if let Some(current_name) = panel.path.file_name() {
            panel.pending_focus = Some(current_name.to_string_lossy().to_string());
        }
        if let Some(parent) = panel.path.parent() {
            panel.path = parent.to_path_buf();
            panel.selected_index = 0;
            panel.selected_files.clear();
            panel.load_files();
        }
    }

    pub fn toggle_selection(&mut self) {
        let panel = self.active_panel_mut();
        if let Some(file) = panel.current_file() {
            if file.name != ".." {
                let name = file.name.clone();
                if panel.selected_files.contains(&name) {
                    panel.selected_files.remove(&name);
                } else {
                    panel.selected_files.insert(name);
                }
                // Move cursor down
                if panel.selected_index < panel.files.len() - 1 {
                    panel.selected_index += 1;
                }
            }
        }
    }

    pub fn toggle_all_selection(&mut self) {
        let panel = self.active_panel_mut();
        if panel.selected_files.is_empty() {
            // Select all (except ..)
            for file in &panel.files {
                if file.name != ".." {
                    panel.selected_files.insert(file.name.clone());
                }
            }
        } else {
            panel.selected_files.clear();
        }
    }

    pub fn toggle_sort_by_name(&mut self) {
        self.active_panel_mut().toggle_sort(SortBy::Name);
    }

    pub fn toggle_sort_by_size(&mut self) {
        self.active_panel_mut().toggle_sort(SortBy::Size);
    }

    pub fn toggle_sort_by_date(&mut self) {
        self.active_panel_mut().toggle_sort(SortBy::Modified);
    }

    pub fn show_message(&mut self, msg: &str) {
        self.message = Some(msg.to_string());
        self.message_timer = 30; // ~3 seconds at 10 FPS
    }

    pub fn refresh_panels(&mut self) {
        self.left_panel.selected_files.clear();
        self.right_panel.selected_files.clear();
        self.left_panel.load_files();
        self.right_panel.load_files();
    }

    pub fn get_operation_files(&self) -> Vec<String> {
        let panel = self.active_panel();
        if !panel.selected_files.is_empty() {
            panel.selected_files.iter().cloned().collect()
        } else if let Some(file) = panel.current_file() {
            if file.name != ".." {
                vec![file.name.clone()]
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    }

    // Dialog methods
    pub fn show_help(&mut self) {
        self.current_screen = Screen::Help;
    }

    pub fn show_file_info(&mut self) {
        let panel = self.active_panel();
        if let Some(file) = panel.current_file() {
            if file.name != ".." {
                self.info_file_path = panel.path.join(&file.name);
                self.current_screen = Screen::FileInfo;
            } else {
                self.show_message("Select a file for info");
            }
        }
    }

    pub fn view_file(&mut self) {
        let panel = self.active_panel();
        if let Some(file) = panel.current_file() {
            if !file.is_directory {
                let path = panel.path.join(&file.name);

                // Check if it's an image file
                if crate::ui::image_viewer::is_image_file(&path) {
                    self.image_viewer_state = Some(
                        crate::ui::image_viewer::ImageViewerState::new(&path)
                    );
                    self.current_screen = Screen::ImageViewer;
                    return;
                }

                // 새로운 고급 뷰어 사용
                let mut viewer = ViewerState::new();
                match viewer.load_file(&path) {
                    Ok(_) => {
                        self.viewer_state = Some(viewer);
                        self.current_screen = Screen::FileViewer;
                    }
                    Err(e) => {
                        self.show_message(&format!("Cannot read file: {}", e));
                    }
                }
            } else {
                self.show_message("Select a file to view");
            }
        }
    }

    pub fn edit_file(&mut self) {
        let panel = self.active_panel();
        if let Some(file) = panel.current_file() {
            if !file.is_directory {
                let path = panel.path.join(&file.name);

                // 새로운 고급 편집기 사용
                let mut editor = EditorState::new();
                match editor.load_file(&path) {
                    Ok(_) => {
                        self.editor_state = Some(editor);
                        self.current_screen = Screen::FileEditor;
                    }
                    Err(e) => {
                        self.show_message(&format!("Cannot open file: {}", e));
                    }
                }
            } else {
                self.show_message("Select a file to edit");
            }
        }
    }

    pub fn show_copy_dialog(&mut self) {
        let files = self.get_operation_files();
        if files.is_empty() {
            self.show_message("No files selected");
            return;
        }
        let file_list = if files.len() <= 3 {
            files.join(", ")
        } else {
            format!("{} and {} more", files[..2].join(", "), files.len() - 2)
        };
        let target = format!("{}/", self.target_panel().path.display());
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Copy,
            input: target,
            message: file_list.clone(),
            completion: Some(PathCompletion::default()),
            selected_button: 0,
        });
    }

    pub fn show_move_dialog(&mut self) {
        let files = self.get_operation_files();
        if files.is_empty() {
            self.show_message("No files selected");
            return;
        }
        let file_list = if files.len() <= 3 {
            files.join(", ")
        } else {
            format!("{} and {} more", files[..2].join(", "), files.len() - 2)
        };
        let target = format!("{}/", self.target_panel().path.display());
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Move,
            input: target,
            message: file_list.clone(),
            completion: Some(PathCompletion::default()),
            selected_button: 0,
        });
    }

    pub fn show_delete_dialog(&mut self) {
        let files = self.get_operation_files();
        if files.is_empty() {
            self.show_message("No files selected");
            return;
        }
        let file_list = if files.len() <= 3 {
            files.join(", ")
        } else {
            format!("{} and {} more", files[..2].join(", "), files.len() - 2)
        };
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Delete,
            input: String::new(),
            message: format!("Delete {}?", file_list),
            completion: None,
            selected_button: 1,  // 기본값: No (안전을 위해)
        });
    }

    pub fn show_mkdir_dialog(&mut self) {
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Mkdir,
            input: String::new(),
            message: String::new(),
            completion: None,
            selected_button: 0,
        });
    }

    pub fn show_rename_dialog(&mut self) {
        let panel = self.active_panel();
        if let Some(file) = panel.current_file() {
            if file.name != ".." {
                self.dialog = Some(Dialog {
                    dialog_type: DialogType::Rename,
                    input: file.name.clone(),
                    message: String::new(),
                    completion: None,
                    selected_button: 0,
                });
            } else {
                self.show_message("Select a file to rename");
            }
        }
    }

    pub fn show_search_dialog(&mut self) {
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Search,
            input: String::new(),
            message: "Search for:".to_string(),
            completion: None,
            selected_button: 0,
        });
    }

    pub fn show_goto_dialog(&mut self) {
        let current_path = self.active_panel().path.display().to_string();
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Goto,
            input: current_path,
            message: "Go to path:".to_string(),
            completion: Some(PathCompletion::default()),
            selected_button: 0,
        });
    }

    pub fn show_process_manager(&mut self) {
        self.processes = crate::services::process::get_process_list();
        self.process_selected_index = 0;
        self.process_confirm_kill = None;
        self.current_screen = Screen::ProcessManager;
    }

    pub fn show_ai_screen(&mut self) {
        let current_path = self.active_panel().path.display().to_string();

        // Restore saved session if available
        if !self.saved_ai_history.is_empty() || self.saved_ai_session_id.is_some() {
            self.ai_state = Some(crate::ui::ai_screen::AIScreenState::with_session(
                current_path,
                std::mem::take(&mut self.saved_ai_history),
                self.saved_ai_session_id.take(),
            ));
        } else {
            self.ai_state = Some(crate::ui::ai_screen::AIScreenState::new(current_path));
        }
        self.current_screen = Screen::AIScreen;
    }

    /// Save AI session state before leaving AI screen
    pub fn save_ai_session(&mut self) {
        if let Some(ref state) = self.ai_state {
            self.saved_ai_history = state.history.clone();
            self.saved_ai_session_id = state.session_id.clone();
        }
    }

    pub fn show_system_info(&mut self) {
        self.system_info_state = crate::ui::system_info::SystemInfoState::default();
        self.current_screen = Screen::SystemInfo;
    }

    #[allow(dead_code)]
    pub fn show_advanced_search_dialog(&mut self) {
        self.advanced_search_state.active = true;
        self.advanced_search_state.reset();
    }

    pub fn execute_advanced_search(&mut self, criteria: &crate::ui::advanced_search::SearchCriteria) {
        let panel = self.active_panel_mut();
        let mut matched_count = 0;

        panel.selected_files.clear();

        for file in &panel.files {
            if file.name == ".." {
                continue;
            }

            if crate::ui::advanced_search::matches_criteria(
                &file.name,
                file.size,
                file.modified,
                criteria,
            ) {
                panel.selected_files.insert(file.name.clone());
                matched_count += 1;
            }
        }

        if matched_count > 0 {
            self.show_message(&format!("Found {} matching file(s)", matched_count));
        } else {
            self.show_message("No files match the criteria");
        }
    }

    // File operations
    #[allow(dead_code)]
    pub fn execute_copy(&mut self) {
        let target_path = self.target_panel().path.clone();
        self.execute_copy_to(&target_path);
    }

    pub fn execute_copy_to(&mut self, target_path: &Path) {
        let files = self.get_operation_files();
        let source_path = self.active_panel().path.clone();

        let mut success_count = 0;
        let mut last_error = String::new();

        for file_name in &files {
            let src = source_path.join(file_name);
            let dest = target_path.join(file_name);
            match file_ops::copy_file(&src, &dest) {
                Ok(_) => success_count += 1,
                Err(e) => last_error = e.to_string(),
            }
        }

        if success_count == files.len() {
            self.show_message(&format!("Copied {} file(s)", success_count));
        } else {
            self.show_message(&format!("Copied {}/{}. Error: {}", success_count, files.len(), last_error));
        }
        self.refresh_panels();
    }

    #[allow(dead_code)]
    pub fn execute_move(&mut self) {
        let target_path = self.target_panel().path.clone();
        self.execute_move_to(&target_path);
    }

    pub fn execute_move_to(&mut self, target_path: &Path) {
        let files = self.get_operation_files();
        let source_path = self.active_panel().path.clone();

        let mut success_count = 0;
        let mut last_error = String::new();

        for file_name in &files {
            let src = source_path.join(file_name);
            let dest = target_path.join(file_name);
            match file_ops::move_file(&src, &dest) {
                Ok(_) => success_count += 1,
                Err(e) => last_error = e.to_string(),
            }
        }

        if success_count == files.len() {
            self.show_message(&format!("Moved {} file(s)", success_count));
        } else {
            self.show_message(&format!("Moved {}/{}. Error: {}", success_count, files.len(), last_error));
        }
        self.refresh_panels();
    }

    pub fn execute_delete(&mut self) {
        let files = self.get_operation_files();
        let source_path = self.active_panel().path.clone();

        let mut success_count = 0;
        let mut last_error = String::new();

        for file_name in &files {
            let path = source_path.join(file_name);
            match file_ops::delete_file(&path) {
                Ok(_) => success_count += 1,
                Err(e) => last_error = e.to_string(),
            }
        }

        if success_count == files.len() {
            self.show_message(&format!("Deleted {} file(s)", success_count));
        } else {
            self.show_message(&format!("Deleted {}/{}. Error: {}", success_count, files.len(), last_error));
        }
        self.refresh_panels();
    }

    pub fn execute_mkdir(&mut self, name: &str) {
        // Validate filename to prevent path traversal attacks
        if let Err(e) = file_ops::is_valid_filename(name) {
            self.show_message(&format!("Error: {}", e));
            return;
        }

        let path = self.active_panel().path.join(name);

        // Additional check: ensure the resulting path is within the current directory
        if let Ok(canonical_parent) = self.active_panel().path.canonicalize() {
            if let Ok(canonical_new) = path.canonicalize().or_else(|_| {
                // For new directories, check the parent path
                path.parent()
                    .and_then(|p| p.canonicalize().ok())
                    .map(|p| p.join(name))
                    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, ""))
            }) {
                if !canonical_new.starts_with(&canonical_parent) {
                    self.show_message("Error: Path traversal attempt detected");
                    return;
                }
            }
        }

        match file_ops::create_directory(&path) {
            Ok(_) => self.show_message(&format!("Created directory: {}", name)),
            Err(e) => self.show_message(&format!("Error: {}", e)),
        }
        self.refresh_panels();
    }

    pub fn execute_rename(&mut self, new_name: &str) {
        // Validate filename to prevent path traversal attacks
        if let Err(e) = file_ops::is_valid_filename(new_name) {
            self.show_message(&format!("Error: {}", e));
            return;
        }

        if let Some(file) = self.active_panel().current_file() {
            let old_path = self.active_panel().path.join(&file.name);
            let new_path = self.active_panel().path.join(new_name);

            // Additional check: ensure the new path stays within the current directory
            if let Ok(canonical_parent) = self.active_panel().path.canonicalize() {
                // For rename, we verify against parent directory
                if let Some(new_parent) = new_path.parent() {
                    if let Ok(canonical_new_parent) = new_parent.canonicalize() {
                        if canonical_new_parent != canonical_parent {
                            self.show_message("Error: Path traversal attempt detected");
                            return;
                        }
                    }
                }
            }

            match file_ops::rename_file(&old_path, &new_path) {
                Ok(_) => self.show_message(&format!("Renamed to: {}", new_name)),
                Err(e) => self.show_message(&format!("Error: {}", e)),
            }
            self.refresh_panels();
        }
    }

    pub fn execute_search(&mut self, term: &str) {
        if term.trim().is_empty() {
            self.show_message("Please enter a search term");
            return;
        }

        // 재귀 검색 수행
        let base_path = self.active_panel().path.clone();
        let results = crate::ui::search_result::execute_recursive_search(
            &base_path,
            term,
            1000,  // 최대 결과 수
        );

        if results.is_empty() {
            self.show_message(&format!("No files found matching \"{}\"", term));
            return;
        }

        // 검색 결과 상태 설정
        self.search_result_state.results = results;
        self.search_result_state.selected_index = 0;
        self.search_result_state.scroll_offset = 0;
        self.search_result_state.search_term = term.to_string();
        self.search_result_state.base_path = base_path;
        self.search_result_state.active = true;

        // 검색 결과 화면으로 전환
        self.current_screen = Screen::SearchResult;
    }

    pub fn execute_goto(&mut self, path_str: &str) {
        let path = if path_str.starts_with('~') {
            dirs::home_dir()
                .map(|h| h.join(path_str[1..].trim_start_matches('/')))
                .unwrap_or_else(|| PathBuf::from(path_str))
        } else {
            let p = PathBuf::from(path_str);
            if p.is_absolute() {
                p
            } else {
                self.active_panel().path.join(path_str)
            }
        };

        // Validate path and find nearest valid parent if necessary
        let fallback = self.active_panel().path.clone();
        let valid_path = get_valid_path(&path, &fallback);

        if valid_path != fallback {
            let panel = self.active_panel_mut();
            panel.path = valid_path.clone();
            panel.selected_index = 0;
            panel.selected_files.clear();
            panel.load_files();

            if valid_path == path {
                self.show_message(&format!("Moved to: {}", valid_path.display()));
            } else {
                self.show_message(&format!("Moved to nearest valid: {}", valid_path.display()));
            }
        } else {
            self.show_message("Error: Path not found or not accessible");
        }
    }

    /// 디렉토리로 이동하고 특정 파일에 커서를 위치시킴
    pub fn goto_directory_with_focus(&mut self, dir: &Path, filename: Option<String>) {
        let panel = self.active_panel_mut();
        panel.path = dir.to_path_buf();
        panel.selected_index = 0;
        panel.selected_files.clear();
        panel.pending_focus = filename;
        panel.load_files();
    }

    /// 검색 결과에서 선택한 항목의 경로로 이동
    pub fn goto_search_result(&mut self) {
        if let Some(item) = self.search_result_state.current_item().cloned() {
            if item.is_directory {
                // 디렉토리인 경우 해당 디렉토리로 이동
                self.goto_directory_with_focus(&item.full_path, None);
            } else {
                // 파일인 경우 부모 디렉토리로 이동하고 해당 파일에 커서
                if let Some(parent) = item.full_path.parent() {
                    self.goto_directory_with_focus(
                        parent,
                        Some(item.name.clone()),
                    );
                }
            }
            // 검색 결과 화면 닫기
            self.search_result_state.active = false;
            self.current_screen = Screen::DualPanel;
            self.show_message(&format!("Moved to: {}", item.relative_path));
        }
    }
}
