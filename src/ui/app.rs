use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use chrono::{DateTime, Local};

use crate::services::file_ops::{self, FileOperationType, ProgressMessage, FileOperationResult};
use crate::ui::file_viewer::ViewerState;
use crate::ui::file_editor::EditorState;
use crate::ui::file_info::FileInfoState;

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
    LargeImageConfirm,
    TrueColorWarning,
    Progress,
}

/// Clipboard operation type for Ctrl+C/X/V operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardOperation {
    Copy,
    Cut,
}

/// Clipboard state for storing files to copy/move
#[derive(Debug, Clone)]
pub struct Clipboard {
    pub files: Vec<String>,
    pub source_path: PathBuf,
    pub operation: ClipboardOperation,
}

/// File operation progress state for progress dialog
pub struct FileOperationProgress {
    pub operation_type: FileOperationType,
    pub is_active: bool,
    pub cancel_flag: Arc<AtomicBool>,
    receiver: Option<Receiver<ProgressMessage>>,

    // Progress state
    pub current_file: String,
    pub current_file_progress: f64,  // 0.0 ~ 1.0
    pub total_files: usize,
    pub completed_files: usize,
    pub total_bytes: u64,
    pub completed_bytes: u64,

    pub result: Option<FileOperationResult>,

    // Store last error before result is created
    last_error: Option<String>,
}

impl FileOperationProgress {
    pub fn new(operation_type: FileOperationType) -> Self {
        Self {
            operation_type,
            is_active: false,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            receiver: None,
            current_file: String::new(),
            current_file_progress: 0.0,
            total_files: 0,
            completed_files: 0,
            total_bytes: 0,
            completed_bytes: 0,
            result: None,
            last_error: None,
        }
    }

    /// Cancel the ongoing operation
    pub fn cancel(&mut self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
    }

    /// Poll for progress messages. Returns true if still active.
    pub fn poll(&mut self) -> bool {
        if !self.is_active {
            return false;
        }

        if let Some(ref receiver) = self.receiver {
            // Process all available messages
            loop {
                match receiver.try_recv() {
                    Ok(msg) => {
                        match msg {
                            ProgressMessage::FileStarted(name) => {
                                self.current_file = name;
                                self.current_file_progress = 0.0;
                            }
                            ProgressMessage::FileProgress(copied, total) => {
                                if total > 0 {
                                    self.current_file_progress = copied as f64 / total as f64;
                                }
                            }
                            ProgressMessage::FileCompleted(_) => {
                                self.current_file_progress = 1.0;
                            }
                            ProgressMessage::TotalProgress(completed_files, total_files, completed_bytes, total_bytes) => {
                                self.completed_files = completed_files;
                                self.total_files = total_files;
                                self.completed_bytes = completed_bytes;
                                self.total_bytes = total_bytes;
                            }
                            ProgressMessage::Completed(success, failure) => {
                                self.result = Some(FileOperationResult {
                                    success_count: success,
                                    failure_count: failure,
                                    last_error: self.last_error.take(),
                                });
                                self.is_active = false;
                                return false;
                            }
                            ProgressMessage::Error(_, err) => {
                                // Store error for later (result is created on Completed)
                                self.last_error = Some(err);
                            }
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        break;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.is_active = false;
                        return false;
                    }
                }
            }
        }

        self.is_active
    }

    /// Get overall progress as percentage (0.0 ~ 1.0)
    pub fn overall_progress(&self) -> f64 {
        if self.total_bytes > 0 {
            self.completed_bytes as f64 / self.total_bytes as f64
        } else if self.total_files > 0 {
            self.completed_files as f64 / self.total_files as f64
        } else {
            0.0
        }
    }
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
    pub file_info_state: Option<FileInfoState>,

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

    // Pending large image path (for confirmation dialog)
    pub pending_large_image: Option<std::path::PathBuf>,

    // Search result state (재귀 검색 결과)
    pub search_result_state: crate::ui::search_result::SearchResultState,

    // Track previous screen for back navigation
    pub previous_screen: Option<Screen>,

    // Clipboard state for Ctrl+C/X/V operations
    pub clipboard: Option<Clipboard>,

    // File operation progress state
    pub file_operation_progress: Option<FileOperationProgress>,
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
            file_info_state: None,

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
            pending_large_image: None,
            search_result_state: crate::ui::search_result::SearchResultState::default(),
            previous_screen: None,
            clipboard: None,
            file_operation_progress: None,
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
        self.message_timer = 10; // ~1 second at 10 FPS
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
        // Clone necessary data first to avoid borrow issues
        let (file_path, is_directory, is_dotdot) = {
            let panel = self.active_panel();
            if let Some(file) = panel.current_file() {
                (
                    panel.path.join(&file.name),
                    file.is_directory,
                    file.name == "..",
                )
            } else {
                return;
            }
        };

        if is_dotdot {
            self.show_message("Select a file for info");
            return;
        }

        self.info_file_path = file_path.clone();

        // For directories, start async size calculation
        if is_directory {
            let mut state = FileInfoState::new();
            state.start_calculation(&file_path);
            self.file_info_state = Some(state);
        } else {
            self.file_info_state = None;
        }

        self.current_screen = Screen::FileInfo;
    }

    pub fn view_file(&mut self) {
        let panel = self.active_panel();
        if let Some(file) = panel.current_file() {
            if !file.is_directory {
                let path = panel.path.join(&file.name);

                // Check if it's an image file
                if crate::ui::image_viewer::is_image_file(&path) {
                    // Check true color support first
                    if !crate::ui::image_viewer::supports_true_color() {
                        self.pending_large_image = Some(path);
                        self.dialog = Some(Dialog {
                            dialog_type: DialogType::TrueColorWarning,
                            input: String::new(),
                            message: "Terminal doesn't support true color. Open anyway?".to_string(),
                            completion: None,
                            selected_button: 1, // Default to "No"
                        });
                        return;
                    }

                    // Check file size (threshold: 50MB)
                    const LARGE_IMAGE_THRESHOLD: u64 = 50 * 1024 * 1024;
                    let file_size = std::fs::metadata(&path)
                        .map(|m| m.len())
                        .unwrap_or(0);

                    if file_size > LARGE_IMAGE_THRESHOLD {
                        // Show confirmation dialog for large image
                        let size_mb = file_size as f64 / (1024.0 * 1024.0);
                        self.pending_large_image = Some(path);
                        self.dialog = Some(Dialog {
                            dialog_type: DialogType::LargeImageConfirm,
                            input: String::new(),
                            message: format!("This image is {:.1}MB. Open anyway?", size_mb),
                            completion: None,
                            selected_button: 1, // Default to "No"
                        });
                        return;
                    }

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

    /// Execute copy with progress dialog
    pub fn execute_copy_to_with_progress(&mut self, target_path: &Path) {
        let files = self.get_operation_files();
        if files.is_empty() {
            self.show_message("No files selected");
            return;
        }

        let source_path = self.active_panel().path.clone();
        let target_path = target_path.to_path_buf();

        // Create progress state
        let mut progress = FileOperationProgress::new(FileOperationType::Copy);
        progress.is_active = true;
        let cancel_flag = progress.cancel_flag.clone();

        // Create channel for progress messages
        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        // Convert files to PathBuf
        let file_paths: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();

        // Start copy in background thread
        thread::spawn(move || {
            file_ops::copy_files_with_progress(
                file_paths,
                &source_path,
                &target_path,
                cancel_flag,
                tx,
            );
        });

        // Store progress state and show dialog
        self.file_operation_progress = Some(progress);
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Progress,
            input: String::new(),
            message: String::new(),
            completion: None,
            selected_button: 0,
        });
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

    /// Execute move with progress dialog
    pub fn execute_move_to_with_progress(&mut self, target_path: &Path) {
        let files = self.get_operation_files();
        if files.is_empty() {
            self.show_message("No files selected");
            return;
        }

        let source_path = self.active_panel().path.clone();
        let target_path = target_path.to_path_buf();

        // Create progress state
        let mut progress = FileOperationProgress::new(FileOperationType::Move);
        progress.is_active = true;
        let cancel_flag = progress.cancel_flag.clone();

        // Create channel for progress messages
        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        // Convert files to PathBuf
        let file_paths: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();

        // Start move in background thread
        thread::spawn(move || {
            file_ops::move_files_with_progress(
                file_paths,
                &source_path,
                &target_path,
                cancel_flag,
                tx,
            );
        });

        // Store progress state and show dialog
        self.file_operation_progress = Some(progress);
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Progress,
            input: String::new(),
            message: String::new(),
            completion: None,
            selected_button: 0,
        });
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

    // ========== Clipboard operations (Ctrl+C/X/V) ==========

    /// Copy selected files to clipboard (Ctrl+C)
    pub fn clipboard_copy(&mut self) {
        let files = self.get_operation_files();
        if files.is_empty() {
            self.show_message("No files selected");
            return;
        }

        let source_path = self.active_panel().path.clone();
        let count = files.len();

        self.clipboard = Some(Clipboard {
            files,
            source_path,
            operation: ClipboardOperation::Copy,
        });

        self.show_message(&format!("{} file(s) copied to clipboard", count));
    }

    /// Cut selected files to clipboard (Ctrl+X)
    pub fn clipboard_cut(&mut self) {
        let files = self.get_operation_files();
        if files.is_empty() {
            self.show_message("No files selected");
            return;
        }

        let source_path = self.active_panel().path.clone();
        let count = files.len();

        self.clipboard = Some(Clipboard {
            files,
            source_path,
            operation: ClipboardOperation::Cut,
        });

        self.show_message(&format!("{} file(s) cut to clipboard", count));
    }

    /// Paste files from clipboard to current panel (Ctrl+V)
    pub fn clipboard_paste(&mut self) {
        let clipboard = match self.clipboard.take() {
            Some(cb) => cb,
            None => {
                self.show_message("Clipboard is empty");
                return;
            }
        };

        let target_path = self.active_panel().path.clone();

        // Check if source and target are the same (use canonical paths for robustness)
        let is_same_folder = match (clipboard.source_path.canonicalize(), target_path.canonicalize()) {
            (Ok(src), Ok(dest)) => src == dest,
            _ => clipboard.source_path == target_path, // Fallback to direct comparison
        };

        if is_same_folder {
            self.clipboard = Some(clipboard);
            self.show_message("Source and target are the same folder");
            return;
        }

        // Verify source path still exists
        if !clipboard.source_path.exists() {
            self.show_message("Source folder no longer exists");
            return; // Don't restore clipboard - source is gone
        }

        // Verify target is a valid directory
        if !target_path.is_dir() {
            self.clipboard = Some(clipboard);
            self.show_message("Target is not a valid directory");
            return;
        }

        // Get canonical target path for cycle detection
        let canonical_target = target_path.canonicalize().ok();

        // Filter out files that would cause cycle
        let mut valid_files: Vec<String> = Vec::new();
        for file_name in &clipboard.files {
            let src = clipboard.source_path.join(file_name);

            // Check for copying/moving directory into itself
            if let (Some(ref target_canon), Ok(src_canon)) = (&canonical_target, src.canonicalize()) {
                if src.is_dir() && target_canon.starts_with(&src_canon) {
                    self.show_message(&format!("Cannot copy '{}' into itself", file_name));
                    continue;
                }
            }
            valid_files.push(file_name.clone());
        }

        if valid_files.is_empty() {
            self.clipboard = Some(clipboard);
            return;
        }

        // Determine operation type for progress
        let operation_type = match clipboard.operation {
            ClipboardOperation::Copy => FileOperationType::Copy,
            ClipboardOperation::Cut => FileOperationType::Move,
        };

        // Create progress state
        let mut progress = FileOperationProgress::new(operation_type);
        progress.is_active = true;
        let cancel_flag = progress.cancel_flag.clone();

        // Create channel for progress messages
        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        // Convert files to PathBuf
        let file_paths: Vec<PathBuf> = valid_files.iter().map(PathBuf::from).collect();
        let source_path = clipboard.source_path.clone();

        // Start operation in background thread
        let clipboard_operation = clipboard.operation;
        thread::spawn(move || {
            match clipboard_operation {
                ClipboardOperation::Copy => {
                    file_ops::copy_files_with_progress(
                        file_paths,
                        &source_path,
                        &target_path,
                        cancel_flag,
                        tx,
                    );
                }
                ClipboardOperation::Cut => {
                    file_ops::move_files_with_progress(
                        file_paths,
                        &source_path,
                        &target_path,
                        cancel_flag,
                        tx,
                    );
                }
            }
        });

        // Store progress state and show dialog
        self.file_operation_progress = Some(progress);
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Progress,
            input: String::new(),
            message: String::new(),
            completion: None,
            selected_button: 0,
        });

        // Keep clipboard for copy operations (can paste multiple times)
        // Clear clipboard for cut operations (files are moved)
        if clipboard.operation == ClipboardOperation::Copy {
            self.clipboard = Some(clipboard);
        }
    }

    /// Check if clipboard has content
    pub fn has_clipboard(&self) -> bool {
        self.clipboard.is_some()
    }

    /// Get clipboard info for status display
    pub fn clipboard_info(&self) -> Option<(usize, &str)> {
        self.clipboard.as_ref().map(|cb| {
            let op = match cb.operation {
                ClipboardOperation::Copy => "copy",
                ClipboardOperation::Cut => "cut",
            };
            (cb.files.len(), op)
        })
    }

    pub fn execute_open_large_image(&mut self) {
        if let Some(path) = self.pending_large_image.take() {
            self.image_viewer_state = Some(
                crate::ui::image_viewer::ImageViewerState::new(&path)
            );
            self.current_screen = Screen::ImageViewer;
        }
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
        // Security: Check for path traversal attempts
        if path_str.contains("..") {
            // Normalize the path to resolve .. components
            let normalized = if path_str.starts_with('~') {
                dirs::home_dir()
                    .map(|h| h.join(path_str[1..].trim_start_matches('/')))
                    .unwrap_or_else(|| PathBuf::from(path_str))
            } else if PathBuf::from(path_str).is_absolute() {
                PathBuf::from(path_str)
            } else {
                self.active_panel().path.join(path_str)
            };

            // Canonicalize to resolve all .. components
            match normalized.canonicalize() {
                Ok(canonical) => {
                    let fallback = self.active_panel().path.clone();
                    let valid_path = get_valid_path(&canonical, &fallback);
                    if valid_path != fallback {
                        let panel = self.active_panel_mut();
                        panel.path = valid_path.clone();
                        panel.selected_index = 0;
                        panel.selected_files.clear();
                        panel.load_files();
                        self.show_message(&format!("Moved to: {}", valid_path.display()));
                    } else {
                        self.show_message("Error: Path not found or not accessible");
                    }
                    return;
                }
                Err(_) => {
                    self.show_message("Error: Invalid path");
                    return;
                }
            }
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Counter for unique temp directory names
    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Helper to create a temporary directory for testing
    fn create_temp_dir() -> PathBuf {
        let unique_id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let temp_dir = std::env::temp_dir().join(format!(
            "cokacdir_app_test_{}_{}",
            std::process::id(),
            unique_id
        ));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");
        temp_dir
    }

    /// Helper to cleanup temp directory
    fn cleanup_temp_dir(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }

    // ========== get_valid_path tests ==========

    #[test]
    fn test_get_valid_path_existing() {
        let temp_dir = create_temp_dir();
        let fallback = PathBuf::from("/tmp");

        let result = get_valid_path(&temp_dir, &fallback);
        assert_eq!(result, temp_dir);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_get_valid_path_nonexistent_uses_parent() {
        let temp_dir = create_temp_dir();
        let nonexistent = temp_dir.join("does_not_exist");
        let fallback = PathBuf::from("/tmp");

        let result = get_valid_path(&nonexistent, &fallback);
        assert_eq!(result, temp_dir);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_get_valid_path_fallback() {
        let nonexistent = PathBuf::from("/nonexistent/path/that/does/not/exist");
        let fallback = PathBuf::from("/tmp");

        let result = get_valid_path(&nonexistent, &fallback);
        // Should fall back to /tmp or /
        assert!(result.exists());
    }

    #[test]
    fn test_get_valid_path_root() {
        let root = PathBuf::from("/");
        let fallback = PathBuf::from("/tmp");

        let result = get_valid_path(&root, &fallback);
        assert_eq!(result, root);
    }

    // ========== PanelState tests ==========

    #[test]
    fn test_panel_state_initialization() {
        let temp_dir = create_temp_dir();

        // Create some test files
        fs::write(temp_dir.join("file1.txt"), "content").unwrap();
        fs::write(temp_dir.join("file2.txt"), "content").unwrap();
        fs::create_dir(temp_dir.join("subdir")).unwrap();

        let panel = PanelState::new(temp_dir.clone());

        assert_eq!(panel.path, temp_dir);
        assert!(!panel.files.is_empty());
        assert_eq!(panel.selected_index, 0);
        assert!(panel.selected_files.is_empty());
        assert_eq!(panel.sort_by, SortBy::Name);
        assert_eq!(panel.sort_order, SortOrder::Asc);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_panel_state_has_parent_entry() {
        let temp_dir = create_temp_dir();
        let subdir = temp_dir.join("subdir");
        fs::create_dir_all(&subdir).unwrap();

        let panel = PanelState::new(subdir);

        // Should have ".." entry
        assert!(panel.files.iter().any(|f| f.name == ".."));

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_panel_state_current_file() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("test.txt"), "content").unwrap();

        let panel = PanelState::new(temp_dir.clone());

        let current = panel.current_file();
        assert!(current.is_some());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_panel_state_toggle_sort() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("a.txt"), "content").unwrap();
        fs::write(temp_dir.join("b.txt"), "content").unwrap();

        let mut panel = PanelState::new(temp_dir.clone());

        // Default is Name Asc
        assert_eq!(panel.sort_by, SortBy::Name);
        assert_eq!(panel.sort_order, SortOrder::Asc);

        // Toggle same sort field -> change order
        panel.toggle_sort(SortBy::Name);
        assert_eq!(panel.sort_by, SortBy::Name);
        assert_eq!(panel.sort_order, SortOrder::Desc);

        // Toggle different sort field -> change field, reset to Asc
        panel.toggle_sort(SortBy::Size);
        assert_eq!(panel.sort_by, SortBy::Size);
        assert_eq!(panel.sort_order, SortOrder::Asc);

        cleanup_temp_dir(&temp_dir);
    }

    // ========== App tests ==========

    #[test]
    fn test_app_initialization() {
        let temp_dir = create_temp_dir();
        let left_path = temp_dir.join("left");
        let right_path = temp_dir.join("right");

        fs::create_dir_all(&left_path).unwrap();
        fs::create_dir_all(&right_path).unwrap();

        let app = App::new(left_path.clone(), right_path.clone());

        assert_eq!(app.left_panel.path, left_path);
        assert_eq!(app.right_panel.path, right_path);
        assert_eq!(app.active_panel, PanelSide::Left);
        assert_eq!(app.current_screen, Screen::DualPanel);
        assert!(app.dialog.is_none());
        assert!(app.message.is_none());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_app_switch_panel() {
        let temp_dir = create_temp_dir();
        fs::create_dir_all(temp_dir.join("left")).unwrap();
        fs::create_dir_all(temp_dir.join("right")).unwrap();

        let mut app = App::new(temp_dir.join("left"), temp_dir.join("right"));

        assert_eq!(app.active_panel, PanelSide::Left);

        app.switch_panel();
        assert_eq!(app.active_panel, PanelSide::Right);

        app.switch_panel();
        assert_eq!(app.active_panel, PanelSide::Left);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_app_cursor_movement() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file1.txt"), "").unwrap();
        fs::write(temp_dir.join("file2.txt"), "").unwrap();
        fs::write(temp_dir.join("file3.txt"), "").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        let initial_index = app.active_panel().selected_index;

        app.move_cursor(1);
        assert_eq!(app.active_panel().selected_index, initial_index + 1);

        app.move_cursor(-1);
        assert_eq!(app.active_panel().selected_index, initial_index);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_app_cursor_bounds() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file.txt"), "").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        // Move cursor way past the end
        app.move_cursor(1000);
        let len = app.active_panel().files.len();
        assert!(app.active_panel().selected_index < len);

        // Move cursor way before the start
        app.move_cursor(-1000);
        assert_eq!(app.active_panel().selected_index, 0);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_app_cursor_to_start_end() {
        let temp_dir = create_temp_dir();
        for i in 0..10 {
            fs::write(temp_dir.join(format!("file{}.txt", i)), "").unwrap();
        }

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        app.cursor_to_end();
        let len = app.active_panel().files.len();
        assert_eq!(app.active_panel().selected_index, len - 1);

        app.cursor_to_start();
        assert_eq!(app.active_panel().selected_index, 0);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_app_show_message() {
        let temp_dir = create_temp_dir();
        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        assert!(app.message.is_none());

        app.show_message("Test message");
        assert_eq!(app.message, Some("Test message".to_string()));
        assert!(app.message_timer > 0);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_app_toggle_selection() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file1.txt"), "").unwrap();
        fs::write(temp_dir.join("file2.txt"), "").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        // Move past ".." if present
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        let file_name = app.active_panel().current_file().unwrap().name.clone();

        app.toggle_selection();
        assert!(app.active_panel().selected_files.contains(&file_name));

        // Move back to same file
        app.move_cursor(-1);
        app.toggle_selection();
        assert!(!app.active_panel().selected_files.contains(&file_name));

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_app_get_operation_files() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file1.txt"), "").unwrap();
        fs::write(temp_dir.join("file2.txt"), "").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        // Move past ".."
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        // No selection - returns current file
        let files = app.get_operation_files();
        assert_eq!(files.len(), 1);

        // With selection - returns selected files
        app.toggle_selection();
        let files = app.get_operation_files();
        assert_eq!(files.len(), 1);

        cleanup_temp_dir(&temp_dir);
    }

    // ========== Enum tests ==========

    #[test]
    fn test_panel_side_equality() {
        assert_eq!(PanelSide::Left, PanelSide::Left);
        assert_eq!(PanelSide::Right, PanelSide::Right);
        assert_ne!(PanelSide::Left, PanelSide::Right);
    }

    #[test]
    fn test_sort_by_equality() {
        assert_eq!(SortBy::Name, SortBy::Name);
        assert_eq!(SortBy::Size, SortBy::Size);
        assert_eq!(SortBy::Modified, SortBy::Modified);
    }

    #[test]
    fn test_screen_equality() {
        assert_eq!(Screen::DualPanel, Screen::DualPanel);
        assert_eq!(Screen::FileViewer, Screen::FileViewer);
        assert_ne!(Screen::DualPanel, Screen::Help);
    }

    #[test]
    fn test_dialog_type_equality() {
        assert_eq!(DialogType::Copy, DialogType::Copy);
        assert_eq!(DialogType::Delete, DialogType::Delete);
        assert_ne!(DialogType::Copy, DialogType::Move);
    }

    // ========== Clipboard tests ==========

    #[test]
    fn test_clipboard_copy() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file1.txt"), "content").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        // Move past ".." if present
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        // Copy to clipboard
        app.clipboard_copy();

        assert!(app.clipboard.is_some());
        let clipboard = app.clipboard.as_ref().unwrap();
        assert_eq!(clipboard.operation, ClipboardOperation::Copy);
        assert_eq!(clipboard.files.len(), 1);
        assert_eq!(clipboard.source_path, temp_dir);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_clipboard_cut() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file1.txt"), "content").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        // Move past ".."
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        // Cut to clipboard
        app.clipboard_cut();

        assert!(app.clipboard.is_some());
        let clipboard = app.clipboard.as_ref().unwrap();
        assert_eq!(clipboard.operation, ClipboardOperation::Cut);
        assert_eq!(clipboard.files.len(), 1);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_clipboard_paste_copy() {
        let temp_dir = create_temp_dir();
        let src_dir = temp_dir.join("src");
        let dest_dir = temp_dir.join("dest");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();
        fs::write(src_dir.join("file.txt"), "content").unwrap();

        let mut app = App::new(src_dir.clone(), dest_dir.clone());

        // Move past ".."
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        // Copy to clipboard
        app.clipboard_copy();

        // Switch to right panel (dest)
        app.switch_panel();

        // Paste
        app.clipboard_paste();

        // Wait for async operation to complete
        while app.file_operation_progress.as_ref().map(|p| p.is_active).unwrap_or(false) {
            if let Some(ref mut progress) = app.file_operation_progress {
                progress.poll();
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // File should exist in both locations
        assert!(src_dir.join("file.txt").exists());
        assert!(dest_dir.join("file.txt").exists());

        // Clipboard should still exist (copy can be pasted multiple times)
        assert!(app.clipboard.is_some());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_clipboard_paste_cut() {
        let temp_dir = create_temp_dir();
        let src_dir = temp_dir.join("src");
        let dest_dir = temp_dir.join("dest");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();
        fs::write(src_dir.join("file.txt"), "content").unwrap();

        let mut app = App::new(src_dir.clone(), dest_dir.clone());

        // Move past ".."
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        // Cut to clipboard
        app.clipboard_cut();

        // Switch to right panel (dest)
        app.switch_panel();

        // Paste
        app.clipboard_paste();

        // Wait for async operation to complete
        while app.file_operation_progress.as_ref().map(|p| p.is_active).unwrap_or(false) {
            if let Some(ref mut progress) = app.file_operation_progress {
                progress.poll();
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // File should only exist in destination
        assert!(!src_dir.join("file.txt").exists());
        assert!(dest_dir.join("file.txt").exists());

        // Clipboard should be cleared (cut can only be pasted once)
        assert!(app.clipboard.is_none());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_clipboard_paste_same_folder_rejected() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file.txt"), "content").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        // Move past ".."
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        // Copy to clipboard
        app.clipboard_copy();

        // Try to paste to the same folder
        app.clipboard_paste();

        // Clipboard should still exist (paste was rejected)
        assert!(app.clipboard.is_some());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_clipboard_empty_rejected() {
        let temp_dir = create_temp_dir();
        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        // Clipboard is empty
        assert!(app.clipboard.is_none());

        // Try to paste
        app.clipboard_paste();

        // Should show message but not crash
        assert!(app.message.is_some());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_has_clipboard() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file.txt"), "content").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        assert!(!app.has_clipboard());

        // Move past ".."
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        app.clipboard_copy();
        assert!(app.has_clipboard());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_clipboard_info() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file.txt"), "content").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        assert!(app.clipboard_info().is_none());

        // Move past ".."
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        app.clipboard_copy();
        let info = app.clipboard_info();
        assert!(info.is_some());
        let (count, op) = info.unwrap();
        assert_eq!(count, 1);
        assert_eq!(op, "copy");

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_clipboard_operation_equality() {
        assert_eq!(ClipboardOperation::Copy, ClipboardOperation::Copy);
        assert_eq!(ClipboardOperation::Cut, ClipboardOperation::Cut);
        assert_ne!(ClipboardOperation::Copy, ClipboardOperation::Cut);
    }
}
